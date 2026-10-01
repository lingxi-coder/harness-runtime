//! Host-owned authoring contracts for generated Local Apps.
//!
//! The model may submit [`AppAuthoringSpec`], but it cannot choose the host
//! identity which surrounds that spec.  The host supplies the app id, runtime
//! profile and workflow binding when it creates an [`AppAuthoringContract`].
//! Candidates are written below `apps/<id>/authoring/`, outside the editable
//! workspace. This module persists immutable contract content, but never
//! chooses the active digest: the Host's successful real build provenance is
//! the sole selector.

use crate::error::AppError;
use crate::ids;
use crate::manifest::{AppLayout, AppRuntimeProfileBinding};
use crate::types::AppRuntimeProfile;
use rooted_fs::AtomicWriteOptions;
use rooted_fs::FsError;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Version of the host-private authoring documents.
pub const AUTHORING_SCHEMA_VERSION: u32 = 1;
/// Maximum serialized contract or candidate document.
pub const MAX_AUTHORING_BYTES: u64 = 512 * 1024;
/// Maximum length of one model-authored string.
pub const MAX_AUTHORING_STRING_BYTES: usize = 16 * 1024;
/// Maximum number of entries in one model-authored list.
pub const MAX_AUTHORING_LIST_ITEMS: usize = 128;

const AUTHORING_DIR: &str = "authoring";
const CONTRACTS_DIR: &str = "contracts";
const CANDIDATE_FILE: &str = "candidate.json";

fn non_empty(value: &str, field: &str) -> Result<(), AppError> {
    if value.trim().is_empty() {
        return Err(AppError::InvalidRequest(format!(
            "authoring {field} must not be empty"
        )));
    }
    if value.len() > MAX_AUTHORING_STRING_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "authoring {field} exceeds {MAX_AUTHORING_STRING_BYTES} bytes"
        )));
    }
    Ok(())
}

fn local_id(value: &str, field: &str) -> Result<(), AppError> {
    non_empty(value, field)?;
    if value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "authoring {field} must be a lowercase stable id"
        )))
    }
}

fn list_size<T>(values: &[T], field: &str) -> Result<(), AppError> {
    if values.len() > MAX_AUTHORING_LIST_ITEMS {
        return Err(AppError::InvalidRequest(format!(
            "authoring {field} has too many entries"
        )));
    }
    Ok(())
}

fn strings(values: &[String], field: &str, allow_empty: bool) -> Result<(), AppError> {
    list_size(values, field)?;
    for value in values {
        if allow_empty {
            if value.len() > MAX_AUTHORING_STRING_BYTES {
                return Err(AppError::InvalidRequest(format!(
                    "authoring {field} entry exceeds {MAX_AUTHORING_STRING_BYTES} bytes"
                )));
            }
        } else {
            non_empty(value, field)?;
        }
    }
    Ok(())
}

/// The product intent supplied by the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppProductSpec {
    pub goal: String,
    pub tasks: Vec<String>,
    pub external_integrations: Vec<String>,
}

impl AppProductSpec {
    fn validate(&self) -> Result<(), AppError> {
        non_empty(&self.goal, "product.goal")?;
        strings(&self.tasks, "product.tasks", false)?;
        if self.tasks.is_empty() {
            return Err(AppError::InvalidRequest(
                "product.tasks must not be empty".into(),
            ));
        }
        strings(
            &self.external_integrations,
            "product.external_integrations",
            false,
        )
    }
}

/// A confirmed native target.  Target identity is part of the authoring
/// contract, but runtime/profile identity is deliberately not model-authored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppTarget {
    pub id: String,
    pub os: String,
    pub form_factor: String,
}

impl AppTarget {
    fn validate(&self) -> Result<(), AppError> {
        local_id(&self.id, "target.id")?;
        non_empty(&self.os, "target.os")?;
        non_empty(&self.form_factor, "target.form_factor")
    }
}

/// UI theme intent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppThemeSpec {
    pub mode: String,
    pub accent: String,
}

impl AppThemeSpec {
    fn validate(&self) -> Result<(), AppError> {
        non_empty(&self.mode, "ui.theme.mode")?;
        non_empty(&self.accent, "ui.theme.accent")
    }
}

/// UI styling intent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppStyleSpec {
    pub direction: String,
    pub density: String,
}

impl AppStyleSpec {
    fn validate(&self) -> Result<(), AppError> {
        non_empty(&self.direction, "ui.style.direction")?;
        non_empty(&self.density, "ui.style.density")
    }
}

/// UI structure and references.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppUiSpec {
    pub structure: Vec<String>,
    pub theme: AppThemeSpec,
    pub style: AppStyleSpec,
    pub references: Vec<String>,
}

impl AppUiSpec {
    fn validate(&self) -> Result<(), AppError> {
        strings(&self.structure, "ui.structure", false)?;
        if self.structure.is_empty() {
            return Err(AppError::InvalidRequest(
                "ui.structure must not be empty".into(),
            ));
        }
        self.theme.validate()?;
        self.style.validate()?;
        strings(&self.references, "ui.references", false)
    }
}

/// Presentation and navigation intent for one target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppPresentationSpec {
    pub target_id: String,
    pub presentation: String,
    pub navigation: String,
}

