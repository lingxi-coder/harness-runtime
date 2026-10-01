//! Host-owned verified Local App template catalog and create candidates.
//!
//! The plugin archive is the only source for the semantic template view, and a
//! host hands it over as a [`PluginBundle`]: the bytes of its verified catalog
//! and the digest of the bundle they belong to.  This module deliberately keeps
//! the sensitive profile/family/path/digest data in the host: selector agents
//! receive a redacted catalog, while later stages resolve an opaque,
//! run-scoped handle through the durable journal.

use crate::runtime_profiles::contract_for_binding;
use local_apps::{AppRuntimeProfile, AppRuntimeProfileBinding, AppSurface};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The verified plugin bundle the template catalog is read from.
///
/// The host verified the bundle when it materialized it; the service trusts the
/// catalog bytes and records the digest in every selection it journals, so a
/// selection made against one bundle is refused against another.
pub trait PluginBundle: Send + Sync {
    /// The bundle's verified template catalog (`assets/templates/catalog.json`).
    fn catalog_bytes(&self) -> &[u8];

    /// The digest of the whole bundle the catalog came from.
    fn bundle_sha256(&self) -> &str;
}

const JOURNAL_DIR: &str = ".lingxi-build-state/template-candidates";
const HANDLE_PREFIX: &str = "vsel_";
const SELECTOR_CAPABILITY_FILE: &str = "selector-capability";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogFile {
    schema_version: u32,
    toolchain_key: String,
    templates: Vec<CatalogTemplate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogTemplate {
    template_id: String,
    family: AppRuntimeProfile,
    revision: u32,
    surface: AppSurface,
    summary: String,
    recommended_for: Vec<String>,
    not_for: Vec<String>,
    contract_sha256: String,
    inventory_sha256: String,
    #[serde(default)]
    mcp_default_enabled: bool,
    #[serde(default)]
    mcp_suggestions: Vec<String>,
    available: bool,
}

/// The deliberately redacted semantic catalog exposed to selector agents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TemplateCatalogView {
    pub catalog_digest: String,
    pub templates: Vec<TemplateCatalogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TemplateCatalogEntry {
    #[serde(rename = "templateId")]
    pub template_id: String,
    pub surface: AppSurface,
    pub summary: String,
    #[serde(rename = "recommendedFor")]
    pub recommended_for: Vec<String>,
    #[serde(rename = "notFor")]
    pub not_for: Vec<String>,
    #[serde(rename = "mcpDefaultEnabled")]
    pub mcp_default_enabled: bool,
    #[serde(rename = "mcpSuggestions")]
    pub mcp_suggestions: Vec<String>,
}

/// Host-verified selection returned by `resolve_template_selection`.  This is
/// never accepted from model input and is only materialized after the journal
/// row has been checked against the app, workflow run and current catalog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ValidatedTemplateSelection {
    pub app_id: String,
    pub workflow_run_id: String,
    pub plugin_name: String,
    pub plugin_bundle_sha256: String,
    pub catalog_digest: String,
    pub template_id: String,
    pub template_sha256: String,
    pub runtime_profile: AppRuntimeProfileBinding,
    pub template_inventory_sha256: String,
    pub surface: AppSurface,
    pub reason: String,
    pub rejected: Vec<RejectedCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RejectedCandidate {
    pub template_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CandidateJournalRow {
    handle: String,
    selection: ValidatedTemplateSelection,
}

fn parse_catalog(bundle: &dyn PluginBundle) -> Result<(CatalogFile, String), String> {
    let catalog: CatalogFile = serde_json::from_slice(bundle.catalog_bytes())
        .map_err(|error| format!("verified Local App catalog is malformed: {error}"))?;
    if !matches!(catalog.schema_version, 1 | 2) || catalog.toolchain_key.trim().is_empty() {
        return Err("verified Local App catalog has an unsupported schema or toolchain".into());
    }
    if catalog.templates.is_empty() {
        return Err("verified Local App catalog contains no templates".into());
    }
    let canonical = serde_json::to_vec(&catalog)
        .map_err(|error| format!("serialize verified Local App catalog: {error}"))?;
    Ok((catalog, format!("{:x}", Sha256::digest(canonical))))
}

pub fn catalog_view(bundle: &dyn PluginBundle) -> Result<TemplateCatalogView, String> {
    let (catalog, catalog_digest) = parse_catalog(bundle)?;
    let templates = catalog
        .templates
        .into_iter()
        .filter(|template| template.available)
        .map(|template| TemplateCatalogEntry {
            template_id: template.template_id,
            surface: template.surface,
            summary: template.summary,
            recommended_for: template.recommended_for,
            not_for: template.not_for,
            mcp_default_enabled: template.mcp_default_enabled,
            mcp_suggestions: template.mcp_suggestions,
        })
        .collect();
    Ok(TemplateCatalogView {
        catalog_digest,
        templates,
    })
}

fn journal_path(root: &Path, app_id: &str, workflow_run_id: &str) -> PathBuf {
    root.join(JOURNAL_DIR)
        .join(app_id)
        .join(workflow_run_id)
        .join("validated-selection.json")
}

fn selector_capability_path(root: &Path, app_id: &str, workflow_run_id: &str) -> PathBuf {
    journal_path(root, app_id, workflow_run_id)
        .parent()
        .expect("candidate journal always has a parent")
        .join(SELECTOR_CAPABILITY_FILE)
}

/// Mint one unguessable, single-use selector capability for a Host-launched
/// create run. It is persisted beside the candidate journal so a process
/// restart cannot turn a leaked/stale selector token into a reusable one.
pub fn issue_selector_capability(
    root: &Path,
    app_id: &str,
    workflow_run_id: &str,
) -> Result<String, String> {
    issue_selector_capability_impl(root, app_id, workflow_run_id, false)
}

/// Same as [`issue_selector_capability`], but for a Host-VERIFIED resume
/// (`trusted_local_app_resume`, resolved from checkpoint provenance, never
/// from caller-supplied args) of an interrupted create run.
///
/// r1-backlog-workflow-runtime-04 / r1-workflow-runtime-06: the plain
/// function fails closed with `selector_capability_exists` whenever the
/// LIVE (never-consumed) token from a prior attempt is still on disk --
/// which is exactly the case for a create that died inside the selector
/// before it could journal a candidate and mark the token `.used`. That
/// fail-closed property is exactly right against a genuine duplicate
/// launch (a second concurrent call for the same run id from an
/// UNverified caller), so this rotation path exists only for the one
/// caller that has already proven the resume is legitimate.
pub fn issue_selector_capability_for_verified_resume(
    root: &Path,
    app_id: &str,
    workflow_run_id: &str,
) -> Result<String, String> {
    issue_selector_capability_impl(root, app_id, workflow_run_id, true)
}

fn issue_selector_capability_impl(
    root: &Path,
    app_id: &str,
    workflow_run_id: &str,
    allow_live_rotation: bool,
) -> Result<String, String> {
    safe_segment(app_id, "app_id")?;
    safe_segment(workflow_run_id, "workflow_run_id")?;
    let path = selector_capability_path(root, app_id, workflow_run_id);
    let used_path = path.with_file_name(format!("{SELECTOR_CAPABILITY_FILE}.used"));
    if path.exists() {
        if !allow_live_rotation {
            return Err(
                "selector_capability_exists: create run already has a selector capability".into(),
            );
        }
        // Host-verified resume of the interrupted run: the live token is
        // exactly what an unfinished selector attempt left behind. Rotate
        // it the same way the `.used` branch below rotates a consumed one.
        std::fs::remove_file(&path)
            .map_err(|error| format!("rotate live selector capability: {error}"))?;
    }
    // A resumed run may already have consumed its first selector capability
    // after journaling a candidate. Re-issue a fresh one for an interrupted
    // selector retry, while retaining the durable candidate row as the
    // recovery record. A direct duplicate launch cannot reach this branch
    // because its run id is reserved by the Workflow launcher.
    if used_path.exists() {
        std::fs::remove_file(&used_path)
            .map_err(|error| format!("rotate selector capability: {error}"))?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create selector capability: {error}"))?;
    }
    let token = format!("sel_{}", uuid::Uuid::new_v4().simple());
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, &token).map_err(|error| format!("write selector capability: {error}"))?;
    std::fs::rename(&temp, &path)
        .map_err(|error| format!("commit selector capability: {error}"))?;
    Ok(token)
}

fn safe_segment(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value
            .bytes()
            .any(|byte| !byte.is_ascii_alphanumeric() && !b"_-".contains(&byte))
    {
        return Err(format!("invalid {field}; expected a safe path segment"));
    }
    Ok(())
}

fn selector_rejected(value: &Value) -> Result<Vec<RejectedCandidate>, String> {
    let Some(values) = value.as_array() else {
        return Ok(Vec::new());
    };
    let mut rejected = Vec::with_capacity(values.len().min(16));
    for candidate in values.iter().take(16) {
        let object = candidate
            .as_object()
            .ok_or_else(|| "rejected entries must be objects".to_string())?;
        let template_id = object
            .get("template_id")
            .or_else(|| object.get("templateId"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "rejected.template_id must be a non-empty string".to_string())?;
        let reason = object
            .get("reason")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "rejected.reason must be a non-empty string".to_string())?;
        rejected.push(RejectedCandidate {
            template_id: template_id.trim().to_string(),
            reason: reason.trim().chars().take(1000).collect(),
        });
    }
    Ok(rejected)
}

pub fn validate_and_journal(
    bundle: &dyn PluginBundle,
    root: &Path,
    app_id: &str,
    workflow_run_id: &str,
    input: &Value,
) -> Result<Value, String> {
    safe_segment(app_id, "app_id")?;
    safe_segment(workflow_run_id, "workflow_run_id")?;
    let catalog = catalog_view(bundle)?;
    let capability = input
        .get("selector_capability")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            "selector_capability_required: Host-issued selector capability is missing".to_string()
        })?;
    let capability_path = selector_capability_path(root, app_id, workflow_run_id);
    let expected_capability = std::fs::read_to_string(&capability_path).map_err(|_| {
        "selector_capability_missing: no Host selector capability for this app/run".to_string()
    })?;
    if expected_capability.trim() != capability {
        return Err(
            "selector_capability_invalid: caller is not the Host-launched template selector".into(),
        );
    }
    let echoed_digest = input
        .get("catalog_digest")
        .or_else(|| input.get("catalogDigest"))
        .and_then(Value::as_str)
        .ok_or_else(|| "catalog_digest is required".to_string())?;
    if echoed_digest != catalog.catalog_digest {
        return Err(format!(
            "catalog_stale: expected {}, received {}",
            catalog.catalog_digest, echoed_digest
        ));
    }
    let template_id = input
        .get("template_id")
        .or_else(|| input.get("templateId"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "template_id is required".to_string())?;
    let reason = input
        .get("reason")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "reason is required".to_string())?;
    let rejected = selector_rejected(input.get("rejected").unwrap_or(&Value::Null))?;
    let (catalog_file, _) = parse_catalog(bundle)?;
    let template = catalog_file
        .templates
        .iter()
        .find(|candidate| candidate.template_id == template_id)
        .ok_or_else(|| format!("template_not_found: {template_id}"))?;
    if !template.available {
        return Err(format!("template_unavailable: {template_id}"));
    }
    let binding = AppRuntimeProfileBinding {
        family: template.family,
        revision: template.revision,
        contract_sha256: template.contract_sha256.clone(),
    };
    let contract = contract_for_binding(&binding)
        .map_err(|error| format!("template_contract_invalid: {error}"))?;
    if contract.surface != template.surface {
        return Err(format!("template_surface_mismatch: {template_id}"));
    }
    let handle = format!("{HANDLE_PREFIX}{}", uuid::Uuid::new_v4().simple());
    let selection = ValidatedTemplateSelection {
        app_id: app_id.to_string(),
        workflow_run_id: workflow_run_id.to_string(),
        plugin_name: "lingxi-local-app@builtin".into(),
        plugin_bundle_sha256: bundle.bundle_sha256().into(),
        catalog_digest: catalog.catalog_digest.clone(),
        template_id: template.template_id.clone(),
        template_sha256: template.contract_sha256.clone(),
        runtime_profile: binding,
        template_inventory_sha256: template.inventory_sha256.clone(),
        surface: template.surface,
        reason: reason.trim().chars().take(4000).collect(),
        rejected: rejected
            .into_iter()
            .filter(|candidate| {
                catalog
                    .templates
                    .iter()
                    .any(|template| template.template_id == candidate.template_id)
            })
            .collect(),
    };
    let path = journal_path(root, app_id, workflow_run_id);
    write_journal_row(
        &path,
        &CandidateJournalRow {
            handle: handle.clone(),
            selection: selection.clone(),
        },
    )?;
    // Consume only after the durable selection row is committed. A failed
    // proposal (including stale catalog) remains retryable with the same
    // Host-issued selector capability; a successful proposal cannot be
    // replayed by another agent or direct caller.
    let consumed = capability_path.with_file_name(format!("{SELECTOR_CAPABILITY_FILE}.used"));
    std::fs::rename(&capability_path, consumed)
        .map_err(|error| format!("consume selector capability: {error}"))?;
    Ok(json!({
        "validated_selection_handle": handle,
        "display_selection": {
            "catalog_digest": selection.catalog_digest,
            "template_id": selection.template_id,
            "surface": selection.surface,
            "reason": selection.reason,
            "rejected": selection.rejected,
        }
    }))
}

