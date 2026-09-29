//! Host-owned Local App QA identity, evidence and result persistence.
//!
//! QA is deliberately split into an in-flight session and an immutable
//! historical result.  Only the Host-facing evidence writer can add evidence;
//! model-facing callers receive replayable JSON/image blocks through
//! [`qa_read_evidence`], never trusted base64 strings.  Every document is
//! app/run/build/profile bound and integrity checked before use.

use crate::authoring::canonical_sha256;
use crate::error::AppError;
use crate::ids;
use crate::manifest::{AppLayout, AppRuntimeProfileBinding};
use crate::types::AppRuntimeProfile;
use lingxi_core::host::rooted_fs::{self, AtomicWriteOptions};
use lingxi_core::host::FsError;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Version of Host-private QA documents.
pub const QA_SCHEMA_VERSION: u32 = 1;
/// Maximum persisted session/result/receipt document.
pub const MAX_QA_DOCUMENT_BYTES: u64 = 1024 * 1024;
/// Maximum JSON/text evidence artifact.
pub const MAX_QA_JSON_ARTIFACT_BYTES: usize = 256 * 1024;
/// Maximum image evidence artifact.
pub const MAX_QA_IMAGE_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum aggregate raw artifacts in one QA run.
pub const MAX_QA_RUN_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum immutable QA candidates retained per app after terminal pruning.
pub const MAX_QA_HISTORY_RESULTS: usize = 32;

const QA_DIR: &str = "qa";
const SESSION_FILE: &str = "session.json";
const ARTIFACTS_DIR: &str = "artifacts";
const RESULTS_DIR: &str = "results";
const RECEIPTS_DIR: &str = "receipts";
const PUBLISHED_DIR: &str = "published";
const LEDGERS_DIR: &str = "ledgers";
const SESSIONS_LOCK_FILE: &str = "sessions.lock";
const LEDGER_LOCK_SUFFIX: &str = ".lock";
const HISTORY_LOCK_FILE: &str = "history.lock";
const SCENARIO_FAILURE_PREFIX: &str = "scenario-failed:";

fn non_empty(value: &str, field: &str, max: usize) -> Result<(), AppError> {
    if value.trim().is_empty() {
        return Err(AppError::InvalidRequest(format!(
            "{field} must not be empty"
        )));
    }
    if value.len() > max {
        return Err(AppError::InvalidRequest(format!(
            "{field} exceeds {max} bytes"
        )));
    }
    Ok(())
}

fn token(value: &str, field: &str) -> Result<(), AppError> {
    non_empty(value, field, 256)?;
    if value
        .bytes()
        .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')))
    {
        return Err(AppError::InvalidRequest(format!(
            "{field} contains unsupported characters"
        )));
    }
    Ok(())
}

fn digest(value: &str, field: &str) -> Result<(), AppError> {
    if value.len() != 64
        || value
            .bytes()
            .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
    {
        return Err(AppError::InvalidRequest(format!(
            "{field} must be lowercase sha256"
        )));
    }
    Ok(())
}

fn list<T>(values: &[T], field: &str, max: usize) -> Result<(), AppError> {
    if values.len() > max {
        return Err(AppError::InvalidRequest(format!(
            "{field} has too many entries"
        )));
    }
    Ok(())
}

/// Host-authenticated verification depth for one QA run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaVerificationStrategy {
    /// Fast workflow: tester evidence only.
    Fast,
    /// Balanced workflow: normal tester coverage.
    Balanced,
    /// Thorough workflow: tester candidate followed by an independent verifier.
    Thorough,
}

/// Host-bound identity for one in-flight QA run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaIdentity {
    pub app_id: String,
    pub workflow_run_id: String,
    pub qa_handle: String,
    pub build_id: String,
    pub runtime_profile: AppRuntimeProfileBinding,
    /// Strategy derived from authenticated workflow arguments, never a model
    /// result field.
    pub verification_strategy: QaVerificationStrategy,
    pub dependency_snapshot_sha256: String,
    pub authoring_contract_sha256: String,
    pub manifest_revision: u64,
    pub runtime_generation: u64,
}

impl QaIdentity {
    /// Validate identity fields supplied by the Host binding layer.
    pub fn validate(&self, layout: &AppLayout) -> Result<(), AppError> {
        if self.app_id != layout.app_id() {
            return Err(AppError::InvalidRequest(
                "QA identity app id does not match layout".into(),
            ));
        }
        self.runtime_profile.validate()?;
        token(&self.workflow_run_id, "workflow_run_id")?;
        ids::validate_qa_handle(&self.qa_handle)?;
        token(&self.build_id, "build_id")?;
        digest(
            &self.dependency_snapshot_sha256,
            "dependency_snapshot_sha256",
        )?;
        digest(&self.authoring_contract_sha256, "authoring_contract_sha256")?;
        if self.manifest_revision == 0 || self.runtime_generation == 0 {
            return Err(AppError::InvalidRequest(
                "QA identity revisions must be non-zero".into(),
            ));
        }
        Ok(())
    }

    /// Stable digest used by the immutable result and receipt.
    pub fn sha256(&self) -> Result<String, AppError> {
        canonical_sha256(self, "QA identity")
    }
}

/// A native target fact captured by Host/device integration.  There is no
/// model-facing constructor for this type; [`qa_record_host_evidence`] is the
/// only persistence path and marks its evidence source as Host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaNativeTargetProvenance {
    pub target_id: String,
    pub os: String,
    pub form_factor: String,
    pub device_model: String,
    pub captured_at_ms: u64,
}

impl QaNativeTargetProvenance {
    fn validate(&self) -> Result<(), AppError> {
        token(&self.target_id, "native target_id")?;
        non_empty(&self.os, "native os", 128)?;
        non_empty(&self.form_factor, "native form_factor", 128)?;
        non_empty(&self.device_model, "native device_model", 256)?;
        if self.captured_at_ms == 0 {
            return Err(AppError::InvalidRequest(
                "native target capture time must be non-zero".into(),
            ));
        }
        Ok(())
    }
}

/// Evidence categories accepted by the Host writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaEvidenceKind {
    Inspect,
    UiAction,
    BridgeWrite,
    Query,
    Capture,
    NativeTargetProvenance,
    Console,
    RuntimeError,
}

/// Source marker persisted for every evidence row.  It has one variant on
/// purpose: model-submitted raw evidence has no trusted source representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaEvidenceSource {
    Host,
}

/// Image MIME values accepted for bounded temporary capture storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaImageFormat {
    Png,
    Jpeg,
    Webp,
}

impl QaImageFormat {
    fn suffix(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }
}

/// Host-provided content written to temporary evidence storage.
#[derive(Debug, Clone, PartialEq)]
pub enum QaHostEvidenceContent {
    Json(Value),
    Text(String),
    Image {
        format: QaImageFormat,
        bytes: Vec<u8>,
    },
    NativeTarget(QaNativeTargetProvenance),
}

/// Host-only evidence input.  `event_id` and `caused_by` make the
/// UI-action→bridge-write→query chain explicit instead of inferring it from
/// report prose.
#[derive(Debug, Clone, PartialEq)]
pub struct QaHostEvidenceInput {
    pub evidence_id: String,
    pub scenario_id: String,
    pub target_id: String,
    pub kind: QaEvidenceKind,
    pub recorded_at_ms: u64,
    pub event_id: String,
    pub caused_by: Option<String>,
    pub content: QaHostEvidenceContent,
}

/// Host-derived acceptance requirements persisted with a QA session.  The
/// model can judge a result, but it cannot add or remove a required target or
/// evidence kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaScenarioRequirement {
    pub scenario_id: String,
    pub required: bool,
    pub target_ids: Vec<String>,
    pub evidence_kinds: Vec<QaEvidenceKind>,
    pub motion_required: bool,
}

/// Host-authenticated verification scope for a QA run.
///
/// `declared_target_ids` and the scenario requirements retain the complete
/// authoring matrix. `in_scope_target_ids` is the current device matrix that
/// this run actually exercises; every other declared target must be listed in
/// `unverified_target_ids`. The explicit partition is persisted with the
/// session and result so a partial device run cannot be presented as a global
/// verification.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaVerificationScope {
    pub declared_target_ids: Vec<String>,
    pub in_scope_target_ids: Vec<String>,
    pub unverified_target_ids: Vec<String>,
    pub unverified_scenario_ids: Vec<String>,
}

impl QaVerificationScope {
    fn all(target_ids: Vec<String>) -> Self {
        Self {
            declared_target_ids: target_ids.clone(),
            in_scope_target_ids: target_ids,
            unverified_target_ids: Vec::new(),
            unverified_scenario_ids: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), AppError> {
        list(&self.declared_target_ids, "declared_target_ids", 128)?;
        list(&self.in_scope_target_ids, "in_scope_target_ids", 128)?;
        list(&self.unverified_target_ids, "unverified_target_ids", 128)?;
        list(
            &self.unverified_scenario_ids,
            "unverified_scenario_ids",
            256,
        )?;
        if self.declared_target_ids.is_empty() {
            return Err(AppError::InvalidRequest(
                "QA verification scope must declare at least one target".into(),
            ));
        }
        if self.in_scope_target_ids.is_empty() {
            return Err(AppError::InvalidRequest(
                "QA verification scope must include at least one current target".into(),
            ));
        }
        let mut declared = BTreeSet::new();
        for target in &self.declared_target_ids {
            token(target, "declared target_id")?;
            if !declared.insert(target.as_str()) {
                return Err(AppError::InvalidRequest(
                    "QA verification scope repeats a declared target".into(),
                ));
            }
        }
        let mut in_scope = BTreeSet::new();
        for target in &self.in_scope_target_ids {
            token(target, "in-scope target_id")?;
            if !declared.contains(target.as_str()) || !in_scope.insert(target.as_str()) {
                return Err(AppError::InvalidRequest(
                    "QA in-scope target is unknown or repeated".into(),
                ));
            }
        }
        let mut unverified = BTreeSet::new();
        for target in &self.unverified_target_ids {
            token(target, "unverified target_id")?;
            if !declared.contains(target.as_str())
                || !unverified.insert(target.as_str())
                || in_scope.contains(target.as_str())
            {
                return Err(AppError::InvalidRequest(
                    "QA unverified target is unknown, repeated, or in scope".into(),
                ));
            }
        }
        if in_scope
            .union(&unverified)
            .copied()
            .collect::<BTreeSet<_>>()
            != declared
        {
            return Err(AppError::InvalidRequest(
                "QA verification scope must partition every declared target".into(),
            ));
        }
        let mut unverified_scenarios = BTreeSet::new();
        for scenario in &self.unverified_scenario_ids {
            token(scenario, "unverified scenario_id")?;
            if !unverified_scenarios.insert(scenario.as_str()) {
                return Err(AppError::InvalidRequest(
                    "QA verification scope repeats an unverified scenario".into(),
                ));
            }
        }
        Ok(())
    }

    fn validate_scenarios(&self, requirements: &[QaScenarioRequirement]) -> Result<(), AppError> {
        self.validate()?;
        let required = requirements
            .iter()
            .filter(|requirement| requirement.required)
            .map(|requirement| requirement.scenario_id.as_str())
            .collect::<BTreeSet<_>>();
        if self
            .unverified_scenario_ids
            .iter()
            .any(|scenario| !required.contains(scenario.as_str()))
        {
            return Err(AppError::InvalidRequest(
                "QA scope lists an unknown or optional unverified scenario".into(),
            ));
        }
        Ok(())
    }
}

/// Durable metadata for one evidence artifact; raw bytes are temporary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaArtifactRef {
    pub artifact_id: String,
    pub format: String,
    pub byte_len: u64,
    pub sha256: String,
    pub created_at_ms: u64,
}

/// Durable evidence metadata. Content is replayed from its bounded artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaEvidence {
    pub evidence_id: String,
    pub scenario_id: String,
    pub target_id: String,
    pub kind: QaEvidenceKind,
    pub source: QaEvidenceSource,
    pub recorded_at_ms: u64,
    pub event_id: String,
    pub caused_by: Option<String>,
    pub artifact: QaArtifactRef,
}

/// In-memory result of reading Host-recorded evidence.
#[derive(Debug, Clone, PartialEq)]
pub enum QaEvidenceBlock {
    Json {
        evidence: QaEvidence,
        content: Value,
    },
    Text {
        evidence: QaEvidence,
        content: String,
    },
    Image {
        evidence: QaEvidence,
        format: QaImageFormat,
        bytes: Vec<u8>,
    },
}

/// Failure inherited from build/smoke/native setup. It cannot silently vanish
/// from a final result without a resolving evidence id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaUpstreamFailure {
    pub id: String,
    pub message: String,
    pub introduced_at_ms: u64,
}

impl QaUpstreamFailure {
    fn validate(&self) -> Result<(), AppError> {
        token(&self.id, "upstream failure id")?;
        non_empty(&self.message, "upstream failure message", 16 * 1024)?;
        if self.introduced_at_ms == 0 {
            return Err(AppError::InvalidRequest(
                "upstream failure time must be non-zero".into(),
            ));
        }
        Ok(())
    }
}

/// Scenario status supplied to finalization after Host evidence was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaScenarioStatus {
    Passed,
    Failed,
}

/// One exact scenario judgement. Finalization requires one and only one row
/// for every required scenario id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaScenarioJudgement {
    pub scenario_id: String,
    pub status: QaScenarioStatus,
    pub evidence_ids: Vec<String>,
    pub summary: String,
}

/// A structured QA finding. Existing upstream findings must remain present
/// unless `resolved_by_evidence_ids` names newly recorded Host evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaFinding {
    pub id: String,
    pub message: String,
    pub blocking: bool,
    pub resolved_by_evidence_ids: Vec<String>,
}

/// In-flight Host QA session. Raw evidence bytes never appear here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaSession {
    pub schema_version: u32,
    pub identity: QaIdentity,
    pub required_scenario_ids: Vec<String>,
    pub scenario_requirements: Vec<QaScenarioRequirement>,
    pub target_ids: Vec<String>,
    /// Full authoring target/scenario matrix plus current-device scope.
    pub verification_scope: QaVerificationScope,
    pub started_at_ms: u64,
    /// Every writable native collection from the bound manifest. An empty
    /// list means the app is stateless and therefore has no persistence gate.
    pub required_collections: Vec<String>,
    pub upstream_failures: Vec<QaUpstreamFailure>,
    pub evidence: Vec<QaEvidence>,
    pub finalized_result_id: Option<String>,
}

/// Publication state of a finalized QA result. Finalization only creates a
/// candidate; Host terminal handling may publish it after revalidating the
/// claimed success against the current build/runtime identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaResultStatus {
    Candidate,
    Published,
}

/// Immutable historical QA result. It carries metadata/hashes only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaResult {
    pub schema_version: u32,
    pub status: QaResultStatus,
    pub identity: QaIdentity,
    pub scenario_judgements: Vec<QaScenarioJudgement>,
    pub findings: Vec<QaFinding>,
    pub evidence: Vec<QaEvidence>,
    /// Immutable scope explaining which declared targets/scenarios this
    /// candidate actually verified.
    pub verification_scope: QaVerificationScope,
    /// Previous immutable candidate created from this same evidence session.
    /// Thorough verification appends a candidate instead of rewriting the
    /// tester's result.
    pub previous_result_id: Option<String>,
    pub finalized_at_ms: u64,
    pub result_sha256: String,
}

/// Host-issued immutable receipt for one finalized result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaReceipt {
    pub receipt_id: String,
    pub app_id: String,
    pub workflow_run_id: String,
    pub qa_handle: String,
    pub result_id: String,
    pub identity_sha256: String,
    pub result_sha256: String,
    pub issued_at_ms: u64,
}

/// Immutable publication marker. The candidate result and receipt are never
/// rewritten when terminal Host handling publishes a success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct QaPublicationMarker {
    pub result_id: String,
    pub result_sha256: String,
    pub identity_sha256: String,
    pub published_at_ms: u64,
}

/// Return value of [`qa_finalize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QaFinalizeOutput {
    pub result: QaResult,
    pub receipt: QaReceipt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct QaLedgerFinding {
    finding: QaFinding,
    introduced_at_ms: u64,
    /// Scenario failures are Host-derived blockers rather than model-authored
    /// findings. Keep their scope and the evidence that introduced them so a
    /// later QA handle can resolve only the same scenario/target with fresh
    /// Host evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scenario_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    evidence_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    qa_handle: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct QaFindingLedger {
    schema_version: u32,
    workflow_run_id: String,
    findings: Vec<QaLedgerFinding>,
}