impl AppPresentationSpec {
    fn validate(&self) -> Result<(), AppError> {
        non_empty(&self.target_id, "design.presentations.target_id")?;
        non_empty(&self.presentation, "design.presentations.presentation")?;
        non_empty(&self.navigation, "design.presentations.navigation")
    }
}

/// Canvas-only design intent.  The field is absent for routed DOM apps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppCanvasDesign {
    pub scene: String,
    pub phases: Vec<String>,
    pub controls: Vec<String>,
    pub hud: String,
}

impl AppCanvasDesign {
    fn validate(&self) -> Result<(), AppError> {
        non_empty(&self.scene, "design.canvas.scene")?;
        strings(&self.phases, "design.canvas.phases", false)?;
        if self.phases.is_empty() {
            return Err(AppError::InvalidRequest(
                "design.canvas.phases must not be empty".into(),
            ));
        }
        strings(&self.controls, "design.canvas.controls", false)?;
        if self.controls.is_empty() {
            return Err(AppError::InvalidRequest(
                "design.canvas.controls must not be empty".into(),
            ));
        }
        non_empty(&self.hud, "design.canvas.hud")
    }
}

/// Named UI states required by the generated app.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppDesignStates {
    pub loading: String,
    pub empty: String,
    pub error: String,
    pub success: String,
    pub permission: String,
}

impl AppDesignStates {
    fn validate(&self) -> Result<(), AppError> {
        for (field, value) in [
            ("loading", &self.loading),
            ("empty", &self.empty),
            ("error", &self.error),
            ("success", &self.success),
            ("permission", &self.permission),
        ] {
            non_empty(value, &format!("design.states.{field}"))?;
        }
        Ok(())
    }
}

/// Input behavior intent across supported native surfaces.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppDesignInputs {
    pub pointer_touch: Vec<String>,
    pub keyboard_mouse: Vec<String>,
    pub back: String,
    pub reduced_motion: String,
}

impl AppDesignInputs {
    fn validate(&self) -> Result<(), AppError> {
        strings(&self.pointer_touch, "design.inputs.pointer_touch", false)?;
        strings(&self.keyboard_mouse, "design.inputs.keyboard_mouse", false)?;
        non_empty(&self.back, "design.inputs.back")?;
        non_empty(&self.reduced_motion, "design.inputs.reduced_motion")
    }
}

/// Designer output that can be merged only into a previously confirmed spec.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppDesignSpec {
    pub presentations: Vec<AppPresentationSpec>,
    pub tokens: BTreeMap<String, String>,
    pub states: AppDesignStates,
    pub inputs: AppDesignInputs,
    #[serde(default)]
    pub canvas: Option<AppCanvasDesign>,
}

impl AppDesignSpec {
    fn validate(&self, target_ids: &BTreeSet<&str>) -> Result<(), AppError> {
        list_size(&self.presentations, "design.presentations")?;
        let mut presentation_targets = BTreeSet::new();
        for presentation in &self.presentations {
            presentation.validate()?;
            if !presentation_targets.insert(presentation.target_id.as_str()) {
                return Err(AppError::InvalidRequest(format!(
                    "design has duplicate presentation for target {:?}",
                    presentation.target_id
                )));
            }
            if !target_ids.contains(presentation.target_id.as_str()) {
                return Err(AppError::InvalidRequest(format!(
                    "design presentation references unknown target {:?}",
                    presentation.target_id
                )));
            }
        }
        if self.presentations.len() != target_ids.len()
            || presentation_targets != target_ids.clone()
        {
            return Err(AppError::InvalidRequest(
                "design.presentations must contain exactly one presentation per target".into(),
            ));
        }
        if self.tokens.len() > MAX_AUTHORING_LIST_ITEMS {
            return Err(AppError::InvalidRequest(
                "authoring design.tokens has too many entries".into(),
            ));
        }
        for (key, value) in &self.tokens {
            non_empty(key, "design.tokens.key")?;
            non_empty(value, "design.tokens.value")?;
        }
        self.states.validate()?;
        self.inputs.validate()?;
        if let Some(canvas) = &self.canvas {
            canvas.validate()?;
        }
        Ok(())
    }
}

/// Evidence requested by an acceptance check.  Evidence strings are kept
/// closed so a model cannot smuggle an unreviewed evidence channel into QA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceEvidence {
    Inspect,
    UiAction,
    Capture,
}

/// One host-verifiable acceptance requirement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AcceptanceCheck {
    pub id: String,
    pub target_ids: Vec<String>,
    pub required: bool,
    pub preconditions: Vec<String>,
    pub steps: Vec<String>,
    pub expected: String,
    pub evidence: Vec<AcceptanceEvidence>,
    /// Whether passing this scenario requires visibly distinct, time-separated
    /// canvas frames. Static and reduced-motion scenarios leave this false.
    #[serde(default)]
    pub motion_required: bool,
}