fn write_journal_row(path: &Path, row: &CandidateJournalRow) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create candidate journal: {error}"))?;
    }
    let bytes = serde_json::to_vec_pretty(row)
        .map_err(|error| format!("serialize candidate journal: {error}"))?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, bytes).map_err(|error| format!("write candidate journal: {error}"))?;
    std::fs::rename(&temp, path).map_err(|error| format!("commit candidate journal: {error}"))
}

/// Journal a template selection the HOST derived from a plan the user approved.
///
/// The plan-driven path has no Host-launched selector agent, so there is no
/// `selector_capability` to spend; the authority is the plan approval the Host
/// observed itself ([`crate::plan_approval`]), and the caller passes the
/// template id that approval named. Everything else the journal row carries is
/// still derived HERE from the compiled catalog, so [`resolve_typed`] keeps
/// re-validating the row against the live catalog — a template that disappears
/// or changes between planning and preparation is refused there, and the caller
/// sends the user back through planning instead of silently swapping templates.
///
/// Returns the journaled row together with the opaque `vsel_` handle that
/// `local_app_contract`/`stage_create` consume (the same pair [`validate_and_journal`]
/// returns, minus the display payload).
pub fn journal_plan_selection(
    bundle: &dyn PluginBundle,
    root: &Path,
    app_id: &str,
    execution_id: &str,
    template_id: &str,
    reason: &str,
) -> Result<(String, ValidatedTemplateSelection), String> {
    safe_segment(app_id, "app_id")?;
    safe_segment(execution_id, "execution_id")?;
    let (catalog_file, catalog_digest) = parse_catalog(bundle)?;
    let template = catalog_file
        .templates
        .iter()
        .find(|candidate| candidate.template_id == template_id)
        .ok_or_else(|| {
            format!(
                "template_stale: the approved plan selected {template_id}, which is not in the \
                 current template catalog; plan again against the current catalog"
            )
        })?;
    if !template.available {
        return Err(format!(
            "template_stale: the approved plan selected {template_id}, which is not available in \
             this build; plan again against the current catalog"
        ));
    }
    let binding = AppRuntimeProfileBinding {
        family: template.family,
        revision: template.revision,
        contract_sha256: template.contract_sha256.clone(),
    };
    let contract = contract_for_binding(&binding)
        .map_err(|error| format!("template_contract_invalid: {error}"))?;
    if contract.surface != template.surface {
        return Err(format!("template_surface_mismatch: {template_id}"));
    }
    let handle = format!("{HANDLE_PREFIX}{}", uuid::Uuid::new_v4().simple());
    let selection = ValidatedTemplateSelection {
        app_id: app_id.to_string(),
        workflow_run_id: execution_id.to_string(),
        plugin_name: "lingxi-local-app@builtin".into(),
        plugin_bundle_sha256: bundle.bundle_sha256().into(),
        catalog_digest,
        template_id: template.template_id.clone(),
        template_sha256: template.contract_sha256.clone(),
        runtime_profile: binding,
        template_inventory_sha256: template.inventory_sha256.clone(),
        surface: template.surface,
        reason: reason.trim().chars().take(4000).collect(),
        rejected: Vec::new(),
    };
    write_journal_row(
        &journal_path(root, app_id, execution_id),
        &CandidateJournalRow {
            handle: handle.clone(),
            selection: selection.clone(),
        },
    )?;
    Ok((handle, selection))
}