/// Publish a candidate result after the Host has revalidated its identity.
/// This intentionally does not accept a caller-supplied build/runtime value;
/// the Host must perform those checks before invoking this state transition.
pub fn publish_qa_result(layout: &AppLayout, result_id: &str) -> Result<QaResult, AppError> {
    let _history_guard = lock_history(layout)?;
    let result = load_qa_result(layout, result_id)?;
    if load_qa_publication_marker(layout, result_id)?.is_some() {
        return Ok(result);
    }
    if result.status != QaResultStatus::Candidate {
        return Err(AppError::WorkflowStateInvalid(
            "QA result is already published".into(),
        ));
    }
    let session = load_qa_session(layout, &result.identity.qa_handle)?;
    if session.finalized_result_id.as_deref() != Some(result_id) {
        return Err(AppError::WorkflowStateInvalid(
            "only the latest QA candidate for a sealed session can be published".into(),
        ));
    }
    let marker = QaPublicationMarker {
        result_id: result_id.to_owned(),
        result_sha256: result.result_sha256.clone(),
        identity_sha256: result.identity.sha256()?,
        published_at_ms: result.finalized_at_ms,
    };
    persist(
        layout,
        &publication_path(layout, result_id)?,
        &qa_root(layout).join(PUBLISHED_DIR),
        &marker,
        "QA publication marker",
        false,
    )?;
    Ok(result)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct QaDocument<T> {
    schema_version: u32,
    app_id: String,
    payload: T,
    payload_sha256: String,
}

fn qa_root(layout: &AppLayout) -> PathBuf {
    layout.app_dir_rel().join(QA_DIR)
}

fn session_path(layout: &AppLayout, handle: &str) -> Result<PathBuf, AppError> {
    ids::validate_qa_handle(handle)?;
    Ok(qa_root(layout).join(handle).join(SESSION_FILE))
}

fn artifact_path(
    layout: &AppLayout,
    handle: &str,
    artifact: &QaArtifactRef,
) -> Result<PathBuf, AppError> {
    ids::validate_qa_handle(handle)?;
    validate_artifact_ref(artifact)?;
    let suffix = artifact.format.as_str();
    Ok(qa_root(layout)
        .join(handle)
        .join(ARTIFACTS_DIR)
        .join(format!("{}.{}", artifact.artifact_id, suffix)))
}

fn artifact_limit(format: &str) -> Result<u64, AppError> {
    match format {
        "json" | "txt" => Ok(MAX_QA_JSON_ARTIFACT_BYTES as u64),
        "png" | "jpg" | "webp" => Ok(MAX_QA_IMAGE_ARTIFACT_BYTES as u64),
        other => Err(AppError::StorageCorrupt(format!(
            "unknown QA artifact format {other:?}"
        ))),
    }
}

fn validate_artifact_ref(artifact: &QaArtifactRef) -> Result<(), AppError> {
    token(&artifact.artifact_id, "artifact_id")?;
    if artifact.byte_len == 0 || artifact.byte_len > artifact_limit(&artifact.format)? {
        return Err(AppError::StorageCorrupt(
            "QA artifact byte length is outside its format limit".into(),
        ));
    }
    digest(&artifact.sha256, "artifact sha256")?;
    if artifact.created_at_ms == 0 {
        return Err(AppError::StorageCorrupt(
            "QA artifact creation time must be non-zero".into(),
        ));
    }
    Ok(())
}

fn result_path(layout: &AppLayout, result_id: &str) -> Result<PathBuf, AppError> {
    token(result_id, "result_id")?;
    Ok(qa_root(layout)
        .join(RESULTS_DIR)
        .join(format!("{result_id}.json")))
}

fn receipt_path(layout: &AppLayout, receipt_id: &str) -> Result<PathBuf, AppError> {
    token(receipt_id, "receipt_id")?;
    Ok(qa_root(layout)
        .join(RECEIPTS_DIR)
        .join(format!("{receipt_id}.json")))
}

fn publication_path(layout: &AppLayout, result_id: &str) -> Result<PathBuf, AppError> {
    token(result_id, "result_id")?;
    Ok(qa_root(layout)
        .join(PUBLISHED_DIR)
        .join(format!("{result_id}.json")))
}

fn sessions_lock_path(layout: &AppLayout) -> PathBuf {
    qa_root(layout).join(SESSIONS_LOCK_FILE)
}

fn ledger_path(layout: &AppLayout, workflow_run_id: &str) -> Result<PathBuf, AppError> {
    token(workflow_run_id, "workflow_run_id")?;
    Ok(qa_root(layout)
        .join(LEDGERS_DIR)
        .join(format!("{workflow_run_id}.json")))
}

fn ledger_lock_path(layout: &AppLayout, workflow_run_id: &str) -> Result<PathBuf, AppError> {
    token(workflow_run_id, "workflow_run_id")?;
    Ok(qa_root(layout)
        .join(LEDGERS_DIR)
        .join(format!("{workflow_run_id}{LEDGER_LOCK_SUFFIX}")))
}

fn lock_sessions(
    layout: &AppLayout,
) -> Result<lingxi_core::host::rooted_fs::RootedFileLock, AppError> {
    layout.initialize()?;
    let parent = qa_root(layout);
    rooted_fs::ensure_private_directory(layout.root(), &parent, 0o700)
        .map_err(|error| AppError::from_fs("create QA root directory", &error))?;
    rooted_fs::lock_exclusive(layout.root(), &sessions_lock_path(layout), 0o700, 0o600)
        .map_err(|error| AppError::from_fs("lock QA sessions", &error))
}

fn lock_session(
    layout: &AppLayout,
    handle: &str,
) -> Result<lingxi_core::host::rooted_fs::RootedFileLock, AppError> {
    ids::validate_qa_handle(handle)?;
    lock_sessions(layout)
}

fn lock_ledger(
    layout: &AppLayout,
    workflow_run_id: &str,
) -> Result<lingxi_core::host::rooted_fs::RootedFileLock, AppError> {
    layout.initialize()?;
    let parent = qa_root(layout).join(LEDGERS_DIR);
    rooted_fs::ensure_private_directory(layout.root(), &parent, 0o700)
        .map_err(|error| AppError::from_fs("create QA finding ledger directory", &error))?;
    rooted_fs::lock_exclusive(
        layout.root(),
        &ledger_lock_path(layout, workflow_run_id)?,
        0o700,
        0o600,
    )
    .map_err(|error| AppError::from_fs("lock QA finding ledger", &error))
}

fn lock_history(
    layout: &AppLayout,
) -> Result<lingxi_core::host::rooted_fs::RootedFileLock, AppError> {
    layout.initialize()?;
    let parent = qa_root(layout);
    rooted_fs::ensure_private_directory(layout.root(), &parent, 0o700)
        .map_err(|error| AppError::from_fs("create QA history directory", &error))?;
    rooted_fs::lock_exclusive(
        layout.root(),
        &qa_root(layout).join(HISTORY_LOCK_FILE),
        0o700,
        0o600,
    )
    .map_err(|error| AppError::from_fs("lock QA history", &error))
}

fn persist<T: Serialize>(
    layout: &AppLayout,
    path: &Path,
    parent: &Path,
    payload: &T,
    what: &str,
    overwrite: bool,
) -> Result<(), AppError> {
    let document = QaDocument {
        schema_version: QA_SCHEMA_VERSION,
        app_id: layout.app_id().to_owned(),
        payload,
        payload_sha256: canonical_sha256(payload, what)?,
    };
    let mut body = serde_json::to_vec_pretty(&document)
        .map_err(|error| AppError::Io(format!("serialize {what}: {error}")))?;
    body.push(b'\n');
    if body.len() as u64 > MAX_QA_DOCUMENT_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "{what} exceeds its size limit"
        )));
    }
    layout.initialize()?;
    rooted_fs::ensure_private_directory(layout.root(), parent, 0o700)
        .map_err(|error| AppError::from_fs(&format!("create {what} directory"), &error))?;
    rooted_fs::atomic_write(
        layout.root(),
        path,
        &body,
        AtomicWriteOptions {
            overwrite,
            create_parents: false,
            file_mode: 0o600,
            ..AtomicWriteOptions::default()
        },
    )
    .or_else(|error| {
        if !overwrite && matches!(error, FsError::AlreadyExists(_)) {
            let existing = rooted_fs::read_tail_bytes(layout.root(), path, MAX_QA_DOCUMENT_BYTES)
                .map_err(|read_error| {
                AppError::from_fs(&format!("read existing {what}"), &read_error)
            })?;
            if existing == body {
                return Ok(());
            }
            return Err(AppError::StorageCorrupt(format!(
                "immutable {what} already exists with different bytes"
            )));
        }
        Err(AppError::from_fs(&format!("write {what}"), &error))
    })
}

fn load<T: DeserializeOwned + Serialize>(
    layout: &AppLayout,
    path: &Path,
    what: &str,
) -> Result<QaDocument<T>, AppError> {
    let body = rooted_fs::read_to_string_limited(layout.root(), path, MAX_QA_DOCUMENT_BYTES)
        .map_err(|error| match error {
            FsError::NotFound(_) => AppError::NotFound(format!("{what} not found")),
            other => AppError::from_fs(&format!("read {what}"), &other),
        })?;
    let document: QaDocument<T> = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("{what}: {error}")))?;
    if document.schema_version != QA_SCHEMA_VERSION || document.app_id != layout.app_id() {
        return Err(AppError::StorageCorrupt(format!(
            "{what} schema or app binding is invalid"
        )));
    }
    let actual = canonical_sha256(&document.payload, what)?;
    if document.payload_sha256 != actual {
        return Err(AppError::StorageCorrupt(format!(
            "{what} digest does not match its content"
        )));
    }
    Ok(document)
}

fn raw_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn artifact_bytes(content: &QaHostEvidenceContent) -> Result<(String, Vec<u8>), AppError> {
    match content {
        QaHostEvidenceContent::Json(value) => {
            let bytes = serde_json::to_vec(value).map_err(|error| {
                AppError::InvalidRequest(format!("invalid JSON evidence: {error}"))
            })?;
            if bytes.len() > MAX_QA_JSON_ARTIFACT_BYTES {
                return Err(AppError::InvalidRequest(
                    "JSON evidence exceeds its size limit".into(),
                ));
            }
            Ok(("json".into(), bytes))
        }
        QaHostEvidenceContent::Text(value) => {
            non_empty(value, "text evidence", MAX_QA_JSON_ARTIFACT_BYTES)?;
            Ok(("txt".into(), value.as_bytes().to_vec()))
        }
        QaHostEvidenceContent::Image { format, bytes } => {
            if bytes.is_empty() || bytes.len() > MAX_QA_IMAGE_ARTIFACT_BYTES {
                return Err(AppError::InvalidRequest(
                    "image evidence exceeds its size limit".into(),
                ));
            }
            Ok((format.suffix().into(), bytes.clone()))
        }
        QaHostEvidenceContent::NativeTarget(provenance) => {
            provenance.validate()?;
            let bytes = serde_json::to_vec(provenance).map_err(|error| {
                AppError::InvalidRequest(format!("invalid target evidence: {error}"))
            })?;
            Ok(("json".into(), bytes))
        }
    }
}

fn load_finding_ledger(
    layout: &AppLayout,
    workflow_run_id: &str,
) -> Result<QaFindingLedger, AppError> {
    let path = ledger_path(layout, workflow_run_id)?;
    let ledger = match load::<QaFindingLedger>(layout, &path, "QA finding ledger") {
        Ok(document) => document.payload,
        Err(AppError::NotFound(_)) => QaFindingLedger {
            schema_version: QA_SCHEMA_VERSION,
            workflow_run_id: workflow_run_id.to_owned(),
            findings: Vec::new(),
        },
        Err(error) => return Err(error),
    };
    if ledger.schema_version != QA_SCHEMA_VERSION || ledger.workflow_run_id != workflow_run_id {
        return Err(AppError::StorageCorrupt(
            "QA finding ledger schema or workflow binding is invalid".into(),
        ));
    }
    if ledger.findings.len() > 256 {
        return Err(AppError::StorageCorrupt(
            "QA finding ledger exceeds its entry limit".into(),
        ));
    }
    let mut ids = BTreeSet::new();
    for entry in &ledger.findings {
        validate_finding(&entry.finding)?;
        if !entry.finding.blocking || !entry.finding.resolved_by_evidence_ids.is_empty() {
            return Err(AppError::StorageCorrupt(
                "QA finding ledger contains a resolved finding".into(),
            ));
        }
        if entry.introduced_at_ms == 0 || !ids.insert(entry.finding.id.as_str()) {
            return Err(AppError::StorageCorrupt(
                "QA finding ledger contains an invalid or duplicate entry".into(),
            ));
        }
        if entry.scenario_id.is_some() != entry.target_id.is_some()
            || entry.scenario_id.is_some() != entry.qa_handle.is_some()
        {
            return Err(AppError::StorageCorrupt(
                "QA finding ledger scenario scope is incomplete".into(),
            ));
        }
        if let Some(scenario_id) = &entry.scenario_id {
            token(scenario_id, "ledger scenario_id").map_err(|error| {
                AppError::StorageCorrupt(format!("invalid QA ledger scenario id: {error}"))
            })?;
            token(
                entry.target_id.as_deref().unwrap_or_default(),
                "ledger target_id",
            )
            .map_err(|error| {
                AppError::StorageCorrupt(format!("invalid QA ledger target id: {error}"))
            })?;
            ids::validate_qa_handle(entry.qa_handle.as_deref().unwrap_or_default()).map_err(
                |error| AppError::StorageCorrupt(format!("invalid QA ledger handle: {error}")),
            )?;
            if entry.evidence_ids.is_empty()
                || entry.evidence_ids.iter().collect::<BTreeSet<_>>().len()
                    != entry.evidence_ids.len()
            {
                return Err(AppError::StorageCorrupt(
                    "QA finding ledger scenario evidence is invalid".into(),
                ));
            }
            for evidence_id in &entry.evidence_ids {
                token(evidence_id, "ledger evidence_id").map_err(|error| {
                    AppError::StorageCorrupt(format!("invalid QA ledger evidence id: {error}"))
                })?;
            }
        } else if !entry.evidence_ids.is_empty() {
            return Err(AppError::StorageCorrupt(
                "non-scenario QA ledger finding carries evidence scope".into(),
            ));
        }
    }
    Ok(ledger)
}

fn save_finding_ledger(layout: &AppLayout, ledger: &QaFindingLedger) -> Result<(), AppError> {
    persist(
        layout,
        &ledger_path(layout, &ledger.workflow_run_id)?,
        &qa_root(layout).join(LEDGERS_DIR),
        ledger,
        "QA finding ledger",
        true,
    )
}

fn merge_upstream_failures(
    ledger: &mut QaFindingLedger,
    failures: &[QaUpstreamFailure],
) -> Result<(), AppError> {
    for failure in failures {
        if let Some(existing) = ledger
            .findings
            .iter()
            .find(|entry| entry.finding.id == failure.id)
        {
            if existing.finding.message != failure.message {
                return Err(AppError::InvalidRequest(format!(
                    "upstream QA failure {:?} conflicts with the durable ledger",
                    failure.id
                )));
            }
            continue;
        }
        ledger.findings.push(QaLedgerFinding {
            finding: QaFinding {
                id: failure.id.clone(),
                message: failure.message.clone(),
                blocking: true,
                resolved_by_evidence_ids: Vec::new(),
            },
            introduced_at_ms: failure.introduced_at_ms,
            scenario_id: None,
            target_id: None,
            evidence_ids: Vec::new(),
            qa_handle: None,
        });
    }
    ledger
        .findings
        .sort_by(|left, right| left.finding.id.cmp(&right.finding.id));
    Ok(())
}

/// Start one Host-bound in-flight QA session.
pub fn qa_begin(
    layout: &AppLayout,
    identity: QaIdentity,
    required_scenario_ids: Vec<String>,
    target_ids: Vec<String>,
    upstream_failures: Vec<QaUpstreamFailure>,
    started_at_ms: u64,
) -> Result<QaSession, AppError> {
    let requirements = required_scenario_ids
        .iter()
        .map(|scenario_id| QaScenarioRequirement {
            scenario_id: scenario_id.clone(),
            required: true,
            target_ids: target_ids.clone(),
            evidence_kinds: Vec::new(),
            // A runtime family does not prove that this scenario animates.
            // Callers that know motion is required must use the preferred
            // `qa_begin_with_requirements` API and say so explicitly.
            motion_required: false,
        })
        .collect();
    qa_begin_with_requirements(
        layout,
        identity,
        requirements,
        target_ids,
        upstream_failures,
        started_at_ms,
    )
}

/// Begin QA with Host-persisted acceptance requirements, including exact
/// target/evidence-kind coverage. This is the preferred Host entry point.
pub fn qa_begin_with_requirements(
    layout: &AppLayout,
    identity: QaIdentity,
    scenario_requirements: Vec<QaScenarioRequirement>,
    target_ids: Vec<String>,
    upstream_failures: Vec<QaUpstreamFailure>,
    started_at_ms: u64,
) -> Result<QaSession, AppError> {
    qa_begin_with_scope(
        layout,
        identity,
        scenario_requirements,
        QaVerificationScope::all(target_ids),
        upstream_failures,
        started_at_ms,
    )
}

/// Begin QA with a complete authoring matrix and an authenticated
/// current-device verification scope. The Host owns this scope; callers may
/// not replace it during finalization.
pub fn qa_begin_with_scope(
    layout: &AppLayout,
    identity: QaIdentity,
    scenario_requirements: Vec<QaScenarioRequirement>,
    verification_scope: QaVerificationScope,
    upstream_failures: Vec<QaUpstreamFailure>,
    started_at_ms: u64,
) -> Result<QaSession, AppError> {
    identity.validate(layout)?;
    verification_scope.validate()?;
    if started_at_ms == 0 {
        return Err(AppError::InvalidRequest(
            "QA start time must be non-zero".into(),
        ));
    }
    list(&scenario_requirements, "scenario_requirements", 256)?;
    list(&upstream_failures, "upstream_failures", 256)?;
    let target_ids = verification_scope.declared_target_ids.clone();
    if scenario_requirements.is_empty() {
        return Err(AppError::InvalidRequest(
            "QA requires at least one scenario".into(),
        ));
    }
    if target_ids.is_empty() {
        return Err(AppError::InvalidRequest(
            "QA requires at least one target".into(),
        ));
    }
    let mut targets = BTreeSet::new();
    for target in &target_ids {
        token(target, "target_id")?;
        if !targets.insert(target) {
            return Err(AppError::InvalidRequest("duplicate QA target id".into()));
        }
    }
    let mut scenarios = BTreeSet::new();
    for requirement in &scenario_requirements {
        token(&requirement.scenario_id, "scenario_id")?;
        if !scenarios.insert(&requirement.scenario_id) {
            return Err(AppError::InvalidRequest(
                "duplicate required scenario id".into(),
            ));
        }
        list(&requirement.target_ids, "scenario target_ids", 128)?;
        list(&requirement.evidence_kinds, "scenario evidence_kinds", 8)?;
        if requirement.required && requirement.target_ids.is_empty() {
            return Err(AppError::InvalidRequest(
                "required QA scenario must bind at least one target".into(),
            ));
        }
        let mut requirement_targets = BTreeSet::new();
        for target in &requirement.target_ids {
            token(target, "scenario target_id")?;
            if !requirement_targets.insert(target) {
                return Err(AppError::InvalidRequest(
                    "duplicate scenario target id".into(),
                ));
            }
            if !targets_contains(&target_ids, target) {
                return Err(AppError::InvalidRequest(
                    "scenario requirement target is not bound to this QA session".into(),
                ));
            }
        }
        let unique_kinds = requirement
            .evidence_kinds
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if unique_kinds.len() != requirement.evidence_kinds.len() {
            return Err(AppError::InvalidRequest(
                "duplicate scenario evidence kind".into(),
            ));
        }
    }
    verification_scope.validate_scenarios(&scenario_requirements)?;
    let manifest = crate::manifest::load_manifest(layout)?;
    if manifest.revision != identity.manifest_revision
        || manifest.runtime_profile.as_ref() != Some(&identity.runtime_profile)
        || manifest.dependency_snapshot_hash()? != identity.dependency_snapshot_sha256
    {
        return Err(AppError::InvalidRequest(
            "QA identity does not match the bound manifest/profile/dependency snapshot".into(),
        ));
    }
    let required_collections = manifest
        .collections
        .iter()
        .map(|collection| collection.id.clone())
        .collect::<Vec<_>>();
    for failure in &upstream_failures {
        failure.validate()?;
    }
    let _session_guard = lock_session(layout, &identity.qa_handle)?;
    let path = session_path(layout, &identity.qa_handle)?;
    match rooted_fs::read_to_string_limited(layout.root(), &path, MAX_QA_DOCUMENT_BYTES) {
        Ok(_) => {
            return Err(AppError::WorkflowStateInvalid(
                "QA handle is already in flight".into(),
            ));
        }
        Err(FsError::NotFound(_)) => {}
        Err(error) => return Err(AppError::from_fs("inspect QA session", &error)),
    }
    let unavailable_required_scenario_ids = scenario_requirements
        .iter()
        .filter(|requirement| {
            requirement.required
                && !requirement.target_ids.iter().any(|target| {
                    verification_scope
                        .in_scope_target_ids
                        .iter()
                        .any(|in_scope| in_scope == target)
                })
        })
        .map(|requirement| requirement.scenario_id.clone())
        .collect::<BTreeSet<_>>();
    let declared_unverified_scenarios = verification_scope
        .unverified_scenario_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if declared_unverified_scenarios != unavailable_required_scenario_ids {
        return Err(AppError::InvalidRequest(
            "QA scope must explicitly list exactly the required scenarios unavailable on the current target"
                .into(),
        ));
    }
    let required_scenario_ids = scenario_requirements
        .iter()
        .filter(|requirement| {
            requirement.required
                && !unavailable_required_scenario_ids.contains(&requirement.scenario_id)
        })
        .map(|requirement| requirement.scenario_id.clone())
        .collect::<Vec<_>>();
    if required_scenario_ids.is_empty() {
        return Err(AppError::InvalidRequest(
            "QA requires at least one required scenario".into(),
        ));
    }
    let _ledger_guard = lock_ledger(layout, &identity.workflow_run_id)?;
    let mut ledger = load_finding_ledger(layout, &identity.workflow_run_id)?;
    merge_upstream_failures(&mut ledger, &upstream_failures)?;
    save_finding_ledger(layout, &ledger)?;
    let upstream_failures = ledger
        .findings
        .iter()
        .filter(|entry| entry.scenario_id.is_none())
        .map(|entry| QaUpstreamFailure {
            id: entry.finding.id.clone(),
            message: entry.finding.message.clone(),
            introduced_at_ms: entry.introduced_at_ms,
        })
        .collect();
    let session = QaSession {
        schema_version: QA_SCHEMA_VERSION,
        identity,
        required_scenario_ids,
        scenario_requirements,
        target_ids,
        verification_scope,
        started_at_ms,
        required_collections,
        upstream_failures,
        evidence: Vec::new(),
        finalized_result_id: None,
    };
    persist(
        layout,
        &path,
        &qa_root(layout).join(session.identity.qa_handle.clone()),
        &session,
        "QA session",
        true,
    )?;
    Ok(session)
}