impl AcceptanceCheck {
    fn validate(&self, target_ids: &BTreeSet<&str>) -> Result<(), AppError> {
        local_id(&self.id, "acceptance_checks.id")?;
        strings(&self.target_ids, "acceptance_checks.target_ids", false)?;
        if self.target_ids.is_empty() {
            return Err(AppError::InvalidRequest(
                "acceptance check target_ids must not be empty".into(),
            ));
        }
        strings(
            &self.preconditions,
            "acceptance_checks.preconditions",
            false,
        )?;
        strings(&self.steps, "acceptance_checks.steps", false)?;
        if self.steps.is_empty() {
            return Err(AppError::InvalidRequest(
                "acceptance check steps must not be empty".into(),
            ));
        }
        non_empty(&self.expected, "acceptance_checks.expected")?;
        if self.evidence.is_empty() {
            return Err(AppError::InvalidRequest(format!(
                "acceptance check {:?} must require evidence",
                self.id
            )));
        }
        let mut evidence = BTreeSet::new();
        for kind in &self.evidence {
            if !evidence.insert(*kind as u8) {
                return Err(AppError::InvalidRequest(format!(
                    "acceptance check {:?} repeats an evidence kind",
                    self.id
                )));
            }
        }
        if self.motion_required && !self.evidence.contains(&AcceptanceEvidence::Capture) {
            return Err(AppError::InvalidRequest(format!(
                "acceptance check {:?} requires capture evidence for motion",
                self.id
            )));
        }
        let mut check_targets = BTreeSet::new();
        for target in &self.target_ids {
            local_id(target, "acceptance_checks.target_ids")?;
            if !check_targets.insert(target.as_str()) {
                return Err(AppError::InvalidRequest(format!(
                    "acceptance check {:?} repeats target {:?}",
                    self.id, target
                )));
            }
            if !target_ids.contains(target.as_str()) {
                return Err(AppError::InvalidRequest(format!(
                    "acceptance check {:?} references unknown target {:?}",
                    self.id, target
                )));
            }
        }
        Ok(())
    }
}

/// Closed, model-authored local-app intent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppAuthoringSpec {
    pub product: AppProductSpec,
    pub targets: Vec<AppTarget>,
    pub ui: AppUiSpec,
    pub design: AppDesignSpec,
    pub acceptance_checks: Vec<AcceptanceCheck>,
}

impl AppAuthoringSpec {
    /// Validate shape, bounds, references and the closed authoring contract.
    pub fn validate(&self) -> Result<(), AppError> {
        self.product.validate()?;
        list_size(&self.targets, "targets")?;
        if self.targets.is_empty() {
            return Err(AppError::InvalidRequest(
                "authoring targets must not be empty".into(),
            ));
        }
        let mut target_ids = BTreeSet::new();
        for target in &self.targets {
            target.validate()?;
            if !target_ids.insert(target.id.as_str()) {
                return Err(AppError::InvalidRequest(format!(
                    "duplicate authoring target id {:?}",
                    target.id
                )));
            }
        }
        self.ui.validate()?;
        self.design.validate(&target_ids)?;
        list_size(&self.acceptance_checks, "acceptance_checks")?;
        if self.acceptance_checks.is_empty() {
            return Err(AppError::InvalidRequest(
                "authoring acceptance_checks must not be empty".into(),
            ));
        }
        let mut check_ids = BTreeSet::new();
        let mut covered_targets = BTreeSet::new();
        let mut required_check_count = 0usize;
        for check in &self.acceptance_checks {
            check.validate(&target_ids)?;
            if check.required {
                required_check_count += 1;
                covered_targets.extend(check.target_ids.iter().map(String::as_str));
            }
            if !check_ids.insert(check.id.as_str()) {
                return Err(AppError::InvalidRequest(format!(
                    "duplicate acceptance check id {:?}",
                    check.id
                )));
            }
        }
        if required_check_count == 0 {
            return Err(AppError::InvalidRequest(
                "authoring requires at least one required acceptance check".into(),
            ));
        }
        if target_ids
            .iter()
            .any(|target| !covered_targets.contains(target))
        {
            return Err(AppError::InvalidRequest(
                "every target must be covered by a required acceptance check".into(),
            ));
        }
        Ok(())
    }
}

/// Host-owned wrapper around a model-authored spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppAuthoringContract {
    pub version: u32,
    pub revision: u64,
    pub app_id: String,
    pub runtime_profile: AppRuntimeProfileBinding,
    pub spec: AppAuthoringSpec,
}

impl AppAuthoringContract {
    /// Validate host and model portions of the contract.
    pub fn validate(&self) -> Result<(), AppError> {
        if self.version != AUTHORING_SCHEMA_VERSION {
            return Err(AppError::InvalidRequest(format!(
                "authoring contract version {} is unsupported",
                self.version
            )));
        }
        if self.revision == 0 {
            return Err(AppError::InvalidRequest(
                "authoring contract revision must be non-zero".into(),
            ));
        }
        ids::validate_app_id(&self.app_id)?;
        self.runtime_profile.validate()?;
        self.spec.validate()?;
        let canvas_profile = self.runtime_profile.family != AppRuntimeProfile::ReactDom;
        if canvas_profile != self.spec.design.canvas.is_some() {
            return Err(AppError::InvalidRequest(
                "authoring design.canvas presence must match the Host runtime profile".into(),
            ));
        }
        if !canvas_profile
            && self
                .spec
                .acceptance_checks
                .iter()
                .any(|check| check.motion_required)
        {
            return Err(AppError::InvalidRequest(
                "motion_required acceptance checks are reserved for canvas profiles".into(),
            ));
        }
        Ok(())
    }

    /// Stable digest of this complete host-bound contract.
    pub fn sha256(&self) -> Result<String, AppError> {
        self.validate()?;
        canonical_sha256(self, "authoring contract")
    }
}

