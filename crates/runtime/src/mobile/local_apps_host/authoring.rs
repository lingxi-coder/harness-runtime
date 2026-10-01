//! Host bindings for the core authoring and QA persistence contracts.

use super::*;
use base64::Engine;
use rooted_fs::AtomicWriteOptions;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use url::Url;

const QA_ACTIVE_RECEIPT_FILE: &str = "active-receipt.json";
const QA_PUBLISHED_DIR: &str = "published";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QaPersistedDocument<T> {
    schema_version: u32,
    app_id: String,
    payload: T,
    payload_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QaActiveReceiptPointer {
    receipt_id: String,
}

fn qa_active_receipt_path(layout: &local_apps::AppLayout) -> std::path::PathBuf {
    layout.app_dir_rel().join("qa").join(QA_ACTIVE_RECEIPT_FILE)
}

fn qa_publication_path(layout: &local_apps::AppLayout, result_id: &str) -> std::path::PathBuf {
    layout
        .app_dir_rel()
        .join("qa")
        .join(QA_PUBLISHED_DIR)
        .join(format!("{result_id}.json"))
}

fn persist_active_qa_receipt(
    layout: &local_apps::AppLayout,
    receipt_id: &str,
) -> Result<(), String> {
    let path = qa_active_receipt_path(layout);
    let parent = path
        .parent()
        .ok_or_else(|| "qa publication pointer has no parent".to_string())?;
    layout.initialize().map_err(|error| error.to_string())?;
    rooted_fs::ensure_private_directory(layout.root(), parent, 0o700)
        .map_err(|error| format!("create QA publication pointer directory: {error}"))?;
    let body = serde_json::to_vec(&json!({"receipt_id": receipt_id}))
        .map_err(|error| format!("serialize QA publication pointer: {error}"))?;
    rooted_fs::atomic_write(
        layout.root(),
        &path,
        &body,
        AtomicWriteOptions {
            create_parents: false,
            file_mode: 0o600,
            ..AtomicWriteOptions::default()
        },
    )
    .map_err(|error| format!("write QA publication pointer: {error}"))
}

fn load_published_marker(
    layout: &local_apps::AppLayout,
    result_id: &str,
) -> Result<local_apps::QaPublicationMarker, String> {
    let path = qa_publication_path(layout, result_id);
    let body = rooted_fs::read_to_string_limited(layout.root(), &path, 1024 * 1024)
        .map_err(|error| format!("read QA publication marker: {error}"))?;
    let document: QaPersistedDocument<local_apps::QaPublicationMarker> =
        serde_json::from_str(&body).map_err(|error| format!("QA publication marker: {error}"))?;
    if document.schema_version != local_apps::QA_SCHEMA_VERSION
        || document.app_id != layout.app_id()
    {
        return Err("QA publication marker schema or app binding is invalid".into());
    }
    let actual = local_apps::canonical_sha256(&document.payload, "QA publication marker")
        .map_err(|error| error.to_string())?;
    if document.payload_sha256 != actual {
        return Err("QA publication marker digest does not match its content".into());
    }
    if document.payload.result_id != result_id {
        return Err("QA publication marker result binding is invalid".into());
    }
    Ok(document.payload)
}

fn result_is_passing(result: &local_apps::QaResult) -> bool {
    !result.scenario_judgements.is_empty()
        && result
            .scenario_judgements
            .iter()
            .all(|judgement| judgement.status == local_apps::QaScenarioStatus::Passed)
        && result.findings.iter().all(|finding| !finding.blocking)
}

/// Decode the strategy carried by the authenticated workflow boundary.  The
/// public QA tool deliberately has no quality/strategy field: the workflow
/// launcher derives this value from its persisted `LocalWorkflow.args` and
/// injects `verification_strategy` when it invokes the Host.  An omitted
/// value is the stable balanced default; the model-facing `quality_level`
/// field is never consulted here.
fn verification_strategy(input: &Value) -> Result<local_apps::QaVerificationStrategy, String> {
    match input
        .get("verification_strategy")
        .and_then(Value::as_str)
        .unwrap_or("balanced")
    {
        "fast" => Ok(local_apps::QaVerificationStrategy::Fast),
        "balanced" => Ok(local_apps::QaVerificationStrategy::Balanced),
        "thorough" => Ok(local_apps::QaVerificationStrategy::Thorough),
        value => Err(format!(
            "qa_begin_rejected: invalid authenticated verification_strategy {value:?}"
        )),
    }
}

fn normalize_qa_findings(value: Value) -> Result<Vec<local_apps::QaFinding>, String> {
    let Some(items) = value.as_array() else {
        return Err("findings must be an array".into());
    };
    let mut findings = BTreeMap::<String, local_apps::QaFinding>::new();
    for (index, item) in items.iter().enumerate() {
        let finding =
            if let Ok(finding) = serde_json::from_value::<local_apps::QaFinding>(item.clone()) {
                finding
            } else {
                let object = item.as_object();
                let id = object
                    .and_then(|object| object.get("id"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("model-finding-{index}"));
                let message = object
                    .and_then(|object| {
                        object
                            .get("message")
                            .or_else(|| object.get("evidence"))
                            .and_then(Value::as_str)
                    })
                    .map(ToString::to_string)
                    .unwrap_or_else(|| item.to_string());
                let blocking = object
                    .and_then(|object| object.get("blocking"))
                    .and_then(Value::as_bool)
                    .or_else(|| {
                        object
                            .and_then(|object| object.get("severity"))
                            .and_then(Value::as_str)
                            .map(|severity| !matches!(severity, "info" | "warning"))
                    })
                    .unwrap_or(true);
                let resolved_by_evidence_ids = object
                    .and_then(|object| object.get("resolved_by_evidence_ids"))
                    .and_then(Value::as_array)
                    .map(|ids| {
                        ids.iter()
                            .filter_map(Value::as_str)
                            .map(ToString::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                local_apps::QaFinding {
                    id,
                    message,
                    blocking,
                    resolved_by_evidence_ids,
                }
            };
        match findings.entry(finding.id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(finding);
            }
            std::collections::btree_map::Entry::Occupied(entry) if entry.get() == &finding => {}
            std::collections::btree_map::Entry::Occupied(_) => {
                return Err(format!(
                    "conflicting duplicate QA finding id {:?}",
                    finding.id
                ));
            }
        }
    }
    Ok(findings.into_values().collect())
}

#[derive(Debug, Clone)]
pub(crate) struct AuthoringBuildInput {
    pub(crate) handle: String,
    pub(crate) workflow_run_id: String,
    pub(crate) base_contract_sha256: Option<String>,
    pub(crate) contract: local_apps::AppAuthoringContract,
    pub(crate) contract_sha256: String,
}

/// The result of terminal QA validation before registry/spool publication.
///
/// This is intentionally an opaque, broker-created value.  Callers may use
/// the canonical JSON for the terminal spool, then pass the same value to
/// [`LocalAppsHostBroker::commit_prepared_workflow_qa_publication`].  The
/// commit path re-checks the immutable receipt/result bytes before writing
/// the publication marker and active receipt pointer.
#[derive(Debug, Clone)]
pub(crate) struct PreparedWorkflowQaPublication {
    pub(crate) canonical_result: Value,
    layout: local_apps::AppLayout,
    receipt: local_apps::QaReceipt,
    result_id: String,
    result_sha256: String,
    identity_sha256: String,
    runtime_publication_cell: RuntimePublicationCell,
}

impl PreparedWorkflowQaPublication {
    pub(crate) fn canonical_result(&self) -> Value {
        self.canonical_result.clone()
    }

    pub(crate) fn receipt_id(&self) -> &str {
        &self.receipt.receipt_id
    }

    pub(crate) fn result_id(&self) -> &str {
        &self.result_id
    }

    pub(crate) fn app_id(&self) -> &str {
        self.layout.app_id()
    }

    pub(crate) fn qa_handle(&self) -> &str {
        &self.receipt.qa_handle
    }
}

fn token(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || value.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        })
    {
        return Err(format!("{label} is invalid"));
    }
    Ok(())
}

/// Identity of the strict URL emitted by the Host when a native action starts.
/// The startup URL is intentionally origin-only: a loaded app may navigate to
/// a business route later, but the Host's requested navigation must not.
fn qa_expected_runtime_url_identity(url: &Url) -> Result<(String, u16, String), String> {
    let host = url
        .host_str()
        .map(str::to_ascii_lowercase)
        .filter(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1"))
        .ok_or_else(|| "QA runtime URL must use a loopback host".to_string())?;
    let port = url
        .port()
        .filter(|port| *port > 0)
        .ok_or_else(|| "QA runtime URL must name a port".to_string())?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("QA runtime URL has an invalid origin or path".into());
    }
    let pairs = url.query_pairs().collect::<Vec<_>>();
    if pairs.len() != 1
        || pairs[0].0 != "lingxi_runtime"
        || pairs[0].1.is_empty()
        || !pairs[0].1.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("QA runtime URL must contain exactly one numeric lingxi_runtime marker".into());
    }
    Ok((host, port, pairs[0].1.to_string()))
}

/// Identity of a URL observed after the WebView loaded. Business paths,
/// fragments, and unrelated query parameters are valid here; only the
/// loopback origin and exactly one numeric runtime marker authenticate the
/// page to the runtime generation requested by the Host.
fn qa_loaded_runtime_url_identity(url: &Url) -> Result<(String, u16, String), String> {
    let host = url
        .host_str()
        .map(str::to_ascii_lowercase)
        .filter(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1"))
        .ok_or_else(|| "QA runtime URL must use a loopback host".to_string())?;
    let port = url
        .port()
        .filter(|port| *port > 0)
        .ok_or_else(|| "QA runtime URL must name a port".to_string())?;
    if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() {
        return Err("QA runtime URL has an invalid origin".into());
    }
    let mut marker = None;
    for (key, value) in url.query_pairs() {
        if key != "lingxi_runtime" {
            continue;
        }
        if marker.is_some() {
            return Err("QA runtime URL has an ambiguous duplicate lingxi_runtime marker".into());
        }
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("QA runtime URL must contain a numeric lingxi_runtime marker".into());
        }
        marker = Some(value.to_string());
    }
    let marker = marker.ok_or_else(|| {
        "QA runtime URL must contain exactly one numeric lingxi_runtime marker".to_string()
    })?;
    Ok((host, port, marker))
}

pub(super) fn qa_result_with_evidence_ids(
    result: Value,
    qa_handle: &str,
    evidence_ids: impl IntoIterator<Item = String>,
) -> Value {
    let mut ids = result
        .get("qa_evidence_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|items| items.iter())
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    for evidence_id in evidence_ids {
        if !ids.iter().any(|existing| existing == &evidence_id) {
            ids.push(evidence_id);
        }
    }
    match result {
        Value::Object(mut object) => {
            object.insert("qa_handle".into(), Value::String(qa_handle.to_string()));
            object.insert(
                "qa_evidence_ids".into(),
                Value::Array(ids.into_iter().map(Value::String).collect()),
            );
            Value::Object(object)
        }
        other => json!({
            "result": other,
            "qa_handle": qa_handle,
            "qa_evidence_ids": ids,
        }),
    }
}

/// Remove reserved QA metadata before crossing from a native/app-controlled
/// response into the Host-authored result envelope. This prevents an app from
/// injecting evidence ids that an operator would later be unable to read.
fn sanitize_untrusted_qa_result(result: Value) -> Value {
    match result {
        Value::Object(mut object) => {
            object.remove("qa_handle");
            object.remove("qa_evidence_ids");
            Value::Object(object)
        }
        other => other,
    }
}

fn qa_query_matches_bridge_write_content(
    write_content: &Value,
    query: &Value,
    collection: &str,
) -> bool {
    let rows = qa_changed_rows(write_content, collection);
    if rows.is_empty() {
        return false;
    }
    rows.iter()
        .any(|(record_id, revision)| qa_query_contains_row(query, collection, record_id, *revision))
}

fn qa_query_causal_event(
    layout: &local_apps::AppLayout,
    session: &local_apps::QaSession,
    query: &Value,
    scenario_id: &str,
    target_id: &str,
    collection: &str,
) -> Result<Option<String>, String> {
    for write in session.evidence.iter().rev().filter(|evidence| {
        evidence.kind == local_apps::QaEvidenceKind::BridgeWrite
            && evidence.scenario_id == scenario_id
            && evidence.target_id == target_id
    }) {
        let block = local_apps::qa::qa_read_evidence_from_loaded_session(
            layout,
            session,
            &write.evidence_id,
        )
        .map_err(|error| error.to_string())?;
        let local_apps::QaEvidenceBlock::Json { content, .. } = block else {
            continue;
        };
        if qa_query_matches_bridge_write_content(&content, query, collection) {
            return Ok(Some(write.event_id.clone()));
        }
    }
    Ok(None)
}

fn upstream_finding_projection(failure: &local_apps::QaUpstreamFailure) -> local_apps::QaFinding {
    local_apps::QaFinding {
        id: failure.id.clone(),
        message: failure.message.clone(),
        blocking: true,
        resolved_by_evidence_ids: Vec::new(),
    }
}

fn qa_query_collection(input: &Value, content: &Value) -> Option<String> {
    input
        .get("collection")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            content
                .get("records")
                .and_then(Value::as_array)
                .and_then(|records| records.first())
                .and_then(|record| record.get("collection"))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
}

fn qa_changed_rows(value: &Value, collection: &str) -> Vec<(String, u64)> {
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
                result.get("recordId")?.as_str()?.to_string(),
                result.get("revision")?.as_u64()?,
            ))
        })
        .filter(|(record_id, revision)| !record_id.is_empty() && *revision > 0)
        .collect()
}