fn targets_contains(targets: &[String], target: &str) -> bool {
    targets.iter().any(|candidate| candidate == target)
}

/// Load the in-flight session. A finalized result is never returned through
/// this API, preserving historical vs in-flight identity separation.
pub fn load_qa_session(layout: &AppLayout, qa_handle: &str) -> Result<QaSession, AppError> {
    let path = session_path(layout, qa_handle)?;
    let session: QaSession = load(layout, &path, "QA session")?.payload;
    session.identity.validate(layout)?;
    if session.identity.qa_handle != qa_handle {
        return Err(AppError::StorageCorrupt(
            "QA session handle is inconsistent".into(),
        ));
    }
    if session.schema_version != QA_SCHEMA_VERSION
        || session.started_at_ms == 0
        || session.required_scenario_ids.is_empty()
        || session.target_ids.is_empty()
    {
        return Err(AppError::StorageCorrupt(
            "QA session has invalid durable bounds".into(),
        ));
    }
    session
        .verification_scope
        .validate_scenarios(&session.scenario_requirements)?;
    if session.verification_scope.declared_target_ids != session.target_ids {
        return Err(AppError::StorageCorrupt(
            "QA session target matrix does not match its verification scope".into(),
        ));
    }
    let required = session
        .scenario_requirements
        .iter()
        .filter(|requirement| {
            requirement.required
                && requirement.target_ids.iter().any(|target| {
                    session
                        .verification_scope
                        .in_scope_target_ids
                        .iter()
                        .any(|in_scope| in_scope == target)
                })
        })
        .map(|requirement| requirement.scenario_id.as_str())
        .collect::<BTreeSet<_>>();
    if required
        != session
            .required_scenario_ids
            .iter()
            .map(String::as_str)
            .collect()
    {
        return Err(AppError::StorageCorrupt(
            "QA session required scenario index is inconsistent".into(),
        ));
    }
    let evidence_ids = session
        .evidence
        .iter()
        .map(|evidence| evidence.evidence_id.as_str())
        .collect::<BTreeSet<_>>();
    let event_ids = session
        .evidence
        .iter()
        .map(|evidence| evidence.event_id.as_str())
        .collect::<BTreeSet<_>>();
    if evidence_ids.len() != session.evidence.len() || event_ids.len() != session.evidence.len() {
        return Err(AppError::StorageCorrupt(
            "QA session contains duplicate evidence or event ids".into(),
        ));
    }
    for evidence in &session.evidence {
        if evidence.source != QaEvidenceSource::Host
            || evidence.recorded_at_ms < session.started_at_ms
            || evidence.artifact.created_at_ms != evidence.recorded_at_ms
            || evidence.artifact.byte_len == 0
        {
            return Err(AppError::StorageCorrupt(
                "QA session contains invalid evidence metadata".into(),
            ));
        }
        token(&evidence.evidence_id, "evidence_id").map_err(|error| {
            AppError::StorageCorrupt(format!("invalid QA evidence id: {error}"))
        })?;
        token(&evidence.event_id, "event_id")
            .map_err(|error| AppError::StorageCorrupt(format!("invalid QA event id: {error}")))?;
        validate_artifact_ref(&evidence.artifact).map_err(|error| {
            AppError::StorageCorrupt(format!("invalid QA artifact metadata: {error}"))
        })?;
    }
    if let Some(result_id) = &session.finalized_result_id {
        token(result_id, "finalized_result_id").map_err(|error| {
            AppError::StorageCorrupt(format!("invalid finalized QA result id: {error}"))
        })?;
    }
    Ok(session)
}

fn save_session(layout: &AppLayout, session: &QaSession) -> Result<(), AppError> {
    session.identity.validate(layout)?;
    let path = session_path(layout, &session.identity.qa_handle)?;
    persist(
        layout,
        &path,
        &qa_root(layout).join(session.identity.qa_handle.clone()),
        session,
        "QA session",
        true,
    )
}

/// Record one Host-produced evidence item and its bounded temporary artifact.
pub fn qa_record_host_evidence(
    layout: &AppLayout,
    qa_handle: &str,
    input: QaHostEvidenceInput,
) -> Result<QaEvidence, AppError> {
    let _guard = lock_session(layout, qa_handle)?;
    let mut session = load_qa_session(layout, qa_handle)?;
    if session.finalized_result_id.is_some() {
        return Err(AppError::WorkflowStateInvalid(
            "QA evidence is sealed after the first candidate finalization".into(),
        ));
    }
    token(&input.evidence_id, "evidence_id")?;
    token(&input.scenario_id, "scenario_id")?;
    token(&input.target_id, "target_id")?;
    token(&input.event_id, "event_id")?;
    if input.recorded_at_ms < session.started_at_ms {
        return Err(AppError::InvalidRequest(
            "evidence predates QA session".into(),
        ));
    }
    if !session
        .required_scenario_ids
        .iter()
        .any(|id| id == &input.scenario_id)
    {
        return Err(AppError::InvalidRequest(
            "evidence scenario is not required by this QA session".into(),
        ));
    }
    if !session
        .verification_scope
        .in_scope_target_ids
        .iter()
        .any(|id| id == &input.target_id)
    {
        return Err(AppError::InvalidRequest(
            "evidence target is outside the current QA verification scope".into(),
        ));
    }
    let requirement = session
        .scenario_requirements
        .iter()
        .find(|requirement| requirement.scenario_id == input.scenario_id)
        .ok_or_else(|| AppError::InvalidRequest("evidence scenario is not Host-bound".into()))?;
    if !requirement.target_ids.is_empty()
        && !requirement
            .target_ids
            .iter()
            .any(|target| target == &input.target_id)
    {
        return Err(AppError::InvalidRequest(
            "evidence target is not required for this scenario".into(),
        ));
    }
    if session
        .evidence
        .iter()
        .any(|item| item.evidence_id == input.evidence_id)
    {
        return Err(AppError::InvalidRequest("duplicate QA evidence id".into()));
    }
    if session.evidence.len() >= 4096 {
        return Err(AppError::InvalidRequest(
            "QA session has too many evidence entries".into(),
        ));
    }
    if session
        .evidence
        .iter()
        .any(|item| item.event_id == input.event_id)
    {
        return Err(AppError::InvalidRequest("duplicate QA event id".into()));
    }
    if matches!(input.kind, QaEvidenceKind::UiAction) && input.caused_by.is_some() {
        return Err(AppError::InvalidRequest(
            "UI action cannot be caused by another event".into(),
        ));
    }
    if let Some(parent_event_id) = input.caused_by.as_deref() {
        token(parent_event_id, "caused_by")?;
        let expected_parent_kind = match input.kind {
            QaEvidenceKind::BridgeWrite => QaEvidenceKind::UiAction,
            QaEvidenceKind::Query => QaEvidenceKind::BridgeWrite,
            _ => {
                return Err(AppError::InvalidRequest(
                    "only bridge write/query evidence may name a causal event".into(),
                ));
            }
        };
        let parent = session
            .evidence
            .iter()
            .find(|evidence| evidence.event_id == parent_event_id)
            .ok_or_else(|| AppError::InvalidRequest("QA causal event does not exist".into()))?;
        if parent.kind != expected_parent_kind
            || parent.scenario_id != input.scenario_id
            || parent.target_id != input.target_id
            || parent.recorded_at_ms >= input.recorded_at_ms
        {
            return Err(AppError::InvalidRequest(
                "QA causal event must be earlier and match scenario, target, and kind".into(),
            ));
        }
    }
    let content_matches_kind = match input.kind {
        QaEvidenceKind::Capture => matches!(input.content, QaHostEvidenceContent::Image { .. }),
        QaEvidenceKind::NativeTargetProvenance => {
            matches!(input.content, QaHostEvidenceContent::NativeTarget(_))
        }
        QaEvidenceKind::Inspect
        | QaEvidenceKind::UiAction
        | QaEvidenceKind::BridgeWrite
        | QaEvidenceKind::Query => matches!(input.content, QaHostEvidenceContent::Json(_)),
        QaEvidenceKind::Console | QaEvidenceKind::RuntimeError => matches!(
            input.content,
            QaHostEvidenceContent::Json(_) | QaHostEvidenceContent::Text(_)
        ),
    };
    if !content_matches_kind {
        return Err(AppError::InvalidRequest(
            "QA evidence kind does not match its Host artifact content".into(),
        ));
    }
    if matches!(
        input.kind,
        QaEvidenceKind::BridgeWrite | QaEvidenceKind::Query
    ) && input.caused_by.is_none()
    {
        return Err(AppError::InvalidRequest(
            "bridge write/query evidence must name its causal event".into(),
        ));
    }
    if matches!(input.kind, QaEvidenceKind::Capture)
        && !matches!(input.content, QaHostEvidenceContent::Image { .. })
    {
        return Err(AppError::InvalidRequest(
            "canvas capture evidence must be an image artifact".into(),
        ));
    }
    if matches!(input.kind, QaEvidenceKind::NativeTargetProvenance)
        && !matches!(input.content, QaHostEvidenceContent::NativeTarget(_))
    {
        return Err(AppError::InvalidRequest(
            "native target evidence must carry Host provenance".into(),
        ));
    }
    if let QaHostEvidenceContent::NativeTarget(provenance) = &input.content {
        provenance.validate()?;
        if provenance.target_id != input.target_id {
            return Err(AppError::InvalidRequest(
                "native target provenance does not match evidence target".into(),
            ));
        }
    }
    let (format, bytes) = artifact_bytes(&input.content)?;
    let current_bytes: u64 = session
        .evidence
        .iter()
        .map(|item| item.artifact.byte_len)
        .sum();
    if current_bytes.saturating_add(bytes.len() as u64) > MAX_QA_RUN_ARTIFACT_BYTES {
        return Err(AppError::InvalidRequest(
            "QA run artifacts exceed their size limit".into(),
        ));
    }
    let artifact = QaArtifactRef {
        artifact_id: format!("artifact-{}", ids::generate_interaction_id()),
        format,
        byte_len: bytes.len() as u64,
        sha256: raw_sha256(&bytes),
        created_at_ms: input.recorded_at_ms,
    };
    let path = artifact_path(layout, qa_handle, &artifact)?;
    layout.initialize()?;
    let artifact_dir = qa_root(layout).join(qa_handle).join(ARTIFACTS_DIR);
    rooted_fs::ensure_private_directory(layout.root(), &artifact_dir, 0o700)
        .map_err(|error| AppError::from_fs("create QA artifact directory", &error))?;
    rooted_fs::atomic_write(
        layout.root(),
        &path,
        &bytes,
        AtomicWriteOptions {
            overwrite: false,
            create_parents: false,
            file_mode: 0o600,
            ..AtomicWriteOptions::default()
        },
    )
    .map_err(|error| AppError::from_fs("write QA evidence artifact", &error))?;
    let evidence = QaEvidence {
        evidence_id: input.evidence_id,
        scenario_id: input.scenario_id,
        target_id: input.target_id,
        kind: input.kind,
        source: QaEvidenceSource::Host,
        recorded_at_ms: input.recorded_at_ms,
        event_id: input.event_id,
        caused_by: input.caused_by,
        artifact,
    };
    session.evidence.push(evidence.clone());
    if let Err(error) = save_session(layout, &session) {
        let _ = rooted_fs::remove_file(layout.root(), &path);
        return Err(error);
    }
    Ok(evidence)
}

/// Read actual Host-recorded evidence content, replaying JSON/image bytes from
/// the bounded per-run artifact file.
pub fn qa_read_evidence(
    layout: &AppLayout,
    qa_handle: &str,
    evidence_id: &str,
) -> Result<QaEvidenceBlock, AppError> {
    token(evidence_id, "evidence_id")?;
    let session = load_qa_session(layout, qa_handle)?;
    let evidence = session
        .evidence
        .into_iter()
        .find(|item| item.evidence_id == evidence_id)
        .ok_or_else(|| AppError::NotFound("QA evidence not found".into()))?;
    read_evidence_artifact(layout, qa_handle, &evidence)
}

/// Read one artifact using an already loaded, identity-validated QA session.
///
/// Same contract as [`qa_read_evidence`] — including the check that the id
/// names evidence of THIS session — minus the per-call session load, so a
/// reverse-ordered causality search over N candidates reads the session once
/// instead of N times. The membership scan stays: it walks the already loaded
/// `session.evidence` in memory, which costs nothing next to the artifact read
/// it guards, and taking a `&QaEvidence` instead would move that invariant out
/// of the type system and into a doc comment on a `pub` function.
pub fn qa_read_evidence_from_loaded_session(
    layout: &AppLayout,
    session: &QaSession,
    evidence_id: &str,
) -> Result<QaEvidenceBlock, AppError> {
    token(evidence_id, "evidence_id")?;
    session.identity.validate(layout)?;
    if session.identity.qa_handle.is_empty() {
        return Err(AppError::StorageCorrupt(
            "QA session has no evidence handle".into(),
        ));
    }
    let evidence = session
        .evidence
        .iter()
        .find(|item| item.evidence_id == evidence_id)
        .ok_or_else(|| AppError::NotFound("QA evidence not found".into()))?;
    read_evidence_artifact(layout, &session.identity.qa_handle, evidence)
}

fn read_evidence_artifact(
    layout: &AppLayout,
    qa_handle: &str,
    evidence: &QaEvidence,
) -> Result<QaEvidenceBlock, AppError> {
    validate_artifact_ref(&evidence.artifact)?;
    let path = artifact_path(layout, qa_handle, &evidence.artifact)?;
    // Read one byte beyond the declared length. `read_tail_bytes` is a log-tail
    // primitive and otherwise accepts a file with an untrusted prefix; exact
    // length plus digest validation is required before a candidate is sealed.
    let bytes = rooted_fs::read_tail_bytes(
        layout.root(),
        &path,
        evidence.artifact.byte_len.saturating_add(1),
    )
    .map_err(|error| AppError::from_fs("read QA evidence artifact", &error))?;
    if bytes.len() as u64 != evidence.artifact.byte_len
        || raw_sha256(&bytes) != evidence.artifact.sha256
    {
        return Err(AppError::StorageCorrupt(
            "QA evidence artifact integrity failed".into(),
        ));
    }
    match evidence.artifact.format.as_str() {
        "json" => {
            let content = serde_json::from_slice(&bytes)
                .map_err(|error| AppError::StorageCorrupt(format!("QA JSON evidence: {error}")))?;
            Ok(QaEvidenceBlock::Json {
                evidence: evidence.clone(),
                content,
            })
        }
        "txt" => {
            let content = String::from_utf8(bytes)
                .map_err(|error| AppError::StorageCorrupt(format!("QA text evidence: {error}")))?;
            Ok(QaEvidenceBlock::Text {
                evidence: evidence.clone(),
                content,
            })
        }
        "png" => Ok(QaEvidenceBlock::Image {
            evidence: evidence.clone(),
            format: QaImageFormat::Png,
            bytes,
        }),
        "jpg" => Ok(QaEvidenceBlock::Image {
            evidence: evidence.clone(),
            format: QaImageFormat::Jpeg,
            bytes,
        }),
        "webp" => Ok(QaEvidenceBlock::Image {
            evidence: evidence.clone(),
            format: QaImageFormat::Webp,
            bytes,
        }),
        other => Err(AppError::StorageCorrupt(format!(
            "unsupported QA evidence format {other:?}"
        ))),
    }
}

fn validate_scenario_coverage(
    layout: &AppLayout,
    session: &QaSession,
    judgements: &[QaScenarioJudgement],
) -> Result<(), AppError> {
    if judgements.len() != session.required_scenario_ids.len() {
        return Err(AppError::InvalidRequest(
            "QA scenario coverage must exactly match required scenarios".into(),
        ));
    }
    let required = session
        .required_scenario_ids
        .iter()
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    let evidence = session
        .evidence
        .iter()
        .map(|item| item.evidence_id.as_str())
        .collect::<BTreeSet<_>>();
    for judgement in judgements {
        token(&judgement.scenario_id, "scenario_id")?;
        if !required.contains(&judgement.scenario_id) || !seen.insert(&judgement.scenario_id) {
            return Err(AppError::InvalidRequest(
                "QA scenario coverage contains an unknown or duplicate scenario".into(),
            ));
        }
        non_empty(&judgement.summary, "scenario summary", 16 * 1024)?;
        list(&judgement.evidence_ids, "scenario evidence_ids", 4096)?;
        if judgement.evidence_ids.is_empty() {
            return Err(AppError::InvalidRequest(
                "each QA scenario requires evidence".into(),
            ));
        }
        if judgement.evidence_ids.iter().collect::<BTreeSet<_>>().len()
            != judgement.evidence_ids.len()
        {
            return Err(AppError::InvalidRequest(
                "scenario repeats a QA evidence id".into(),
            ));
        }
        for evidence_id in &judgement.evidence_ids {
            if !evidence.contains(evidence_id.as_str()) {
                return Err(AppError::InvalidRequest(
                    "scenario references unknown QA evidence".into(),
                ));
            }
        }
        let requirement = session
            .scenario_requirements
            .iter()
            .find(|requirement| requirement.scenario_id == judgement.scenario_id)
            .ok_or_else(|| AppError::StorageCorrupt("missing Host scenario requirement".into()))?;
        let referenced = judgement
            .evidence_ids
            .iter()
            .filter_map(|evidence_id| {
                session
                    .evidence
                    .iter()
                    .find(|item| item.evidence_id == *evidence_id)
            })
            .collect::<Vec<_>>();
        if referenced
            .iter()
            .any(|item| item.scenario_id != judgement.scenario_id)
        {
            return Err(AppError::InvalidRequest(
                "scenario cannot borrow evidence from another scenario".into(),
            ));
        }
        let required_targets = if requirement.target_ids.is_empty() {
            &session.target_ids
        } else {
            &requirement.target_ids
        };
        // A failed judgement is an authenticated report of observed failure,
        // not a claim that every success artifact exists. Native provenance
        // and referenced Host evidence are still required below; success-only
        // evidence kinds (write/query/capture) are enforced only for Passed.
        if judgement.status == QaScenarioStatus::Failed {
            continue;
        }
        for action in referenced
            .iter()
            .filter(|item| item.kind == QaEvidenceKind::UiAction)
        {
            if ui_action_failed(layout, session, action)? {
                return Err(AppError::InvalidRequest(
                    "passed QA scenario references a failed Host UI action".into(),
                ));
            }
        }
        for target in required_targets {
            if !session
                .verification_scope
                .in_scope_target_ids
                .iter()
                .any(|in_scope| in_scope == target)
            {
                continue;
            }
            for kind in &requirement.evidence_kinds {
                if !referenced
                    .iter()
                    .any(|item| item.target_id == *target && item.kind == *kind)
                {
                    return Err(AppError::InvalidRequest(format!(
                        "scenario {:?} lacks {:?} evidence for target {:?}",
                        judgement.scenario_id, kind, target
                    )));
                }
            }
        }
    }
    if seen != required {
        return Err(AppError::InvalidRequest(
            "QA scenario coverage is missing a required scenario".into(),
        ));
    }
    Ok(())
}