/// Durable candidate returned by staging.  It is intentionally separate from
/// [`AppAuthoringContract`], which is the effective last-successful contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppAuthoringCandidate {
    pub schema_version: u32,
    pub app_id: String,
    pub workflow_run_id: String,
    pub handle: String,
    pub base_contract_sha256: Option<String>,
    pub contract: AppAuthoringContract,
    pub contract_sha256: String,
}

/// Public stage result.  The handle is opaque and Host-issued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AppAuthoringStage {
    pub handle: String,
    pub app_id: String,
    pub workflow_run_id: String,
    pub revision: u64,
    pub contract_sha256: String,
    pub contract: AppAuthoringContract,
}

fn authoring_dir(layout: &AppLayout) -> PathBuf {
    layout.app_dir_rel().join(AUTHORING_DIR)
}

fn contract_path(layout: &AppLayout, contract_sha256: &str) -> Result<PathBuf, AppError> {
    verify_sha256(contract_sha256, contract_sha256, "authoring contract")?;
    Ok(authoring_dir(layout)
        .join(CONTRACTS_DIR)
        .join(format!("{contract_sha256}.json")))
}

fn candidate_path(layout: &AppLayout) -> PathBuf {
    authoring_dir(layout).join(CANDIDATE_FILE)
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let ordered = object
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect::<BTreeMap<_, _>>();
            let mut rebuilt = Map::with_capacity(ordered.len());
            for (key, value) in ordered {
                rebuilt.insert(key, value);
            }
            Value::Object(rebuilt)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

/// Serialize a value to canonical JSON bytes with recursively sorted object
/// keys. Array ordering remains meaningful.
pub fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, AppError> {
    let value = serde_json::to_value(value)
        .map_err(|error| AppError::Io(format!("serialize canonical authoring JSON: {error}")))?;
    serde_json::to_vec(&canonicalize(value))
        .map_err(|error| AppError::Io(format!("encode canonical authoring JSON: {error}")))
}

/// Stable SHA-256 helper for authoring and Host-bound receipts.
pub fn canonical_sha256<T: Serialize>(value: &T, context: &str) -> Result<String, AppError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(canonical_json_bytes(value).map_err(|error| match error {
            AppError::Io(message) => AppError::Io(format!("{context}: {message}")),
            other => other,
        })?)
    ))
}

/// Stable digest of model-authored intent alone.
pub fn authoring_spec_sha256(spec: &AppAuthoringSpec) -> Result<String, AppError> {
    spec.validate()?;
    canonical_sha256(spec, "authoring spec")
}

/// Stable digest of a Host-bound contract.
pub fn authoring_contract_sha256(contract: &AppAuthoringContract) -> Result<String, AppError> {
    contract.sha256()
}

fn verify_sha256(value: &str, expected: &str, what: &str) -> Result<(), AppError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(AppError::StorageCorrupt(format!(
            "{what} digest is not lowercase sha256"
        )));
    }
    if value != expected {
        return Err(AppError::StorageCorrupt(format!(
            "{what} digest does not match its content"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct AuthoringDocument<T> {
    schema_version: u32,
    app_id: String,
    payload: T,
    payload_sha256: String,
}

fn encode_document<T: Serialize>(app_id: &str, payload: &T) -> Result<Vec<u8>, AppError> {
    let document = AuthoringDocument {
        schema_version: AUTHORING_SCHEMA_VERSION,
        app_id: app_id.to_owned(),
        payload,
        payload_sha256: canonical_sha256(payload, "authoring document")?,
    };
    let mut body = serde_json::to_vec_pretty(&document)
        .map_err(|error| AppError::Io(format!("serialize authoring document: {error}")))?;
    body.push(b'\n');
    if body.len() as u64 > MAX_AUTHORING_BYTES {
        return Err(AppError::InvalidRequest(
            "authoring document exceeds its size limit".into(),
        ));
    }
    Ok(body)
}

fn read_document<T: DeserializeOwned + Serialize>(
    layout: &AppLayout,
    path: &std::path::Path,
    what: &str,
) -> Result<AuthoringDocument<T>, AppError> {
    let body = rooted_fs::read_to_string_limited(layout.root(), path, MAX_AUTHORING_BYTES)
        .map_err(|error| match error {
            FsError::NotFound(_) => AppError::NotFound(format!("{what} not found")),
            other => AppError::from_fs(&format!("read {what}"), &other),
        })?;
    let document: AuthoringDocument<T> = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("{what}: {error}")))?;
    if document.schema_version != AUTHORING_SCHEMA_VERSION || document.app_id != layout.app_id() {
        return Err(AppError::StorageCorrupt(format!(
            "{what} schema or app binding is invalid"
        )));
    }
    let actual = canonical_sha256(&document.payload, what)?;
    verify_sha256(&document.payload_sha256, &actual, what)?;
    Ok(document)
}

fn persist_document<T: Serialize>(
    layout: &AppLayout,
    path: &std::path::Path,
    payload: &T,
    what: &str,
    overwrite: bool,
) -> Result<(), AppError> {
    let body = encode_document(layout.app_id(), payload)?;
    layout.initialize()?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::InvalidRequest("authoring document path has no parent".into()))?;
    rooted_fs::ensure_private_directory(layout.root(), parent, 0o700)
        .map_err(|error| AppError::from_fs("create authoring directory", &error))?;
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
            let existing = rooted_fs::read_tail_bytes(layout.root(), path, MAX_AUTHORING_BYTES)
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

fn validate_workflow_run_id(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 256
        || value.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        })
    {
        return Err(AppError::InvalidRequest(
            "workflow_run_id contains unsupported characters".into(),
        ));
    }
    Ok(())
}

