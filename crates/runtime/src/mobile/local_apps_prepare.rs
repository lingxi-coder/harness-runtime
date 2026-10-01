//! `LocalAppPrepare` — the Host half of the plan-driven create/modify flow.
//!
//! Replaces the fixed workflow's mandatory template-selection stage, five-part
//! design stages and quality tier. The user approves a PLAN (see
//! [`crate::mobile::plan_approval`]); this operation turns that approval into the two
//! things the rest of the engine already understands:
//!
//! * CREATE — the template is landed through the EXISTING scaffold transaction
//!   (journaled selection → authoring contract → isolated create staging →
//!   one-shot receipt → `scaffold_shell_app_value`'s reservation/rollback), so
//!   nothing about how a template reaches disk is new here. The only thing the
//!   plan changes is who answers the create confirmation: the plan approval,
//!   not a second native sheet.
//! * MODIFY — the authoring contract is staged and nothing else. No template
//!   is landed, no receipt is minted, and the active build's contract is
//!   untouched, so a failed modify leaves the last successful build and its
//!   valid contract exactly as they were.
//!
//! Everything it needs from the approval arrives in `input` because the
//! transport resolves the approval itself and rebuilds the request from it —
//! see `LocalAppsMcpTransport::call_prepare`. A model that forges `name`,
//! `brief`, `spec` or `template_id` into its own call changes nothing: those
//! keys are never read from the model's object.

use super::*;
use rooted_fs::{AtomicWriteOptions, FsError};
use serde::{Deserialize, Serialize};

const MAX_STATE_BYTES: u64 = 2 * 1024 * 1024;
const OPERATION_CREATE: &str = "create";
const OPERATION_MODIFY: &str = "modify";
static PREPARE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// What one app's preparation settled on. Durable, because a repeated call
/// must return the SAME execution id and contract rather than prepare twice —
/// the spec requires that a retry never overwrites written source.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareState {
    version: u32,
    app_id: String,
    operation: String,
    /// Host-minted id that plays the `workflow_run_id` role throughout the
    /// journal, receipt, staging and build plumbing.
    execution_id: String,
    session_uuid: String,
    plan_path: String,
    plan_sha256: String,
    /// Absent for MODIFY: the app's runtime profile was fixed when it was
    /// scaffolded, and a plan may not swap it.
    template_id: Option<String>,
    name: String,
    brief: String,
    contract_handle: String,
    contract_sha256: String,
    runtime_profile: Option<local_apps::AppRuntimeProfileBinding>,
    /// Flipped only after `scaffold_shell_app_value` returned. A crash between
    /// the seeded workspace and this flip is repaired by
    /// [`committed_scaffold`] on the next call, which is why the commit proof
    /// below is written BEFORE the record says `scaffolded`.
    #[serde(default)]
    scaffolded: bool,
}

/// Commit proof for the scaffold half. Written from inside the scaffold
/// transaction (before `commit_scaffold` flips the record) so a crash after
/// the record commit is still attributable to this preparation.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareScaffoldCommit {
    app_id: String,
    execution_id: String,
    contract_sha256: String,
    runtime_profile: local_apps::AppRuntimeProfileBinding,
    name: String,
    brief: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document<T> {
    payload_sha256: String,
    payload: T,
}

fn state_path(layout: &AppLayout) -> PathBuf {
    layout.app_dir_rel().join("prepare/state.json")
}

fn commit_path(layout: &AppLayout) -> PathBuf {
    layout.app_dir_rel().join("prepare/scaffold-commit.json")
}