/// Seal-time integrity pass for every model-referenced evidence artifact. The
/// scenario metadata is not sufficient: the raw artifact must still exist,
/// have the exact declared length, and hash to the Host-recorded digest.
fn validate_referenced_artifacts(
    layout: &AppLayout,
    session: &QaSession,
    judgements: &[QaScenarioJudgement],
) -> Result<(), AppError> {
    let mut checked = BTreeSet::new();
    for judgement in judgements {
        for evidence_id in &judgement.evidence_ids {
            let evidence = session
                .evidence
                .iter()
                .find(|evidence| evidence.evidence_id == *evidence_id)
                .ok_or_else(|| AppError::StorageCorrupt("missing referenced QA evidence".into()))?;
            if checked.insert(evidence_id.as_str()) {
                let _ = read_evidence_artifact(layout, &session.identity.qa_handle, evidence)?;
            }
        }
    }
    Ok(())
}

fn validate_native_targets(
    layout: &AppLayout,
    session: &QaSession,
    judgements: &[QaScenarioJudgement],
) -> Result<(), AppError> {
    for requirement in session
        .scenario_requirements
        .iter()
        .filter(|requirement| requirement.required)
    {
        if session
            .verification_scope
            .unverified_scenario_ids
            .iter()
            .any(|scenario| scenario == &requirement.scenario_id)
        {
            continue;
        }
        let judgement = judgements
            .iter()
            .find(|judgement| judgement.scenario_id == requirement.scenario_id)
            .ok_or_else(|| AppError::StorageCorrupt("missing QA judgement".into()))?;
        for target in requirement.target_ids.iter().filter(|target| {
            session
                .verification_scope
                .in_scope_target_ids
                .iter()
                .any(|in_scope| in_scope == *target)
        }) {
            let evidence = session.evidence.iter().find(|item| {
                item.kind == QaEvidenceKind::NativeTargetProvenance
                    && item.scenario_id == requirement.scenario_id
                    && item.target_id == *target
                    && judgement.evidence_ids.contains(&item.evidence_id)
            });
            let Some(evidence) = evidence else {
                return Err(AppError::InvalidRequest(format!(
                    "QA scenario {:?} is missing referenced Host native target provenance for {target:?}",
                    requirement.scenario_id
                )));
            };
            let QaEvidenceBlock::Json { content, .. } =
                read_evidence_artifact(layout, &session.identity.qa_handle, evidence)?
            else {
                return Err(AppError::StorageCorrupt(
                    "native target provenance is not JSON".into(),
                ));
            };
            let provenance: QaNativeTargetProvenance =
                serde_json::from_value(content).map_err(|error| {
                    AppError::StorageCorrupt(format!(
                        "native target provenance artifact is invalid: {error}"
                    ))
                })?;
            provenance.validate()?;
            if provenance.target_id != *target || provenance.captured_at_ms < session.started_at_ms
            {
                return Err(AppError::StorageCorrupt(
                    "native target provenance is stale or target-mismatched".into(),
                ));
            }
        }
    }
    Ok(())
}

fn json_evidence(
    layout: &AppLayout,
    session: &QaSession,
    evidence: &QaEvidence,
) -> Result<Value, AppError> {
    match qa_read_evidence(layout, &session.identity.qa_handle, &evidence.evidence_id)? {
        QaEvidenceBlock::Json { content, .. } => Ok(content),
        _ => Err(AppError::StorageCorrupt(format!(
            "QA {:?} evidence is not JSON",
            evidence.kind
        ))),
    }
}

/// A Host UI action may return a logical failure as a valid, authenticated
/// result. It is useful evidence for a Failed judgement, but it cannot prove
/// a Passed judgement. Absence of "ok" remains compatible with successful
/// native action payloads; only an explicit "ok: false" is failure.
fn ui_action_failed(
    layout: &AppLayout,
    session: &QaSession,
    evidence: &QaEvidence,
) -> Result<bool, AppError> {
    let content = json_evidence(layout, session, evidence)?;
    Ok(content.get("ok") == Some(&Value::Bool(false)))
}

fn changed_rows(value: &Value, collection: &str) -> Vec<(String, u64)> {
    value
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|result| {
            result.get("collection").and_then(Value::as_str) == Some(collection)
                && result.get("deleted").and_then(Value::as_bool) == Some(false)
        })
        .filter_map(|result| {
            Some((
                result.get("recordId")?.as_str()?.to_owned(),
                result.get("revision")?.as_u64()?,
            ))
        })
        .filter(|(record_id, revision)| !record_id.is_empty() && *revision > 0)
        .collect()
}

fn query_contains_row(value: &Value, collection: &str, record_id: &str, revision: u64) -> bool {
    value
        .get("records")
        .and_then(Value::as_array)
        .is_some_and(|records| {
            records.iter().any(|record| {
                record.get("collection").and_then(Value::as_str) == Some(collection)
                    && record.get("recordId").and_then(Value::as_str) == Some(record_id)
                    && record.get("revision").and_then(Value::as_u64) == Some(revision)
            })
        })
}

fn validate_roundtrip(layout: &AppLayout, session: &QaSession) -> Result<(), AppError> {
    if session.required_collections.is_empty() {
        return Ok(());
    }
    for collection in &session.required_collections {
        let mut proven = false;
        for write in session
            .evidence
            .iter()
            .filter(|item| item.kind == QaEvidenceKind::BridgeWrite)
        {
            let Some(action) = write.caused_by.as_ref().and_then(|parent| {
                session.evidence.iter().find(|candidate| {
                    candidate.kind == QaEvidenceKind::UiAction
                        && candidate.event_id == *parent
                        && candidate.scenario_id == write.scenario_id
                        && candidate.target_id == write.target_id
                        && candidate.recorded_at_ms < write.recorded_at_ms
                })
            }) else {
                continue;
            };
            if ui_action_failed(layout, session, action)? {
                continue;
            }
            let rows = changed_rows(&json_evidence(layout, session, write)?, collection);
            if rows.is_empty() {
                continue;
            }
            for query in session.evidence.iter().filter(|candidate| {
                candidate.kind == QaEvidenceKind::Query
                    && candidate.caused_by.as_deref() == Some(write.event_id.as_str())
                    && candidate.scenario_id == action.scenario_id
                    && candidate.target_id == action.target_id
                    && candidate.recorded_at_ms > write.recorded_at_ms
            }) {
                let content = json_evidence(layout, session, query)?;
                if rows.iter().any(|(record_id, revision)| {
                    query_contains_row(&content, collection, record_id, *revision)
                }) {
                    proven = true;
                    break;
                }
            }
            if proven {
                break;
            }
        }
        if !proven {
            return Err(AppError::InvalidRequest(format!(
                "QA collection {collection:?} lacks an actual UI action -> page bridge write -> matching query roundtrip"
            )));
        }
    }
    Ok(())
}

fn validate_canvas_captures(
    session: &QaSession,
    judgements: &[QaScenarioJudgement],
) -> Result<(), AppError> {
    if session.identity.runtime_profile.family == AppRuntimeProfile::ReactDom {
        return Ok(());
    }
    for requirement in session.scenario_requirements.iter().filter(|requirement| {
        requirement.required
            && !session
                .verification_scope
                .unverified_scenario_ids
                .iter()
                .any(|scenario| scenario == &requirement.scenario_id)
            && (requirement.motion_required
                || requirement
                    .evidence_kinds
                    .contains(&QaEvidenceKind::Capture))
    }) {
        let judgement = judgements
            .iter()
            .find(|judgement| judgement.scenario_id == requirement.scenario_id)
            .ok_or_else(|| AppError::StorageCorrupt("missing QA judgement".into()))?;
        if judgement.status == QaScenarioStatus::Failed {
            continue;
        }
        for target in requirement.target_ids.iter().filter(|target| {
            session
                .verification_scope
                .in_scope_target_ids
                .iter()
                .any(|in_scope| in_scope == *target)
        }) {
            let mut captures = session
                .evidence
                .iter()
                .filter(|item| {
                    item.scenario_id == requirement.scenario_id
                        && item.target_id == *target
                        && item.kind == QaEvidenceKind::Capture
                        && judgement.evidence_ids.contains(&item.evidence_id)
                })
                .collect::<Vec<_>>();
            if captures.len() < 2 {
                return Err(AppError::InvalidRequest(format!(
                    "canvas QA scenario {:?} target {target:?} requires at least two referenced captures",
                    requirement.scenario_id
                )));
            }
            captures.sort_by_key(|item| item.recorded_at_ms);
            if captures
                .windows(2)
                .any(|pair| pair[0].recorded_at_ms >= pair[1].recorded_at_ms)
            {
                return Err(AppError::InvalidRequest(
                    "canvas captures must be time-separated".into(),
                ));
            }
            let first = &captures[0].artifact.sha256;
            if requirement.motion_required
                && captures
                    .iter()
                    .skip(1)
                    .all(|capture| &capture.artifact.sha256 == first)
            {
                return Err(AppError::InvalidRequest(
                    "animated canvas captures must contain distinct frames".into(),
                ));
            }
        }
    }
    Ok(())
}

fn scenario_failure_id(scenario_id: &str, target_id: &str) -> Result<String, AppError> {
    let scope_sha256 = canonical_sha256(&(scenario_id, target_id), "QA scenario failure scope")?;
    Ok(format!("{SCENARIO_FAILURE_PREFIX}{scope_sha256}"))
}

/// Turn a failed Host-bound scenario into a durable blocker. The finding is
/// intentionally synthesized from the judgement and persisted in the
/// workflow ledger, so a verifier cannot make the failure disappear merely by
/// sending a green judgement with an empty findings array for the same sealed
/// evidence session.
fn record_failed_scenarios(
    ledger: &mut QaFindingLedger,
    session: &QaSession,
    judgements: &[QaScenarioJudgement],
    finalized_at_ms: u64,
) -> Result<(), AppError> {
    for judgement in judgements
        .iter()
        .filter(|judgement| judgement.status == QaScenarioStatus::Failed)
    {
        let requirement = session
            .scenario_requirements
            .iter()
            .find(|requirement| requirement.scenario_id == judgement.scenario_id)
            .ok_or_else(|| AppError::StorageCorrupt("missing Host scenario requirement".into()))?;
        let all_targets = if requirement.target_ids.is_empty() {
            &session.target_ids
        } else {
            &requirement.target_ids
        };
        let targets = all_targets
            .iter()
            .filter(|target| {
                session
                    .verification_scope
                    .in_scope_target_ids
                    .iter()
                    .any(|in_scope| in_scope == *target)
            })
            .collect::<Vec<_>>();
        for target in targets {
            let evidence_ids = session
                .evidence
                .iter()
                .filter(|evidence| {
                    evidence.scenario_id == judgement.scenario_id
                        && evidence.target_id == *target
                        && judgement.evidence_ids.contains(&evidence.evidence_id)
                })
                .map(|evidence| evidence.evidence_id.clone())
                .collect::<Vec<_>>();
            if evidence_ids.is_empty() {
                return Err(AppError::InvalidRequest(format!(
                    "failed QA scenario {:?} has no evidence for target {:?}",
                    judgement.scenario_id, target
                )));
            }
            let id = scenario_failure_id(&judgement.scenario_id, target)?;
            if let Some(existing) = ledger
                .findings
                .iter_mut()
                .find(|entry| entry.finding.id == id)
            {
                if existing.scenario_id.as_deref() != Some(judgement.scenario_id.as_str())
                    || existing.target_id.as_deref() != Some(target.as_str())
                {
                    return Err(AppError::StorageCorrupt(
                        "QA scenario failure ledger scope conflicts with its id".into(),
                    ));
                }
                // The first Host failure is durable evidence. A later
                // verifier must not rewrite it with a different summary.
                continue;
            }
            ledger.findings.push(QaLedgerFinding {
                finding: QaFinding {
                    id,
                    message: format!(
                        "QA scenario {:?} failed for target {:?}: {}",
                        judgement.scenario_id, target, judgement.summary
                    ),
                    blocking: true,
                    resolved_by_evidence_ids: Vec::new(),
                },
                introduced_at_ms: finalized_at_ms,
                scenario_id: Some(judgement.scenario_id.clone()),
                target_id: Some(target.clone()),
                evidence_ids,
                qa_handle: Some(session.identity.qa_handle.clone()),
            });
        }
    }
    ledger
        .findings
        .sort_by(|left, right| left.finding.id.cmp(&right.finding.id));
    Ok(())
}

/// Resolve only scenario blockers that have a passed judgement for the same
/// scenario/target and fresh Host evidence from a different QA handle. A
/// verifier's second candidate uses the original sealed handle and evidence,
/// so it cannot resolve the tester's failed judgement. A later resample or
/// rebuilt run gets a new handle and newly recorded evidence and may resolve
/// it, even if the Host reuses a handle-local evidence id.
fn resolve_scenario_findings(
    ledger: &mut QaFindingLedger,
    session: &QaSession,
    judgements: &[QaScenarioJudgement],
) {
    ledger.findings.retain(|entry| {
        let (Some(scenario_id), Some(target_id), Some(previous_handle)) = (
            entry.scenario_id.as_deref(),
            entry.target_id.as_deref(),
            entry.qa_handle.as_deref(),
        ) else {
            return true;
        };
        if previous_handle == session.identity.qa_handle {
            return true;
        }
        let Some(judgement) = judgements
            .iter()
            .find(|judgement| judgement.scenario_id == scenario_id)
        else {
            return true;
        };
        if judgement.status != QaScenarioStatus::Passed {
            return true;
        }
        let has_fresh_matching_evidence = session.evidence.iter().any(|evidence| {
            evidence.scenario_id == scenario_id
                && evidence.target_id == target_id
                && judgement.evidence_ids.contains(&evidence.evidence_id)
                && evidence.recorded_at_ms > entry.introduced_at_ms
                && evidence.source == QaEvidenceSource::Host
        });
        !has_fresh_matching_evidence
    });
}

fn validate_finding(finding: &QaFinding) -> Result<(), AppError> {
    token(&finding.id, "finding id")?;
    non_empty(&finding.message, "finding message", 16 * 1024)?;
    list(
        &finding.resolved_by_evidence_ids,
        "finding resolved_by_evidence_ids",
        4096,
    )?;
    if finding
        .resolved_by_evidence_ids
        .iter()
        .collect::<BTreeSet<_>>()
        .len()
        != finding.resolved_by_evidence_ids.len()
    {
        return Err(AppError::InvalidRequest(
            "QA finding repeats resolving evidence".into(),
        ));
    }
    if finding.blocking && !finding.resolved_by_evidence_ids.is_empty() {
        return Err(AppError::InvalidRequest(
            "a resolved QA finding cannot remain blocking".into(),
        ));
    }
    Ok(())
}