/// Stage a host-bound candidate without changing the effective contract.
pub fn stage_authoring(
    layout: &AppLayout,
    workflow_run_id: &str,
    contract: AppAuthoringContract,
    base_contract_sha256: Option<String>,
) -> Result<AppAuthoringStage, AppError> {
    validate_workflow_run_id(workflow_run_id)?;
    if contract.app_id != layout.app_id() {
        return Err(AppError::InvalidRequest(
            "authoring contract app id does not match layout".into(),
        ));
    }
    contract.validate()?;
    let contract_sha256 = contract.sha256()?;
    if let Some(base) = &base_contract_sha256 {
        verify_sha256(base, base, "base authoring contract")?;
        let current = load_authoring_contract(layout, base)?;
        let current_digest = current.sha256()?;
        if &current_digest != base {
            return Err(AppError::RevisionConflict {
                expected: contract.revision.saturating_sub(1),
                actual: current.revision,
            });
        }
        let expected_revision = current.revision.checked_add(1).ok_or_else(|| {
            AppError::InvalidRequest("authoring contract revision is exhausted".into())
        })?;
        if contract.revision != expected_revision {
            return Err(AppError::RevisionConflict {
                expected: expected_revision,
                actual: contract.revision,
            });
        }
    } else if contract.revision != 1 {
        return Err(AppError::RevisionConflict {
            expected: 1,
            actual: contract.revision,
        });
    }
    let handle = ids::generate_authoring_handle();
    let candidate = AppAuthoringCandidate {
        schema_version: AUTHORING_SCHEMA_VERSION,
        app_id: layout.app_id().to_owned(),
        workflow_run_id: workflow_run_id.to_owned(),
        handle: handle.clone(),
        base_contract_sha256,
        contract: contract.clone(),
        contract_sha256: contract_sha256.clone(),
    };
    persist_document(
        layout,
        &candidate_path(layout),
        &candidate,
        "authoring candidate",
        true,
    )?;
    Ok(AppAuthoringStage {
        handle,
        app_id: layout.app_id().to_owned(),
        workflow_run_id: workflow_run_id.to_owned(),
        revision: contract.revision,
        contract_sha256,
        contract,
    })
}

/// Persist immutable contract content without selecting it as active.
///
/// The Host may call this before compiling so the staged build can name the
/// digest. Only a successfully promoted real build receipt may later select
/// the file.
pub fn save_authoring_contract(
    layout: &AppLayout,
    contract: &AppAuthoringContract,
) -> Result<String, AppError> {
    if contract.app_id != layout.app_id() {
        return Err(AppError::InvalidRequest(
            "authoring contract app id does not match layout".into(),
        ));
    }
    let digest = contract.sha256()?;
    let path = contract_path(layout, &digest)?;
    persist_document(layout, &path, contract, "authoring contract", false)?;
    Ok(digest)
}

/// Load the effective last-successful authoring contract.
pub fn load_authoring_contract(
    layout: &AppLayout,
    contract_sha256: &str,
) -> Result<AppAuthoringContract, AppError> {
    let path = contract_path(layout, contract_sha256)?;
    let document: AuthoringDocument<AppAuthoringContract> =
        read_document(layout, &path, "authoring contract")?;
    document.payload.validate()?;
    let actual = document.payload.sha256()?;
    if actual != contract_sha256 {
        return Err(AppError::StorageCorrupt(
            "authoring contract filename digest does not match its content".into(),
        ));
    }
    Ok(document.payload)
}

/// Load and validate the private candidate for one app.
pub fn load_authoring_candidate(layout: &AppLayout) -> Result<AppAuthoringCandidate, AppError> {
    let document: AuthoringDocument<AppAuthoringCandidate> =
        read_document(layout, &candidate_path(layout), "authoring candidate")?;
    let candidate = document.payload;
    if candidate.schema_version != AUTHORING_SCHEMA_VERSION || candidate.app_id != layout.app_id() {
        return Err(AppError::StorageCorrupt(
            "authoring candidate schema or app binding is invalid".into(),
        ));
    }
    validate_workflow_run_id(&candidate.workflow_run_id)?;
    ids::validate_authoring_handle(&candidate.handle).map_err(|error| {
        AppError::StorageCorrupt(format!("invalid authoring candidate handle: {error}"))
    })?;
    candidate.contract.validate()?;
    if candidate.contract.app_id != candidate.app_id {
        return Err(AppError::StorageCorrupt(
            "authoring candidate contract app binding is invalid".into(),
        ));
    }
    let digest = candidate.contract.sha256()?;
    verify_sha256(
        &candidate.contract_sha256,
        &digest,
        "authoring candidate contract",
    )?;
    if let Some(base) = &candidate.base_contract_sha256 {
        verify_sha256(base, base, "base authoring contract")?;
    }
    Ok(candidate)
}