pub fn resolve(
    bundle: &dyn PluginBundle,
    root: &Path,
    app_id: &str,
    workflow_run_id: &str,
    handle: &str,
) -> Result<Value, String> {
    serde_json::to_value(resolve_typed(
        bundle,
        root,
        app_id,
        workflow_run_id,
        handle,
    )?)
    .map_err(|error| format!("serialize selection: {error}"))
}

pub fn resolve_typed(
    bundle: &dyn PluginBundle,
    root: &Path,
    app_id: &str,
    workflow_run_id: &str,
    handle: &str,
) -> Result<ValidatedTemplateSelection, String> {
    safe_segment(app_id, "app_id")?;
    safe_segment(workflow_run_id, "workflow_run_id")?;
    if !handle.starts_with(HANDLE_PREFIX) || handle.len() != HANDLE_PREFIX.len() + 32 {
        return Err("validated_selection_invalid: malformed opaque handle".into());
    }
    let catalog = catalog_view(bundle)?;
    let path = journal_path(root, app_id, workflow_run_id);
    let bytes = std::fs::read(&path)
        .map_err(|_| "validated_selection_missing: journal row not found".to_string())?;
    let row: CandidateJournalRow = serde_json::from_slice(&bytes)
        .map_err(|error| format!("validated_selection_invalid: journal is malformed: {error}"))?;
    if row.handle != handle {
        return Err("validated_selection_invalid: forged or mismatched handle".into());
    }
    if row.selection.app_id != app_id || row.selection.workflow_run_id != workflow_run_id {
        return Err("validated_selection_invalid: app or workflow run mismatch".into());
    }
    if row.selection.catalog_digest != catalog.catalog_digest {
        return Err("catalog_stale: candidate was issued for an older catalog".into());
    }
    if row.selection.plugin_name != "lingxi-local-app@builtin" {
        return Err("validated_selection_invalid: unexpected plugin identity".into());
    }
    if row.selection.plugin_bundle_sha256 != bundle.bundle_sha256() {
        return Err("validated_selection_invalid: plugin bundle digest changed".into());
    }
    let (catalog_file, _) = parse_catalog(bundle)?;
    let template = catalog_file
        .templates
        .iter()
        .find(|candidate| candidate.template_id == row.selection.template_id)
        .ok_or_else(|| {
            format!(
                "validated_selection_invalid: template {} is no longer in the catalog",
                row.selection.template_id
            )
        })?;
    if !template.available {
        return Err("validated_selection_invalid: template is no longer available".into());
    }
    let canonical_binding = AppRuntimeProfileBinding {
        family: template.family,
        revision: template.revision,
        contract_sha256: template.contract_sha256.clone(),
    };
    let contract = contract_for_binding(&canonical_binding)
        .map_err(|error| format!("template_contract_invalid: {error}"))?;
    if contract.surface != template.surface {
        return Err(
            "validated_selection_invalid: template surface no longer matches its contract".into(),
        );
    }
    if row.selection.template_sha256 != template.contract_sha256
        || row.selection.template_inventory_sha256 != template.inventory_sha256
        || row.selection.runtime_profile != canonical_binding
        || row.selection.surface != template.surface
    {
        return Err("validated_selection_invalid: candidate journal was tampered".into());
    }
    if row.selection.rejected.iter().any(|candidate| {
        !catalog
            .templates
            .iter()
            .any(|entry| entry.template_id == candidate.template_id)
    }) {
        return Err(
            "validated_selection_invalid: rejected candidates escaped the current catalog".into(),
        );
    }
    Ok(row.selection)
}