fn merge_findings(
    ledger: &QaFindingLedger,
    previous: Option<&QaResult>,
    supplied: Vec<QaFinding>,
) -> Result<Vec<QaFinding>, AppError> {
    let mut merged = ledger
        .findings
        .iter()
        .map(|entry| (entry.finding.id.clone(), entry.finding.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut immutable_ids = BTreeSet::new();
    if let Some(previous) = previous {
        for finding in &previous.findings {
            validate_finding(finding)?;
            if let Some(existing) = merged.get(&finding.id) {
                if existing != finding {
                    return Err(AppError::StorageCorrupt(
                        "previous QA candidate conflicts with finding ledger".into(),
                    ));
                }
            } else {
                merged.insert(finding.id.clone(), finding.clone());
            }
            immutable_ids.insert(finding.id.clone());
        }
    }
    let mut supplied_ids = BTreeSet::new();
    for finding in supplied {
        validate_finding(&finding)?;
        if !supplied_ids.insert(finding.id.clone()) {
            return Err(AppError::InvalidRequest("duplicate QA finding id".into()));
        }
        if finding.id.starts_with(SCENARIO_FAILURE_PREFIX) {
            match ledger
                .findings
                .iter()
                .find(|entry| entry.finding.id == finding.id && entry.scenario_id.is_some())
            {
                Some(entry) if entry.finding == finding => {}
                Some(_) => {
                    return Err(AppError::InvalidRequest(format!(
                        "Host-derived scenario finding {:?} cannot be rewritten by the caller",
                        finding.id
                    )));
                }
                None => {
                    return Err(AppError::InvalidRequest(format!(
                        "finding id {:?} uses the reserved Host scenario namespace",
                        finding.id
                    )));
                }
            }
        }
        if let Some(existing) = merged.get(&finding.id) {
            if immutable_ids.contains(&finding.id) && existing != &finding {
                return Err(AppError::InvalidRequest(format!(
                    "finding {:?} from the previous candidate cannot be rewritten",
                    finding.id
                )));
            }
            if existing.message != finding.message {
                return Err(AppError::InvalidRequest(format!(
                    "finding {:?} conflicts with the durable Host ledger",
                    finding.id
                )));
            }
        }
        merged.insert(finding.id.clone(), finding);
    }
    if merged.len() > 256 {
        return Err(AppError::InvalidRequest(
            "QA result has too many findings".into(),
        ));
    }
    Ok(merged.into_values().collect())
}

fn validate_failures(session: &QaSession, findings: &[QaFinding]) -> Result<(), AppError> {
    let evidence = session
        .evidence
        .iter()
        .map(|item| item.evidence_id.as_str())
        .collect::<BTreeSet<_>>();
    let finding_by_id = findings
        .iter()
        .map(|finding| (finding.id.as_str(), finding))
        .collect::<BTreeMap<_, _>>();
    if finding_by_id.len() != findings.len() {
        return Err(AppError::InvalidRequest("duplicate QA finding id".into()));
    }
    for failure in &session.upstream_failures {
        let Some(finding) = finding_by_id.get(failure.id.as_str()) else {
            return Err(AppError::InvalidRequest(format!(
                "upstream QA failure {:?} cannot disappear without resolution",
                failure.id
            )));
        };
        for evidence_id in &finding.resolved_by_evidence_ids {
            if !evidence.contains(evidence_id.as_str()) {
                return Err(AppError::InvalidRequest(
                    "QA finding references unknown resolving evidence".into(),
                ));
            }
            let Some(record) = session
                .evidence
                .iter()
                .find(|item| item.evidence_id == *evidence_id)
            else {
                return Err(AppError::InvalidRequest(
                    "QA finding references unknown resolving evidence".into(),
                ));
            };
            if record.recorded_at_ms <= failure.introduced_at_ms {
                return Err(AppError::InvalidRequest(
                    "resolving QA evidence must be newer than the upstream failure".into(),
                ));
            }
        }
        if finding.resolved_by_evidence_ids.is_empty() && !finding.blocking {
            return Err(AppError::InvalidRequest(
                "unresolved upstream QA failure must remain blocking".into(),
            ));
        }
    }
    for finding in findings {
        validate_finding(finding)?;
        for evidence_id in &finding.resolved_by_evidence_ids {
            if !evidence.contains(evidence_id.as_str()) {
                return Err(AppError::InvalidRequest(
                    "QA finding references unknown resolving evidence".into(),
                ));
            }
        }
    }
    Ok(())
}

fn update_finding_ledger(
    ledger: &mut QaFindingLedger,
    findings: &[QaFinding],
    finalized_at_ms: u64,
) {
    let existing_entries = ledger
        .findings
        .iter()
        .map(|entry| (entry.finding.id.clone(), entry.clone()))
        .collect::<BTreeMap<_, _>>();
    ledger.findings = findings
        .iter()
        .filter(|finding| finding.blocking && finding.resolved_by_evidence_ids.is_empty())
        .map(|finding| {
            existing_entries
                .get(&finding.id)
                .cloned()
                .unwrap_or_else(|| QaLedgerFinding {
                    finding: finding.clone(),
                    introduced_at_ms: finalized_at_ms,
                    scenario_id: None,
                    target_id: None,
                    evidence_ids: Vec::new(),
                    qa_handle: None,
                })
        })
        .collect();
    ledger
        .findings
        .sort_by(|left, right| left.finding.id.cmp(&right.finding.id));
}

/// Finalize an in-flight run after exact scenario and Host-evidence checks.
pub fn qa_finalize(
    layout: &AppLayout,
    qa_handle: &str,
    scenario_judgements: Vec<QaScenarioJudgement>,
    findings: Vec<QaFinding>,
    finalized_at_ms: u64,
) -> Result<QaFinalizeOutput, AppError> {
    list(&scenario_judgements, "scenario_judgements", 256)?;
    list(&findings, "findings", 256)?;
    let _session_guard = lock_session(layout, qa_handle)?;
    let mut session = load_qa_session(layout, qa_handle)?;
    let previous = session
        .finalized_result_id
        .as_deref()
        .map(|result_id| load_qa_result(layout, result_id))
        .transpose()?;
    if let Some(previous) = &previous {
        if previous.identity != session.identity
            || previous.evidence != session.evidence
            || previous.verification_scope != session.verification_scope
        {
            return Err(AppError::StorageCorrupt(
                "previous QA candidate does not match its sealed session".into(),
            ));
        }
    }
    if finalized_at_ms <= session.started_at_ms
        || previous
            .as_ref()
            .is_some_and(|result| finalized_at_ms <= result.finalized_at_ms)
    {
        return Err(AppError::InvalidRequest(
            "QA finalization time must advance beyond the session and prior candidate".into(),
        ));
    }
    let _ledger_guard = lock_ledger(layout, &session.identity.workflow_run_id)?;
    let mut ledger = load_finding_ledger(layout, &session.identity.workflow_run_id)?;
    for entry in &ledger.findings {
        // Scenario failures are resolved by the scoped fresh-evidence path
        // below; they are not upstream failures that must be copied into the
        // session's immutable identity.
        if entry.scenario_id.is_some() {
            continue;
        }
        if !session
            .upstream_failures
            .iter()
            .any(|failure| failure.id == entry.finding.id)
        {
            session.upstream_failures.push(QaUpstreamFailure {
                id: entry.finding.id.clone(),
                message: entry.finding.message.clone(),
                introduced_at_ms: entry.introduced_at_ms,
            });
        }
    }
    validate_scenario_coverage(layout, &session, &scenario_judgements)?;
    validate_referenced_artifacts(layout, &session, &scenario_judgements)?;
    validate_native_targets(layout, &session, &scenario_judgements)?;
    if scenario_judgements
        .iter()
        .all(|judgement| judgement.status == QaScenarioStatus::Passed)
    {
        validate_roundtrip(layout, &session)?;
    }
    validate_canvas_captures(&session, &scenario_judgements)?;
    record_failed_scenarios(&mut ledger, &session, &scenario_judgements, finalized_at_ms)?;
    resolve_scenario_findings(&mut ledger, &session, &scenario_judgements);
    let findings = merge_findings(&ledger, previous.as_ref(), findings)?;
    validate_failures(&session, &findings)?;
    let _history_guard = lock_history(layout)?;
    if let Some(previous_result_id) = session.finalized_result_id.as_deref() {
        let locked_previous = load_qa_result(layout, previous_result_id)?;
        if previous.as_ref() != Some(&locked_previous) {
            return Err(AppError::StorageCorrupt(
                "previous QA candidate changed during finalization".into(),
            ));
        }
        if load_qa_publication_marker(layout, previous_result_id)?.is_some() {
            return Err(AppError::WorkflowStateInvalid(
                "published QA evidence cannot be finalized again".into(),
            ));
        }
    }
    let result_id = format!("result-{}", ids::generate_interaction_id());
    let mut result = QaResult {
        schema_version: QA_SCHEMA_VERSION,
        status: QaResultStatus::Candidate,
        identity: session.identity.clone(),
        scenario_judgements,
        findings,
        evidence: session.evidence.clone(),
        verification_scope: session.verification_scope.clone(),
        previous_result_id: session.finalized_result_id.clone(),
        finalized_at_ms,
        result_sha256: String::new(),
    };
    result.result_sha256 = canonical_sha256(&result, "QA result")?;
    persist(
        layout,
        &result_path(layout, &result_id)?,
        &qa_root(layout).join(RESULTS_DIR),
        &result,
        "QA result",
        false,
    )?;
    let identity_sha256 = session.identity.sha256()?;
    let receipt = QaReceipt {
        receipt_id: format!("qa-{}", ids::generate_interaction_id()),
        app_id: layout.app_id().to_owned(),
        workflow_run_id: session.identity.workflow_run_id.clone(),
        qa_handle: session.identity.qa_handle.clone(),
        result_id,
        identity_sha256,
        result_sha256: result.result_sha256.clone(),
        issued_at_ms: finalized_at_ms,
    };
    persist(
        layout,
        &receipt_path(layout, &receipt.receipt_id)?,
        &qa_root(layout).join(RECEIPTS_DIR),
        &receipt,
        "QA receipt",
        false,
    )?;
    session.finalized_result_id = Some(receipt.result_id.clone());
    save_session(layout, &session)?;
    update_finding_ledger(&mut ledger, &result.findings, finalized_at_ms);
    save_finding_ledger(layout, &ledger)?;
    Ok(QaFinalizeOutput { result, receipt })
}

/// Remove an in-flight/finalized session and its temporary artifacts only
/// after independent verifier/terminal handling has completed.
pub fn qa_cleanup_session(layout: &AppLayout, qa_handle: &str) -> Result<(), AppError> {
    ids::validate_qa_handle(qa_handle)?;
    let path = session_path(layout, qa_handle)?;
    match rooted_fs::read_to_string_limited(layout.root(), &path, MAX_QA_DOCUMENT_BYTES) {
        Ok(_) => {}
        Err(FsError::NotFound(_)) => return Ok(()),
        // Re-read under the sessions lock so a malformed or transient file is
        // reported from the authenticated document loader.
        Err(_) => {}
    }
    let _guard = lock_session(layout, qa_handle)?;
    let session = match load_qa_session(layout, qa_handle) {
        Ok(session) => session,
        Err(AppError::NotFound(_)) => return Ok(()),
        Err(error) => return Err(error),
    };
    cleanup_loaded_qa_session(layout, qa_handle, &session)
}

fn cleanup_loaded_qa_session(
    layout: &AppLayout,
    qa_handle: &str,
    session: &QaSession,
) -> Result<(), AppError> {
    if session.identity.qa_handle != qa_handle || session.identity.app_id != layout.app_id() {
        return Err(AppError::StorageCorrupt(
            "QA cleanup session identity is inconsistent".into(),
        ));
    }
    let session_dir = qa_root(layout).join(qa_handle);
    let session_absolute = layout.root().join(&session_dir);
    match std::fs::symlink_metadata(&session_absolute) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(AppError::StorageCorrupt(
                "QA session path is not a regular directory".into(),
            ));
        }
        Ok(_) => {}
        Err(error) => {
            return Err(AppError::Io(format!(
                "inspect QA session directory {}: {error}",
                session_absolute.display()
            )));
        }
    }

    // Preflight the whole bounded session directory before deleting anything.
    // This keeps cleanup from interpreting unrelated files or links as raw QA
    // evidence.
    for entry in std::fs::read_dir(&session_absolute).map_err(|error| {
        AppError::Io(format!(
            "list QA session directory {}: {error}",
            session_absolute.display()
        ))
    })? {
        let entry = entry.map_err(|error| AppError::Io(format!("list QA session: {error}")))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| AppError::StorageCorrupt("QA session entry is not valid UTF-8".into()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| AppError::Io(format!("inspect QA session entry: {error}")))?;
        match name.as_str() {
            SESSION_FILE if !file_type.is_symlink() && file_type.is_file() => {}
            ARTIFACTS_DIR if !file_type.is_symlink() && file_type.is_dir() => {}
            _ => {
                return Err(AppError::StorageCorrupt(
                    "QA session directory contains an unexpected entry".into(),
                ));
            }
        }
    }

    // Remove every regular file in the bounded artifacts directory, not only
    // rows still referenced by the session. Superseded/incomplete writes may
    // leave an unreferenced raw artifact, and it is still temporary evidence.
    let artifact_dir = qa_root(layout).join(qa_handle).join(ARTIFACTS_DIR);
    let artifact_absolute = layout.root().join(&artifact_dir);
    match std::fs::symlink_metadata(&artifact_absolute) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(AppError::StorageCorrupt(
                "QA artifact path is not a regular directory".into(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::Io(format!(
                "inspect QA artifact directory {}: {error}",
                artifact_absolute.display()
            )))
        }
    }
    let artifact_names = match std::fs::read_dir(&artifact_absolute) {
        Ok(entries) => {
            let mut names = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|error| {
                    AppError::Io(format!("list QA artifacts for cleanup: {error}"))
                })?;
                let file_type = entry.file_type().map_err(|error| {
                    AppError::Io(format!("inspect QA artifact for cleanup: {error}"))
                })?;
                if file_type.is_symlink() || !file_type.is_file() {
                    return Err(AppError::StorageCorrupt(
                        "QA artifact directory contains a non-file entry".into(),
                    ));
                }
                let name = entry.file_name().into_string().map_err(|_| {
                    AppError::StorageCorrupt("QA artifact filename is not valid UTF-8".into())
                })?;
                let (artifact_id, format) = name.rsplit_once('.').ok_or_else(|| {
                    AppError::StorageCorrupt("QA artifact filename is malformed".into())
                })?;
                token(artifact_id, "artifact_id").map_err(|error| {
                    AppError::StorageCorrupt(format!("invalid QA artifact filename: {error}"))
                })?;
                if !matches!(format, "json" | "txt" | "png" | "jpg" | "webp") {
                    return Err(AppError::StorageCorrupt(
                        "QA artifact filename has an unsupported format".into(),
                    ));
                }
                names.push(name);
            }
            names
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(AppError::Io(format!(
                "list QA artifact directory {}: {error}",
                artifact_absolute.display()
            )))
        }
    };
    for name in artifact_names {
        match rooted_fs::remove_file(layout.root(), &artifact_dir.join(&name)) {
            Ok(()) | Err(FsError::NotFound(_)) => {}
            Err(error) => return Err(AppError::from_fs("remove QA artifact", &error)),
        }
    }
    match std::fs::remove_dir(&artifact_absolute) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::Io(format!(
                "remove QA artifact directory {}: {error}",
                artifact_absolute.display()
            )));
        }
    }
    match rooted_fs::remove_file(layout.root(), &session_path(layout, qa_handle)?) {
        Ok(()) | Err(FsError::NotFound(_)) => {}
        Err(error) => return Err(AppError::from_fs("remove QA session", &error)),
    }
    match std::fs::remove_dir(&session_absolute) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::Io(format!(
                "remove QA session directory {}: {error}",
                session_absolute.display()
            )));
        }
    }
    Ok(())
}

/// Remove temporary QA sessions and raw evidence for one Host workflow run.
///
/// The walk is deliberately limited to app-owned QA session directories. Each
/// candidate is required to be a regular directory with a valid Host QA
/// handle, and its signed/digested session document is loaded before anything
/// is removed. Immutable results, receipts, publication markers and finding
/// ledgers live outside those directories and are never touched.
pub fn qa_cleanup_run(layout: &AppLayout, workflow_run_id: &str) -> Result<usize, AppError> {
    token(workflow_run_id, "workflow_run_id")?;
    let root = layout.root().join(qa_root(layout));
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(AppError::StorageCorrupt(
                    "QA root is not a regular directory".into(),
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(AppError::Io(format!(
                "inspect QA root {}: {error}",
                root.display()
            )));
        }
    }
    // One bounded app-level lock keeps enumeration and deletion stable without
    // creating a lock directory for every retired or nonexistent QA handle.
    let _sessions_guard = lock_sessions(layout)?;
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(AppError::Io(format!(
                "list QA root {}: {error}",
                root.display()
            )))
        }
    };
    let reserved = [
        RESULTS_DIR,
        RECEIPTS_DIR,
        PUBLISHED_DIR,
        LEDGERS_DIR,
        SESSIONS_LOCK_FILE,
        HISTORY_LOCK_FILE,
        "active-receipt.json",
    ];
    let mut handles = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| AppError::Io(format!("list QA root: {error}")))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| AppError::StorageCorrupt("QA root entry is not valid UTF-8".into()))?;
        if reserved.contains(&name.as_str()) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| AppError::Io(format!("inspect QA root entry: {error}")))?;
        if file_type.is_symlink() || !file_type.is_dir() {
            return Err(AppError::StorageCorrupt(
                "QA root contains an unexpected non-session entry".into(),
            ));
        }
        ids::validate_qa_handle(&name).map_err(|error| {
            AppError::StorageCorrupt(format!("invalid QA session directory name: {error}"))
        })?;
        let session = match load_qa_session(layout, &name) {
            Ok(session) => session,
            Err(AppError::NotFound(_)) => continue,
            Err(error) => return Err(error),
        };
        if session.identity.workflow_run_id == workflow_run_id {
            handles.push(name);
        }
    }
    let mut removed = 0usize;
    for handle in handles {
        let session = match load_qa_session(layout, &handle) {
            Ok(session) => session,
            Err(AppError::NotFound(_)) => continue,
            Err(error) => return Err(error),
        };
        if session.identity.workflow_run_id != workflow_run_id {
            continue;
        }
        cleanup_loaded_qa_session(layout, &handle, &session)?;
        removed += 1;
    }
    Ok(removed)
}

/// Load an immutable historical result, never an in-flight session.
pub fn load_qa_result(layout: &AppLayout, result_id: &str) -> Result<QaResult, AppError> {
    let result: QaResult = load(layout, &result_path(layout, result_id)?, "QA result")?.payload;
    result.identity.validate(layout)?;
    if result.schema_version != QA_SCHEMA_VERSION
        || result.status != QaResultStatus::Candidate
        || result.finalized_at_ms == 0
        || result.scenario_judgements.is_empty()
    {
        return Err(AppError::StorageCorrupt(
            "QA result has invalid durable bounds or status".into(),
        ));
    }
    result.verification_scope.validate().map_err(|error| {
        AppError::StorageCorrupt(format!("invalid QA verification scope: {error}"))
    })?;
    if let Some(previous_result_id) = &result.previous_result_id {
        token(previous_result_id, "previous_result_id").map_err(|error| {
            AppError::StorageCorrupt(format!("invalid previous QA result id: {error}"))
        })?;
        if previous_result_id == result_id {
            return Err(AppError::StorageCorrupt(
                "QA result cannot point to itself".into(),
            ));
        }
    }
    let scenario_ids = result
        .scenario_judgements
        .iter()
        .map(|judgement| judgement.scenario_id.as_str())
        .collect::<BTreeSet<_>>();
    let finding_ids = result
        .findings
        .iter()
        .map(|finding| finding.id.as_str())
        .collect::<BTreeSet<_>>();
    let evidence_ids = result
        .evidence
        .iter()
        .map(|evidence| evidence.evidence_id.as_str())
        .collect::<BTreeSet<_>>();
    if scenario_ids.len() != result.scenario_judgements.len()
        || finding_ids.len() != result.findings.len()
        || evidence_ids.len() != result.evidence.len()
    {
        return Err(AppError::StorageCorrupt(
            "QA result contains duplicate durable ids".into(),
        ));
    }
    for finding in &result.findings {
        validate_finding(finding).map_err(|error| {
            AppError::StorageCorrupt(format!("invalid durable QA finding: {error}"))
        })?;
    }
    let expected = canonical_sha256(
        &QaResult {
            result_sha256: String::new(),
            ..result.clone()
        },
        "QA result",
    )?;
    if result.result_sha256 != expected {
        return Err(AppError::StorageCorrupt(
            "QA result digest is invalid".into(),
        ));
    }
    Ok(result)
}

/// Load and validate a historical receipt.
pub fn load_qa_receipt(layout: &AppLayout, receipt_id: &str) -> Result<QaReceipt, AppError> {
    let receipt: QaReceipt =
        load(layout, &receipt_path(layout, receipt_id)?, "QA receipt")?.payload;
    if receipt.app_id != layout.app_id() {
        return Err(AppError::StorageCorrupt(
            "QA receipt app binding is invalid".into(),
        ));
    }
    ids::validate_qa_handle(&receipt.qa_handle).map_err(|error| {
        AppError::StorageCorrupt(format!("invalid QA receipt binding: {error}"))
    })?;
    for (field, value) in [
        ("receipt_id", receipt.receipt_id.as_str()),
        ("workflow_run_id", receipt.workflow_run_id.as_str()),
        ("result_id", receipt.result_id.as_str()),
    ] {
        token(value, field).map_err(|error| {
            AppError::StorageCorrupt(format!("invalid QA receipt binding: {error}"))
        })?;
    }
    if receipt.receipt_id != receipt_id || receipt.issued_at_ms == 0 {
        return Err(AppError::StorageCorrupt(
            "QA receipt path or timestamp is invalid".into(),
        ));
    }
    digest(&receipt.identity_sha256, "QA receipt identity_sha256")?;
    digest(&receipt.result_sha256, "QA receipt result_sha256")?;
    let result = load_qa_result(layout, &receipt.result_id)?;
    if result.identity.app_id != receipt.app_id
        || result.identity.workflow_run_id != receipt.workflow_run_id
        || result.identity.qa_handle != receipt.qa_handle
        || result.result_sha256 != receipt.result_sha256
        || result.identity.sha256()? != receipt.identity_sha256
        || result.finalized_at_ms != receipt.issued_at_ms
    {
        return Err(AppError::StorageCorrupt(
            "QA receipt does not match its immutable result".into(),
        ));
    }
    Ok(receipt)
}