/// Persist a staged candidate and remove its mutable handle document.
///
/// This does not select the contract as active; only Host build provenance
/// can do that.
pub fn commit_staged_authoring(
    layout: &AppLayout,
    handle: &str,
    workflow_run_id: &str,
) -> Result<AppAuthoringContract, AppError> {
    ids::validate_authoring_handle(handle)?;
    validate_workflow_run_id(workflow_run_id)?;
    let candidate = load_authoring_candidate(layout)?;
    if candidate.handle != handle || candidate.workflow_run_id != workflow_run_id {
        return Err(AppError::InvalidRequest(
            "authoring candidate handle or workflow binding does not match".into(),
        ));
    }
    if let Some(base) = &candidate.base_contract_sha256 {
        let current = load_authoring_contract(layout, base).map_err(|error| match error {
            AppError::NotFound(_) => AppError::RevisionConflict {
                expected: candidate.contract.revision.saturating_sub(1),
                actual: 0,
            },
            other => other,
        })?;
        if current.revision >= candidate.contract.revision {
            return Err(AppError::RevisionConflict {
                expected: candidate.contract.revision.saturating_sub(1),
                actual: current.revision,
            });
        }
    }
    save_authoring_contract(layout, &candidate.contract)?;
    delete_authoring_candidate(layout)?;
    Ok(candidate.contract)
}

/// Delete a private candidate; missing candidates are harmless.
pub fn delete_authoring_candidate(layout: &AppLayout) -> Result<(), AppError> {
    match rooted_fs::remove_file(layout.root(), &candidate_path(layout)) {
        Ok(()) | Err(FsError::NotFound(_)) => Ok(()),
        Err(error) => Err(AppError::from_fs("delete authoring candidate", &error)),
    }
}