/// Install-before-build dependency identity.  This is intentionally a digest
/// of the requested/effective/lock inputs, not the post-install tree or SBOM.
pub fn dependency_input_sha256(
    requested: &[u8],
    effective_package: &[u8],
    base_lock: &[u8],
    toolchain_key: &str,
) -> String {
    let mut hasher = Sha256::new();
    for bytes in [requested, effective_package, base_lock] {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    hasher.update(toolchain_key.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::CheckedInBundle;

    #[test]
    fn semantic_view_redacts_host_identity_and_unavailable_babylon() {
        fn assert_no_host_only_keys(value: &Value) {
            match value {
                Value::Object(object) => {
                    for forbidden in [
                        "family",
                        "revision",
                        "contractSha256",
                        "inventorySha256",
                        "path",
                    ] {
                        assert!(
                            !object.contains_key(forbidden),
                            "semantic view leaked key {forbidden}"
                        );
                    }
                    for child in object.values() {
                        assert_no_host_only_keys(child);
                    }
                }
                Value::Array(values) => {
                    for child in values {
                        assert_no_host_only_keys(child);
                    }
                }
                _ => {}
            }
        }

        let view = catalog_view(&CheckedInBundle).expect("checked-in catalog");
        assert_eq!(view.templates.len(), 4);
        let value = serde_json::to_value(&view).expect("view JSON");
        assert_no_host_only_keys(&value);
        let encoded = value.to_string();
        assert!(!encoded.contains("babylon-3d-r4"));
        assert!(encoded.contains("react-dom-r4"));
        assert!(view
            .templates
            .iter()
            .all(|template| !template.mcp_default_enabled));
        assert!(view
            .templates
            .iter()
            .all(|template| !template.mcp_suggestions.is_empty()));
    }

    #[test]
    fn selector_capability_is_single_use_and_forged_values_are_rejected() {
        let root = tempfile::tempdir().expect("temp root");
        let app_id = "aaaa1111";
        let run_id = "wf_selector1";
        let capability =
            issue_selector_capability(root.path(), app_id, run_id).expect("capability");
        let view = catalog_view(&CheckedInBundle).expect("catalog");
        let mut input = json!({
            "app_id": app_id,
            "workflow_run_id": run_id,
            "catalog_digest": view.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "ordinary form",
            "rejected": [],
            "selector_capability": capability.clone(),
        });
        input["selector_capability"] = json!("sel_00000000000000000000000000000000");
        assert!(
            validate_and_journal(&CheckedInBundle, root.path(), app_id, run_id, &input)
                .expect_err("forged capability must fail")
                .contains("selector_capability_invalid")
        );
        input["selector_capability"] = json!(capability);
        let issued = validate_and_journal(&CheckedInBundle, root.path(), app_id, run_id, &input)
            .expect("valid proposal");
        let handle = issued["validated_selection_handle"]
            .as_str()
            .expect("handle");
        assert!(resolve(&CheckedInBundle, root.path(), app_id, run_id, handle).is_ok());
        assert!(
            validate_and_journal(&CheckedInBundle, root.path(), app_id, run_id, &input)
                .expect_err("single-use capability")
                .contains("selector_capability_missing")
        );
        assert!(resolve(&CheckedInBundle, root.path(), "bbbb2222", run_id, handle).is_err());
        assert!(resolve(&CheckedInBundle, root.path(), app_id, "wf_other1", handle).is_err());
    }

    /// r1-backlog-workflow-runtime-04 / r1-workflow-runtime-06: a create run
    /// that dies inside the selector -- before it can journal a candidate
    /// and mark its token `.used` -- leaves a LIVE (never-consumed)
    /// capability on disk. The plain function must keep failing closed on
    /// that (a genuine concurrent duplicate launch must not mint a second
    /// live token), but a Host-verified resume of the SAME interrupted run
    /// must succeed by rotating it.
    #[test]
    fn issue_selector_capability_fails_closed_on_a_live_duplicate_but_rotates_for_a_verified_resume(
    ) {
        let root = tempfile::tempdir().expect("temp root");
        let app_id = "bbbb2222";
        let run_id = "wf_selector_resume";

        let first = issue_selector_capability(root.path(), app_id, run_id).expect("first mint");

        let duplicate = issue_selector_capability(root.path(), app_id, run_id);
        assert_eq!(
            duplicate,
            Err("selector_capability_exists: create run already has a selector capability"
                .to_string()),
            "an UNverified duplicate call must still fail closed on a live token, got {duplicate:?}"
        );

        let resumed = issue_selector_capability_for_verified_resume(root.path(), app_id, run_id)
            .expect("a Host-verified resume must rotate the live capability, not fail closed");
        assert_ne!(
            resumed, first,
            "the resumed capability must be a freshly rotated token, not the stale one"
        );

        // The old token must actually be GONE, not merely shadowed: a plain
        // call right after still sees exactly one live token (the new one),
        // proving the rotation replaced rather than duplicated it.
        let second_duplicate = issue_selector_capability(root.path(), app_id, run_id);
        assert_eq!(
            second_duplicate,
            Err(
                "selector_capability_exists: create run already has a selector capability"
                    .to_string()
            ),
            "the rotation must leave exactly one live token behind, got {second_duplicate:?}"
        );
    }

    #[test]
    fn tampered_candidate_journal_fails_closed() {
        let root = tempfile::tempdir().expect("temp root");
        let app_id = "aaaa1111";
        let run_id = "wf_selector2";
        let capability =
            issue_selector_capability(root.path(), app_id, run_id).expect("capability");
        let view = catalog_view(&CheckedInBundle).expect("catalog");
        let issued = validate_and_journal(
            &CheckedInBundle,
            root.path(),
            app_id,
            run_id,
            &json!({
                "app_id": app_id,
                "workflow_run_id": run_id,
                "catalog_digest": view.catalog_digest,
                "template_id": "react-dom-r4",
                "reason": "ordinary form",
                "rejected": [],
                "selector_capability": capability,
            }),
        )
        .expect("valid proposal");
        let handle = issued["validated_selection_handle"]
            .as_str()
            .expect("handle")
            .to_string();
        let path = journal_path(root.path(), app_id, run_id);
        let mut row: CandidateJournalRow =
            serde_json::from_slice(&std::fs::read(&path).expect("read journal"))
                .expect("parse journal");
        row.selection.runtime_profile.family = AppRuntimeProfile::Canvas2d;
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&row).expect("serialize tampered row"),
        )
        .expect("rewrite journal");
        let error = resolve_typed(&CheckedInBundle, root.path(), app_id, run_id, &handle)
            .expect_err("tampering must fail");
        assert!(
            error.contains("tampered"),
            "unexpected error after tampering: {error}"
        );
    }

    #[test]
    fn dependency_digest_changes_when_any_install_input_changes() {
        let baseline = dependency_input_sha256(b"{}", b"package", b"lock", "pnpm@x");
        assert_ne!(
            baseline,
            dependency_input_sha256(b"{x}", b"package", b"lock", "pnpm@x")
        );
        assert_ne!(
            baseline,
            dependency_input_sha256(b"{}", b"package2", b"lock", "pnpm@x")
        );
        assert_ne!(
            baseline,
            dependency_input_sha256(b"{}", b"package", b"lock2", "pnpm@x")
        );
        assert_ne!(
            baseline,
            dependency_input_sha256(b"{}", b"package", b"lock", "pnpm@y")
        );
    }
}