/// Load the terminal publication marker without mutating the immutable
/// candidate result. A missing marker means the result is still a candidate.
pub fn load_qa_publication_marker(
    layout: &AppLayout,
    result_id: &str,
) -> Result<Option<QaPublicationMarker>, AppError> {
    match load::<QaPublicationMarker>(
        layout,
        &publication_path(layout, result_id)?,
        "QA publication marker",
    ) {
        Ok(document) => {
            let marker = document.payload;
            let result = load_qa_result(layout, result_id)?;
            if marker.result_id != result_id
                || marker.published_at_ms == 0
                || marker.result_sha256 != result.result_sha256
                || marker.identity_sha256 != result.identity.sha256()?
            {
                return Err(AppError::StorageCorrupt(
                    "QA publication marker does not match its candidate result".into(),
                ));
            }
            Ok(Some(marker))
        }
        Err(AppError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn list_json_document_ids(
    layout: &AppLayout,
    directory: &Path,
    what: &str,
) -> Result<Vec<String>, AppError> {
    let absolute = layout.root().join(directory);
    let entries = match std::fs::read_dir(&absolute) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(AppError::Io(format!(
                "list {what} directory {}: {error}",
                absolute.display()
            )));
        }
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| AppError::Io(format!("list {what}: {error}")))?;
        let file_type = entry
            .file_type()
            .map_err(|error| AppError::Io(format!("inspect {what}: {error}")))?;
        if !file_type.is_file() || file_type.is_symlink() {
            return Err(AppError::StorageCorrupt(format!(
                "{what} directory contains a non-file entry"
            )));
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| AppError::StorageCorrupt(format!("{what} filename is not valid UTF-8")))?;
        let id = name.strip_suffix(".json").ok_or_else(|| {
            AppError::StorageCorrupt(format!("{what} filename is not a JSON document"))
        })?;
        token(id, what).map_err(|error| {
            AppError::StorageCorrupt(format!("invalid {what} filename: {error}"))
        })?;
        ids.push(id.to_owned());
    }
    ids.sort();
    Ok(ids)
}