fn read_document<T: serde::de::DeserializeOwned + Serialize>(
    layout: &AppLayout,
    path: &Path,
) -> Result<Option<T>, String> {
    let bytes = match rooted_fs::read_to_string_limited(layout.root(), path, MAX_STATE_BYTES) {
        Ok(bytes) => bytes,
        Err(FsError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(format!("prepare_state_invalid: {error}")),
    };
    let document: Document<T> =
        serde_json::from_str(&bytes).map_err(|error| format!("prepare_state_invalid: {error}"))?;
    let digest = local_apps::canonical_sha256(&document.payload, "prepare state")
        .map_err(|error| error.to_string())?;
    if digest != document.payload_sha256 {
        return Err("prepare_state_invalid: persisted digest does not match".into());
    }
    Ok(Some(document.payload))
}

fn write_document<T: Serialize>(
    layout: &AppLayout,
    path: &Path,
    payload: &T,
) -> Result<(), String> {
    let document = Document {
        payload_sha256: local_apps::canonical_sha256(payload, "prepare state")
            .map_err(|error| error.to_string())?,
        payload,
    };
    let bytes = serde_json::to_vec(&document).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("prepare_state_invalid: state exceeds size limit".into());
    }
    rooted_fs::ensure_private_directory(layout.root(), path.parent().expect("state parent"), 0o700)
        .map_err(|error| error.to_string())?;
    rooted_fs::atomic_write(
        layout.root(),
        path,
        &bytes,
        AtomicWriteOptions {
            overwrite: true,
            create_parents: false,
            file_mode: 0o600,
            ..AtomicWriteOptions::default()
        },
    )
    .map_err(|error| format!("prepare_state_write_failed: {error}"))
}

fn read_state(layout: &AppLayout) -> Result<Option<PrepareState>, String> {
    let state: Option<PrepareState> = read_document(layout, &state_path(layout))?;
    if let Some(state) = &state {
        if state.version != 1 || state.app_id != layout.app_id() {
            return Err("prepare_state_invalid: app identity mismatch".into());
        }
    }
    Ok(state)
}

fn save_state(layout: &AppLayout, state: &PrepareState) -> Result<(), String> {
    write_document(layout, &state_path(layout), state)
}

/// The app's own mirror of the committed record plus the commit proof. Both
/// halves are required: the proof alone is an intent, the mirror alone proves
/// only that SOME scaffold committed.
fn committed_scaffold(layout: &AppLayout, state: &PrepareState) -> Result<bool, String> {
    let Some(commit): Option<PrepareScaffoldCommit> = read_document(layout, &commit_path(layout))?
    else {
        return Ok(false);
    };
    if commit.app_id != state.app_id || commit.execution_id != state.execution_id {
        return Ok(false);
    }
    if commit.contract_sha256 != state.contract_sha256 {
        return Err("prepare_state_invalid: scaffold contract mismatch".into());
    }
    let bytes = rooted_fs::read_to_string_limited(
        layout.root(),
        &local_apps::storage::metadata_rel(layout.app_id()),
        MAX_STATE_BYTES,
    )
    .map_err(|error| error.to_string())?;
    let mirror: Value = serde_json::from_str(&bytes).map_err(|error| error.to_string())?;
    if mirror["app"]["scaffolded"].as_bool() != Some(true) {
        return Ok(false);
    }
    let manifest = load_manifest(layout).map_err(|error| error.to_string())?;
    if manifest.runtime_profile.as_ref() != Some(&commit.runtime_profile)
        || mirror["app"]["name"].as_str() != Some(commit.name.as_str())
        || mirror["app"]["brief"].as_str() != Some(commit.brief.as_str())
    {
        return Err("prepare_state_invalid: scaffold proof does not match the app".into());
    }
    Ok(true)
}

fn prepared_result(state: &PrepareState, scaffolded: bool) -> Value {
    json!({
        "ok": true,
        "operation": state.operation,
        "app_id": state.app_id,
        "execution_id": state.execution_id,
        "contract_handle": state.contract_handle,
        "contract_sha256": state.contract_sha256,
        "runtime_profile": state.runtime_profile,
        "template_id": state.template_id,
        "name": state.name,
        "brief": state.brief,
        "scaffolded": scaffolded,
    })
}