fn qa_query_contains_row(value: &Value, collection: &str, record_id: &str, revision: u64) -> bool {
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

pub(super) fn qa_observation_id(value: &Value) -> Option<String> {
    value
        .get("evidence")
        .and_then(|evidence| evidence.get("evidence_id"))
        .or_else(|| value.get("evidence_id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

#[derive(Debug)]
pub(super) struct QaActionGuard {
    active_actions: Arc<std::sync::Mutex<HashMap<String, super::QaActiveActionState>>>,
    app_id: String,
    event_id: String,
}

impl QaActionGuard {
    fn disarm(mut self) {
        // Keep the compatibility API's explicit `end_qa_action` lifecycle
        // without leaking the guard allocation or removing the active slot.
        self.app_id.clear();
        self.event_id.clear();
    }
}

impl Drop for QaActionGuard {
    fn drop(&mut self) {
        let Ok(mut active_actions) = self.active_actions.lock() else {
            return;
        };
        if active_actions
            .get(&self.app_id)
            .is_some_and(|action| action.event_id == self.event_id)
        {
            active_actions.remove(&self.app_id);
        }
    }
}

#[derive(Debug)]
pub(super) struct QaBridgeRequestGuard {
    active_actions: Arc<std::sync::Mutex<HashMap<String, super::QaActiveActionState>>>,
    app_id: String,
    event_id: String,
}

impl QaBridgeRequestGuard {
    pub(super) fn event_id(&self) -> &str {
        &self.event_id
    }
}

impl Drop for QaBridgeRequestGuard {
    fn drop(&mut self) {
        let Ok(mut active_actions) = self.active_actions.lock() else {
            return;
        };
        let Some(action) = active_actions
            .get_mut(&self.app_id)
            .filter(|action| action.event_id == self.event_id)
        else {
            return;
        };
        action.bridge_requests_in_flight = action.bridge_requests_in_flight.saturating_sub(1);
        action.bridge_last_activity = tokio::time::Instant::now();
        if action.bridge_requests_in_flight == 0 {
            action.bridge_settled.notify_one();
        }
    }
}

fn active_contract(
    layout: &local_apps::AppLayout,
) -> Result<(String, local_apps::AppAuthoringContract), String> {
    let digest = crate::mobile::local_apps_build::active_build_authoring_contract_sha256(layout)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            "authoring_contract_unavailable: active build has no contract".to_string()
        })?;
    let contract =
        local_apps::load_authoring_contract(layout, &digest).map_err(|error| error.to_string())?;
    Ok((digest, contract))
}

fn active_contract_optional(
    layout: &local_apps::AppLayout,
) -> Result<Option<(String, local_apps::AppAuthoringContract)>, String> {
    let Some(digest) =
        crate::mobile::local_apps_build::active_build_authoring_contract_sha256(layout)
            .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let contract =
        local_apps::load_authoring_contract(layout, &digest).map_err(|error| error.to_string())?;
    Ok(Some((digest, contract)))
}

/// Derive the only QA surface this Host can honestly exercise. The complete
/// authoring target matrix remains immutable in the scope; only targets whose
/// declared OS/form-factor pair matches the attached native device enter the
/// current run.
fn qa_scope_for_contract(
    contract: &local_apps::AppAuthoringContract,
    current_device: &local_apps::DeviceContext,
) -> Result<
    (
        local_apps::QaVerificationScope,
        Vec<local_apps::QaScenarioRequirement>,
    ),
    String,
> {
    let declared_target_ids = contract
        .spec
        .targets
        .iter()
        .map(|target| target.id.clone())
        .collect::<Vec<_>>();
    let in_scope_target_ids = contract
        .spec
        .targets
        .iter()
        .filter(|target| {
            target.os == current_device.os && target.form_factor == current_device.form_factor
        })
        .map(|target| target.id.clone())
        .collect::<Vec<_>>();
    if in_scope_target_ids.is_empty() {
        return Err(format!(
            "qa_begin_unavailable: no authoring target matches current Host device {}/{}",
            current_device.os, current_device.form_factor
        ));
    }
    let in_scope = in_scope_target_ids.iter().collect::<BTreeSet<_>>();
    let unverified_target_ids = declared_target_ids
        .iter()
        .filter(|target| !in_scope.contains(target))
        .cloned()
        .collect::<Vec<_>>();
    let mut unverified_scenario_ids = Vec::new();
    let scenario_requirements = contract
        .spec
        .acceptance_checks
        .iter()
        .map(|check| {
            let in_scope_scenario_targets = check
                .target_ids
                .iter()
                .filter(|target| in_scope.contains(target))
                .cloned()
                .collect::<Vec<_>>();
            if check.required && in_scope_scenario_targets.is_empty() {
                unverified_scenario_ids.push(check.id.clone());
            }
            local_apps::QaScenarioRequirement {
                scenario_id: check.id.clone(),
                required: check.required,
                // Preserve the complete authoring requirement. Core derives
                // effective current-device coverage from verification_scope;
                // intersecting here would erase declared targets from the
                // immutable QA contract.
                target_ids: check.target_ids.clone(),
                evidence_kinds: check
                    .evidence
                    .iter()
                    .map(|kind| match kind {
                        local_apps::AcceptanceEvidence::Inspect => {
                            local_apps::QaEvidenceKind::Inspect
                        }
                        local_apps::AcceptanceEvidence::UiAction => {
                            local_apps::QaEvidenceKind::UiAction
                        }
                        local_apps::AcceptanceEvidence::Capture => {
                            local_apps::QaEvidenceKind::Capture
                        }
                    })
                    .collect(),
                motion_required: check.motion_required,
            }
        })
        .collect::<Vec<_>>();
    Ok((
        local_apps::QaVerificationScope {
            declared_target_ids,
            in_scope_target_ids,
            unverified_target_ids,
            unverified_scenario_ids,
        },
        scenario_requirements,
    ))
}

fn checked_binding(
    manifest: &local_apps::AppManifest,
    app_id: &str,
    workflow_run_id: &str,
    root: &Path,
    input: &Value,
) -> Result<local_apps::AppRuntimeProfileBinding, String> {
    if let Some(binding) = manifest.runtime_profile.clone() {
        return Ok(binding);
    }
    let handle = required_string(input, "validated_selection_handle")?;
    crate::mobile::local_app_template_catalog::resolve_typed(root, app_id, workflow_run_id, handle)
        .map(|selection| selection.runtime_profile)
        .map_err(|error| error.to_string())
}

fn concise_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.trim().chars();
    let mut bounded = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

impl LocalAppsHostBroker {
    pub(crate) async fn begin_qa_action(&self, input: &Value) -> Result<Option<String>, String> {
        let Some((event_id, guard)) = self.begin_qa_action_with_guard(input).await? else {
            return Ok(None);
        };
        // This compatibility entry point is paired with explicit
        // `end_qa_action` calls by its callers. The UI tool uses the guarded
        // variant below so cancellation still deactivates synchronously.
        guard.disarm();
        Ok(Some(event_id))
    }

    pub(super) async fn begin_qa_action_with_guard(
        &self,
        input: &Value,
    ) -> Result<Option<(String, QaActionGuard)>, String> {
        let Some(handle) = input.get("qa_handle").and_then(Value::as_str) else {
            return Ok(None);
        };
        let app_id = required_string(input, "app_id")?;
        let scenario_id = required_string(input, "scenario_id")?;
        let target_id = required_string(input, "target_id")?;
        let layout = self.layout(app_id)?;
        let session =
            local_apps::load_qa_session(&layout, handle).map_err(|error| error.to_string())?;
        self.validate_qa_identity(&session.identity).await?;
        if !session
            .required_scenario_ids
            .iter()
            .any(|candidate| candidate == scenario_id)
        {
            return Err("qa_scenario_invalid: scenario is not required by this QA run".into());
        }
        if !session
            .target_ids
            .iter()
            .any(|candidate| candidate == target_id)
        {
            return Err("qa_target_invalid: target is not bound to this QA run".into());
        }
        if !session
            .verification_scope
            .in_scope_target_ids
            .iter()
            .any(|candidate| candidate == target_id)
        {
            return Err(
                "qa_target_unavailable: target is outside the current Host device scope".into(),
            );
        }
        let event_id = self.request_id("qa-action");
        let mut actions = self.qa_inflight_actions.lock().await;
        if actions.contains_key(app_id) {
            let active = self
                .qa_active_actions
                .lock()
                .map_err(|_| "qa_action_state_poisoned".to_string())?;
            if active.contains_key(app_id) {
                return Err(
                    "qa_action_in_flight: another native UI action is still pending".into(),
                );
            }
            // A cancelled future cannot await the async map lock. Its
            // synchronous liveness guard has already deactivated the action,
            // so reclaim the stale async entry before allowing a restart.
            actions.remove(app_id);
        }
        actions.insert(
            app_id.to_string(),
            super::QaInFlightAction {
                qa_handle: handle.to_string(),
                scenario_id: scenario_id.to_string(),
                target_id: target_id.to_string(),
                event_id: event_id.clone(),
                pending_bridge_results: Vec::new(),
            },
        );
        drop(actions);
        self.qa_active_actions
            .lock()
            .map_err(|_| "qa_action_state_poisoned".to_string())?
            .insert(
                app_id.to_string(),
                super::QaActiveActionState {
                    event_id: event_id.clone(),
                    bridge_requests_in_flight: 0,
                    bridge_last_activity: tokio::time::Instant::now(),
                    bridge_settled: Arc::new(Notify::new()),
                },
            );
        let guard = QaActionGuard {
            active_actions: self.qa_active_actions.clone(),
            app_id: app_id.to_string(),
            event_id: event_id.clone(),
        };
        Ok(Some((event_id, guard)))
    }

    pub(crate) async fn end_qa_action(&self, app_id: &str, event_id: &str) {
        let mut actions = self.qa_inflight_actions.lock().await;
        if actions
            .get(app_id)
            .is_some_and(|action| action.event_id == event_id)
        {
            actions.remove(app_id);
        }
        drop(actions);
        if let Ok(mut active_actions) = self.qa_active_actions.lock() {
            if active_actions
                .get(app_id)
                .is_some_and(|action| action.event_id == event_id)
            {
                active_actions.remove(app_id);
            }
        }
    }

    /// Buffer the result of a real foreground page-bridge mutation inside the
    /// currently pending native UI action. A normal tool/background mutation
    /// never calls this method, so it cannot be laundered into UI persistence
    /// evidence.
    pub(crate) async fn buffer_qa_bridge_result(
        &self,
        app_id: &str,
        event_id: &str,
        result: Value,
    ) -> Result<(), String> {
        let active_matches = {
            let active = self
                .qa_active_actions
                .lock()
                .map_err(|_| "qa_action_state_poisoned".to_string())?;
            active
                .get(app_id)
                .is_some_and(|active_action| active_action.event_id == event_id)
        };
        if !active_matches {
            return Err("qa_action_stale: page mutation outlived its native UI action".to_string());
        }
        let mut actions = self.qa_inflight_actions.lock().await;
        let action = actions
            .get_mut(app_id)
            .filter(|action| action.event_id == event_id)
            .ok_or_else(|| {
                "qa_action_stale: page mutation outlived its native UI action".to_string()
            })?;
        if action.pending_bridge_results.len() >= local_apps::MAX_MUTATION_BATCH_SIZE {
            return Err(format!(
                "qa_bridge_mutation_limit: one native action may attribute at most {} bridge writes",
                local_apps::MAX_MUTATION_BATCH_SIZE
            ));
        }
        action.pending_bridge_results.push(result);
        Ok(())
    }

    /// Bind a page-bridge mutation request to the QA action that was current
    /// when the request entered the Host. The event id prevents a delayed
    /// request from being attributed to a newer action for the same app.
    pub(super) async fn begin_qa_bridge_request(
        &self,
        app_id: &str,
    ) -> Option<QaBridgeRequestGuard> {
        let mut active = self.qa_active_actions.lock().ok()?;
        let action = active.get_mut(app_id)?;
        action.bridge_requests_in_flight = action.bridge_requests_in_flight.saturating_add(1);
        action.bridge_last_activity = tokio::time::Instant::now();
        Some(QaBridgeRequestGuard {
            active_actions: self.qa_active_actions.clone(),
            app_id: app_id.to_string(),
            event_id: action.event_id.clone(),
        })
    }

    pub(super) async fn end_qa_bridge_request(&self, app_id: &str, event_id: &str) {
        let Some(guard) = self.qa_active_actions.lock().ok().and_then(|actions| {
            actions.get(app_id).map(|_| QaBridgeRequestGuard {
                active_actions: self.qa_active_actions.clone(),
                app_id: app_id.to_string(),
                event_id: event_id.to_string(),
            })
        }) else {
            return;
        };
        drop(guard);
    }

    /// After native success, admit bridge requests that were asynchronously
    /// scheduled by the action during a short grace period, then wait for all
    /// requests that entered that window to settle. Failure/cancellation uses
    /// `end_qa_action` instead and drops only pending evidence attribution;
    /// business writes have already completed with their real results.
    pub(super) async fn settle_qa_action(
        &self,
        app_id: &str,
        event_id: &str,
    ) -> Result<super::QaInFlightAction, String> {
        let deadline = tokio::time::Instant::now() + super::QA_ACTION_BRIDGE_SETTLEMENT_TIMEOUT;
        if let Ok(mut active_actions) = self.qa_active_actions.lock() {
            let action = active_actions
                .get_mut(app_id)
                .filter(|action| action.event_id == event_id)
                .ok_or_else(|| {
                    "qa_action_stale: native UI action window changed before commit".to_string()
                })?;
            // The first quiet interval starts at native success, even when the
            // action itself took longer than the admission grace.
            action.bridge_last_activity = tokio::time::Instant::now();
        } else {
            return Err("qa_action_state_poisoned".into());
        }
        loop {
            let (settled, wait_for, ready) = {
                let mut active_actions = self
                    .qa_active_actions
                    .lock()
                    .map_err(|_| "qa_action_state_poisoned".to_string())?;
                let action = active_actions
                    .get(app_id)
                    .filter(|action| action.event_id == event_id)
                    .ok_or_else(|| {
                        "qa_action_stale: native UI action window changed before commit".to_string()
                    })?;
                let now = tokio::time::Instant::now();
                let quiet_remaining = (action.bridge_last_activity + super::QA_ACTION_BRIDGE_GRACE)
                    .saturating_duration_since(now);
                let deadline_remaining = deadline.saturating_duration_since(now);
                let wait_for = if action.bridge_requests_in_flight == 0 {
                    quiet_remaining.min(deadline_remaining)
                } else {
                    deadline_remaining
                };
                let settled = action.bridge_settled.clone();
                let ready = action.bridge_requests_in_flight == 0 && quiet_remaining.is_zero();
                if ready {
                    // Close admission while holding the same synchronous lock
                    // that every page bridge request uses to enter the action.
                    // Removing this only after awaiting the async result map
                    // left a gap where a bridge request was admitted to a
                    // window that settlement had already declared closed.
                    active_actions.remove(app_id);
                }
                (settled, wait_for, ready)
            };
            if ready {
                let mut actions = self.qa_inflight_actions.lock().await;
                let removed = actions.remove(app_id).ok_or_else(|| {
                    "qa_action_stale: native UI action window disappeared".to_string()
                })?;
                if removed.event_id != event_id {
                    return Err(
                        "qa_action_stale: native UI action window changed before commit".into(),
                    );
                }
                return Ok(removed);
            }
            if wait_for.is_zero() {
                self.end_qa_action(app_id, event_id).await;
                return Err(
                    "qa_action_settlement_timeout: page mutation did not finish after native success"
                        .into(),
                );
            }
            let _ = tokio::time::timeout(wait_for, settled.notified()).await;
        }
    }

    pub(super) async fn take_qa_action(
        &self,
        app_id: &str,
        event_id: &str,
    ) -> Result<super::QaInFlightAction, String> {
        let mut actions = self.qa_inflight_actions.lock().await;
        if !actions
            .get(app_id)
            .is_some_and(|action| action.event_id == event_id)
        {
            return Err("qa_action_stale: native UI action window changed before commit".into());
        }
        let removed = actions
            .remove(app_id)
            .ok_or_else(|| "qa_action_stale: native UI action window disappeared".to_string())?;
        if let Ok(mut active_actions) = self.qa_active_actions.lock() {
            if active_actions
                .get(app_id)
                .is_some_and(|active_action| active_action.event_id == event_id)
            {
                active_actions.remove(app_id);
            }
        }
        Ok(removed)
    }

    /// Persist evidence for page writes that already completed with their real
    /// datastore results. Native failure drops only this attribution buffer;
    /// it never rolls back a business write or replaces its response.
    pub(super) async fn commit_qa_action_mutations(
        &self,
        app_id: &str,
        action: &super::QaInFlightAction,
    ) -> Result<Vec<String>, String> {
        if action.pending_bridge_results.is_empty() {
            return Ok(Vec::new());
        }
        let layout = self.layout(app_id)?;
        let session = local_apps::load_qa_session(&layout, &action.qa_handle)
            .map_err(|error| error.to_string())?;
        self.validate_qa_identity(&session.identity).await?;
        self.service()?
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let bridge_input = json!({
            "app_id": app_id,
            "qa_handle": action.qa_handle,
            "scenario_id": action.scenario_id,
            "target_id": action.target_id,
        });
        let mut evidence_ids = Vec::with_capacity(action.pending_bridge_results.len());
        for result in &action.pending_bridge_results {
            self.validate_qa_identity(&session.identity).await?;
            let evidence = self
                .record_qa_observation(
                    &bridge_input,
                    "bridge_write",
                    result.clone(),
                    self.request_id("qa-bridge-write"),
                    Some(action.event_id.clone()),
                )
                .await?;
            if let Some(evidence_id) = qa_observation_id(&evidence) {
                evidence_ids.push(evidence_id);
            }
        }
        Ok(evidence_ids)
    }

    pub(crate) fn active_authoring_contract_sha256(
        &self,
        layout: &local_apps::AppLayout,
    ) -> Result<Option<String>, String> {
        crate::mobile::local_apps_build::active_build_authoring_contract_sha256(layout)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn verify_authoring_candidate_digest(
        &self,
        layout: &local_apps::AppLayout,
        expected_digest: &str,
    ) -> Result<(), String> {
        let candidate = local_apps::load_authoring_candidate(layout)
            .map_err(|error| format!("authoring_candidate_invalid: {error}"))?;
        if candidate.contract_sha256 != expected_digest {
            return Err(
                "authoring_contract_stale: candidate changed while the build was running".into(),
            );
        }
        let active_digest =
            crate::mobile::local_apps_build::active_build_authoring_contract_sha256(layout)
                .map_err(|error| error.to_string())?;
        if candidate.base_contract_sha256 != active_digest {
            return Err(
                "authoring_contract_stale: candidate base no longer matches the active build"
                    .into(),
            );
        }
        Ok(())
    }

    pub(crate) fn verify_authoring_candidate_identity(
        &self,
        layout: &local_apps::AppLayout,
        expected: &crate::mobile::local_apps_build::AuthoringCandidateIdentity,
    ) -> Result<(), String> {
        let candidate = local_apps::load_authoring_candidate(layout)
            .map_err(|error| format!("authoring_candidate_invalid: {error}"))?;
        if candidate.handle != expected.handle
            || candidate.workflow_run_id != expected.workflow_run_id
            || candidate.contract_sha256 != expected.contract_sha256
            || candidate.base_contract_sha256 != expected.base_contract_sha256
        {
            return Err(
                "authoring_contract_stale: candidate identity changed while the build was running"
                    .into(),
            );
        }
        self.verify_authoring_candidate_digest(layout, &expected.contract_sha256)?;
        let manifest = load_manifest(layout).map_err(|error| error.to_string())?;
        if manifest.runtime_profile.as_ref() != Some(&candidate.contract.runtime_profile) {
            return Err(
                "authoring_contract_stale: candidate runtime profile no longer matches the committed manifest"
                    .into(),
            );
        }
        Ok(())
    }

    pub(crate) async fn local_app_contract(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let operation = input
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("get");
        let service = self.service()?;
        service
            .record(&app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(&app_id)?;
        match operation {
            "get" => {
                let contract = active_contract_optional(&layout)?;
                Ok(json!({
                    "ok": true,
                    "app_id": app_id,
                    "build_id": crate::mobile::local_apps_build::active_build_id(&layout).map_err(|error| error.to_string())?,
                    "contract_sha256": contract.as_ref().map(|(digest, _)| digest),
                    "contract": contract.as_ref().map(|(_, contract)| contract),
                    "summary": contract.as_ref().map(|(_, contract)| json!({
                        "goal": contract.spec.product.goal,
                        "target_count": contract.spec.targets.len(),
                        "acceptance_check_count": contract.spec.acceptance_checks.len(),
                    })),
                }))
            }
            "stage" => {
                let build_lock = self.build_lock();
                let _build_guard = build_lock.lock().await;
                let _process_build_guard =
                    local_apps::storage::lock_app_build(layout.root(), layout.app_id())
                        .map_err(|error| error.to_string())?;
                let workflow_run_id = required_string(&input, "workflow_run_id")?;
                token(workflow_run_id, "workflow_run_id")?;
                let spec: local_apps::AppAuthoringSpec = serde_json::from_value(
                    input
                        .get("spec")
                        .cloned()
                        .ok_or_else(|| "spec is required for contract staging".to_string())?,
                )
                .map_err(|error| format!("authoring spec invalid: {error}"))?;
                let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
                let binding =
                    checked_binding(&manifest, &app_id, workflow_run_id, &self.root, &input)?;
                let revision = active_contract_optional(&layout)?
                    .map(|(_, contract)| contract.revision.saturating_add(1))
                    .unwrap_or(1);
                let contract = local_apps::AppAuthoringContract {
                    version: local_apps::AUTHORING_SCHEMA_VERSION,
                    revision,
                    app_id: app_id.clone(),
                    runtime_profile: binding,
                    spec,
                };
                let base_contract_sha256 = input
                    .get("base_contract_sha256")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                let effective_digest =
                    crate::mobile::local_apps_build::active_build_authoring_contract_sha256(
                        &layout,
                    )
                    .map_err(|error| error.to_string())?;
                if base_contract_sha256.as_deref() != effective_digest.as_deref()
                    && (base_contract_sha256.is_some() || effective_digest.is_some())
                {
                    return Err(
                        "authoring_contract_stale: base digest does not match the effective build"
                            .into(),
                    );
                }
                let staged = local_apps::stage_authoring(
                    &layout,
                    workflow_run_id,
                    contract,
                    base_contract_sha256,
                )
                .map_err(|error| error.to_string())?;
                Ok(json!({
                    "ok": true,
                    "contract_handle": staged.handle,
                    "contract_sha256": staged.contract_sha256,
                    "contract": staged.contract,
                    "summary": {
                        "goal": staged.contract.spec.product.goal,
                        "target_count": staged.contract.spec.targets.len(),
                        "acceptance_check_count": staged.contract.spec.acceptance_checks.len(),
                    },
                }))
            }
            _ => Err("operation must be get or stage".into()),
        }
    }

    pub(crate) fn authoring_contract_for_build(
        &self,
        layout: &local_apps::AppLayout,
        input: &Value,
    ) -> Result<Option<AuthoringBuildInput>, String> {
        let Some(handle) = input.get("contract_handle").and_then(Value::as_str) else {
            return Ok(None);
        };
        token(handle, "contract_handle")?;
        let candidate =
            local_apps::load_authoring_candidate(layout).map_err(|error| error.to_string())?;
        if candidate.handle != handle {
            return Err("authoring_candidate_invalid: handle or workflow binding mismatch".into());
        }
        let workflow_run_id = required_string(input, "workflow_run_id")?;
        token(workflow_run_id, "workflow_run_id")?;
        if candidate.workflow_run_id != workflow_run_id {
            return Err("authoring_candidate_invalid: handle or workflow binding mismatch".into());
        }
        Ok(Some(AuthoringBuildInput {
            handle: candidate.handle,
            workflow_run_id: candidate.workflow_run_id,
            base_contract_sha256: candidate.base_contract_sha256,
            contract: candidate.contract,
            contract_sha256: candidate.contract_sha256,
        }))
    }

    async fn validate_qa_identity(&self, identity: &local_apps::QaIdentity) -> Result<(), String> {
        let layout = self.layout(&identity.app_id)?;
        let active_build = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "qa_stale_build: active build is missing".to_string())?;
        if active_build != identity.build_id {
            return Err("qa_stale_build: active build changed during QA".into());
        }
        let (generation, runtime_build) = self
            .runtime_identity(&identity.app_id)
            .await?
            .ok_or_else(|| "qa_stale_runtime: runtime is not running".to_string())?;
        if generation != identity.runtime_generation || runtime_build != identity.build_id {
            return Err("qa_stale_runtime: running runtime identity changed".into());
        }
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        if manifest.revision != identity.manifest_revision
            || manifest.runtime_profile.as_ref() != Some(&identity.runtime_profile)
            || manifest
                .dependency_snapshot_hash()
                .map_err(|error| error.to_string())?
                != identity.dependency_snapshot_sha256
        {
            return Err("qa_stale_manifest: manifest or dependencies changed during QA".into());
        }
        if crate::mobile::local_apps_build::active_build_authoring_contract_sha256(&layout)
            .map_err(|error| error.to_string())?
            .as_deref()
            != Some(identity.authoring_contract_sha256.as_str())
        {
            return Err("qa_stale_authoring_contract: active contract changed during QA".into());
        }
        Ok(())
    }

    pub(crate) async fn qa_begin(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let workflow_run_id = required_string(&input, "workflow_run_id")?.to_string();
        token(&workflow_run_id, "workflow_run_id")?;
        let service = self.service()?;
        service
            .record(&app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(&app_id)?;
        let build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "qa_begin_rejected: app has no active build".to_string())?;
        crate::mobile::local_apps_build::validate_build_for_launch(&layout)
            .map_err(|error| format!("qa_begin_rejected: {error}"))?;
        let (runtime_generation, running_build) = self
            .runtime_identity(&app_id)
            .await?
            .ok_or_else(|| "qa_begin_rejected: runtime is not running".to_string())?;
        if running_build != build_id {
            return Err("qa_begin_rejected: running runtime serves a stale build".into());
        }
        if input
            .get("build_id")
            .and_then(Value::as_str)
            .is_some_and(|claimed| claimed != build_id)
        {
            return Err("qa_begin_rejected: claimed build_id is not active".into());
        }
        let (authoring_contract_sha256, contract) = active_contract(&layout)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let verification_strategy = verification_strategy(&input)?;
        let current_device = self.host_device_context().ok_or_else(|| {
            "qa_begin_unavailable: current Host device context is unavailable".to_string()
        })?;
        let (verification_scope, scenario_requirements) =
            qa_scope_for_contract(&contract, &current_device)?;
        let identity = local_apps::QaIdentity {
            app_id: app_id.clone(),
            workflow_run_id: workflow_run_id.clone(),
            qa_handle: local_apps::ids::generate_qa_handle(),
            build_id,
            runtime_profile: contract.runtime_profile.clone(),
            verification_strategy,
            dependency_snapshot_sha256: manifest
                .dependency_snapshot_hash()
                .map_err(|error| error.to_string())?,
            authoring_contract_sha256,
            manifest_revision: manifest.revision,
            runtime_generation,
        };
        // Upstream failures are a Host-owned ledger, never caller-supplied
        // proof.  A workflow may report findings at finalization, but it
        // cannot inject or erase the durable failures that bind this run.
        let upstream_failures = Vec::new();
        let session = local_apps::qa_begin_with_scope(
            &layout,
            identity,
            scenario_requirements,
            verification_scope,
            upstream_failures,
            now_ms(),
        )
        .map_err(|error| error.to_string())?;
        Ok(json!({
            "ok": true,
            "qa_handle": session.identity.qa_handle,
            "app_id": app_id,
            "workflow_run_id": workflow_run_id,
            "build_id": session.identity.build_id,
            "runtime_generation": session.identity.runtime_generation,
            "scenario_ids": session.required_scenario_ids,
            "target_ids": session.verification_scope.in_scope_target_ids,
            "declared_target_ids": session.verification_scope.declared_target_ids,
            "unverified_target_ids": session.verification_scope.unverified_target_ids,
            "unverified_scenario_ids": session.verification_scope.unverified_scenario_ids,
            "verification_scope": session.verification_scope,
            // The durable Host ledger is part of the repair contract. A fresh
            // tester/verifier must receive the exact IDs and messages so it
            // can resolve source blockers with newer Host evidence instead of
            // accidentally dropping them from its findings projection.
            "upstream_failures": session.upstream_failures,
            "upstream_findings": session
                .upstream_failures
                .iter()
                .map(upstream_finding_projection)
                .collect::<Vec<_>>(),
        }))
    }

    pub(crate) async fn validate_qa_request(&self, input: &Value) -> Result<(), String> {
        let Some(handle) = input.get("qa_handle").and_then(Value::as_str) else {
            return Ok(());
        };
        let app_id = required_string(input, "app_id")?;
        let layout = self.layout(app_id)?;
        let session =
            local_apps::load_qa_session(&layout, handle).map_err(|error| error.to_string())?;
        self.validate_qa_identity(&session.identity).await?;
        let target_id = input
            .get("target_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "qa_target_required: target_id is required".to_string())?;
        let scenario_id = input
            .get("scenario_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "qa_scenario_required: scenario_id is required".to_string())?;
        if !session
            .required_scenario_ids
            .iter()
            .any(|candidate| candidate == scenario_id)
        {
            return Err("qa_scenario_invalid: scenario is not required by this QA run".into());
        }
        if !session
            .verification_scope
            .in_scope_target_ids
            .iter()
            .any(|candidate| candidate == target_id)
        {
            return Err(
                "qa_target_unavailable: target is outside the current Host device scope".into(),
            );
        }
        let requirement = session
            .scenario_requirements
            .iter()
            .find(|requirement| requirement.scenario_id == scenario_id)
            .ok_or_else(|| {
                "qa_scenario_invalid: scenario requirement is unavailable".to_string()
            })?;
        if !requirement
            .target_ids
            .iter()
            .any(|candidate| candidate == target_id)
        {
            return Err("qa_target_invalid: target is not declared for this QA scenario".into());
        }
        Ok(())
    }

    pub(crate) async fn record_qa_observation(
        &self,
        input: &Value,
        operation: &str,
        content: Value,
        event_id: String,
        causal_event_id: Option<String>,
    ) -> Result<Value, String> {
        let Some(handle) = input.get("qa_handle").and_then(Value::as_str) else {
            return Ok(Value::Null);
        };
        let app_id = required_string(input, "app_id")?;
        let layout = self.layout(app_id)?;
        let session =
            local_apps::load_qa_session(&layout, handle).map_err(|error| error.to_string())?;
        self.validate_qa_identity(&session.identity).await?;
        let target_id = input
            .get("target_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "qa_target_required: target_id is required".to_string())?;
        let scenario_id = input
            .get("scenario_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "qa_scenario_required: scenario_id is required".to_string())?;
        let kind = match operation {
            "inspect_ui" => local_apps::QaEvidenceKind::Inspect,
            "act_on_ui" => local_apps::QaEvidenceKind::UiAction,
            "capture_ui" => local_apps::QaEvidenceKind::Capture,
            "bridge_write" => local_apps::QaEvidenceKind::BridgeWrite,
            "query_data" => local_apps::QaEvidenceKind::Query,
            _ => local_apps::QaEvidenceKind::Console,
        };
        let query_collection = (kind == local_apps::QaEvidenceKind::Query)
            .then(|| qa_query_collection(input, &content))
            .flatten();
        // Keep filesystem reads off Tokio. The worker owns the already loaded
        // session and query payload, then returns both so the caller can retain
        // the same authoritative values for causality timestamps and evidence
        // persistence. Reverse ordering still short-circuits at the newest
        // matching write instead of retaining every candidate JSON artifact.
        let (caused_by, session, content) = match kind {
            // Only the Host action window may supply this id. Never fall back
            // to an older action: normal/background writes intentionally have
            // no way to enter this branch.
            local_apps::QaEvidenceKind::BridgeWrite => (causal_event_id, session, content),
            // Query input carries no model-authored causal id. Match the
            // actual query page against the exact collection/record/revision
            // tuple returned by a Host-recorded page write. This is important
            // when one click writes collection A and then B in separate bridge
            // calls: an A query must point at A's write, never merely B's
            // latest timestamp.
            local_apps::QaEvidenceKind::Query => {
                let Some(collection) = query_collection else {
                    return Err(
                        "qa_query_causality_unresolved: query has no collection identity".into(),
                    );
                };
                let worker_layout = layout.clone();
                let worker_session = session;
                let worker_query = content;
                let worker_scenario = scenario_id.to_string();
                let worker_target = target_id.to_string();
                let (event_id, session, content) = tokio::task::spawn_blocking(move || {
                    let event_id = qa_query_causal_event(
                        &worker_layout,
                        &worker_session,
                        &worker_query,
                        &worker_scenario,
                        &worker_target,
                        &collection,
                    )
                    .map_err(|error| {
                        format!("qa_query_causality_unresolved: {error}")
                    })?
                    .ok_or_else(|| {
                        format!(
                            "qa_query_causality_unresolved: query collection {collection:?} does not match a recorded page write"
                        )
                    })?;
                    Ok::<_, String>((event_id, worker_session, worker_query))
                })
                .await
                .map_err(|error| format!("qa_query_causality_worker_failed: {error}"))??;
                (Some(event_id), session, content)
            }
            _ => (None, session, content),
        };
        let recorded_at_ms = caused_by
            .as_deref()
            .and_then(|parent| {
                session
                    .evidence
                    .iter()
                    .find(|evidence| evidence.event_id == parent)
            })
            .map_or_else(now_ms, |parent| {
                now_ms().max(parent.recorded_at_ms.saturating_add(1))
            });
        let evidence_content = if matches!(kind, local_apps::QaEvidenceKind::Capture) {
            let image = content.get("image").unwrap_or(&content);
            let encoded = image
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| "qa_capture_invalid: Host capture has no image data".to_string())?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| format!("qa_capture_invalid: {error}"))?;
            let format = match image
                .get("mime_type")
                .or_else(|| image.get("format"))
                .and_then(Value::as_str)
            {
                Some("image/png" | "png") => local_apps::QaImageFormat::Png,
                Some("image/jpeg" | "jpeg" | "jpg") => local_apps::QaImageFormat::Jpeg,
                Some("image/webp" | "webp") => local_apps::QaImageFormat::Webp,
                Some(other) => {
                    return Err(format!(
                        "qa_capture_invalid: unsupported Host image format {other}"
                    ))
                }
                None => return Err("qa_capture_invalid: Host capture has no image format".into()),
            };
            local_apps::QaHostEvidenceContent::Image { format, bytes }
        } else {
            local_apps::QaHostEvidenceContent::Json(content)
        };
        let evidence = local_apps::qa_record_host_evidence(
            &layout,
            handle,
            local_apps::QaHostEvidenceInput {
                evidence_id: self.request_id("qa-evidence"),
                scenario_id: scenario_id.to_string(),
                target_id: target_id.to_string(),
                kind,
                recorded_at_ms,
                event_id,
                caused_by,
                content: evidence_content,
            },
        )
        .map_err(|error| error.to_string())?;
        self.validate_qa_identity(&session.identity).await?;
        serde_json::to_value(evidence).map_err(|error| error.to_string())
    }

    pub(crate) async fn qa_ui_request_value(
        &self,
        input: &Value,
        action_value: Option<String>,
    ) -> Result<Option<String>, String> {
        if input.get("qa_handle").and_then(Value::as_str).is_none() {
            return Ok(action_value);
        }
        self.validate_qa_request(input).await?;
        let app_id = required_string(input, "app_id")?;
        let record = self
            .service()?
            .runtime_record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        if record.port.is_none() {
            return Err("qa_stale_runtime: runtime has no pinned port".into());
        }
        let runtime_url = crate::mobile::local_apps_bridge::runtime_preview_url(&record)
            .ok_or_else(|| "qa_stale_runtime: runtime has no preview URL".to_string())?;
        let parsed_runtime = Url::parse(&runtime_url)
            .map_err(|error| format!("qa_stale_runtime: invalid preview URL: {error}"))?;
        qa_expected_runtime_url_identity(&parsed_runtime)
            .map_err(|error| format!("qa_stale_runtime: {error}"))?;
        Ok(Some(
            json!({
                "lingxi_qa": {
                    "version": 1,
                    "expected_runtime_url": runtime_url,
                    "action_value": action_value,
                },
            })
            .to_string(),
        ))
    }

    pub(crate) async fn qa_ui_response_value(
        &self,
        input: &Value,
        result: Value,
    ) -> Result<Value, String> {
        if input.get("qa_handle").and_then(Value::as_str).is_none() {
            return Ok(result);
        }
        let attestation = result
            .get("lingxi_qa")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                "qa_native_attestation_missing: UI response has no Host attestation".to_string()
            })?;
        if attestation.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("qa_native_attestation_invalid: unsupported attestation version".into());
        }
        if attestation
            .get("navigation_generation")
            .and_then(Value::as_u64)
            .is_none_or(|generation| generation == 0)
        {
            return Err("qa_native_attestation_invalid: navigation generation is missing".into());
        }
        let requested = attestation
            .get("requested_runtime_url")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "qa_native_attestation_invalid: requested runtime URL missing".to_string()
            })?;
        let loaded = attestation
            .get("loaded_runtime_url")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "qa_native_attestation_invalid: loaded runtime URL missing".to_string()
            })?;
        let requested_url = Url::parse(requested)
            .map_err(|error| format!("qa_native_attestation_invalid: requested URL: {error}"))?;
        let loaded_url = Url::parse(loaded)
            .map_err(|error| format!("qa_native_attestation_invalid: loaded URL: {error}"))?;
        let requested_identity = qa_expected_runtime_url_identity(&requested_url)
            .map_err(|error| format!("qa_native_attestation_invalid: {error}"))?;
        let loaded_identity = qa_loaded_runtime_url_identity(&loaded_url)
            .map_err(|error| format!("qa_native_attestation_invalid: {error}"))?;
        if requested_identity != loaded_identity {
            return Err(
                "qa_native_attestation_invalid: loaded URL is not the requested runtime generation"
                    .into(),
            );
        }
        let app_id = required_string(input, "app_id")?;
        let record = self
            .service()?
            .runtime_record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        if record.port.is_none() {
            return Err("qa_stale_runtime: runtime has no pinned port".into());
        }
        let expected = crate::mobile::local_apps_bridge::runtime_preview_url(&record)
            .ok_or_else(|| "qa_stale_runtime: runtime has no preview URL".to_string())?;
        let expected_url = Url::parse(&expected)
            .map_err(|error| format!("qa_stale_runtime: expected URL: {error}"))?;
        let expected_identity = qa_expected_runtime_url_identity(&expected_url)
            .map_err(|error| format!("qa_stale_runtime: {error}"))?;
        if requested_identity != expected_identity || loaded_identity != expected_identity {
            return Err("qa_stale_runtime: native attestation is for a stale runtime URL".into());
        }
        let target_id = input
            .get("target_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "qa_target_required: target_id is required for native attestation".to_string()
            })?;
        let session = local_apps::load_qa_session(
            &self.layout(app_id)?,
            required_string(input, "qa_handle")?,
        )
        .map_err(|error| error.to_string())?;
        // A build/runtime can change while the native response is in flight.
        // Re-check before recording the attestation so stale responses cannot
        // leave apparently valid evidence in a superseded session.
        self.validate_qa_identity(&session.identity).await?;
        let contract = active_contract(&self.layout(app_id)?)?.1;
        let target = contract
            .spec
            .targets
            .iter()
            .find(|target| target.id == target_id)
            .ok_or_else(|| "qa_target_invalid: target is not in the active contract".to_string())?;
        let current_device = self.host_device_context().ok_or_else(|| {
            "qa_native_attestation_unavailable: current Host device context is unavailable"
                .to_string()
        })?;
        if target.os != current_device.os || target.form_factor != current_device.form_factor {
            return Err(
                "qa_native_attestation_invalid: target is outside the current Host device scope"
                    .into(),
            );
        }
        if attestation.get("platform").and_then(Value::as_str) != Some(target.os.as_str())
            || attestation.get("form_factor").and_then(Value::as_str)
                != Some(target.form_factor.as_str())
        {
            return Err(
                "qa_native_attestation_invalid: target platform or form factor mismatch".into(),
            );
        }
        let native_event = self.request_id("qa-native");
        local_apps::qa_record_host_evidence(
            &self.layout(app_id)?,
            required_string(input, "qa_handle")?,
            local_apps::QaHostEvidenceInput {
                evidence_id: native_event.clone(),
                scenario_id: required_string(input, "scenario_id")?.to_string(),
                target_id: target_id.to_string(),
                kind: local_apps::QaEvidenceKind::NativeTargetProvenance,
                recorded_at_ms: now_ms(),
                event_id: native_event.clone(),
                caused_by: None,
                content: local_apps::QaHostEvidenceContent::NativeTarget(
                    local_apps::QaNativeTargetProvenance {
                        target_id: target_id.to_string(),
                        os: target.os.clone(),
                        form_factor: target.form_factor.clone(),
                        device_model: attestation
                            .get("device_model")
                            .and_then(Value::as_str)
                            .unwrap_or("native")
                            .to_string(),
                        captured_at_ms: now_ms(),
                    },
                ),
            },
        )
        .map_err(|error| error.to_string())?;
        self.validate_qa_identity(&session.identity).await?;
        Ok(qa_result_with_evidence_ids(
            sanitize_untrusted_qa_result(result.get("result").cloned().unwrap_or(Value::Null)),
            required_string(input, "qa_handle")?,
            [native_event],
        ))
    }

    pub(crate) async fn qa_read_evidence(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?;
        let handle = required_string(&input, "qa_handle")?;
        let evidence_id = required_string(&input, "evidence_id")?;
        let layout = self.layout(app_id)?;
        let block = local_apps::qa_read_evidence(&layout, handle, evidence_id)
            .map_err(|error| error.to_string())?;
        match block {
            local_apps::QaEvidenceBlock::Json { evidence, content } => {
                Ok(json!({"evidence": evidence, "content": content}))
            }
            local_apps::QaEvidenceBlock::Text { evidence, content } => {
                Ok(json!({"evidence": evidence, "content": content}))
            }
            local_apps::QaEvidenceBlock::Image {
                evidence,
                format,
                bytes,
            } => Ok(json!({
                "evidence": evidence,
                "content": {
                    "type": "image",
                    "mime_type": match format {
                        local_apps::QaImageFormat::Png => "image/png",
                        local_apps::QaImageFormat::Jpeg => "image/jpeg",
                        local_apps::QaImageFormat::Webp => "image/webp",
                    },
                    "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                }
            })),
        }
    }

    pub(crate) async fn qa_finalize(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?;
        let handle = required_string(&input, "qa_handle")?;
        let layout = self.layout(app_id)?;
        let mut judgements_value = input
            .get("scenario_judgements")
            .cloned()
            .ok_or_else(|| "scenario_judgements is required".to_string())?;
        let mut finding_values = input
            .get("findings")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let finding_values_array = finding_values
            .as_array_mut()
            .ok_or_else(|| "findings must be an array".to_string())?;
        if let Some(judgements) = judgements_value.as_array_mut() {
            for judgement in judgements {
                if let Some(object) = judgement.as_object_mut() {
                    // Workflow schemas may carry findings beside each
                    // scenario. Merge them into the canonical top-level
                    // union before strict core decoding; silently deleting a
                    // blocker would let a failed scenario appear passing.
                    if let Some(scenario_findings) = object.remove("findings") {
                        let Some(scenario_findings) = scenario_findings.as_array() else {
                            return Err("scenario findings must be an array".into());
                        };
                        finding_values_array.extend(scenario_findings.iter().cloned());
                    }
                    if object.get("status").and_then(Value::as_str) == Some("blocked") {
                        object.insert("status".into(), Value::String("failed".into()));
                    }
                }
            }
        }
        let judgements: Vec<local_apps::QaScenarioJudgement> =
            serde_json::from_value(judgements_value)
                .map_err(|error| format!("invalid scenario_judgements: {error}"))?;
        let findings = normalize_qa_findings(finding_values)?;
        let output = local_apps::qa_finalize(&layout, handle, judgements, findings, now_ms())
            .map_err(|error| error.to_string())?;
        // Core merges durable ledger findings and prior candidate findings;
        // derive the visible result from that canonical union, not from the
        // caller's projection before finalization.
        let passed = result_is_passing(&output.result);
        Ok(
            json!({"ok": passed, "status": "candidate", "receipt": output.receipt, "result": output.result}),
        )
    }

    fn workflow_receipt_id(result: &Value) -> Result<&str, String> {
        result
            .get("receipt_id")
            .or_else(|| {
                result
                    .get("receipt")
                    .and_then(|receipt| receipt.get("receipt_id"))
            })
            .or_else(|| {
                result
                    .get("verification")
                    .and_then(|verification| verification.get("receipt_id"))
            })
            .or_else(|| {
                result
                    .get("verification")
                    .and_then(|verification| verification.get("receipt"))
                    .and_then(|receipt| receipt.get("receipt_id"))
            })
            .and_then(Value::as_str)
            .ok_or_else(|| "qa_terminal_rejected: receipt_id is required".to_string())
    }

    /// Validate a workflow's candidate without publishing it.  The expected
    /// strategy comes from the authenticated `LocalWorkflow.args` row at the
    /// composition boundary; it is never inferred from the model result.
    pub(crate) async fn prepare_workflow_qa_outcome(
        &self,
        app_id: &str,
        workflow_run_id: &str,
        expected_strategy: local_apps::QaVerificationStrategy,
        result: Value,
    ) -> Result<PreparedWorkflowQaPublication, String> {
        let _perf = LocalAppPerfDiagnosticTimer::start("qa_prepare");
        let receipt_id = Self::workflow_receipt_id(&result)?;
        let layout = self.layout(app_id)?;
        let receipt =
            local_apps::load_qa_receipt(&layout, receipt_id).map_err(|error| error.to_string())?;
        if receipt.workflow_run_id != workflow_run_id {
            return Err("qa_terminal_rejected: workflow run mismatch".into());
        }
        let result_record = local_apps::load_qa_result(&layout, &receipt.result_id)
            .map_err(|error| error.to_string())?;
        if result_record.identity.workflow_run_id != workflow_run_id
            || result_record.identity.app_id != app_id
        {
            return Err("qa_terminal_rejected: receipt identity mismatch".into());
        }
        if result_record.identity.verification_strategy != expected_strategy {
            return Err(
                "qa_terminal_rejected: verification strategy does not match authenticated workflow"
                    .into(),
            );
        }
        if receipt.identity_sha256
            != result_record
                .identity
                .sha256()
                .map_err(|error| error.to_string())?
            || receipt.result_sha256 != result_record.result_sha256
        {
            return Err("qa_terminal_rejected: receipt does not bind the candidate result".into());
        }
        let session = local_apps::load_qa_session(&layout, &result_record.identity.qa_handle)
            .map_err(|error| format!("qa_terminal_rejected: QA session unavailable: {error}"))?;
        if session.finalized_result_id.as_deref() != Some(receipt.result_id.as_str()) {
            return Err(
                "qa_terminal_rejected: candidate is not the latest finalized QA result".into(),
            );
        }
        if session.verification_scope != result_record.verification_scope {
            return Err("qa_terminal_rejected: candidate verification scope changed".into());
        }
        let current_device = self.host_device_context().ok_or_else(|| {
            "qa_terminal_rejected: current Host device context is unavailable".to_string()
        })?;
        let (_, contract) = active_contract(&layout)?;
        let (current_scope, _) = qa_scope_for_contract(&contract, &current_device)?;
        if current_scope != result_record.verification_scope {
            return Err("qa_terminal_rejected: current Host device scope changed since QA".into());
        }
        if !result_is_passing(&result_record) {
            return Err(
                "qa_terminal_rejected: candidate contains failed scenarios or blocking findings"
                    .into(),
            );
        }
        self.validate_qa_identity(&result_record.identity).await?;
        let runtime_publication_cell = self.runtime_publication_cell(app_id)?;
        let expected_runtime = RuntimePublicationIdentity {
            generation: result_record.identity.runtime_generation,
            build_id: result_record.identity.build_id.clone(),
        };
        if runtime_publication_cell
            .read()
            .map_err(|_| "qa_terminal_rejected: runtime publication identity is poisoned")?
            .as_ref()
            != Some(&expected_runtime)
        {
            return Err("qa_terminal_rejected: current runtime generation changed".into());
        }

        // The model packet is retained only as an explicitly untrusted
        // diagnostic. Every identity, finding and verification field consumed
        // by the terminal UI comes from the immutable Host result/receipt.
        let partial_scope = !result_record
            .verification_scope
            .unverified_target_ids
            .is_empty()
            || !result_record
                .verification_scope
                .unverified_scenario_ids
                .is_empty();
        let host_verification = json!({
            "status": if partial_scope {
                "validated_current_device"
            } else {
                "validated"
            },
            "receipt": receipt,
            "result": result_record,
        });
        let canonical_result = json!({
            "ok": true,
            "checked": true,
            "host_checked": true,
            "status": if partial_scope { "verified_current_device" } else { "verified" },
            "app_id": result_record.identity.app_id,
            "workflow_run_id": result_record.identity.workflow_run_id,
            "qa_handle": result_record.identity.qa_handle,
            "build_id": result_record.identity.build_id,
            "runtime_profile": result_record.identity.runtime_profile,
            "verification_strategy": result_record.identity.verification_strategy,
            "dependency_snapshot_sha256": result_record.identity.dependency_snapshot_sha256,
            "authoring_contract_sha256": result_record.identity.authoring_contract_sha256,
            "manifest_revision": result_record.identity.manifest_revision,
            "runtime_generation": result_record.identity.runtime_generation,
            "findings": result_record.findings,
            "verification_scope": result_record.verification_scope,
            "unverified_target_ids": result_record.verification_scope.unverified_target_ids,
            "unverified_scenario_ids": result_record.verification_scope.unverified_scenario_ids,
            "summary": if partial_scope {
                "Host QA passed on the current device; remaining authoring targets/scenarios are unverified."
            } else {
                "Host QA passed for the complete authoring target matrix."
            },
            "verification": host_verification,
            "host_verification": host_verification,
            "workflow_result_diagnostic": result,
        });
        Ok(PreparedWorkflowQaPublication {
            canonical_result,
            layout,
            result_id: receipt.result_id.clone(),
            result_sha256: receipt.result_sha256.clone(),
            identity_sha256: receipt.identity_sha256.clone(),
            receipt,
            runtime_publication_cell,
        })
    }

    /// Commit a prepared candidate after its canonical terminal JSON has been
    /// written to the task spool.  This path is deliberately bounded and
    /// local: it only rechecks immutable receipt/result bytes, publishes the
    /// core marker, and atomically advances the active receipt pointer.
    pub(crate) fn commit_prepared_workflow_qa_publication(
        &self,
        prepared: &PreparedWorkflowQaPublication,
    ) -> Result<(), String> {
        let _perf = LocalAppPerfDiagnosticTimer::start("qa_publish");
        let receipt = local_apps::load_qa_receipt(&prepared.layout, prepared.receipt_id())
            .map_err(|error| format!("qa_terminal_rejected: receipt changed: {error}"))?;
        if receipt != prepared.receipt {
            return Err("qa_terminal_rejected: prepared receipt changed before commit".into());
        }
        let result = local_apps::load_qa_result(&prepared.layout, prepared.result_id())
            .map_err(|error| format!("qa_terminal_rejected: result changed: {error}"))?;
        if result.result_sha256 != prepared.result_sha256
            || result
                .identity
                .sha256()
                .map_err(|error| error.to_string())?
                != prepared.identity_sha256
        {
            return Err("qa_terminal_rejected: prepared result changed before commit".into());
        }
        let active_build = crate::mobile::local_apps_build::active_build_id(&prepared.layout)
            .map_err(|error| error.to_string())?;
        if active_build.as_deref() != Some(result.identity.build_id.as_str()) {
            return Err("qa_terminal_rejected: active build changed before commit".into());
        }
        let active_contract =
            crate::mobile::local_apps_build::active_build_authoring_contract_sha256(
                &prepared.layout,
            )
            .map_err(|error| error.to_string())?;
        if active_contract.as_deref() != Some(result.identity.authoring_contract_sha256.as_str()) {
            return Err("qa_terminal_rejected: active contract changed before commit".into());
        }
        let manifest = load_manifest(&prepared.layout).map_err(|error| error.to_string())?;
        if manifest.revision != result.identity.manifest_revision
            || manifest.runtime_profile.as_ref() != Some(&result.identity.runtime_profile)
            || manifest
                .dependency_snapshot_hash()
                .map_err(|error| error.to_string())?
                != result.identity.dependency_snapshot_sha256
        {
            return Err(
                "qa_terminal_rejected: manifest or dependencies changed before commit".into(),
            );
        }
        let expected_runtime = RuntimePublicationIdentity {
            generation: result.identity.runtime_generation,
            build_id: result.identity.build_id.clone(),
        };
        // Held through both publication writes. Runtime stop/restart updates
        // this same per-app cell while holding the runtime map, so a transition
        // either completes before this comparison and is rejected, or begins
        // only after the active receipt pointer is durable.
        let runtime_identity = prepared
            .runtime_publication_cell
            .read()
            .map_err(|_| "qa_terminal_rejected: runtime publication identity is poisoned")?;
        if runtime_identity.as_ref() != Some(&expected_runtime) {
            return Err("qa_terminal_rejected: runtime generation changed before commit".into());
        }
        local_apps::publish_qa_result(&prepared.layout, prepared.result_id())
            .map_err(|error| format!("qa_terminal_rejected: publish failed: {error}"))?;
        persist_active_qa_receipt(&prepared.layout, prepared.receipt_id())
    }

    fn checked_qa_ui_verification_summary(
        &self,
        app_id: &str,
    ) -> Result<Option<LocalAppVerificationSummaryDto>, String> {
        let layout = self.layout(app_id)?;
        let pointer_path = qa_active_receipt_path(&layout);
        let pointer =
            match rooted_fs::read_to_string_limited(layout.root(), &pointer_path, 16 * 1024) {
                Ok(pointer) => pointer,
                Err(rooted_fs::FsError::NotFound(_)) => return Ok(None),
                Err(error) => return Err(format!("read active QA receipt pointer: {error}")),
            };
        let pointer: QaActiveReceiptPointer = serde_json::from_str(&pointer)
            .map_err(|error| format!("active QA receipt pointer is corrupt: {error}"))?;
        token(&pointer.receipt_id, "receipt_id")?;
        let receipt = local_apps::load_qa_receipt(&layout, &pointer.receipt_id)
            .map_err(|error| format!("active QA receipt is corrupt: {error}"))?;
        let result = local_apps::load_qa_result(&layout, &receipt.result_id)
            .map_err(|error| format!("active QA result is corrupt: {error}"))?;
        let marker = load_published_marker(&layout, &receipt.result_id)?;
        let identity_sha256 = result
            .identity
            .sha256()
            .map_err(|error| error.to_string())?;
        if receipt.app_id != app_id
            || receipt.workflow_run_id != result.identity.workflow_run_id
            || receipt.qa_handle != result.identity.qa_handle
            || receipt.result_sha256 != result.result_sha256
            || receipt.identity_sha256 != identity_sha256
            || marker.result_id != receipt.result_id
            || marker.result_sha256 != result.result_sha256
            || marker.identity_sha256 != identity_sha256
            || !result_is_passing(&result)
        {
            return Err("active QA publication receipt/result binding is corrupt".into());
        }
        let active_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?;
        let Some(active_build_id) = active_build_id else {
            return Ok(None);
        };
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let dependency_sha256 = manifest
            .dependency_snapshot_hash()
            .map_err(|error| error.to_string())?;
        let authoring_sha256 =
            crate::mobile::local_apps_build::active_build_authoring_contract_sha256(&layout)
                .map_err(|error| error.to_string())?;
        if active_build_id != result.identity.build_id
            || authoring_sha256.as_deref()
                != Some(result.identity.authoring_contract_sha256.as_str())
            || manifest.revision != result.identity.manifest_revision
            || manifest.runtime_profile.as_ref() != Some(&result.identity.runtime_profile)
            || dependency_sha256 != result.identity.dependency_snapshot_sha256
        {
            // The immutable published result remains valid history for its old
            // build, but it is not proof for the app's current active identity.
            return Ok(None);
        }
        if !result.verification_scope.unverified_target_ids.is_empty()
            || !result.verification_scope.unverified_scenario_ids.is_empty()
        {
            return Ok(Some(partial_ui_summary(&result.verification_scope)));
        }
        Ok(Some(LocalAppVerificationSummaryDto {
            status: LocalAppVerificationStatusDto::Passed,
            summary: "Host UI/data verification evidence passed.".into(),
            code: Some("ui_verification_passed".into()),
        }))
    }

    pub(crate) async fn qa_ui_verification_summary(
        &self,
        app_id: &str,
    ) -> LocalAppVerificationSummaryDto {
        match self.checked_qa_ui_verification_summary(app_id) {
            Ok(Some(summary)) => summary,
            Ok(None) => unverified_ui_summary(),
            Err(error) => failed_ui_summary(error),
        }
    }

    /// Emit the current verification identity for exactly one app.
    ///
    /// Builds and QA publication both use this path so a client cannot retain
    /// a Passed summary after the active build receipt changes. Notification
    /// is deliberately best-effort: it follows the durable build/publication
    /// commit and cannot roll that commit back.
    pub(crate) async fn emit_current_verification_summary(
        &self,
        app_id: &str,
        layout: &local_apps::AppLayout,
    ) {
        let ui_verification = self.qa_ui_verification_summary(app_id).await;
        let summary = (|| -> Result<_, String> {
            let manifest = load_manifest(layout).map_err(|error| error.to_string())?;
            let active_build_id = crate::mobile::local_apps_build::active_build_id(layout)
                .map_err(|error| error.to_string())?;
            let publication = local_apps::derive_publication_state(
                &manifest,
                active_build_id.as_deref(),
                ui_verification.status == LocalAppVerificationStatusDto::Passed,
            )
            .map_err(|error| error.to_string())?;
            let publication_state = match publication {
                local_apps::AppPublicationState::Draft => AppWorkflowStateDto::Draft,
                local_apps::AppPublicationState::PublishedUnverified => {
                    AppWorkflowStateDto::PublishedUnverified
                }
                local_apps::AppPublicationState::PublishedVerified => {
                    AppWorkflowStateDto::PublishedVerified
                }
            };
            let mcp_verification = if let Some(active) = manifest.active_mcp_catalog.as_ref() {
                let catalog = local_apps::load_mcp_catalog(layout, &active.catalog_sha256)
                    .map_err(|error| error.to_string())?;
                if catalog.get("appId").and_then(Value::as_str) != Some(app_id)
                    || catalog.get("buildId").and_then(Value::as_str)
                        != Some(active.build_id.as_str())
                {
                    LocalAppVerificationSummaryDto {
                        status: LocalAppVerificationStatusDto::Failed,
                        summary: "The approved MCP catalog does not match this app and build."
                            .into(),
                        code: Some("active_state_corrupt".into()),
                    }
                } else if active_build_id.as_deref() != Some(active.build_id.as_str()) {
                    LocalAppVerificationSummaryDto {
                        status: LocalAppVerificationStatusDto::Unverified,
                        summary: "The approved MCP catalog no longer matches the active build and must be revalidated.".into(),
                        code: Some("needs_revalidation".into()),
                    }
                } else {
                    LocalAppVerificationSummaryDto {
                        status: LocalAppVerificationStatusDto::Passed,
                        summary: "MCP schema, Flow, call and isolation verification passed.".into(),
                        code: None,
                    }
                }
            } else {
                LocalAppVerificationSummaryDto {
                    status: LocalAppVerificationStatusDto::Unverified,
                    summary: "No approved Local App MCP catalog is active yet.".into(),
                    code: Some("needs_setup".into()),
                }
            };
            Ok((publication_state, mcp_verification))
        })();
        match summary {
            Ok((publication_state, mcp_verification)) => {
                self.event_sink
                    .emit(ClientEvent::AppEvent {
                        event: AppEventDto::VerificationSummaryChanged {
                            app_id: app_id.to_string(),
                            publication_state,
                            mcp_verification,
                            ui_verification,
                        },
                    })
                    .await;
            }
            Err(error) => {
                tracing::warn!(app_id = %app_id, %error, "failed to refresh Local App verification summary");
            }
        }
    }

    /// Emit the one app whose UI verification just committed, then reclaim
    /// raw evidence and bounded durable history. The terminal registry lock is
    /// already released before this async best-effort notification is called.
    pub(crate) async fn emit_committed_workflow_qa_summary(
        &self,
        prepared: &PreparedWorkflowQaPublication,
    ) {
        let app_id = prepared.app_id();
        self.emit_current_verification_summary(app_id, &prepared.layout)
            .await;
        if let Err(error) = self.cleanup_workflow_qa_run(app_id, &prepared.receipt.workflow_run_id)
        {
            tracing::warn!(app_id = %app_id, %error, "failed to clean terminal QA raw evidence");
        }
        if let Err(error) = local_apps::prune_qa_history(
            &prepared.layout,
            std::slice::from_ref(&prepared.result_id),
        ) {
            tracing::warn!(app_id = %app_id, %error, "failed to prune terminal QA history");
        }
    }

    /// Clean every in-flight QA session for one authenticated workflow run.
    /// Core removes only temporary session/artifact state; immutable results,
    /// receipts, and the active publication marker remain durable history.
    pub(crate) fn cleanup_workflow_qa_run(
        &self,
        app_id: &str,
        workflow_run_id: &str,
    ) -> Result<usize, String> {
        let layout = self.layout(app_id)?;
        local_apps::qa_cleanup_run(&layout, workflow_run_id).map_err(|error| error.to_string())
    }

    /// Rebind a cleanup request to the authenticated task scope before
    /// deleting a failed/cancelled run's raw evidence.
    pub(crate) fn cleanup_workflow_qa_session(
        &self,
        app_id: &str,
        workflow_run_id: &str,
        qa_handle: &str,
    ) -> Result<(), String> {
        let layout = self.layout(app_id)?;
        let session =
            local_apps::load_qa_session(&layout, qa_handle).map_err(|error| error.to_string())?;
        if session.identity.app_id != app_id
            || session.identity.workflow_run_id != workflow_run_id
            || session.identity.qa_handle != qa_handle
        {
            return Err(
                "qa_cleanup_rejected: session does not match authenticated task scope".into(),
            );
        }
        local_apps::qa_cleanup_session(&layout, qa_handle).map_err(|error| error.to_string())?;
        let retained = match rooted_fs::read_to_string_limited(
            layout.root(),
            &qa_active_receipt_path(&layout),
            16 * 1024,
        ) {
            Ok(pointer) => {
                let pointer: QaActiveReceiptPointer = serde_json::from_str(&pointer)
                    .map_err(|error| format!("active QA receipt pointer is corrupt: {error}"))?;
                let receipt = local_apps::load_qa_receipt(&layout, &pointer.receipt_id)
                    .map_err(|error| error.to_string())?;
                vec![receipt.result_id]
            }
            Err(rooted_fs::FsError::NotFound(_)) => Vec::new(),
            Err(error) => return Err(format!("read active QA receipt pointer: {error}")),
        };
        local_apps::prune_qa_history(&layout, &retained)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

fn unverified_ui_summary() -> LocalAppVerificationSummaryDto {
    LocalAppVerificationSummaryDto {
        status: LocalAppVerificationStatusDto::Unverified,
        summary: "UI verification has not completed on the active app.".into(),
        code: Some("ui_verification_required".into()),
    }
}

fn partial_ui_summary(scope: &local_apps::QaVerificationScope) -> LocalAppVerificationSummaryDto {
    let mut surfaces = Vec::new();
    if !scope.unverified_target_ids.is_empty() {
        surfaces.push(format!(
            "targets {}",
            scope.unverified_target_ids.join(", ")
        ));
    }
    if !scope.unverified_scenario_ids.is_empty() {
        surfaces.push(format!(
            "scenarios {}",
            scope.unverified_scenario_ids.join(", ")
        ));
    }
    LocalAppVerificationSummaryDto {
        status: LocalAppVerificationStatusDto::Unverified,
        summary: format!(
            "Host UI/data verification passed on the current device; {} remain unverified.",
            surfaces.join(" and ")
        ),
        code: Some("ui_verification_partial".into()),
    }
}

fn failed_ui_summary(error: String) -> LocalAppVerificationSummaryDto {
    LocalAppVerificationSummaryDto {
        status: LocalAppVerificationStatusDto::Failed,
        summary: format!("Stored UI verification evidence is corrupt: {error}"),
        code: Some("ui_verification_corrupt".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_findings_are_canonicalized_as_blocking_by_default() {
        let findings = normalize_qa_findings(json!([
            {"kind": "data", "severity": "blocking", "evidence": "wrong collection"},
            {"kind": "note", "severity": "warning", "evidence": "slow"}
        ]))
        .expect("findings normalize");
        assert_eq!(findings.len(), 2);
        assert!(findings[0].blocking);
        assert!(!findings[1].blocking);
        assert_eq!(findings[0].message, "wrong collection");
    }

    #[test]
    fn strict_findings_preserve_host_resolution_links() {
        let findings = normalize_qa_findings(json!([
            {
                "id": "finding-1",
                "message": "resolved",
                "blocking": false,
                "resolved_by_evidence_ids": ["evidence-1"]
            }
        ]))
        .expect("findings normalize");
        assert_eq!(findings[0].resolved_by_evidence_ids, vec!["evidence-1"]);
        assert!(!findings[0].blocking);
    }

    #[test]
    fn upstream_findings_project_the_complete_finding_contract() {
        let failure = local_apps::QaUpstreamFailure {
            id: "build-failed".into(),
            message: "the Host build failed".into(),
            introduced_at_ms: 7,
        };
        let projected = upstream_finding_projection(&failure);
        assert_eq!(
            projected,
            local_apps::QaFinding {
                id: "build-failed".into(),
                message: "the Host build failed".into(),
                blocking: true,
                resolved_by_evidence_ids: Vec::new(),
            }
        );
        assert_eq!(
            serde_json::to_value(projected).expect("serialize typed Host finding projection"),
            json!({
                "id": "build-failed",
                "message": "the Host build failed",
                "blocking": true,
                "resolved_by_evidence_ids": [],
            })
        );
    }

    #[test]
    fn query_causality_checks_each_write_collection_and_revision() {
        let earlier_write = json!({
            "results": [{
                "collection": "chores",
                "recordId": "row-a",
                "revision": 1,
                "deleted": false
            }]
        });
        let later_nonmatching_write = json!({
            "results": [{
                "collection": "notes",
                "recordId": "row-b",
                "revision": 2,
                "deleted": false
            }]
        });
        let matching_query = json!({
            "records": [{"collection": "chores", "recordId": "row-a", "revision": 1}]
        });
        assert!(qa_query_matches_bridge_write_content(
            &earlier_write,
            &matching_query,
            "chores",
        ));
        assert!(!qa_query_matches_bridge_write_content(
            &later_nonmatching_write,
            &matching_query,
            "chores",
        ));
        assert!(!qa_query_matches_bridge_write_content(
            &earlier_write,
            &json!({
                "records": [{"collection": "chores", "recordId": "row-a", "revision": 9}]
            }),
            "chores",
        ));
    }

    #[test]
    fn duplicate_nested_findings_deduplicate_but_conflicts_fail_closed() {
        let duplicate = json!({
            "id": "source:broken-submit",
            "message": "submit stays disabled",
            "blocking": true,
            "resolved_by_evidence_ids": [],
        });
        let findings = normalize_qa_findings(json!([duplicate.clone(), duplicate]))
            .expect("identical top-level and nested findings deduplicate");
        assert_eq!(findings.len(), 1);
        assert!(findings[0].blocking);

        let error = normalize_qa_findings(json!([
            {
                "id": "source:broken-submit",
                "message": "submit stays disabled",
                "blocking": true,
                "resolved_by_evidence_ids": [],
            },
            {
                "id": "source:broken-submit",
                "message": "rewritten as harmless",
                "blocking": false,
                "resolved_by_evidence_ids": [],
            }
        ]))
        .expect_err("a conflicting duplicate must not erase a blocker");
        assert!(error.contains("conflicting duplicate QA finding"));
    }

    #[test]
    fn authenticated_strategy_defaults_balanced_and_never_reads_quality_level() {
        assert_eq!(
            verification_strategy(&json!({"quality_level": "thorough"})).unwrap(),
            local_apps::QaVerificationStrategy::Balanced
        );
        assert_eq!(
            verification_strategy(&json!({"verification_strategy": "thorough"})).unwrap(),
            local_apps::QaVerificationStrategy::Thorough
        );
        assert!(verification_strategy(&json!({
            "verification_strategy": "unknown"
        }))
        .is_err());
    }

    #[test]
    fn qa_scope_preserves_declared_matrix_and_marks_other_platforms_unverified() {
        let mut fixture: Value = serde_json::from_str(include_str!(
            "../../../../local-apps/tests/fixtures/authoring-spec.valid-null-canvas.json"
        ))
        .expect("authoring fixture");
        fixture["targets"]
            .as_array_mut()
            .expect("targets")
            .push(json!({"id": "android-tablet", "os": "android", "form_factor": "tablet"}));
        fixture["design"]["presentations"]
            .as_array_mut()
            .expect("presentations")
            .push(json!({
                "target_id": "android-tablet",
                "presentation": "tablet list",
                "navigation": "split",
            }));
        fixture["acceptance_checks"][0]["target_ids"]
            .as_array_mut()
            .expect("acceptance targets")
            .push(Value::String("android-tablet".into()));
        fixture["acceptance_checks"]
            .as_array_mut()
            .expect("acceptance checks")
            .push(json!({
                "id": "android-only",
                "target_ids": ["android-tablet"],
                "required": true,
                "preconditions": [],
                "steps": ["open tablet view"],
                "expected": "tablet view appears",
                "evidence": ["inspect"],
            }));
        let spec: local_apps::AppAuthoringSpec =
            serde_json::from_value(fixture).expect("expanded authoring fixture");
        let contract = local_apps::AppAuthoringContract {
            version: local_apps::AUTHORING_SCHEMA_VERSION,
            revision: 1,
            app_id: "scope-test".into(),
            runtime_profile: local_apps::AppRuntimeProfileBinding {
                family: local_apps::AppRuntimeProfile::ReactDom,
                revision: 1,
                contract_sha256: "c".repeat(64),
            },
            spec,
        };
        let (scope, requirements) = qa_scope_for_contract(
            &contract,
            &local_apps::DeviceContext {
                os: "ios".into(),
                form_factor: "iphone".into(),
            },
        )
        .expect("derive current-device scope");
        assert_eq!(scope.declared_target_ids, vec!["primary", "android-tablet"]);
        assert_eq!(scope.in_scope_target_ids, vec!["primary"]);
        assert_eq!(scope.unverified_target_ids, vec!["android-tablet"]);
        assert_eq!(scope.unverified_scenario_ids, vec!["android-only"]);
        assert_eq!(
            requirements[0].target_ids,
            vec!["primary", "android-tablet"]
        );
        let unavailable = qa_scope_for_contract(
            &contract,
            &local_apps::DeviceContext {
                os: "android".into(),
                form_factor: "phone".into(),
            },
        )
        .expect_err("a device without a declared target must be unavailable");
        assert!(
            unavailable.contains("qa_begin_unavailable"),
            "{unavailable}"
        );
    }

    #[test]
    fn passing_requires_a_non_empty_all_pass_result() {
        assert!(!result_is_passing(&local_apps::QaResult {
            schema_version: local_apps::QA_SCHEMA_VERSION,
            status: local_apps::QaResultStatus::Candidate,
            identity: local_apps::QaIdentity {
                app_id: "app-test".into(),
                workflow_run_id: "run-test".into(),
                qa_handle: "qa-test".into(),
                build_id: "build-test".into(),
                runtime_profile: local_apps::AppRuntimeProfileBinding {
                    family: local_apps::AppRuntimeProfile::ReactDom,
                    revision: 1,
                    contract_sha256: "c".repeat(64),
                },
                verification_strategy: local_apps::QaVerificationStrategy::Balanced,
                dependency_snapshot_sha256: "d".repeat(64),
                authoring_contract_sha256: "e".repeat(64),
                manifest_revision: 1,
                runtime_generation: 1,
            },
            scenario_judgements: Vec::new(),
            findings: Vec::new(),
            evidence: Vec::new(),
            verification_scope: local_apps::QaVerificationScope::default(),
            previous_result_id: None,
            finalized_at_ms: 1,
            result_sha256: "f".repeat(64),
        }));
    }

    #[test]
    fn qa_runtime_identity_separates_startup_and_loaded_urls() {
        assert_eq!(
            qa_expected_runtime_url_identity(
                &Url::parse("http://127.0.0.1:20001/?lingxi_runtime=7").unwrap()
            )
            .unwrap(),
            ("127.0.0.1".into(), 20_001, "7".into())
        );
        for invalid in [
            "https://127.0.0.1:20001/?lingxi_runtime=7",
            "http://example.com:20001/?lingxi_runtime=7",
            "http://127.0.0.1:20001/route?lingxi_runtime=7",
            "http://127.0.0.1:20001/?lingxi_runtime=7&lingxi_runtime=8",
            "http://127.0.0.1:20001/?lingxi_runtime=seven",
            "http://user@127.0.0.1:20001/?lingxi_runtime=7",
        ] {
            assert!(
                qa_expected_runtime_url_identity(&Url::parse(invalid).unwrap()).is_err(),
                "accepted invalid QA runtime URL {invalid}"
            );
        }
        assert_eq!(
            qa_loaded_runtime_url_identity(
                &Url::parse(
                    "http://127.0.0.1:20001/business/path?tab=todo&lingxi_runtime=7#row-1",
                )
                .unwrap(),
            )
            .unwrap(),
            ("127.0.0.1".into(), 20_001, "7".into())
        );
        for invalid in [
            "http://127.0.0.1:20001/business/path?tab=todo",
            "http://127.0.0.1:20001/business/path?lingxi_runtime=7&lingxi_runtime=8",
            "http://127.0.0.1:20001/business/path?lingxi_runtime=nope",
            "https://127.0.0.1:20001/business/path?lingxi_runtime=7",
            "http://example.com:20001/business/path?lingxi_runtime=7",
        ] {
            assert!(
                qa_loaded_runtime_url_identity(&Url::parse(invalid).unwrap()).is_err(),
                "accepted invalid loaded QA runtime URL {invalid}"
            );
        }
    }

    #[test]
    fn untrusted_result_cannot_inject_host_evidence_ids() {
        let result = sanitize_untrusted_qa_result(json!({
            "image": "app data",
            "qa_handle": "qa_attacker",
            "qa_evidence_ids": ["evidence-attacker"],
        }));
        let result = qa_result_with_evidence_ids(result, "qa_host", ["evidence-host".to_string()]);
        assert_eq!(result["qa_handle"], "qa_host");
        assert_eq!(result["qa_evidence_ids"], json!(["evidence-host"]));
    }

    #[test]
    fn scalar_qa_result_still_returns_host_evidence_metadata() {
        let result =
            qa_result_with_evidence_ids(Value::Bool(true), "qa_host", ["evidence-host".into()]);
        assert_eq!(result["result"], true);
        assert_eq!(result["qa_handle"], "qa_host");
        assert_eq!(result["qa_evidence_ids"], json!(["evidence-host"]));
    }
}