/// Bound immutable QA history after terminal publication while retaining the
/// active result ids named by the Host.
///
/// Raw evidence is managed separately by [`qa_cleanup_session`]. This method
/// removes only old result/receipt/publication documents and shares a lock
/// with finalization and publication, so it cannot observe a half-written
/// candidate.
pub fn prune_qa_history(
    layout: &AppLayout,
    retain_result_ids: &[String],
) -> Result<usize, AppError> {
    if retain_result_ids.len() > MAX_QA_HISTORY_RESULTS {
        return Err(AppError::InvalidRequest(format!(
            "cannot retain more than {MAX_QA_HISTORY_RESULTS} QA results"
        )));
    }
    let mut keep = BTreeSet::new();
    for result_id in retain_result_ids {
        token(result_id, "retained result_id")?;
        if !keep.insert(result_id.clone()) {
            return Err(AppError::InvalidRequest(
                "duplicate retained QA result id".into(),
            ));
        }
    }
    let _history_guard = lock_history(layout)?;
    let result_directory = qa_root(layout).join(RESULTS_DIR);
    let mut results = list_json_document_ids(layout, &result_directory, "QA result")?
        .into_iter()
        .map(|result_id| {
            load_qa_result(layout, &result_id).map(|result| (result_id, result.finalized_at_ms))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let all_results = results
        .iter()
        .map(|(result_id, _)| result_id.clone())
        .collect::<BTreeSet<_>>();
    if let Some(missing) = keep
        .iter()
        .find(|result_id| !all_results.contains(*result_id))
    {
        return Err(AppError::NotFound(format!(
            "retained QA result {missing:?} was not found"
        )));
    }
    results.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.0.cmp(&left.0)));
    for (result_id, _) in &results {
        if keep.len() >= MAX_QA_HISTORY_RESULTS {
            break;
        }
        keep.insert(result_id.clone());
    }
    let prune = all_results
        .difference(&keep)
        .cloned()
        .collect::<BTreeSet<_>>();
    for receipt_id in
        list_json_document_ids(layout, &qa_root(layout).join(RECEIPTS_DIR), "QA receipt")?
    {
        let receipt: QaReceipt =
            load(layout, &receipt_path(layout, &receipt_id)?, "QA receipt")?.payload;
        if receipt.receipt_id != receipt_id {
            return Err(AppError::StorageCorrupt(
                "QA receipt filename binding is invalid".into(),
            ));
        }
        if prune.contains(&receipt.result_id) || !all_results.contains(&receipt.result_id) {
            rooted_fs::remove_file(layout.root(), &receipt_path(layout, &receipt_id)?)
                .map_err(|error| AppError::from_fs("prune QA receipt", &error))?;
        } else {
            load_qa_receipt(layout, &receipt_id)?;
        }
    }
    for result_id in list_json_document_ids(
        layout,
        &qa_root(layout).join(PUBLISHED_DIR),
        "QA publication marker",
    )? {
        if prune.contains(&result_id) || !all_results.contains(&result_id) {
            rooted_fs::remove_file(layout.root(), &publication_path(layout, &result_id)?)
                .map_err(|error| AppError::from_fs("prune QA publication marker", &error))?;
        } else {
            load_qa_publication_marker(layout, &result_id)?;
        }
    }
    for result_id in &prune {
        rooted_fs::remove_file(layout.root(), &result_path(layout, result_id)?)
            .map_err(|error| AppError::from_fs("prune QA result", &error))?;
    }
    Ok(prune.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        save_manifest, AppDependencySnapshot, AppManifest, AppTemplateOrigin, DataCollectionSchema,
    };
    use tempfile::TempDir;

    fn layout(root: &TempDir) -> AppLayout {
        AppLayout::new(root.path(), "abc12345").unwrap()
    }

    fn install_manifest(layout: &AppLayout, profile: AppRuntimeProfile, collections: Vec<&str>) {
        let binding = AppRuntimeProfileBinding {
            family: profile,
            revision: 1,
            contract_sha256: "c".repeat(64),
        };
        let mut manifest = AppManifest::for_new_app(layout.app_id(), "QA fixture");
        manifest.revision = 1;
        manifest.collections = collections
            .into_iter()
            .map(|id| DataCollectionSchema {
                id: id.into(),
                name: id.into(),
                fields: Vec::new(),
            })
            .collect();
        manifest.surface = Some(profile.surface());
        manifest.runtime_profile = Some(binding.clone());
        manifest.dependency_snapshot = Some(AppDependencySnapshot {
            requested_sha256: "1".repeat(64),
            package_sha256: "2".repeat(64),
            lockfile_sha256: "3".repeat(64),
            dependency_tree_sha256: "4".repeat(64),
            sbom_sha256: "5".repeat(64),
            toolchain_key: "pnpm@test/node@test".into(),
            verified_profile_contract_sha256: binding.contract_sha256.clone(),
        });
        manifest.template_origin = Some(AppTemplateOrigin {
            plugin_id: AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
            plugin_version: "test".into(),
            template_id: "test".into(),
            template_sha256: binding.contract_sha256,
        });
        save_manifest(layout, &manifest).unwrap();
    }

    fn identity(layout: &AppLayout, profile: AppRuntimeProfile) -> QaIdentity {
        let manifest = crate::manifest::load_manifest(layout).unwrap();
        QaIdentity {
            app_id: layout.app_id().into(),
            workflow_run_id: "run-1".into(),
            qa_handle: "qa_00000000000000000000000000000000".into(),
            build_id: "build-1".into(),
            runtime_profile: AppRuntimeProfileBinding {
                family: profile,
                revision: 1,
                contract_sha256: "c".repeat(64),
            },
            verification_strategy: QaVerificationStrategy::Balanced,
            dependency_snapshot_sha256: manifest.dependency_snapshot_hash().unwrap(),
            authoring_contract_sha256: "b".repeat(64),
            manifest_revision: 1,
            runtime_generation: 1,
        }
    }

    fn begin(root: &TempDir, profile: AppRuntimeProfile) -> AppLayout {
        let layout = layout(root);
        install_manifest(&layout, profile, vec!["chores"]);
        qa_begin(
            &layout,
            identity(&layout, profile),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        layout
    }

    fn evidence(
        id: &str,
        scenario: &str,
        target: &str,
        kind: QaEvidenceKind,
        at: u64,
        event: &str,
        caused_by: Option<&str>,
        content: QaHostEvidenceContent,
    ) -> QaHostEvidenceInput {
        QaHostEvidenceInput {
            evidence_id: id.into(),
            scenario_id: scenario.into(),
            target_id: target.into(),
            kind,
            recorded_at_ms: at,
            event_id: event.into(),
            caused_by: caused_by.map(str::to_owned),
            content,
        }
    }

    fn target(at: u64) -> QaHostEvidenceContent {
        native_target("primary", at)
    }

    fn native_target(target_id: &str, at: u64) -> QaHostEvidenceContent {
        QaHostEvidenceContent::NativeTarget(QaNativeTargetProvenance {
            target_id: target_id.into(),
            os: "ios".into(),
            form_factor: "iphone".into(),
            device_model: "sim".into(),
            captured_at_ms: at,
        })
    }

    fn record_native(
        layout: &AppLayout,
        handle: &str,
        scenario: &str,
        target_id: &str,
        evidence_id: &str,
        at: u64,
    ) {
        qa_record_host_evidence(
            layout,
            handle,
            evidence(
                evidence_id,
                scenario,
                target_id,
                QaEvidenceKind::NativeTargetProvenance,
                at,
                evidence_id,
                None,
                native_target(target_id, at),
            ),
        )
        .unwrap();
    }

    fn roundtrip(layout: &AppLayout) {
        qa_record_host_evidence(
            layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "target",
                "scenario-1",
                "primary",
                QaEvidenceKind::NativeTargetProvenance,
                110,
                "target",
                None,
                target(110),
            ),
        )
        .unwrap();
        qa_record_host_evidence(
            layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "action",
                "scenario-1",
                "primary",
                QaEvidenceKind::UiAction,
                120,
                "action",
                None,
                QaHostEvidenceContent::Json(serde_json::json!({"tap":"save"})),
            ),
        )
        .unwrap();
        qa_record_host_evidence(
            layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "write",
                "scenario-1",
                "primary",
                QaEvidenceKind::BridgeWrite,
                130,
                "write",
                Some("action"),
                QaHostEvidenceContent::Json(serde_json::json!({
                    "results":[{
                        "collection":"chores",
                        "recordId":"row-1",
                        "revision":1,
                        "deleted":false
                    }]
                })),
            ),
        )
        .unwrap();
        qa_record_host_evidence(
            layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "query",
                "scenario-1",
                "primary",
                QaEvidenceKind::Query,
                140,
                "query",
                Some("write"),
                QaHostEvidenceContent::Json(serde_json::json!({
                    "records":[{
                        "collection":"chores",
                        "recordId":"row-1",
                        "revision":1,
                        "document":{},
                        "createdAtMs":130,
                        "updatedAtMs":130
                    }]
                })),
            ),
        )
        .unwrap();
    }

    #[test]
    fn exact_scenario_coverage_and_native_provenance_are_required() {
        let root = TempDir::new().unwrap();
        let layout = begin(&root, AppRuntimeProfile::ReactDom);
        roundtrip(&layout);
        let error = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![],
            vec![],
            200,
        )
        .unwrap_err();
        assert!(matches!(error, AppError::InvalidRequest(_)));
    }

    #[test]
    fn direct_seed_mutation_without_ui_roundtrip_is_rejected() {
        let root = TempDir::new().unwrap();
        let layout = begin(&root, AppRuntimeProfile::ReactDom);
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "seed",
                "scenario-1",
                "primary",
                QaEvidenceKind::Query,
                120,
                "seed",
                None,
                QaHostEvidenceContent::Json(serde_json::json!({"found":true})),
            ),
        )
        .unwrap_err();
        assert!(qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["missing".into()],
                summary: "pass".into()
            }],
            vec![],
            200,
        )
        .is_err());
    }

    #[test]
    fn canvas_requires_distinct_time_separated_captures() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::Canvas2d, vec!["chores"]);
        qa_begin_with_requirements(
            &layout,
            identity(&layout, AppRuntimeProfile::Canvas2d),
            vec![QaScenarioRequirement {
                scenario_id: "scenario-1".into(),
                required: true,
                target_ids: vec!["primary".into()],
                evidence_kinds: vec![QaEvidenceKind::Capture],
                motion_required: true,
            }],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        roundtrip(&layout);
        for (id, at) in [("capture1", 150), ("capture2", 150)] {
            qa_record_host_evidence(
                &layout,
                "qa_00000000000000000000000000000000",
                evidence(
                    id,
                    "scenario-1",
                    "primary",
                    QaEvidenceKind::Capture,
                    at,
                    id,
                    None,
                    QaHostEvidenceContent::Image {
                        format: QaImageFormat::Png,
                        bytes: vec![1, 2, 3],
                    },
                ),
            )
            .unwrap();
        }
        let error = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["target".into(), "capture1".into(), "capture2".into()],
                summary: "pass".into(),
            }],
            vec![],
            200,
        )
        .unwrap_err();
        assert!(matches!(error, AppError::InvalidRequest(_)));
    }

    #[test]
    fn animated_canvas_rejects_identical_time_separated_frames() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::Canvas2d, vec![]);
        qa_begin_with_requirements(
            &layout,
            identity(&layout, AppRuntimeProfile::Canvas2d),
            vec![QaScenarioRequirement {
                scenario_id: "motion".into(),
                required: true,
                target_ids: vec!["primary".into()],
                evidence_kinds: vec![QaEvidenceKind::Capture],
                motion_required: true,
            }],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "motion",
            "primary",
            "native",
            105,
        );
        for (id, at) in [("capture-1", 110), ("capture-2", 120)] {
            qa_record_host_evidence(
                &layout,
                "qa_00000000000000000000000000000000",
                evidence(
                    id,
                    "motion",
                    "primary",
                    QaEvidenceKind::Capture,
                    at,
                    id,
                    None,
                    QaHostEvidenceContent::Image {
                        format: QaImageFormat::Png,
                        bytes: vec![9, 8, 7],
                    },
                ),
            )
            .unwrap();
        }
        let error = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "motion".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native".into(), "capture-1".into(), "capture-2".into()],
                summary: "motion observed".into(),
            }],
            vec![],
            130,
        )
        .unwrap_err();
        assert!(error.to_string().contains("distinct frames"));
    }

    #[test]
    fn failed_canvas_scenario_seals_without_success_captures() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::Canvas2d, vec![]);
        qa_begin_with_requirements(
            &layout,
            identity(&layout, AppRuntimeProfile::Canvas2d),
            vec![QaScenarioRequirement {
                scenario_id: "motion".into(),
                required: true,
                target_ids: vec!["primary".into()],
                evidence_kinds: vec![QaEvidenceKind::Capture],
                motion_required: true,
            }],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "motion",
            "primary",
            "native",
            110,
        );

        let output = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "motion".into(),
                status: QaScenarioStatus::Failed,
                evidence_ids: vec!["native".into()],
                summary: "motion never rendered".into(),
            }],
            vec![],
            120,
        )
        .expect("authenticated Canvas failure must seal without success captures");

        assert_eq!(output.result.status, QaResultStatus::Candidate);
        assert_eq!(
            output.result.scenario_judgements[0].status,
            QaScenarioStatus::Failed
        );
        assert!(output
            .result
            .findings
            .iter()
            .any(|finding| finding.blocking));
    }

    #[test]
    fn unresolved_upstream_failure_is_automatically_carried_into_the_result() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec!["chores"]);
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![QaUpstreamFailure {
                id: "build-failed".into(),
                message: "compile".into(),
                introduced_at_ms: 90,
            }],
            100,
        )
        .unwrap();
        roundtrip(&layout);
        let output = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["target".into(), "query".into()],
                summary: "pass".into(),
            }],
            vec![],
            200,
        )
        .unwrap();
        assert_eq!(output.result.findings.len(), 1);
        assert_eq!(output.result.findings[0].id, "build-failed");
        assert!(output.result.findings[0].blocking);
    }

    #[test]
    fn evidence_replays_as_json_and_result_is_separate_from_session() {
        let root = TempDir::new().unwrap();
        let layout = begin(&root, AppRuntimeProfile::ReactDom);
        roundtrip(&layout);
        let block =
            qa_read_evidence(&layout, "qa_00000000000000000000000000000000", "query").unwrap();
        assert!(matches!(block, QaEvidenceBlock::Json { .. }));
        let out = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["target".into(), "query".into()],
                summary: "pass".into(),
            }],
            vec![],
            200,
        )
        .unwrap();
        assert!(load_qa_session(&layout, "qa_00000000000000000000000000000000").is_ok());
        assert_eq!(
            load_qa_result(&layout, &out.receipt.result_id).unwrap(),
            out.result
        );
        assert_eq!(
            load_qa_receipt(&layout, &out.receipt.receipt_id).unwrap(),
            out.receipt
        );
        qa_cleanup_session(&layout, "qa_00000000000000000000000000000000").unwrap();
        assert!(load_qa_session(&layout, "qa_00000000000000000000000000000000").is_err());
    }

    #[test]
    fn loaded_session_artifact_read_reuses_session_and_rejects_tampering() {
        let root = TempDir::new().unwrap();
        let layout = begin(&root, AppRuntimeProfile::ReactDom);
        roundtrip(&layout);
        let session = load_qa_session(&layout, "qa_00000000000000000000000000000000").unwrap();
        let write = session
            .evidence
            .iter()
            .find(|evidence| evidence.evidence_id == "write")
            .expect("bridge write evidence");

        // Removing the persisted session proves this path uses the already
        // loaded authoritative session instead of reloading it per artifact.
        rooted_fs::remove_file(
            layout.root(),
            &session_path(&layout, &session.identity.qa_handle).unwrap(),
        )
        .unwrap();
        let QaEvidenceBlock::Json { content, .. } =
            qa_read_evidence_from_loaded_session(&layout, &session, &write.evidence_id).unwrap()
        else {
            panic!("bridge write evidence must be JSON");
        };
        assert_eq!(content["results"][0]["collection"], "chores");

        // A well-formed id that this session does not own is still refused:
        // skipping the reload must not also skip the membership check.
        let foreign =
            qa_read_evidence_from_loaded_session(&layout, &session, "notmyevidence").unwrap_err();
        assert!(
            matches!(foreign, AppError::NotFound(_)),
            "an id outside this session must not resolve: {foreign}"
        );

        let path = artifact_path(&layout, &session.identity.qa_handle, &write.artifact).unwrap();
        rooted_fs::atomic_write(
            layout.root(),
            &path,
            br#"{"tampered":true}"#,
            AtomicWriteOptions {
                overwrite: true,
                create_parents: false,
                file_mode: 0o600,
                ..AtomicWriteOptions::default()
            },
        )
        .unwrap();
        let error = qa_read_evidence_from_loaded_session(&layout, &session, &write.evidence_id)
            .unwrap_err();
        assert!(error.to_string().contains("integrity"));
    }

    #[test]
    fn stateless_app_requires_no_persistence_roundtrip() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native",
            110,
        );
        qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native".into()],
                summary: "native render passed".into(),
            }],
            vec![],
            120,
        )
        .unwrap();
    }

    #[test]
    fn each_manifest_collection_needs_matching_mutation_and_query_payloads() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(
            &layout,
            AppRuntimeProfile::ReactDom,
            vec!["chores", "notes"],
        );
        qa_begin_with_requirements(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec![QaScenarioRequirement {
                scenario_id: "scenario-1".into(),
                required: true,
                target_ids: vec!["primary".into()],
                evidence_kinds: vec![QaEvidenceKind::UiAction],
                motion_required: false,
            }],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native",
            105,
        );
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "action-1",
                "scenario-1",
                "primary",
                QaEvidenceKind::UiAction,
                110,
                "action-1",
                None,
                QaHostEvidenceContent::Json(serde_json::json!({"action":"save"})),
            ),
        )
        .unwrap();
        for (suffix, collection, base) in [("1", "chores", 120), ("2", "notes", 150)] {
            qa_record_host_evidence(
                &layout,
                "qa_00000000000000000000000000000000",
                evidence(
                    &format!("write-{suffix}"),
                    "scenario-1",
                    "primary",
                    QaEvidenceKind::BridgeWrite,
                    base,
                    &format!("write-{suffix}"),
                    Some("action-1"),
                    QaHostEvidenceContent::Json(serde_json::json!({"results":[{
                        "collection":collection,
                        "recordId":format!("row-{suffix}"),
                        "revision":1,
                        "deleted":false
                    }]})),
                ),
            )
            .unwrap();
            qa_record_host_evidence(
                &layout,
                "qa_00000000000000000000000000000000",
                evidence(
                    &format!("query-{suffix}"),
                    "scenario-1",
                    "primary",
                    QaEvidenceKind::Query,
                    base + 1,
                    &format!("query-{suffix}"),
                    Some(&format!("write-{suffix}")),
                    QaHostEvidenceContent::Json(serde_json::json!({"records":[{
                        "collection":collection,
                        "recordId":format!("row-{suffix}"),
                        "revision":if collection == "chores" { 99 } else { 1 }
                    }]})),
                ),
            )
            .unwrap();
        }
        let judgement = QaScenarioJudgement {
            scenario_id: "scenario-1".into(),
            status: QaScenarioStatus::Passed,
            evidence_ids: vec![
                "native".into(),
                "action-1".into(),
                "write-1".into(),
                "query-1".into(),
                "write-2".into(),
                "query-2".into(),
            ],
            summary: "checked both collections".into(),
        };
        let error = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![judgement.clone()],
            vec![],
            200,
        )
        .unwrap_err();
        assert!(error.to_string().contains("chores"));

        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "query-3",
                "scenario-1",
                "primary",
                QaEvidenceKind::Query,
                180,
                "query-3",
                Some("write-1"),
                QaHostEvidenceContent::Json(serde_json::json!({"records":[{
                    "collection":"chores", "recordId":"row-1", "revision":1
                }]})),
            ),
        )
        .unwrap();
        let mut judgement = judgement;
        judgement.evidence_ids.push("query-3".into());
        qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![judgement],
            vec![],
            210,
        )
        .unwrap();
    }

    #[test]
    fn causal_chain_rejects_cross_scenario_or_nonadvancing_events() {
        let root = TempDir::new().unwrap();
        let layout = begin(&root, AppRuntimeProfile::ReactDom);
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "action",
                "scenario-1",
                "primary",
                QaEvidenceKind::UiAction,
                120,
                "action",
                None,
                QaHostEvidenceContent::Json(serde_json::json!({"tap":"save"})),
            ),
        )
        .unwrap();
        let error = qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "write",
                "scenario-1",
                "primary",
                QaEvidenceKind::BridgeWrite,
                120,
                "write",
                Some("action"),
                QaHostEvidenceContent::Json(serde_json::json!({"results":[]})),
            ),
        )
        .unwrap_err();
        assert!(error.to_string().contains("earlier"));
    }

    #[test]
    fn required_evidence_kind_is_checked_for_each_scenario_target() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin_with_requirements(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec![QaScenarioRequirement {
                scenario_id: "scenario-1".into(),
                required: true,
                target_ids: vec!["phone".into(), "tablet".into()],
                evidence_kinds: vec![QaEvidenceKind::Inspect],
                motion_required: false,
            }],
            vec!["phone".into(), "tablet".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "phone",
            "native-phone",
            105,
        );
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "tablet",
            "native-tablet",
            106,
        );
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "inspect-phone",
                "scenario-1",
                "phone",
                QaEvidenceKind::Inspect,
                110,
                "inspect-phone",
                None,
                QaHostEvidenceContent::Json(serde_json::json!({"visible":true})),
            ),
        )
        .unwrap();
        let error = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec![
                    "native-phone".into(),
                    "native-tablet".into(),
                    "inspect-phone".into(),
                ],
                summary: "only the phone was inspected".into(),
            }],
            vec![],
            120,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Inspect"));
        assert!(message.contains("tablet"));
    }

    #[test]
    fn static_canvas_allows_identical_but_time_separated_capture_bytes() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::Canvas2d, vec![]);
        qa_begin_with_requirements(
            &layout,
            identity(&layout, AppRuntimeProfile::Canvas2d),
            vec![QaScenarioRequirement {
                scenario_id: "static-scene".into(),
                required: true,
                target_ids: vec!["primary".into()],
                evidence_kinds: vec![QaEvidenceKind::Capture],
                motion_required: false,
            }],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "static-scene",
            "primary",
            "native",
            105,
        );
        for (id, at) in [("capture-1", 110), ("capture-2", 120)] {
            qa_record_host_evidence(
                &layout,
                "qa_00000000000000000000000000000000",
                evidence(
                    id,
                    "static-scene",
                    "primary",
                    QaEvidenceKind::Capture,
                    at,
                    id,
                    None,
                    QaHostEvidenceContent::Image {
                        format: QaImageFormat::Png,
                        bytes: vec![1, 2, 3],
                    },
                ),
            )
            .unwrap();
        }
        qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "static-scene".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native".into(), "capture-1".into(), "capture-2".into()],
                summary: "static scene remained stable".into(),
            }],
            vec![],
            130,
        )
        .unwrap();
    }

    #[test]
    fn verifier_finalization_appends_an_immutable_union_candidate() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native",
            110,
        );
        let judgement = QaScenarioJudgement {
            scenario_id: "scenario-1".into(),
            status: QaScenarioStatus::Passed,
            evidence_ids: vec!["native".into()],
            summary: "passed".into(),
        };
        let tester_finding = QaFinding {
            id: "tester-note".into(),
            message: "tester observation".into(),
            blocking: false,
            resolved_by_evidence_ids: vec![],
        };
        let first = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![judgement.clone()],
            vec![tester_finding.clone()],
            120,
        )
        .unwrap();
        let verifier_finding = QaFinding {
            id: "verifier-note".into(),
            message: "independent observation".into(),
            blocking: false,
            resolved_by_evidence_ids: vec![],
        };
        let second = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![judgement.clone()],
            vec![verifier_finding],
            130,
        )
        .unwrap();
        assert_eq!(
            second.result.previous_result_id.as_deref(),
            Some(first.receipt.result_id.as_str())
        );
        assert_eq!(second.result.findings.len(), 2);
        assert_eq!(
            load_qa_result(&layout, &first.receipt.result_id)
                .unwrap()
                .findings,
            vec![tester_finding]
        );
        assert!(publish_qa_result(&layout, &first.receipt.result_id).is_err());
        let immutable_second =
            serde_json::to_vec(&load_qa_result(&layout, &second.receipt.result_id).unwrap())
                .unwrap();
        publish_qa_result(&layout, &second.receipt.result_id).unwrap();
        assert!(
            load_qa_publication_marker(&layout, &second.receipt.result_id)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            serde_json::to_vec(&load_qa_result(&layout, &second.receipt.result_id).unwrap())
                .unwrap(),
            immutable_second,
            "publication must not rewrite the candidate"
        );
        assert!(matches!(
            qa_finalize(
                &layout,
                "qa_00000000000000000000000000000000",
                vec![judgement],
                vec![],
                140,
            ),
            Err(AppError::WorkflowStateInvalid(_))
        ));
        assert!(qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "late",
                "scenario-1",
                "primary",
                QaEvidenceKind::Inspect,
                140,
                "late",
                None,
                QaHostEvidenceContent::Json(serde_json::json!({"late":true})),
            )
        )
        .is_err());
        qa_cleanup_session(&layout, "qa_00000000000000000000000000000000").unwrap();
        assert_eq!(
            publish_qa_result(&layout, &second.receipt.result_id)
                .unwrap()
                .result_sha256,
            second.result.result_sha256,
            "a published historical result remains valid after terminal cleanup/restart"
        );
    }

    #[test]
    fn unresolved_findings_cross_fresh_handles_without_caller_input() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native-1",
            110,
        );
        qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Failed,
                evidence_ids: vec!["native-1".into()],
                summary: "failed".into(),
            }],
            vec![QaFinding {
                id: "persistent-bug".into(),
                message: "still broken".into(),
                blocking: true,
                resolved_by_evidence_ids: vec![],
            }],
            120,
        )
        .unwrap();
        let mut second_identity = identity(&layout, AppRuntimeProfile::ReactDom);
        second_identity.qa_handle = "qa_11111111111111111111111111111111".into();
        second_identity.runtime_generation = 2;
        qa_begin(
            &layout,
            second_identity,
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            200,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_11111111111111111111111111111111",
            "scenario-1",
            "primary",
            "native-2",
            210,
        );
        let output = qa_finalize(
            &layout,
            "qa_11111111111111111111111111111111",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native-2".into()],
                summary: "rechecked".into(),
            }],
            vec![],
            220,
        )
        .unwrap();
        assert_eq!(output.result.findings[0].id, "persistent-bug");
        assert!(output.result.findings[0].blocking);
    }

    #[test]
    fn failed_scenario_is_durable_against_same_session_verifier_but_resolves_with_fresh_evidence() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native-failed",
            110,
        );
        let failed = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Failed,
                evidence_ids: vec!["native-failed".into()],
                summary: "button did not respond".into(),
            }],
            vec![],
            120,
        )
        .unwrap();
        assert_eq!(failed.result.findings.len(), 1);
        assert!(failed.result.findings[0].blocking);
        assert!(failed.result.findings[0]
            .id
            .starts_with(SCENARIO_FAILURE_PREFIX));

        // The verifier sees the same sealed tester evidence and therefore
        // cannot turn the failed scenario into a green, finding-free result.
        let verifier = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native-failed".into()],
                summary: "claimed pass without new evidence".into(),
            }],
            vec![],
            130,
        )
        .unwrap();
        assert!(verifier
            .result
            .findings
            .iter()
            .any(|finding| finding.blocking));

        // A verifier may echo the exact Host blocker, but cannot alter it.
        let echoed = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native-failed".into()],
                summary: "same evidence, exact blocker echo".into(),
            }],
            failed.result.findings.clone(),
            140,
        )
        .unwrap();
        assert!(echoed.result.findings[0].blocking);

        // A resample gets a new handle and new Host evidence. It can resolve
        // the exact scenario/target blocker and leave no stale failure.
        let mut fresh_identity = identity(&layout, AppRuntimeProfile::ReactDom);
        fresh_identity.qa_handle = "qa_11111111111111111111111111111111".into();
        fresh_identity.runtime_generation = 2;
        qa_begin(
            &layout,
            fresh_identity,
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            200,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_11111111111111111111111111111111",
            "scenario-1",
            "primary",
            // Evidence ids are scoped to a QA handle. Reusing a deterministic
            // local id on a new handle must not make fresh Host evidence stale.
            "native-failed",
            210,
        );
        let resolved = qa_finalize(
            &layout,
            "qa_11111111111111111111111111111111",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native-failed".into()],
                summary: "resampled pass".into(),
            }],
            vec![],
            220,
        )
        .unwrap();
        assert!(resolved.result.findings.is_empty());
    }

    #[test]
    fn failed_ui_attempt_can_finalize_without_success_roundtrip() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec!["chores"]);
        qa_begin_with_requirements(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec![QaScenarioRequirement {
                scenario_id: "scenario-1".into(),
                required: true,
                target_ids: vec!["primary".into()],
                evidence_kinds: vec![
                    QaEvidenceKind::Inspect,
                    QaEvidenceKind::UiAction,
                    QaEvidenceKind::BridgeWrite,
                    QaEvidenceKind::Query,
                ],
                motion_required: false,
            }],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native",
            110,
        );
        for (id, kind, at, value) in [
            (
                "inspect",
                QaEvidenceKind::Inspect,
                120,
                serde_json::json!({"ready": true}),
            ),
            (
                "action",
                QaEvidenceKind::UiAction,
                130,
                serde_json::json!({"ok": false, "error": "Save handler did not respond"}),
            ),
        ] {
            qa_record_host_evidence(
                &layout,
                "qa_00000000000000000000000000000000",
                evidence(
                    id,
                    "scenario-1",
                    "primary",
                    kind,
                    at,
                    id,
                    None,
                    QaHostEvidenceContent::Json(value),
                ),
            )
            .unwrap();
        }

        let output = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Failed,
                evidence_ids: vec!["native".into(), "inspect".into(), "action".into()],
                summary: "Save handler did not produce a bridge write".into(),
            }],
            vec![],
            200,
        )
        .unwrap();
        assert_eq!(
            output.result.scenario_judgements[0].status,
            QaScenarioStatus::Failed
        );
        assert!(output
            .result
            .findings
            .iter()
            .any(|finding| finding.blocking));
    }

    #[test]
    fn passed_judgement_rejects_an_explicitly_failed_ui_action() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin_with_requirements(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec![QaScenarioRequirement {
                scenario_id: "scenario-1".into(),
                required: true,
                target_ids: vec!["primary".into()],
                evidence_kinds: vec![QaEvidenceKind::UiAction],
                motion_required: false,
            }],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native",
            110,
        );
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "failed-action",
                "scenario-1",
                "primary",
                QaEvidenceKind::UiAction,
                120,
                "failed-action",
                None,
                QaHostEvidenceContent::Json(serde_json::json!({
                    "ok": false,
                    "error": "Save handler rejected the request",
                })),
            ),
        )
        .unwrap();

        let error = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native".into(), "failed-action".into()],
                summary: "claimed pass".into(),
            }],
            vec![],
            200,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("passed QA scenario references a failed Host UI action"));
    }

    #[test]
    fn failed_ui_action_cannot_prove_a_persistence_roundtrip() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec!["chores"]);
        qa_begin_with_requirements(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec![QaScenarioRequirement {
                scenario_id: "scenario-1".into(),
                required: true,
                target_ids: vec!["primary".into()],
                evidence_kinds: vec![QaEvidenceKind::Inspect],
                motion_required: false,
            }],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native",
            110,
        );
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "failed-action",
                "scenario-1",
                "primary",
                QaEvidenceKind::UiAction,
                120,
                "failed-action",
                None,
                QaHostEvidenceContent::Json(serde_json::json!({
                    "ok": false,
                    "error": "Save handler rejected the request",
                })),
            ),
        )
        .unwrap();
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "write",
                "scenario-1",
                "primary",
                QaEvidenceKind::BridgeWrite,
                130,
                "write",
                Some("failed-action"),
                QaHostEvidenceContent::Json(serde_json::json!({
                    "results":[{
                        "collection":"chores",
                        "recordId":"row-1",
                        "revision":1,
                        "deleted":false
                    }]
                })),
            ),
        )
        .unwrap();
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "query",
                "scenario-1",
                "primary",
                QaEvidenceKind::Query,
                140,
                "query",
                Some("write"),
                QaHostEvidenceContent::Json(serde_json::json!({
                    "records":[{
                        "collection":"chores",
                        "recordId":"row-1",
                        "revision":1
                    }]
                })),
            ),
        )
        .unwrap();
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "inspect",
                "scenario-1",
                "primary",
                QaEvidenceKind::Inspect,
                150,
                "inspect",
                None,
                QaHostEvidenceContent::Json(serde_json::json!({"visible": true})),
            ),
        )
        .unwrap();

        let error = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native".into(), "inspect".into()],
                summary: "claimed persistence pass".into(),
            }],
            vec![],
            200,
        )
        .unwrap_err();
        assert!(error.to_string().contains("lacks an actual UI action"));
    }

    #[test]
    fn passed_scenario_still_requires_success_roundtrip_when_failed_policy_is_used() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec!["chores"]);
        qa_begin_with_requirements(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec![QaScenarioRequirement {
                scenario_id: "scenario-1".into(),
                required: true,
                target_ids: vec!["primary".into()],
                evidence_kinds: vec![
                    QaEvidenceKind::Inspect,
                    QaEvidenceKind::UiAction,
                    QaEvidenceKind::BridgeWrite,
                    QaEvidenceKind::Query,
                ],
                motion_required: false,
            }],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native",
            110,
        );
        for (id, kind, at) in [
            ("inspect", QaEvidenceKind::Inspect, 120),
            ("action", QaEvidenceKind::UiAction, 130),
        ] {
            qa_record_host_evidence(
                &layout,
                "qa_00000000000000000000000000000000",
                evidence(
                    id,
                    "scenario-1",
                    "primary",
                    kind,
                    at,
                    id,
                    None,
                    QaHostEvidenceContent::Json(serde_json::json!({"id": id})),
                ),
            )
            .unwrap();
        }

        let error = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native".into(), "inspect".into(), "action".into()],
                summary: "claimed pass without persistence proof".into(),
            }],
            vec![],
            200,
        )
        .unwrap_err();
        assert!(error.to_string().contains("lacks") || error.to_string().contains("roundtrip"));
    }

    #[test]
    fn mixed_stateful_scenarios_seal_a_failed_candidate_without_global_roundtrip() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec!["chores"]);
        qa_begin_with_requirements(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec![
                QaScenarioRequirement {
                    scenario_id: "render".into(),
                    required: true,
                    target_ids: vec!["primary".into()],
                    evidence_kinds: vec![QaEvidenceKind::Inspect],
                    motion_required: false,
                },
                QaScenarioRequirement {
                    scenario_id: "save".into(),
                    required: true,
                    target_ids: vec!["primary".into()],
                    evidence_kinds: vec![QaEvidenceKind::BridgeWrite, QaEvidenceKind::Query],
                    motion_required: false,
                },
            ],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "render",
            "primary",
            "native-render",
            110,
        );
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "save",
            "primary",
            "native-save",
            111,
        );
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "inspect-render",
                "render",
                "primary",
                QaEvidenceKind::Inspect,
                120,
                "inspect-render",
                None,
                QaHostEvidenceContent::Json(serde_json::json!({"visible": true})),
            ),
        )
        .unwrap();
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "save-error",
                "save",
                "primary",
                QaEvidenceKind::RuntimeError,
                130,
                "save-error",
                None,
                QaHostEvidenceContent::Text("save failed before bridge write".into()),
            ),
        )
        .unwrap();

        let output = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![
                QaScenarioJudgement {
                    scenario_id: "render".into(),
                    status: QaScenarioStatus::Passed,
                    evidence_ids: vec!["native-render".into(), "inspect-render".into()],
                    summary: "render passed".into(),
                },
                QaScenarioJudgement {
                    scenario_id: "save".into(),
                    status: QaScenarioStatus::Failed,
                    evidence_ids: vec!["native-save".into(), "save-error".into()],
                    summary: "save failed before persistence".into(),
                },
            ],
            vec![],
            200,
        )
        .expect("mixed authenticated QA must seal an authoritative failed candidate");

        assert_eq!(output.result.status, QaResultStatus::Candidate);
        assert!(output
            .result
            .scenario_judgements
            .iter()
            .any(|judgement| judgement.status == QaScenarioStatus::Failed));
        assert!(output
            .result
            .findings
            .iter()
            .any(|finding| finding.blocking));
    }

    #[test]
    fn referenced_inspect_artifact_must_still_exist_and_match_before_sealing() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin_with_requirements(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec![QaScenarioRequirement {
                scenario_id: "scenario-1".into(),
                required: true,
                target_ids: vec!["primary".into()],
                evidence_kinds: vec![QaEvidenceKind::Inspect],
                motion_required: false,
            }],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native",
            110,
        );
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "inspect",
                "scenario-1",
                "primary",
                QaEvidenceKind::Inspect,
                120,
                "inspect",
                None,
                QaHostEvidenceContent::Json(serde_json::json!({"visible": true})),
            ),
        )
        .unwrap();
        let session = load_qa_session(&layout, "qa_00000000000000000000000000000000").unwrap();
        let inspect = session
            .evidence
            .iter()
            .find(|item| item.evidence_id == "inspect")
            .unwrap();
        let path = layout
            .root()
            .join(artifact_path(&layout, &session.identity.qa_handle, &inspect.artifact).unwrap());
        std::fs::write(path, b"tampered").unwrap();

        let error = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native".into(), "inspect".into()],
                summary: "pass".into(),
            }],
            vec![],
            200,
        )
        .unwrap_err();
        assert!(error.to_string().contains("integrity"));
    }

    #[test]
    fn every_referenced_nonvisual_artifact_must_match_before_sealing() {
        for tampered_id in ["write", "query", "console", "runtime-error"] {
            let root = TempDir::new().unwrap();
            let layout = layout(&root);
            install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
            qa_begin_with_requirements(
                &layout,
                identity(&layout, AppRuntimeProfile::ReactDom),
                vec![QaScenarioRequirement {
                    scenario_id: "scenario-1".into(),
                    required: true,
                    target_ids: vec!["primary".into()],
                    evidence_kinds: vec![
                        QaEvidenceKind::UiAction,
                        QaEvidenceKind::BridgeWrite,
                        QaEvidenceKind::Query,
                        QaEvidenceKind::Console,
                        QaEvidenceKind::RuntimeError,
                    ],
                    motion_required: false,
                }],
                vec!["primary".into()],
                vec![],
                100,
            )
            .unwrap();
            record_native(
                &layout,
                "qa_00000000000000000000000000000000",
                "scenario-1",
                "primary",
                "native",
                105,
            );
            for input in [
                evidence(
                    "action",
                    "scenario-1",
                    "primary",
                    QaEvidenceKind::UiAction,
                    110,
                    "action",
                    None,
                    QaHostEvidenceContent::Json(serde_json::json!({"ok": true})),
                ),
                evidence(
                    "write",
                    "scenario-1",
                    "primary",
                    QaEvidenceKind::BridgeWrite,
                    120,
                    "write",
                    Some("action"),
                    QaHostEvidenceContent::Json(serde_json::json!({"results": []})),
                ),
                evidence(
                    "query",
                    "scenario-1",
                    "primary",
                    QaEvidenceKind::Query,
                    130,
                    "query",
                    Some("write"),
                    QaHostEvidenceContent::Json(serde_json::json!({"records": []})),
                ),
                evidence(
                    "console",
                    "scenario-1",
                    "primary",
                    QaEvidenceKind::Console,
                    140,
                    "console",
                    None,
                    QaHostEvidenceContent::Text("console output".into()),
                ),
                evidence(
                    "runtime-error",
                    "scenario-1",
                    "primary",
                    QaEvidenceKind::RuntimeError,
                    150,
                    "runtime-error",
                    None,
                    QaHostEvidenceContent::Text("runtime error".into()),
                ),
            ] {
                qa_record_host_evidence(&layout, "qa_00000000000000000000000000000000", input)
                    .unwrap();
            }
            let session = load_qa_session(&layout, "qa_00000000000000000000000000000000").unwrap();
            let tampered = session
                .evidence
                .iter()
                .find(|item| item.evidence_id == tampered_id)
                .unwrap();
            let path = layout.root().join(
                artifact_path(&layout, &session.identity.qa_handle, &tampered.artifact).unwrap(),
            );
            std::fs::write(path, vec![b'x'; tampered.artifact.byte_len as usize]).unwrap();

            let error = qa_finalize(
                &layout,
                "qa_00000000000000000000000000000000",
                vec![QaScenarioJudgement {
                    scenario_id: "scenario-1".into(),
                    status: QaScenarioStatus::Passed,
                    evidence_ids: vec![
                        "native".into(),
                        "action".into(),
                        "write".into(),
                        "query".into(),
                        "console".into(),
                        "runtime-error".into(),
                    ],
                    summary: "pass".into(),
                }],
                vec![],
                200,
            )
            .expect_err("tampered nonvisual evidence must block sealing");
            assert!(
                error.to_string().contains("integrity"),
                "unexpected error for {tampered_id}: {error}"
            );
        }
    }

    #[test]
    fn scoped_device_run_preserves_unverified_targets_and_scenarios() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        let session = qa_begin_with_scope(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec![
                QaScenarioRequirement {
                    scenario_id: "phone-flow".into(),
                    required: true,
                    target_ids: vec!["phone".into(), "tablet".into()],
                    evidence_kinds: vec![QaEvidenceKind::Inspect],
                    motion_required: false,
                },
                QaScenarioRequirement {
                    scenario_id: "tablet-only".into(),
                    required: true,
                    target_ids: vec!["tablet".into()],
                    evidence_kinds: vec![QaEvidenceKind::Inspect],
                    motion_required: false,
                },
            ],
            QaVerificationScope {
                declared_target_ids: vec!["phone".into(), "tablet".into()],
                in_scope_target_ids: vec!["phone".into()],
                unverified_target_ids: vec!["tablet".into()],
                unverified_scenario_ids: vec!["tablet-only".into()],
            },
            vec![],
            100,
        )
        .unwrap();
        assert_eq!(session.required_scenario_ids, vec!["phone-flow"]);
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "phone-flow",
            "phone",
            "native-phone",
            110,
        );
        qa_record_host_evidence(
            &layout,
            "qa_00000000000000000000000000000000",
            evidence(
                "inspect-phone",
                "phone-flow",
                "phone",
                QaEvidenceKind::Inspect,
                120,
                "inspect-phone",
                None,
                QaHostEvidenceContent::Json(serde_json::json!({"visible": true})),
            ),
        )
        .unwrap();
        let output = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "phone-flow".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native-phone".into(), "inspect-phone".into()],
                summary: "phone verified".into(),
            }],
            vec![],
            200,
        )
        .unwrap();
        assert_eq!(
            output.result.verification_scope.unverified_target_ids,
            vec!["tablet"]
        );
        assert_eq!(
            output.result.verification_scope.unverified_scenario_ids,
            vec!["tablet-only"]
        );
    }

    #[test]
    fn scoped_device_run_rejects_empty_current_target_scope() {
        let scope = QaVerificationScope {
            declared_target_ids: vec!["phone".into()],
            in_scope_target_ids: vec![],
            unverified_target_ids: vec!["phone".into()],
            unverified_scenario_ids: vec![],
        };
        assert!(scope.validate().is_err());
    }

    #[test]
    fn failed_scenario_is_not_resolved_by_fresh_evidence_for_another_scope() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native-failed",
            110,
        );
        let failed = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Failed,
                evidence_ids: vec!["native-failed".into()],
                summary: "failed on primary".into(),
            }],
            vec![],
            120,
        )
        .unwrap();
        let mut false_resolution = failed.result.findings[0].clone();
        false_resolution.blocking = false;
        false_resolution.resolved_by_evidence_ids = vec!["native-other-target".into()];

        let mut second_identity = identity(&layout, AppRuntimeProfile::ReactDom);
        second_identity.qa_handle = "qa_11111111111111111111111111111111".into();
        second_identity.runtime_generation = 2;
        qa_begin(
            &layout,
            second_identity,
            vec!["scenario-1".into()],
            vec!["secondary".into()],
            vec![],
            200,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_11111111111111111111111111111111",
            "scenario-1",
            "secondary",
            "native-other-target",
            210,
        );
        assert!(qa_finalize(
            &layout,
            "qa_11111111111111111111111111111111",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native-other-target".into()],
                summary: "different target passed".into(),
            }],
            vec![false_resolution.clone()],
            220,
        )
        .is_err());
        let other_target = qa_finalize(
            &layout,
            "qa_11111111111111111111111111111111",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native-other-target".into()],
                summary: "different target passed".into(),
            }],
            vec![],
            221,
        )
        .unwrap();
        assert_eq!(other_target.result.findings.len(), 1);
        assert_eq!(
            other_target.result.findings[0].id,
            scenario_failure_id("scenario-1", "primary").unwrap()
        );
        assert!(other_target.result.findings[0].blocking);

        let mut third_identity = identity(&layout, AppRuntimeProfile::ReactDom);
        third_identity.qa_handle = "qa_22222222222222222222222222222222".into();
        third_identity.runtime_generation = 3;
        qa_begin(
            &layout,
            third_identity,
            vec!["scenario-2".into()],
            vec!["primary".into()],
            vec![],
            300,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_22222222222222222222222222222222",
            "scenario-2",
            "primary",
            "native-other-scenario",
            310,
        );
        false_resolution.resolved_by_evidence_ids = vec!["native-other-scenario".into()];
        assert!(qa_finalize(
            &layout,
            "qa_22222222222222222222222222222222",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-2".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native-other-scenario".into()],
                summary: "different scenario passed".into(),
            }],
            vec![false_resolution],
            320,
        )
        .is_err());
        let other_scenario = qa_finalize(
            &layout,
            "qa_22222222222222222222222222222222",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-2".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native-other-scenario".into()],
                summary: "different scenario passed".into(),
            }],
            vec![],
            321,
        )
        .unwrap();
        assert_eq!(other_scenario.result.findings.len(), 1);
        assert_eq!(
            other_scenario.result.findings[0].id,
            scenario_failure_id("scenario-1", "primary").unwrap()
        );
        assert!(other_scenario.result.findings[0].blocking);
    }

    #[test]
    fn scenario_failure_ids_are_bounded_unambiguous_and_host_reserved() {
        let left = scenario_failure_id("a:b", "c").unwrap();
        let right = scenario_failure_id("a", "b:c").unwrap();
        assert_ne!(left, right);
        assert!(left.len() <= 256);
        assert!(right.len() <= 256);
        assert!(
            scenario_failure_id(&"s".repeat(256), &"t".repeat(256))
                .unwrap()
                .len()
                <= 256
        );

        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native",
            110,
        );
        let error = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native".into()],
                summary: "passed".into(),
            }],
            vec![QaFinding {
                id: scenario_failure_id("forged", "scope").unwrap(),
                message: "caller invented Host finding".into(),
                blocking: true,
                resolved_by_evidence_ids: vec![],
            }],
            120,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("reserved Host scenario namespace"));
    }

    #[test]
    fn concurrent_host_evidence_writes_do_not_lose_rows() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native",
            101,
        );
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(17));
        let mut threads = Vec::new();
        for index in 0..16u64 {
            let layout = layout.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let id = format!("inspect-{index}");
                qa_record_host_evidence(
                    &layout,
                    "qa_00000000000000000000000000000000",
                    evidence(
                        &id,
                        "scenario-1",
                        "primary",
                        QaEvidenceKind::Inspect,
                        110 + index,
                        &id,
                        None,
                        QaHostEvidenceContent::Json(serde_json::json!({"index":index})),
                    ),
                )
                .unwrap();
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            load_qa_session(&layout, "qa_00000000000000000000000000000000")
                .unwrap()
                .evidence
                .len(),
            17
        );
    }

    #[test]
    fn qa_identity_must_match_the_full_manifest_profile_and_dependency_binding() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        let mut wrong_profile = identity(&layout, AppRuntimeProfile::ReactDom);
        wrong_profile.runtime_profile.revision = 2;
        let error = qa_begin(
            &layout,
            wrong_profile,
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap_err();
        assert!(error.to_string().contains("manifest/profile/dependency"));

        let mut wrong_dependency = identity(&layout, AppRuntimeProfile::ReactDom);
        wrong_dependency.dependency_snapshot_sha256 = "f".repeat(64);
        let error = qa_begin(
            &layout,
            wrong_dependency,
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap_err();
        assert!(error.to_string().contains("manifest/profile/dependency"));
    }

    #[test]
    fn durable_qa_fixture_and_schema_have_the_exact_result_dto_keys() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/qa-result.valid.json")).unwrap();
        let dto: QaResult = serde_json::from_value(fixture.clone()).unwrap();
        let root = TempDir::new().unwrap();
        assert!(dto.identity.validate(&layout(&root)).is_ok());
        assert_eq!(serde_json::to_value(dto).unwrap(), fixture);
        let schema: Value = serde_json::from_str(include_str!(
            "../../plugins/lingxi-local-app/schemas/qa-report.schema.json"
        ))
        .unwrap();
        let required = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<BTreeSet<_>>();
        let actual = fixture
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(required, actual);
        assert_eq!(schema["properties"]["status"]["const"], "candidate");
        assert_eq!(
            schema["$defs"]["qa_handle"]["pattern"],
            ids::QA_HANDLE_PATTERN
        );

        let mut unknown = fixture;
        unknown["legacy_agent_boolean"] = Value::Bool(true);
        assert!(serde_json::from_value::<QaResult>(unknown).is_err());
    }

    #[test]
    fn qa_utf8_text_limits_are_byte_bounded() {
        let at_limit = QaFinding {
            id: "unicode".into(),
            message: "界".repeat((16 * 1024) / 3),
            blocking: false,
            resolved_by_evidence_ids: vec![],
        };
        validate_finding(&at_limit).unwrap();
        let mut over_limit = at_limit;
        over_limit.message.push('界');
        let error = validate_finding(&over_limit).unwrap_err().to_string();
        assert!(error.contains("finding message"));
        assert!(error.contains("bytes"));
    }

    #[test]
    fn terminal_cleanup_is_idempotent_and_history_prunes_complete_document_sets() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native",
            110,
        );
        let judgement = QaScenarioJudgement {
            scenario_id: "scenario-1".into(),
            status: QaScenarioStatus::Passed,
            evidence_ids: vec!["native".into()],
            summary: "passed".into(),
        };
        let mut outputs = Vec::new();
        for index in 0..(MAX_QA_HISTORY_RESULTS + 2) {
            outputs.push(
                qa_finalize(
                    &layout,
                    "qa_00000000000000000000000000000000",
                    vec![judgement.clone()],
                    vec![],
                    120 + index as u64,
                )
                .unwrap(),
            );
        }
        let latest = outputs.last().unwrap();
        publish_qa_result(&layout, &latest.receipt.result_id).unwrap();
        assert_eq!(
            prune_qa_history(&layout, std::slice::from_ref(&latest.receipt.result_id)).unwrap(),
            2
        );
        for pruned in &outputs[..2] {
            assert!(matches!(
                load_qa_result(&layout, &pruned.receipt.result_id),
                Err(AppError::NotFound(_))
            ));
            assert!(matches!(
                load_qa_receipt(&layout, &pruned.receipt.receipt_id),
                Err(AppError::NotFound(_))
            ));
        }
        assert!(load_qa_result(&layout, &latest.receipt.result_id).is_ok());
        assert!(
            load_qa_publication_marker(&layout, &latest.receipt.result_id)
                .unwrap()
                .is_some()
        );
        qa_cleanup_session(&layout, "qa_00000000000000000000000000000000").unwrap();
        qa_cleanup_session(&layout, "qa_00000000000000000000000000000000").unwrap();
    }

    #[test]
    fn cleanup_run_removes_all_matching_raw_sessions_but_preserves_history() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_00000000000000000000000000000000",
            "scenario-1",
            "primary",
            "native-history",
            110,
        );
        let result = qa_finalize(
            &layout,
            "qa_00000000000000000000000000000000",
            vec![QaScenarioJudgement {
                scenario_id: "scenario-1".into(),
                status: QaScenarioStatus::Passed,
                evidence_ids: vec!["native-history".into()],
                summary: "history candidate".into(),
            }],
            vec![],
            120,
        )
        .unwrap();

        let mut second_identity = identity(&layout, AppRuntimeProfile::ReactDom);
        second_identity.qa_handle = "qa_11111111111111111111111111111111".into();
        second_identity.runtime_generation = 2;
        qa_begin(
            &layout,
            second_identity,
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            200,
        )
        .unwrap();
        record_native(
            &layout,
            "qa_11111111111111111111111111111111",
            "scenario-1",
            "primary",
            "native-inflight",
            210,
        );

        assert_eq!(qa_cleanup_run(&layout, "run-1").unwrap(), 2);
        assert!(load_qa_session(&layout, "qa_00000000000000000000000000000000").is_err());
        assert!(load_qa_session(&layout, "qa_11111111111111111111111111111111").is_err());
        assert_eq!(
            load_qa_result(&layout, &result.receipt.result_id).unwrap(),
            result.result
        );
        assert_eq!(
            load_qa_receipt(&layout, &result.receipt.receipt_id).unwrap(),
            result.receipt
        );
        assert_eq!(
            qa_cleanup_run(&layout, "run-1").unwrap(),
            0,
            "cleanup is idempotent after the raw sessions are gone"
        );
    }

    #[test]
    fn cleanup_does_not_leak_per_handle_lock_directories() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        let missing_handle = "qa_99999999999999999999999999999999";
        let qa_directory = layout.root().join(qa_root(&layout));
        let missing_dir = layout.root().join(qa_root(&layout)).join(missing_handle);
        qa_cleanup_session(&layout, missing_handle).unwrap();
        assert!(!missing_dir.exists());
        assert!(!qa_directory.exists());

        let handle = "qa_00000000000000000000000000000000";
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(&layout, handle, "scenario-1", "primary", "native", 110);
        let session_dir = layout.root().join(qa_root(&layout)).join(handle);
        assert!(session_dir.is_dir());
        qa_cleanup_session(&layout, handle).unwrap();
        assert!(!session_dir.exists());
    }

    #[test]
    fn cleanup_run_preserves_sessions_from_other_workflow_runs() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();

        let other_handle = "qa_11111111111111111111111111111111";
        let mut other_identity = identity(&layout, AppRuntimeProfile::ReactDom);
        other_identity.workflow_run_id = "run-2".into();
        other_identity.qa_handle = other_handle.into();
        other_identity.runtime_generation = 2;
        qa_begin(
            &layout,
            other_identity,
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            200,
        )
        .unwrap();

        assert_eq!(qa_cleanup_run(&layout, "run-1").unwrap(), 1);
        assert!(matches!(
            load_qa_session(&layout, "qa_00000000000000000000000000000000"),
            Err(AppError::NotFound(_))
        ));
        assert_eq!(
            load_qa_session(&layout, other_handle)
                .unwrap()
                .identity
                .workflow_run_id,
            "run-2"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_refuses_symlinked_raw_artifacts_without_touching_the_target() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        install_manifest(&layout, AppRuntimeProfile::ReactDom, vec![]);
        let handle = "qa_00000000000000000000000000000000";
        qa_begin(
            &layout,
            identity(&layout, AppRuntimeProfile::ReactDom),
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            100,
        )
        .unwrap();
        record_native(&layout, handle, "scenario-1", "primary", "native", 110);

        let outside = root.path().join("outside.txt");
        std::fs::write(&outside, b"keep").unwrap();
        let link = layout
            .root()
            .join(qa_root(&layout))
            .join(handle)
            .join(ARTIFACTS_DIR)
            .join("unexpected.txt");
        symlink(&outside, &link).unwrap();

        assert!(matches!(
            qa_cleanup_run(&layout, "run-1"),
            Err(AppError::StorageCorrupt(_))
        ));
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        assert!(load_qa_session(&layout, handle).is_ok());
    }
}