/// Consume a candidate only when the complete identity captured by a build
/// still names the candidate on disk.
///
/// The caller must hold the app/build lock while invoking this helper.  A
/// staged candidate is a single mutable slot, so comparing every identity
/// component under that lock prevents an older successful build from deleting
/// a newer candidate that happens to contain the same contract bytes.
pub fn consume_authoring_candidate_if_matches(
    layout: &AppLayout,
    handle: &str,
    workflow_run_id: &str,
    contract_sha256: &str,
    base_contract_sha256: Option<&str>,
) -> Result<bool, AppError> {
    ids::validate_authoring_handle(handle)?;
    validate_workflow_run_id(workflow_run_id)?;
    verify_sha256(
        contract_sha256,
        contract_sha256,
        "authoring candidate contract",
    )?;
    if let Some(base) = base_contract_sha256 {
        verify_sha256(base, base, "base authoring contract")?;
    }
    let candidate = match load_authoring_candidate(layout) {
        Ok(candidate) => candidate,
        Err(AppError::NotFound(_)) => return Ok(false),
        Err(error) => return Err(error),
    };
    if candidate.handle != handle
        || candidate.workflow_run_id != workflow_run_id
        || candidate.contract_sha256 != contract_sha256
        || candidate.base_contract_sha256.as_deref() != base_contract_sha256
    {
        return Ok(false);
    }
    delete_authoring_candidate(layout)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn valid_spec() -> AppAuthoringSpec {
        AppAuthoringSpec {
            product: AppProductSpec {
                goal: "track chores".into(),
                tasks: vec!["add a chore".into()],
                external_integrations: vec![],
            },
            targets: vec![AppTarget {
                id: "primary".into(),
                os: "ios".into(),
                form_factor: "iphone".into(),
            }],
            ui: AppUiSpec {
                structure: vec!["list".into()],
                theme: AppThemeSpec {
                    mode: "system".into(),
                    accent: "blue".into(),
                },
                style: AppStyleSpec {
                    direction: "calm".into(),
                    density: "comfortable".into(),
                },
                references: vec![],
            },
            design: AppDesignSpec {
                presentations: vec![AppPresentationSpec {
                    target_id: "primary".into(),
                    presentation: "single list".into(),
                    navigation: "push detail".into(),
                }],
                tokens: BTreeMap::from([(String::from("accent"), String::from("blue"))]),
                states: AppDesignStates {
                    loading: "spinner".into(),
                    empty: "no chores".into(),
                    error: "retry".into(),
                    success: "saved".into(),
                    permission: "explain".into(),
                },
                inputs: AppDesignInputs {
                    pointer_touch: vec!["tap".into()],
                    keyboard_mouse: vec!["enter".into()],
                    back: "navigate back".into(),
                    reduced_motion: "remove transitions".into(),
                },
                canvas: None,
            },
            acceptance_checks: vec![AcceptanceCheck {
                id: "primary-action".into(),
                target_ids: vec!["primary".into()],
                required: true,
                preconditions: vec!["list loaded".into()],
                steps: vec!["tap add".into()],
                expected: "chore appears".into(),
                evidence: vec![AcceptanceEvidence::Inspect, AcceptanceEvidence::UiAction],
                motion_required: false,
            }],
        }
    }

    fn contract(app_id: &str) -> AppAuthoringContract {
        AppAuthoringContract {
            version: AUTHORING_SCHEMA_VERSION,
            revision: 1,
            app_id: app_id.into(),
            runtime_profile: AppRuntimeProfileBinding {
                family: AppRuntimeProfile::ReactDom,
                revision: 1,
                contract_sha256: "c".repeat(64),
            },
            spec: valid_spec(),
        }
    }

    fn layout(root: &TempDir) -> AppLayout {
        AppLayout::new(root.path(), "abc12345").unwrap()
    }

    #[test]
    fn canonical_digest_ignores_object_key_order() {
        let left = serde_json::json!({"b": 1, "a": {"d": 2, "c": 3}});
        let right = serde_json::json!({"a": {"c": 3, "d": 2}, "b": 1});
        assert_eq!(
            canonical_sha256(&left, "left").unwrap(),
            canonical_sha256(&right, "right").unwrap()
        );
    }

    #[test]
    fn authoring_design_serializes_canvas_as_explicit_null_for_dom() {
        let json = serde_json::to_value(valid_spec()).unwrap();
        assert_eq!(json["design"]["canvas"], Value::Null);
    }

    #[test]
    fn shared_authoring_fixtures_accept_null_and_omitted_canvas() {
        for fixture in [
            include_str!("../tests/fixtures/authoring-spec.valid-null-canvas.json"),
            include_str!("../tests/fixtures/authoring-spec.valid-omitted-canvas.json"),
        ] {
            let spec: AppAuthoringSpec = serde_json::from_str(fixture).unwrap();
            spec.validate().unwrap();
            assert_eq!(
                serde_json::to_value(spec).unwrap()["design"]["canvas"],
                Value::Null,
                "the durable DTO has one canonical serialized spelling"
            );
        }
    }

    #[test]
    fn required_authoring_lists_fail_at_their_own_boundary() {
        let mut cases: Vec<(&str, AppAuthoringSpec)> = Vec::new();
        let mut tasks = valid_spec();
        tasks.product.tasks.clear();
        cases.push(("product.tasks", tasks));
        let mut targets = valid_spec();
        targets.targets.clear();
        cases.push(("targets", targets));
        let mut structure = valid_spec();
        structure.ui.structure.clear();
        cases.push(("ui.structure", structure));
        let mut presentations = valid_spec();
        presentations.design.presentations.clear();
        cases.push(("design.presentations", presentations));
        let mut checks = valid_spec();
        checks.acceptance_checks.clear();
        cases.push(("acceptance_checks", checks));
        let mut check_targets = valid_spec();
        check_targets.acceptance_checks[0].target_ids.clear();
        cases.push(("target_ids", check_targets));
        let mut steps = valid_spec();
        steps.acceptance_checks[0].steps.clear();
        cases.push(("steps", steps));
        for (expected, spec) in cases {
            let error = spec.validate().unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "expected {expected:?} failure, got {error:?}"
            );
        }
    }

    #[test]
    fn canvas_lists_and_stable_acceptance_ids_are_closed() {
        let mut canvas = valid_spec();
        canvas.design.canvas = Some(AppCanvasDesign {
            scene: "board".into(),
            phases: Vec::new(),
            controls: vec!["tap".into()],
            hud: "score".into(),
        });
        assert!(canvas
            .validate()
            .unwrap_err()
            .to_string()
            .contains("design.canvas.phases"));

        let mut duplicate_target = valid_spec();
        duplicate_target.acceptance_checks[0]
            .target_ids
            .push("primary".into());
        assert!(duplicate_target
            .validate()
            .unwrap_err()
            .to_string()
            .contains("repeats target"));

        let mut unstable_id = valid_spec();
        unstable_id.acceptance_checks[0].id = "Primary action".into();
        assert!(unstable_id
            .validate()
            .unwrap_err()
            .to_string()
            .contains("lowercase stable id"));
    }

    #[test]
    fn host_runtime_profile_strictly_binds_canvas_design_presence() {
        let mut canvas_contract = contract("abc12345");
        canvas_contract.runtime_profile.family = AppRuntimeProfile::Canvas2d;
        assert!(canvas_contract
            .validate()
            .unwrap_err()
            .to_string()
            .contains("canvas presence"));
        canvas_contract.spec.design.canvas = Some(AppCanvasDesign {
            scene: "board".into(),
            phases: vec!["ready".into()],
            controls: vec!["tap".into()],
            hud: "score".into(),
        });
        canvas_contract.validate().unwrap();

        canvas_contract.runtime_profile.family = AppRuntimeProfile::ReactDom;
        assert!(canvas_contract
            .validate()
            .unwrap_err()
            .to_string()
            .contains("canvas presence"));
    }

    #[test]
    fn authoring_utf8_bounds_count_bytes_without_splitting_unicode() {
        let mut at_limit = valid_spec();
        at_limit.product.goal = "界".repeat(MAX_AUTHORING_STRING_BYTES / 3);
        assert!(at_limit.product.goal.len() <= MAX_AUTHORING_STRING_BYTES);
        at_limit.validate().unwrap();

        let mut over_limit = at_limit;
        over_limit.product.goal.push('界');
        let error = over_limit.validate().unwrap_err().to_string();
        assert!(error.contains("product.goal"));
        assert!(error.contains("bytes"));
    }

    #[test]
    fn authoring_schema_tracks_the_dto_canvas_and_required_list_shape() {
        let schema: Value = serde_json::from_str(include_str!(
            "../../plugins/lingxi-local-app/schemas/authoring-spec.schema.json"
        ))
        .unwrap();
        let design_required = schema["properties"]["design"]["required"]
            .as_array()
            .unwrap();
        assert!(!design_required.iter().any(|field| field == "canvas"));
        assert_eq!(
            schema["properties"]["product"]["properties"]["tasks"]["minItems"],
            1
        );
        assert_eq!(
            schema["properties"]["ui"]["properties"]["structure"]["minItems"],
            1
        );
        assert_eq!(
            schema["properties"]["acceptance_checks"]["items"]["properties"]["target_ids"]
                ["minItems"],
            1
        );
    }

    #[test]
    fn presentations_cover_each_target_once_even_when_screens_differ() {
        let mut spec = valid_spec();
        spec.targets.push(AppTarget {
            id: "desktop".into(),
            os: "macos".into(),
            form_factor: "desktop".into(),
        });
        assert!(spec
            .validate()
            .unwrap_err()
            .to_string()
            .contains("exactly one presentation per target"));
        let mut second_screen = spec.design.presentations[0].clone();
        second_screen.presentation = "another screen".into();
        spec.design.presentations.push(second_screen);
        assert!(spec
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate presentation"));
        spec.design.presentations[1].target_id = "desktop".into();
        spec.acceptance_checks[0].target_ids.push("desktop".into());
        spec.validate().unwrap();
    }

    #[test]
    fn unknown_fields_and_unknown_targets_are_rejected() {
        let json =
            serde_json::json!({"goal":"x","tasks":[],"external_integrations":[],"extra":true});
        assert!(serde_json::from_value::<AppProductSpec>(json).is_err());
        let mut unknown_evidence = serde_json::to_value(valid_spec()).unwrap();
        unknown_evidence["acceptance_checks"][0]["evidence"][0] = Value::from("video");
        assert!(serde_json::from_value::<AppAuthoringSpec>(unknown_evidence).is_err());
        let mut spec = valid_spec();
        spec.design.presentations[0].target_id = "missing".into();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn staging_does_not_replace_effective_contract_until_commit() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        let first = contract(layout.app_id());
        save_authoring_contract(&layout, &first).unwrap();
        let mut second = first.clone();
        second.revision = 2;
        second.spec.product.goal = "a different goal".into();
        let stage = stage_authoring(
            &layout,
            "run-1",
            second.clone(),
            Some(first.sha256().unwrap()),
        )
        .unwrap();
        assert!(ids::is_valid_authoring_handle(&stage.handle));
        assert!(stage.handle.starts_with("contract_"));
        assert_eq!(stage.handle.len(), "contract_".len() + 32);
        assert_eq!(
            load_authoring_contract(&layout, &first.sha256().unwrap()).unwrap(),
            first
        );
        assert_eq!(
            load_authoring_candidate(&layout).unwrap().handle,
            stage.handle
        );
        commit_staged_authoring(&layout, &stage.handle, "run-1").unwrap();
        assert_eq!(
            load_authoring_contract(&layout, &second.sha256().unwrap()).unwrap(),
            second
        );
    }

    #[test]
    fn tampered_candidate_and_cross_run_commit_fail_closed() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        let stage = stage_authoring(&layout, "run-1", contract(layout.app_id()), None).unwrap();
        let path = root.path().join("apps/abc12345/authoring/candidate.json");
        let mut value: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["payload"]["contract"]["revision"] = Value::from(99);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            load_authoring_candidate(&layout),
            Err(AppError::StorageCorrupt(_))
        ));
        assert!(commit_staged_authoring(&layout, &stage.handle, "run-other").is_err());
    }

    #[test]
    fn candidate_consumption_is_identity_bound_across_replacement() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        let first = stage_authoring(&layout, "run-1", contract(layout.app_id()), None).unwrap();
        let replacement =
            stage_authoring(&layout, "run-2", contract(layout.app_id()), None).unwrap();
        assert_ne!(first.handle, replacement.handle);
        assert_eq!(first.contract_sha256, replacement.contract_sha256);

        assert!(!consume_authoring_candidate_if_matches(
            &layout,
            &first.handle,
            &first.workflow_run_id,
            &first.contract_sha256,
            None,
        )
        .unwrap());
        assert_eq!(
            load_authoring_candidate(&layout).unwrap().handle,
            replacement.handle
        );
        assert!(consume_authoring_candidate_if_matches(
            &layout,
            &replacement.handle,
            &replacement.workflow_run_id,
            &replacement.contract_sha256,
            None,
        )
        .unwrap());
        assert!(matches!(
            load_authoring_candidate(&layout),
            Err(AppError::NotFound(_))
        ));
    }

    #[test]
    fn content_addressed_contract_is_idempotent_but_never_overwritten() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        let contract = contract(layout.app_id());
        let digest = save_authoring_contract(&layout, &contract).unwrap();
        assert_eq!(save_authoring_contract(&layout, &contract).unwrap(), digest);
        let path = root
            .path()
            .join(format!("apps/abc12345/authoring/contracts/{digest}.json"));
        std::fs::write(&path, b"tampered\n").unwrap();
        let error = save_authoring_contract(&layout, &contract).unwrap_err();
        assert!(matches!(error, AppError::StorageCorrupt(_)));
        assert_eq!(std::fs::read(&path).unwrap(), b"tampered\n");
    }

    #[test]
    fn staged_revisions_must_advance_exactly_once() {
        let root = TempDir::new().unwrap();
        let layout = layout(&root);
        let first = contract(layout.app_id());
        let digest = save_authoring_contract(&layout, &first).unwrap();
        let mut skipped = first.clone();
        skipped.revision = 3;
        assert!(matches!(
            stage_authoring(&layout, "run-1", skipped, Some(digest)),
            Err(AppError::RevisionConflict { .. })
        ));
        let mut non_initial = first;
        non_initial.revision = 2;
        assert!(matches!(
            stage_authoring(&layout, "run-1", non_initial, None),
            Err(AppError::RevisionConflict { .. })
        ));
    }
}