impl LocalAppsHostBroker {
    /// The Host half of `LocalAppPrepare`. Every value it trusts came from the
    /// transport's own read of the plan approval, not from the caller.
    pub(crate) async fn prepare_value(&self, request: Value) -> Result<Value, String> {
        let app_id = required_string(&request, "app_id")?.to_string();
        let plan_path = required_string(&request, "plan_path")?.to_string();
        let session_uuid = required_string(&request, "session_uuid")?.to_string();
        let plan_sha256 = required_string(&request, "plan_sha256")?.to_string();
        let name = confirmed_field(&request, "name")?.to_string();
        let brief = confirmed_field(&request, "brief")?.to_string();
        let template_id = request
            .get("template_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        let spec_value = request
            .get("spec")
            .cloned()
            .ok_or("prepare_invalid: the approved plan carries no authoring spec")?;
        let spec: local_apps::AppAuthoringSpec = serde_json::from_value(spec_value.clone())
            .map_err(|error| format!("prepare_invalid: {error}"))?;
        spec.validate()
            .map_err(|error| format!("prepare_invalid: {error}"))?;

        // The plan on disk must still be the bytes the user approved. The user
        // may have edited it in the approval dialog, or the model may have
        // rewritten it; either way this approval no longer describes what is
        // there, and the only correct answer is to plan again.
        crate::mobile::plan_approval::verify_plan_unchanged(&plan_path, &plan_sha256)?;

        let service = self.service()?;
        let record = service
            .record(&app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(&app_id)?;
        let _guard = PREPARE_LOCK.get_or_init(|| Mutex::new(())).lock().await;

        if let Some(state) = read_state(&layout)? {
            if state.plan_path != plan_path || state.plan_sha256 != plan_sha256 {
                return Err(
                    "prepare_rejected: this app was already prepared from a different plan; \
                     start a new app rather than reusing this one"
                        .into(),
                );
            }
            if state.scaffolded || committed_scaffold(&layout, &state)? {
                return Ok(prepared_result(&state, true));
            }
            // The state is durable but the scaffold never committed — the
            // same app retrying, which the plan approval deliberately allows.
            // Everything before the scaffold already landed under this
            // execution id, so only the tail is re-run.
            return self.finish_create_scaffold(&layout, state).await;
        }

        // CREATE and MODIFY are derived from the record, never from the
        // caller: an app is created until it is scaffolded and modified after.
        let operation = if record.scaffolded {
            OPERATION_MODIFY
        } else {
            OPERATION_CREATE
        };
        let execution_id = format!("exec_{}", uuid::Uuid::new_v4().simple());
        Self::validate_workflow_run_id(&execution_id)?;
        match operation {
            OPERATION_MODIFY => {
                if template_id.is_some() {
                    return Err(
                        "prepare_rejected: this app's runtime profile was fixed when it was \
                         scaffolded; a modify plan may not select a template"
                            .into(),
                    );
                }
                let staged = self
                    .local_app_contract(json!({
                        "operation": "stage",
                        "app_id": app_id,
                        "workflow_run_id": execution_id,
                        "spec": spec_value,
                    }))
                    .await?;
                let runtime_profile: local_apps::AppRuntimeProfileBinding = staged
                    .pointer("/contract/runtime_profile")
                    .cloned()
                    .ok_or_else(|| {
                        "prepare_state_invalid: staged contract has no runtime profile".to_string()
                    })
                    .and_then(|value| {
                        serde_json::from_value(value)
                            .map_err(|error| format!("prepare_state_invalid: {error}"))
                    })?;
                let state = PrepareState {
                    version: 1,
                    app_id,
                    operation: OPERATION_MODIFY.into(),
                    execution_id,
                    session_uuid,
                    plan_path,
                    plan_sha256,
                    template_id: None,
                    name,
                    brief,
                    contract_handle: staged["contract_handle"]
                        .as_str()
                        .ok_or("prepare_state_invalid: staged contract has no handle")?
                        .to_string(),
                    contract_sha256: staged["contract_sha256"]
                        .as_str()
                        .ok_or("prepare_state_invalid: staged contract has no digest")?
                        .to_string(),
                    runtime_profile: Some(runtime_profile),
                    scaffolded: true,
                };
                save_state(&layout, &state)?;
                Ok(prepared_result(&state, true))
            }
            _ => {
                let template_id = template_id.ok_or(
                    "prepare_rejected: the approved plan does not name a template, so there is \
                     nothing to land; add `template_id` to the plan's authoring block and let the \
                     user approve it",
                )?;
                // Re-validated against the LIVE catalog HERE, not at plan time.
                // A template that disappeared or changed since the plan was
                // written is refused by name instead of being swapped for a
                // similar one.
                let (handle, selection) =
                    crate::mobile::local_app_template_catalog::journal_plan_selection(
                        &self.root,
                        &app_id,
                        &execution_id,
                        &template_id,
                        "the user approved this template in the plan",
                    )?;
                let staged = self
                    .local_app_contract(json!({
                        "operation": "stage",
                        "app_id": app_id,
                        "workflow_run_id": execution_id,
                        "validated_selection_handle": handle,
                        "spec": spec_value,
                    }))
                    .await?;
                let design_spec = spec_value.get("design").cloned();
                let mut stage = json!({
                    "app_id": app_id,
                    "workflow_run_id": execution_id,
                    "validated_selection_handle": handle,
                    "quality_level": "balanced",
                    "name": name,
                    "brief": brief,
                });
                if let Some(design_spec) = design_spec {
                    stage["design_spec"] = design_spec;
                }
                self.stage_create(stage).await?;
                let state = PrepareState {
                    version: 1,
                    app_id,
                    operation: OPERATION_CREATE.into(),
                    execution_id,
                    session_uuid,
                    plan_path,
                    plan_sha256,
                    template_id: Some(template_id),
                    name,
                    brief,
                    contract_handle: staged["contract_handle"]
                        .as_str()
                        .ok_or("prepare_state_invalid: staged contract has no handle")?
                        .to_string(),
                    contract_sha256: staged["contract_sha256"]
                        .as_str()
                        .ok_or("prepare_state_invalid: staged contract has no digest")?
                        .to_string(),
                    runtime_profile: Some(selection.runtime_profile),
                    scaffolded: false,
                };
                // Durable BEFORE the scaffold: `record_prepare_scaffold_commit`
                // runs inside it and needs this identity.
                save_state(&layout, &state)?;
                self.finish_create_scaffold(&layout, state).await
            }
        }
    }

    /// Seal the create approval and land the template, then mark the state
    /// scaffolded. Also the retry path: `approve_mcp_proposal_with` reuses an
    /// already-sealed candidate instead of asking again, so re-running this is
    /// safe and never rewrites source the first attempt wrote.
    async fn finish_create_scaffold(
        &self,
        layout: &AppLayout,
        mut state: PrepareState,
    ) -> Result<Value, String> {
        if state.operation != OPERATION_CREATE {
            return Err("prepare_state_invalid: modify state cannot be scaffolded".into());
        }
        let approval = self
            .approve_mcp_proposal_with(
                json!({
                    "app_id": state.app_id,
                    "workflow_run_id": state.execution_id,
                    "create_without_mcp": true,
                }),
                crate::mobile::plan_approval::CreateApprovalAuthority::ApprovedPlan,
            )
            .await?;
        let receipt_id = approval
            .get("receipt_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or("prepare_state_invalid: the sealed create approval has no receipt")?;
        self.scaffold_shell_app_value(json!({
            "app_id": state.app_id,
            "workflow_run_id": state.execution_id,
            "name": state.name,
            "brief": state.brief,
            "receipt_id": receipt_id,
        }))
        .await?;
        state.scaffolded = true;
        save_state(layout, &state)?;
        Ok(prepared_result(&state, true))
    }

    /// Written after the seeded workspace and its dependencies are ready and
    /// BEFORE `commit_scaffold` changes the record. The record is the commit
    /// bit, so a crash after it remains attributable even though the JS side
    /// never got to record anything.
    pub(super) fn record_prepare_scaffold_commit(
        &self,
        app_id: &str,
        execution_id: &str,
        profile: &local_apps::AppRuntimeProfileBinding,
    ) -> Result<(), String> {
        let layout = self.layout(app_id)?;
        let Some(state) = read_state(&layout)? else {
            // Not a plan-driven preparation: this app was scaffolded without
            // going through `prepare`, so there is no preparation to prove.
            return Ok(());
        };
        if state.execution_id != execution_id {
            return Ok(());
        }
        let candidate =
            local_apps::load_authoring_candidate(&layout).map_err(|error| error.to_string())?;
        if candidate.workflow_run_id != execution_id
            || candidate.contract_sha256 != state.contract_sha256
            || &candidate.contract.runtime_profile != profile
        {
            return Err("prepare_state_invalid: scaffold candidate identity mismatch".into());
        }
        write_document(
            &layout,
            &commit_path(&layout),
            &PrepareScaffoldCommit {
                app_id: app_id.into(),
                execution_id: execution_id.into(),
                contract_sha256: state.contract_sha256.clone(),
                runtime_profile: profile.clone(),
                name: state.name.clone(),
                brief: state.brief.clone(),
            },
        )
    }
}
