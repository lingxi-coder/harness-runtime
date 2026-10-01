//! Workflow-on-mobile composition pieces (plan v3 Phase 1).
//!
//! Everything the `Workflow` tool needs to run on the mobile engine — a real
//! `TaskRegistry`, a `PoolSubagentSpawner`, and the launcher that spawns
//! `LocalWorkflow` background tasks — adapted from the desktop composition
//! root (`harness-runtime::desktop/src/lib.rs`; the launcher mirrors
//! `TaskRegistryWorkflowLauncher`, the invoker mirrors `DeferredToolInvoker`).
//! Deliberately omitted desktop seams: worktree isolation (an
//! `isolation:"worktree"` agent degrades to a plain spawn — the documented
//! fallback), LSP, and the coordinator. Subagents keep upstream interactivity
//! semantics: a one-shot spawn is `is_async=false`, so `AskUserQuestion`
//! inside a workflow agent rides the SAME shared `Arc<ToolRegistry>` (and
//! therefore the same `TuiBridgeResolver` → broker → client channel) the
//! main session uses.

use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone)]
struct WorkflowCheckpoint {
    task_id: String,
    workflow_run_id: String,
    workflow_id: String,
    script_path: String,
    script_sha256: Option<String>,
    script_is_verbatim_builtin: Option<bool>,
    args_json: Option<String>,
    description: String,
    start_time: Option<u64>,
    transcript_dir: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalAppResumeProvenance {
    /// A trusted plugin-owned Local App workflow resume.
    TrustedPlugin,
    /// A trusted checkpoint for a custom script that reused a Local App
    /// workflow shape without inheriting Local App authority.
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalAppResumeResolution {
    provenance: LocalAppResumeProvenance,
    workflow_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkflowProvenanceRecord {
    workflow_id: String,
    script_sha256: String,
    script_is_verbatim_builtin: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkflowProvenanceLookup {
    Missing,
    Invalid { workflow_id: Option<String> },
    Valid(WorkflowProvenanceRecord),
}

const WORKFLOW_PROVENANCE_VERSION: u64 = 1;
const WORKFLOW_PROVENANCE_FILE: &str = "provenance.json";

/// Events emitted by a workflow worker before its launcher has finished
/// persisting the task/session ownership checkpoint.  The task registry starts
/// the handler before returning the generated task id, so this short handoff
/// window is real even though the worker waits for registry registration.
#[derive(Debug, Clone)]
enum PendingWorkflowEvent {
    Status {
        status: tasks::TaskStatus,
        /// Terminal failure reason, when `finish_workflow_terminal` supplied
        /// one. Buffered alongside the status so a transition that lands
        /// before the ownership checkpoint does not lose its reason.
        error: Option<String>,
    },
    Progress {
        run_id: String,
        progress: tasks::handlers::local_workflow::WorkflowProgressUpdate,
    },
}

#[derive(Debug, Clone)]
struct WorkflowAdoptFile {
    written_at_ms: u64,
    origin: String,
    shells: Vec<serde_json::Value>,
    cron: Vec<serde_json::Value>,
    workflows: Vec<WorkflowCheckpoint>,
    agents: Vec<serde_json::Value>,
}

/// Session-scoped workflow handoff store. Mobile cannot rely on a graceful
/// process-exit hook, so it keeps Claude Code's `adopt.json` payload current
/// from launch until a terminal status. A later process adopts it as `Paused`.
pub(crate) struct MobileWorkflowCheckpointStore {
    lingxi_home: std::path::PathBuf,
    cwd: std::path::PathBuf,
    lock: std::sync::Mutex<()>,
    task_runs: std::sync::Mutex<std::collections::HashMap<String, (String, String)>>,
    pending_events: std::sync::Mutex<std::collections::HashMap<String, Vec<PendingWorkflowEvent>>>,
}

impl MobileWorkflowCheckpointStore {
    pub(crate) fn new(lingxi_home: std::path::PathBuf, cwd: std::path::PathBuf) -> Self {
        Self {
            lingxi_home,
            cwd,
            lock: std::sync::Mutex::new(()),
            task_runs: std::sync::Mutex::new(std::collections::HashMap::new()),
            pending_events: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn session_dir(&self, session_uuid: &str) -> std::path::PathBuf {
        orchestrator::transcript_paths::subagents_dir(
            &self.lingxi_home,
            &self.cwd.to_string_lossy(),
            session_uuid,
        )
        .parent()
        .expect("subagents directory has a session parent")
        .to_path_buf()
    }

    fn path(&self, session_uuid: &str) -> std::path::PathBuf {
        self.session_dir(session_uuid).join("adopt.json")
    }

    fn track_task_owner(
        &self,
        task_id: &str,
        session_uuid: &str,
        run_id: &str,
    ) -> Vec<PendingWorkflowEvent> {
        let mut task_runs = self
            .task_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        task_runs.retain(|_, (existing_session, existing_run)| {
            existing_session != session_uuid || existing_run != run_id
        });
        task_runs.insert(
            task_id.to_string(),
            (session_uuid.to_string(), run_id.to_string()),
        );
        self.pending_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(task_id)
            .unwrap_or_default()
    }

    fn buffer_event(&self, task_id: &str, event: PendingWorkflowEvent) {
        self.pending_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(task_id.to_string())
            .or_default()
            .push(event);
    }

    fn task_owner(&self, task_id: &str) -> Option<(String, String)> {
        self.task_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(task_id)
            .cloned()
    }

    /// Return host-owned provenance for a Local App plugin workflow resume
    /// (build, use-test or MCP-authoring) in the current session. All launch
    /// coordinates must match: workflow run id, persisted script path,
    /// session, and recorded script hash.
    ///
    /// Recognising the id here is NOT authority on its own: this legacy
    /// checkpoint row is written for every launch, so a custom script whose
    /// `meta.name` merely copies a plugin-qualified id lands here too. The
    /// `script_is_verbatim_builtin` marker checked by
    /// [`local_app_resume_resolution_for_record`] is what separates the two,
    /// and it is set only when the launch resolved to the Plugin registry's
    /// own bytes.
    fn local_app_resume_record(
        &self,
        session_uuid: &str,
        run_id: &str,
        script_path: &std::path::Path,
    ) -> Option<WorkflowProvenanceRecord> {
        if session_uuid.is_empty() || !tool_workflow::is_valid_run_id(run_id) {
            return None;
        }
        let Some(script_path) = script_path.to_str() else {
            return None;
        };
        let script_path = std::path::Path::new(script_path);
        self.read(session_uuid)
            .workflows
            .into_iter()
            .find_map(|checkpoint| {
                if checkpoint
                    .script_sha256
                    .as_deref()
                    .is_none_or(|hash| !is_sha256_hex(hash))
                {
                    return None;
                }
                if !(crate::mobile::local_app_plugin_binding::is_plugin_workflow_id(
                    &checkpoint.workflow_id,
                ) && checkpoint.workflow_run_id == run_id
                    && paths_equivalent(script_path, std::path::Path::new(&checkpoint.script_path)))
                {
                    return None;
                }
                Some(WorkflowProvenanceRecord {
                    workflow_id: checkpoint.workflow_id,
                    script_sha256: checkpoint.script_sha256.expect("validated script hash"),
                    script_is_verbatim_builtin: checkpoint.script_is_verbatim_builtin,
                })
            })
    }

    #[cfg(test)]
    fn local_app_resume_provenance(
        &self,
        session_uuid: &str,
        run_id: &str,
        script_path: &std::path::Path,
        script: &str,
    ) -> Option<LocalAppResumeProvenance> {
        self.local_app_resume_record(session_uuid, run_id, script_path)
            .and_then(|record| {
                local_app_resume_resolution_for_record(&record, script).map(|r| r.provenance)
            })
    }

    /// Compatibility predicate for focused tests that only need to know
    /// whether the checkpoint belongs to a local-app workflow.
    #[cfg(test)]
    fn is_trusted_local_app_resume(
        &self,
        session_uuid: &str,
        run_id: &str,
        script_path: &std::path::Path,
        script: &str,
    ) -> bool {
        self.local_app_resume_provenance(session_uuid, run_id, script_path, script)
            .is_some()
    }

    fn persisted_workflow_script_path(
        &self,
        session_uuid: &str,
        run_id: &str,
    ) -> std::path::PathBuf {
        self.session_dir(session_uuid)
            .join("workflows")
            .join(format!("{run_id}.js"))
    }

    fn provenance_path(&self, session_uuid: &str, run_id: &str) -> std::path::PathBuf {
        self.session_dir(session_uuid)
            .join("subagents")
            .join("workflows")
            .join(run_id)
            .join(WORKFLOW_PROVENANCE_FILE)
    }

    fn read_provenance_sidecar(
        &self,
        session_uuid: &str,
        run_id: &str,
        script_path: &std::path::Path,
    ) -> WorkflowProvenanceLookup {
        if session_uuid.is_empty() || !tool_workflow::is_valid_run_id(run_id) {
            return WorkflowProvenanceLookup::Missing;
        }
        let path = self.provenance_path(session_uuid, run_id);
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return WorkflowProvenanceLookup::Missing;
            }
            Err(_) => return WorkflowProvenanceLookup::Invalid { workflow_id: None },
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
            return WorkflowProvenanceLookup::Invalid { workflow_id: None };
        };
        let workflow_id = value
            .get("workflowId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let Some(recorded_script_path) = value
            .get("scriptPath")
            .and_then(serde_json::Value::as_str)
            .filter(|path| !path.is_empty())
        else {
            return WorkflowProvenanceLookup::Invalid { workflow_id };
        };
        let Some(script_sha256) = value
            .get("scriptSha256")
            .and_then(serde_json::Value::as_str)
            .filter(|hash| is_sha256_hex(hash))
        else {
            return WorkflowProvenanceLookup::Invalid { workflow_id };
        };
        let valid = value.get("version").and_then(serde_json::Value::as_u64)
            == Some(WORKFLOW_PROVENANCE_VERSION)
            && value.get("sessionUuid").and_then(serde_json::Value::as_str) == Some(session_uuid)
            && value
                .get("workflowRunId")
                .and_then(serde_json::Value::as_str)
                == Some(run_id)
            && workflow_id.as_deref().is_some_and(|id| !id.is_empty())
            && paths_equivalent(script_path, std::path::Path::new(recorded_script_path));
        if !valid {
            return WorkflowProvenanceLookup::Invalid { workflow_id };
        }
        WorkflowProvenanceLookup::Valid(WorkflowProvenanceRecord {
            workflow_id: workflow_id.expect("validated workflow id"),
            script_sha256: script_sha256.to_string(),
            script_is_verbatim_builtin: value
                .get("scriptIsVerbatimBuiltin")
                .and_then(serde_json::Value::as_bool),
        })
    }

    fn write_provenance_sidecar(
        &self,
        session_uuid: &str,
        run_id: &str,
        script_path: &std::path::Path,
        script: &str,
        workflow_id: &str,
        script_is_verbatim_builtin: bool,
    ) -> std::io::Result<()> {
        let path = self.provenance_path(session_uuid, run_id);
        let parent = path.parent().expect("provenance path has parent");
        std::fs::create_dir_all(parent)?;
        let recorded_script_path = std::fs::canonicalize(script_path)
            .unwrap_or_else(|_| script_path.to_path_buf())
            .to_string_lossy()
            .into_owned();
        let value = serde_json::json!({
            "version": WORKFLOW_PROVENANCE_VERSION,
            "sessionUuid": session_uuid,
            "workflowRunId": run_id,
            "scriptPath": recorded_script_path,
            "scriptSha256": sha256_hex(script.as_bytes()),
            "workflowId": workflow_id,
            "scriptIsVerbatimBuiltin": script_is_verbatim_builtin,
        });
        let temp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(&value).map_err(std::io::Error::other)?;
        std::fs::write(&temp, json)?;
        std::fs::rename(temp, path)
    }

    fn read(&self, session_uuid: &str) -> WorkflowAdoptFile {
        let empty = || WorkflowAdoptFile {
            written_at_ms: unix_time_ms(),
            origin: "exit".to_string(),
            shells: Vec::new(),
            cron: Vec::new(),
            workflows: Vec::new(),
            agents: Vec::new(),
        };
        let Some(value) = std::fs::read_to_string(self.path(session_uuid))
            .ok()
            .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        else {
            return empty();
        };
        let workflows = value
            .get("workflows")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|checkpoint| {
                Some(WorkflowCheckpoint {
                    task_id: checkpoint.get("taskId")?.as_str()?.to_string(),
                    workflow_run_id: checkpoint.get("workflowRunId")?.as_str()?.to_string(),
                    script_path: checkpoint.get("scriptPath")?.as_str()?.to_string(),
                    script_sha256: checkpoint
                        .get("scriptSha256")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    script_is_verbatim_builtin: checkpoint
                        .get("scriptIsVerbatimBuiltin")
                        .and_then(serde_json::Value::as_bool),
                    args_json: checkpoint
                        .get("argsJson")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    workflow_id: checkpoint
                        .get("workflowId")
                        .and_then(serde_json::Value::as_str)
                        // Checkpoints written before workflow identity was
                        // split used description for both fields.
                        .or_else(|| {
                            checkpoint
                                .get("description")
                                .and_then(serde_json::Value::as_str)
                        })?
                        .to_string(),
                    description: checkpoint.get("description")?.as_str()?.to_string(),
                    start_time: checkpoint
                        .get("startTime")
                        .and_then(serde_json::Value::as_u64),
                    transcript_dir: checkpoint.get("transcriptDir")?.as_str()?.to_string(),
                })
            })
            .collect();
        WorkflowAdoptFile {
            written_at_ms: value
                .get("writtenAtMs")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_else(unix_time_ms),
            origin: value
                .get("origin")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("exit")
                .to_string(),
            shells: value
                .get("shells")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default(),
            cron: value
                .get("cron")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default(),
            workflows,
            agents: value
                .get("agents")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default(),
        }
    }

    fn write(&self, session_uuid: &str, mut file: WorkflowAdoptFile) -> std::io::Result<()> {
        let path = self.path(session_uuid);
        if file.workflows.is_empty()
            && file.shells.is_empty()
            && file.cron.is_empty()
            && file.agents.is_empty()
        {
            return match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            };
        }
        file.written_at_ms = unix_time_ms();
        file.origin = "exit".to_string();
        let parent = path.parent().expect("adopt file has parent");
        std::fs::create_dir_all(parent)?;
        let temp = parent.join("adopt.json.tmp");
        let workflows = file
            .workflows
            .into_iter()
            .map(|checkpoint| {
                let mut value = serde_json::Map::new();
                value.insert("taskId".into(), checkpoint.task_id.into());
                value.insert("workflowRunId".into(), checkpoint.workflow_run_id.into());
                value.insert("workflowId".into(), checkpoint.workflow_id.into());
                value.insert("scriptPath".into(), checkpoint.script_path.into());
                if let Some(hash) = checkpoint.script_sha256 {
                    value.insert("scriptSha256".into(), hash.into());
                }
                if let Some(verbatim) = checkpoint.script_is_verbatim_builtin {
                    value.insert("scriptIsVerbatimBuiltin".into(), verbatim.into());
                }
                if let Some(args) = checkpoint.args_json {
                    value.insert("argsJson".into(), args.into());
                }
                value.insert("description".into(), checkpoint.description.into());
                if let Some(start_time) = checkpoint.start_time {
                    value.insert("startTime".into(), start_time.into());
                }
                value.insert("transcriptDir".into(), checkpoint.transcript_dir.into());
                serde_json::Value::Object(value)
            })
            .collect::<Vec<_>>();
        let json = serde_json::to_vec_pretty(&serde_json::json!({
            "writtenAtMs": file.written_at_ms,
            "origin": file.origin,
            "shells": file.shells,
            "cron": file.cron,
            "workflows": workflows,
            "agents": file.agents,
        }))
        .map_err(std::io::Error::other)?;
        std::fs::write(&temp, json)?;
        std::fs::rename(temp, path)
    }

    fn upsert(
        &self,
        session_uuid: &str,
        checkpoint: WorkflowCheckpoint,
    ) -> std::io::Result<Vec<PendingWorkflowEvent>> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut file = self.read(session_uuid);
        file.workflows
            .retain(|existing| existing.workflow_run_id != checkpoint.workflow_run_id);
        let task_id = checkpoint.task_id.clone();
        let workflow_run_id = checkpoint.workflow_run_id.clone();
        file.workflows.push(checkpoint);
        match self.write(session_uuid, file) {
            Ok(()) => Ok(self.track_task_owner(&task_id, session_uuid, &workflow_run_id)),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn remove_task(&self, task_id: &str) {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some((session_uuid, run_id)) = self
            .task_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(task_id)
            .cloned()
        else {
            return;
        };
        let mut file = self.read(&session_uuid);
        file.workflows
            .retain(|checkpoint| checkpoint.workflow_run_id != run_id);
        match self.write(&session_uuid, file) {
            Ok(()) => {
                // Remove the in-memory owner only after the durable file has
                // been updated. If the write failed, a later terminal event
                // or shutdown retry can still find the checkpoint and clean
                // it up instead of silently orphaning it for adoption.
                self.task_runs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(task_id);
            }
            Err(error) => {
                tracing::warn!(%error, task_id, "could not remove workflow checkpoint; retaining owner for retry");
            }
        }
    }

    pub(crate) async fn adopt_session(
        &self,
        session_uuid: &str,
        registry: &tasks::registry::TaskRegistry,
        app_data_root: &std::path::Path,
    ) {
        self.adopt_matching(session_uuid, None, registry, app_data_root)
            .await;
    }

    /// Recover an evicted failed create without reviving unrelated terminal rows.
    pub(crate) async fn adopt_task(
        &self,
        session_uuid: &str,
        task_id: &str,
        registry: &tasks::registry::TaskRegistry,
        app_data_root: &std::path::Path,
    ) {
        // A filtered-out or terminal row is still owned by the live registry.
        // Never overwrite it while recovering a missing current-session row.
        if registry.get(task_id).await.is_some() {
            return;
        }
        self.adopt_matching(session_uuid, Some(task_id), registry, app_data_root)
            .await;
    }

    async fn adopt_matching(
        &self,
        session_uuid: &str,
        task_id: Option<&str>,
        registry: &tasks::registry::TaskRegistry,
        app_data_root: &std::path::Path,
    ) {
        let checkpoints = {
            let _guard = self
                .lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.read(session_uuid).workflows
        };
        let expected_root = self
            .session_dir(session_uuid)
            .join("subagents")
            .join("workflows");
        for checkpoint in checkpoints {
            if task_id.is_some_and(|id| checkpoint.task_id != id) {
                continue;
            }
            let valid_task_id = checkpoint.task_id.len() == 9
                && checkpoint.task_id.starts_with('w')
                && checkpoint
                    .task_id
                    .bytes()
                    .skip(1)
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
            let valid_run_id = tool_workflow::is_valid_run_id(&checkpoint.workflow_run_id);
            let script = std::fs::read(&checkpoint.script_path).ok();
            let valid_hash = match (&script, checkpoint.script_sha256.as_deref()) {
                (Some(script), Some(expected)) => sha256_hex(script) == expected,
                (Some(_), None) => true,
                _ => false,
            };
            let transcript_dir = std::path::PathBuf::from(&checkpoint.transcript_dir);
            let valid_transcript = std::fs::canonicalize(&expected_root)
                .ok()
                .zip(std::fs::canonicalize(&transcript_dir).ok())
                .is_some_and(|(root, directory)| directory.starts_with(root))
                && transcript_dir.join("journal.jsonl").is_file();
            let valid_args = checkpoint
                .args_json
                .as_deref()
                .is_none_or(|json| serde_json::from_str::<serde_json::Value>(json).is_ok());
            if !(valid_task_id && valid_run_id && valid_hash && valid_transcript && valid_args) {
                tracing::warn!(
                    task_id = %checkpoint.task_id,
                    run_id = %checkpoint.workflow_run_id,
                    "ignored invalid workflow checkpoint"
                );
                continue;
            }
            if task_id.is_some() {
                let Some(script_text) = script
                    .as_deref()
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                else {
                    continue;
                };
                let resolution = local_app_resume_resolution_for_launch(
                    self,
                    session_uuid,
                    &checkpoint.workflow_run_id,
                    std::path::Path::new(&checkpoint.script_path),
                    script_text,
                );
                // An explicit single-task recovery may only adopt a VERBATIM
                // plugin workflow (`resolution.provenance` is derived from the
                // registry's own bytes, never the caller's `meta.name`). The
                // retired create coordinator's recovery sidecars are gone with
                // it: a create whose template never landed is re-planned
                // through the new plan approval instead of being resumed.
                if !resolution.is_some_and(|resolution| {
                    resolution.provenance == LocalAppResumeProvenance::TrustedPlugin
                }) {
                    continue;
                }
            }
            // Re-derive Local App workflow authority from Host-owned state
            // (the real script bytes on disk + the real app manifest),
            // never from `checkpoint.workflow_id`/`args` themselves -- see
            // `resolve_adopted_local_app_scope`'s doc comment and
            // `tasks::registry::AdoptedWorkflow`'s doc comment for why. A
            // checkpoint that fails re-validation (forged, unscaffolded,
            // wrong app, or simply not a Local App build at all) yields
            // `None`, which is exactly `register_adopted_workflow`'s
            // pre-existing, safe behavior.
            let scope = script.as_deref().and_then(|bytes| {
                resolve_adopted_local_app_scope(
                    app_data_root,
                    &checkpoint.workflow_id,
                    bytes,
                    checkpoint.script_sha256.as_deref(),
                    checkpoint.script_is_verbatim_builtin,
                    checkpoint.args_json.as_deref(),
                )
            });
            let start_time = checkpoint
                .start_time
                .map(|millis| std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis))
                .unwrap_or_else(std::time::SystemTime::now);
            let adopted = tasks::registry::AdoptedWorkflow {
                task_id: checkpoint.task_id.clone(),
                session_uuid: Some(session_uuid.to_string()),
                workflow_id: checkpoint.workflow_id.clone(),
                run_id: checkpoint.workflow_run_id.clone(),
                script_path: checkpoint.script_path.clone(),
                args: checkpoint.args_json.clone(),
                transcript_dir: checkpoint.transcript_dir.clone(),
                description: checkpoint.description.clone(),
                start_time,
            };
            if let Err(error) = registry
                .register_adopted_workflow_with_scope(adopted, scope)
                .await
            {
                tracing::warn!(%error, "could not register adopted workflow");
                continue;
            }
            self.task_runs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    checkpoint.task_id,
                    (session_uuid.to_string(), checkpoint.workflow_run_id),
                );
        }
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn paths_equivalent(left: &std::path::Path, right: &std::path::Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

/// Mobile workflow status adapter: persist every worker transition in the task
/// registry, then publish the same transition to the native conversation UI.
///
/// The registry sink remains the source of truth. This wrapper only closes the
/// previously missing engine-to-client leg; without it a completed workflow was
/// visible to `TaskList` but never produced the existing `TaskStatusChanged`
/// event unless the user stopped it manually.
pub(crate) struct MobileWorkflowStatusSink {
    registry: Arc<tasks::registry_status_sink::RegistryStatusSink>,
    event_sink: Arc<dyn client::adapter::ClientEventSink>,
    listener: Arc<dyn client::adapter::ClientEventListener>,
    checkpoints: Arc<MobileWorkflowCheckpointStore>,
    active_session_uuid: Arc<std::sync::Mutex<String>>,
    /// Host-owned broker used to validate verified Local App build/use-test
    /// workflows. A weak reference avoids a broker↔workflow ownership cycle.
    local_apps_host: Arc<
        std::sync::OnceLock<std::sync::Weak<crate::mobile::local_apps_host::LocalAppsHostBroker>>,
    >,
    /// The bound registry is needed to resolve verified launch metadata and
    /// the handler-owned spool path without trusting workflow result JSON.
    task_registry: Arc<std::sync::OnceLock<Arc<tasks::registry::TaskRegistry>>>,
}

/// Host work that was fully validated outside the registry lock and is ready
/// for the short terminal commit section.  Keeping the broker with its opaque
/// prepared value prevents the sink from reconstructing publication identity
/// out of model-authored result JSON.
struct PreparedMobileQaCommit {
    host: Arc<crate::mobile::local_apps_host::LocalAppsHostBroker>,
    publication: crate::mobile::local_apps_host::PreparedWorkflowQaPublication,
}

struct MobileQaCleanup {
    host: Arc<crate::mobile::local_apps_host::LocalAppsHostBroker>,
    app_id: String,
    workflow_run_id: String,
}

fn workflow_verification_strategy(
    args_json: Option<&str>,
) -> Result<local_apps::QaVerificationStrategy, String> {
    let Some(args_json) = args_json else {
        return Ok(local_apps::QaVerificationStrategy::Balanced);
    };
    let args: Value = serde_json::from_str(args_json)
        .map_err(|error| format!("authenticated workflow args are invalid JSON: {error}"))?;
    let object = args
        .as_object()
        .ok_or_else(|| "authenticated workflow args are not an object".to_string())?;
    match object.get("quality_level") {
        None => Ok(local_apps::QaVerificationStrategy::Balanced),
        Some(Value::String(value)) if value == "fast" => {
            Ok(local_apps::QaVerificationStrategy::Fast)
        }
        Some(Value::String(value)) if value == "balanced" => {
            Ok(local_apps::QaVerificationStrategy::Balanced)
        }
        Some(Value::String(value)) if value == "thorough" => {
            Ok(local_apps::QaVerificationStrategy::Thorough)
        }
        Some(value) => Err(format!(
            "authenticated workflow quality_level is invalid: {value}"
        )),
    }
}

impl MobileWorkflowStatusSink {
    fn qa_cleanup_for_terminal_workflow(
        &self,
        workflow: &tasks::state::LocalWorkflowTaskState,
    ) -> Option<MobileQaCleanup> {
        let scope = workflow.scope.as_ref()?;
        if !matches!(
            scope.purpose(),
            tasks::scope::LocalAppWorkflowPurpose::Build
                | tasks::scope::LocalAppWorkflowPurpose::UseTest
        ) {
            return None;
        }
        let workflow_run_id = workflow
            .run_id
            .as_deref()
            .filter(|value| !value.is_empty())?;
        let host = self
            .local_apps_host
            .get()
            .and_then(std::sync::Weak::upgrade)?;
        Some(MobileQaCleanup {
            host,
            app_id: scope.app_id().to_string(),
            workflow_run_id: workflow_run_id.to_string(),
        })
    }

    /// Clean all Host-owned QA sessions for an authenticated app/run.  This
    /// deliberately does not inspect the model result: failures, cancellation
    /// and superseded sessions may have no result or stale/partial handles,
    /// while the Host's app/run identity remains authoritative.
    async fn cleanup_qa_for_terminal_task(&self, task_id: &str) {
        let Some(registry) = self.task_registry.get() else {
            return;
        };
        let Some(tasks::state::TaskState::LocalWorkflow(workflow)) = registry.get(task_id).await
        else {
            return;
        };
        let Some(cleanup) = self.qa_cleanup_for_terminal_workflow(&workflow) else {
            return;
        };
        if let Err(error) = cleanup
            .host
            .cleanup_workflow_qa_run(&cleanup.app_id, &cleanup.workflow_run_id)
        {
            tracing::warn!(%error, "failed to clean workflow QA run");
        }
    }

    pub(crate) fn new(
        listener: Arc<dyn client::adapter::ClientEventListener>,
        checkpoints: Arc<MobileWorkflowCheckpointStore>,
        active_session_uuid: Arc<std::sync::Mutex<String>>,
    ) -> Self {
        Self {
            registry: Arc::new(tasks::registry_status_sink::RegistryStatusSink::new()),
            event_sink: client::adapter::ListenerSink::arc(listener.clone()),
            listener,
            checkpoints,
            active_session_uuid,
            local_apps_host: Arc::new(std::sync::OnceLock::new()),
            task_registry: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Attach the Host QA receipt validator. The composition root owns the
    /// broker and therefore the app/run/build identity; the sink resolves the
    /// verified app/run scope from the task registry.
    pub(crate) fn attach_local_apps_host(
        &self,
        host: std::sync::Weak<crate::mobile::local_apps_host::LocalAppsHostBroker>,
    ) -> Result<(), std::sync::Weak<crate::mobile::local_apps_host::LocalAppsHostBroker>> {
        self.local_apps_host.set(host)
    }

    pub(crate) fn bind(&self, registry: Arc<tasks::registry::TaskRegistry>) {
        self.registry.bind(Arc::clone(&registry));
        let _ = self.task_registry.set(registry);
    }

    fn is_active_origin(&self, origin_session_id: &str) -> bool {
        self.active_session_uuid
            .lock()
            .map(|active| active.as_str() == origin_session_id)
            .unwrap_or(false)
    }

    async fn emit_status_for_owner(
        &self,
        task_id: &str,
        status: tasks::TaskStatus,
        origin_session_id: String,
        error: Option<String>,
    ) {
        if !self.is_active_origin(&origin_session_id) {
            return;
        }
        self.event_sink
            .emit(client::protocol::events::ClientEvent::TaskStatusChanged {
                task_id: task_id.to_string(),
                status: client::adapter::lowering::lower_task_status(match status {
                    tasks::TaskStatus::Pending => "pending",
                    tasks::TaskStatus::Running => "running",
                    tasks::TaskStatus::Paused => "paused",
                    tasks::TaskStatus::Completed => "completed",
                    tasks::TaskStatus::Failed => "failed",
                    tasks::TaskStatus::Killed => "killed",
                }),
                origin_session_id: Some(origin_session_id),
                // Only a real failure carries a reason; a completed/killed
                // transition must never inherit a stale one.
                error: if matches!(status, tasks::TaskStatus::Failed) {
                    error
                } else {
                    None
                },
            })
            .await;
    }

    async fn emit_progress_for_owner(
        &self,
        task_id: &str,
        run_id: &str,
        origin_session_id: String,
        progress: tasks::handlers::local_workflow::WorkflowProgressUpdate,
    ) {
        if !self.is_active_origin(&origin_session_id) {
            return;
        }
        self.listener
            .on_workflow_progress(
                origin_session_id,
                task_id.to_string(),
                run_id.to_string(),
                client::protocol::listings::WorkflowProgressDto {
                    kind: progress.kind,
                    index: progress.index,
                    title: progress.title,
                    message: progress.message,
                    label: progress.label,
                    phase_index: progress.phase_index,
                    phase_title: progress.phase_title,
                    agent_id: progress.agent_id,
                    agent_type: progress.agent_type,
                    model: progress.model,
                    fallback_model: progress.fallback_model,
                    state: progress.state,
                    error: progress.error,
                    tool_use_id: progress.tool_use_id,
                    queued_at_ms: progress.queued_at_ms,
                    started_at_ms: progress.started_at_ms,
                    last_progress_at_ms: progress.last_progress_at_ms,
                    attempt: progress.attempt,
                    last_attempt_reason: progress.last_attempt_reason,
                    tokens: progress.tokens,
                    tool_calls: progress.tool_calls,
                    last_tool_name: progress.last_tool_name,
                    last_tool_summary: progress.last_tool_summary,
                    prompt_preview: progress.prompt_preview,
                },
            )
            .await;
    }

    /// Persist a transition in the registry, then publish it to the owning
    /// session with its optional failure reason. Shared by `set_status`,
    /// `set_failed` and `finish_workflow_terminal` so all three buffer and
    /// de-register identically.
    async fn publish_status(
        &self,
        task_id: &str,
        status: tasks::TaskStatus,
        error: Option<String>,
    ) {
        tasks::handlers::TaskStatusSink::set_status(&*self.registry, task_id, status).await;
        if status.is_terminal() {
            self.cleanup_qa_for_terminal_task(task_id).await;
        }
        self.deliver_status(task_id, status, error).await;
    }

    /// The client-facing half of [`Self::publish_status`], for callers that
    /// already persisted the transition in the registry (the atomic
    /// `finish_workflow_terminal` path).
    async fn deliver_status(
        &self,
        task_id: &str,
        status: tasks::TaskStatus,
        error: Option<String>,
    ) {
        let Some((origin_session_id, _)) = self.checkpoints.task_owner(task_id) else {
            self.checkpoints
                .buffer_event(task_id, PendingWorkflowEvent::Status { status, error });
            return;
        };
        if status.is_terminal() {
            self.checkpoints.remove_task(task_id);
        }
        self.emit_status_for_owner(task_id, status, origin_session_id, error)
            .await;
    }

    /// Replace the handler's raw terminal payload before publishing a
    /// fail-closed Local App result. The task registry owns the spool path and
    /// output-manager capability; callers never provide a path from workflow
    /// result JSON.
    async fn fail_local_app_completion(
        registry: &tasks::registry::TaskRegistry,
        output_file: &std::path::Path,
        outcome: lingxi_core::host::task_registry::WorkflowTerminalOutcome,
        reason: String,
    ) -> (
        lingxi_core::host::task_registry::WorkflowTerminalOutcome,
        tasks::TaskStatus,
    ) {
        let terminal_payload = serde_json::json!({
            "ok": false,
            "error": reason,
            "verified": false,
        });
        let payload = terminal_payload.to_string();
        let error = match registry
            .output_manager
            .replace_terminal_result_fail_closed(output_file, &payload)
            .await
        {
            Ok(()) => reason,
            Err(replace_error) => {
                format!("{reason}; terminal spool replacement failed: {replace_error}")
            }
        };
        (
            lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                result: None,
                error: Some(error),
                ..outcome
            },
            tasks::TaskStatus::Failed,
        )
    }

    /// Gate verified Local App build/use-test success on a Host QA receipt.
    /// Generic workflows and MCP-authoring results retain the existing sink
    /// behavior. This runs before the registry's atomic outcome/status write,
    /// so a raw script result cannot win a race with the checked result.
    async fn validate_local_app_completion(
        &self,
        task_id: &str,
        outcome: lingxi_core::host::task_registry::WorkflowTerminalOutcome,
        status: tasks::TaskStatus,
    ) -> (
        lingxi_core::host::task_registry::WorkflowTerminalOutcome,
        tasks::TaskStatus,
        Option<PreparedMobileQaCommit>,
    ) {
        if status != tasks::TaskStatus::Completed {
            return (outcome, status, None);
        }
        let Some(registry) = self.task_registry.get() else {
            // An unbound sink is used by standalone/generic workflow tests and
            // during composition handoff. Without task metadata there is no
            // authenticated Local App scope to classify, so preserve the
            // existing generic sink behavior.
            return (outcome, status, None);
        };
        let Some(tasks::state::TaskState::LocalWorkflow(workflow)) = registry.get(task_id).await
        else {
            // Generic/custom workflows have no Local App scope and remain
            // untouched. A missing state for a terminal callback is a normal
            // teardown race, not evidence of a Local App build.
            return (outcome, status, None);
        };
        let Some(scope) = workflow.scope.as_ref() else {
            return (outcome, status, None);
        };
        let purpose = scope.purpose();
        if !matches!(
            purpose,
            tasks::scope::LocalAppWorkflowPurpose::Build
                | tasks::scope::LocalAppWorkflowPurpose::UseTest
        ) {
            return (outcome, status, None);
        }
        // A concurrent terminal transition (especially user kill) is already
        // authoritative. Do not validate or publish a second Host receipt for
        // a row that has left the running state.
        if workflow.base.status.is_terminal() {
            return (workflow.outcome.clone(), workflow.base.status, None);
        }
        let Some(run_id) = workflow.run_id.as_deref().filter(|value| !value.is_empty()) else {
            let (outcome, status) = Self::fail_local_app_completion(
                registry,
                &workflow.base.output_file,
                outcome,
                "local_app_completion_unverified: verified workflow has no run id".into(),
            )
            .await;
            return (outcome, status, None);
        };
        let Some(raw_result) = outcome.result.as_deref() else {
            let (outcome, status) = Self::fail_local_app_completion(
                registry,
                &workflow.base.output_file,
                outcome,
                "local_app_completion_unverified: verified workflow returned no result".into(),
            )
            .await;
            return (outcome, status, None);
        };
        let Ok(raw_value) = serde_json::from_str::<Value>(raw_result) else {
            let (outcome, status) = Self::fail_local_app_completion(
                registry,
                &workflow.base.output_file,
                outcome,
                "local_app_completion_unverified: workflow result is not JSON".into(),
            )
            .await;
            return (outcome, status, None);
        };
        let Some(raw_object) = raw_value.as_object() else {
            let (outcome, status) = Self::fail_local_app_completion(
                registry,
                &workflow.base.output_file,
                outcome,
                "local_app_completion_unverified: workflow result is not an object".into(),
            )
            .await;
            return (outcome, status, None);
        };
        // `ok:false` is a deliberate logical workflow result, not a transport
        // failure. Preserve its existing Completed-with-result semantics; the
        // plugin's own findings explain why no QA receipt was issued.
        if raw_object.get("ok") == Some(&Value::Bool(false)) {
            return (outcome, status, None);
        }
        if raw_object.get("ok") != Some(&Value::Bool(true)) {
            let (outcome, status) = Self::fail_local_app_completion(
                registry,
                &workflow.base.output_file,
                outcome,
                "local_app_completion_unverified: verified workflow result has no boolean ok:true"
                    .into(),
            )
            .await;
            return (outcome, status, None);
        }
        let Some(host) = self
            .local_apps_host
            .get()
            .and_then(std::sync::Weak::upgrade)
        else {
            let (outcome, status) = Self::fail_local_app_completion(
                registry,
                &workflow.base.output_file,
                outcome,
                "local_app_completion_validation_unavailable: Host broker is not attached".into(),
            )
            .await;
            return (outcome, status, None);
        };
        let expected_strategy = match workflow_verification_strategy(workflow.args.as_deref()) {
            Ok(strategy) => strategy,
            Err(error) => {
                let (outcome, status) = Self::fail_local_app_completion(
                    registry,
                    &workflow.base.output_file,
                    outcome,
                    format!("local_app_completion_unverified: {error}"),
                )
                .await;
                return (outcome, status, None);
            }
        };
        let prepared = match host
            .prepare_workflow_qa_outcome(scope.app_id(), run_id, expected_strategy, raw_value)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                let (outcome, status) = Self::fail_local_app_completion(
                    registry,
                    &workflow.base.output_file,
                    outcome,
                    format!("local_app_completion_unverified: {error}"),
                )
                .await;
                return (outcome, status, None);
            }
        };
        let checked_json = match serde_json::to_string(&prepared.canonical_result()) {
            Ok(value) => value,
            Err(error) => {
                let (outcome, status) = Self::fail_local_app_completion(
                    registry,
                    &workflow.base.output_file,
                    outcome,
                    format!(
                        "local_app_completion_unverified: cannot serialize checked result: {error}"
                    ),
                )
                .await;
                return (outcome, status, None);
            }
        };
        let mut checked_outcome = outcome;
        checked_outcome.result = Some(checked_json);
        (
            checked_outcome,
            tasks::TaskStatus::Completed,
            Some(PreparedMobileQaCommit {
                host,
                publication: prepared,
            }),
        )
    }

    async fn flush_pending_events(&self, task_id: &str, pending: Vec<PendingWorkflowEvent>) {
        let Some((origin_session_id, _)) = self.checkpoints.task_owner(task_id) else {
            return;
        };
        for event in pending {
            match event {
                PendingWorkflowEvent::Status { status, error } => {
                    self.emit_status_for_owner(task_id, status, origin_session_id.clone(), error)
                        .await;
                    if status.is_terminal() {
                        self.checkpoints.remove_task(task_id);
                    }
                }
                PendingWorkflowEvent::Progress { run_id, progress } => {
                    self.emit_progress_for_owner(
                        task_id,
                        &run_id,
                        origin_session_id.clone(),
                        progress,
                    )
                    .await;
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl tasks::handlers::TaskStatusSink for MobileWorkflowStatusSink {
    async fn set_status(&self, task_id: &str, status: tasks::TaskStatus) {
        self.publish_status(task_id, status, None).await;
    }

    /// Forward the failure reason so a `failed` transition reported through the
    /// dedicated `set_failed` seam reaches the client event, not just the log.
    async fn set_failed(&self, task_id: &str, error: &str) {
        self.publish_status(task_id, tasks::TaskStatus::Failed, Some(error.to_string()))
            .await;
    }

    /// Persist the workflow's terminal payload in the registry — the default
    /// trait method is a no-op, so without this override a mobile workflow's
    /// result/error never reached `TaskRecord` and the `TaskList` rows showed a
    /// reason-less failure.
    async fn set_workflow_outcome(
        &self,
        task_id: &str,
        outcome: lingxi_core::host::task_registry::WorkflowTerminalOutcome,
    ) {
        tasks::handlers::TaskStatusSink::set_workflow_outcome(&*self.registry, task_id, outcome)
            .await;
    }

    /// Atomic terminal publish: store the payload in the registry (so the
    /// `TaskList` row carries `error`), then push the status transition WITH
    /// the reason attached (so the conversation notice can name it).
    async fn finish_workflow_terminal(
        &self,
        task_id: &str,
        outcome: lingxi_core::host::task_registry::WorkflowTerminalOutcome,
        status: tasks::TaskStatus,
    ) {
        // Local App Build/UseTest needs the concrete registry transition, not
        // the RegistryStatusSink's fire-and-forget forwarding: a concurrent
        // kill may have won while Host QA validation was running. Publish the
        // status/outcome that the registry actually retained and make the
        // spool agree with that authoritative terminal state.
        if let Some(registry) = self.task_registry.get() {
            if let Some(tasks::state::TaskState::LocalWorkflow(workflow)) =
                registry.get(task_id).await
            {
                let is_local_app_build_or_use_test = workflow.scope.as_ref().is_some_and(|scope| {
                    matches!(
                        scope.purpose(),
                        tasks::scope::LocalAppWorkflowPurpose::Build
                            | tasks::scope::LocalAppWorkflowPurpose::UseTest
                    )
                });
                if is_local_app_build_or_use_test {
                    let output_file = workflow.base.output_file.clone();
                    let cleanup = self.qa_cleanup_for_terminal_workflow(&workflow);
                    let (candidate, candidate_status, prepared) = self
                        .validate_local_app_completion(task_id, outcome, status)
                        .await;
                    let committed_publication = prepared
                        .as_ref()
                        .map(|prepared| (prepared.host.clone(), prepared.publication.clone()));
                    let actual = match prepared {
                        Some(prepared) if candidate_status == tasks::TaskStatus::Completed => {
                            registry
                                .commit_local_app_workflow_terminal(task_id, candidate, move || {
                                    let PreparedMobileQaCommit { host, publication } = prepared;
                                    async move {
                                        host.commit_prepared_workflow_qa_publication(&publication)
                                    }
                                })
                                .await
                        }
                        _ => registry
                            .finish_workflow_terminal(task_id, candidate, candidate_status)
                            .await
                            .map(|state| (state, false)),
                    };
                    let Ok((tasks::state::TaskState::LocalWorkflow(actual), publication_committed)) =
                        actual
                    else {
                        // A task removed during teardown is already terminal
                        // from the client's perspective; do not re-emit the
                        // caller-requested Completed status into that race.
                        if let Some(cleanup) = cleanup {
                            if let Err(error) = cleanup
                                .host
                                .cleanup_workflow_qa_run(&cleanup.app_id, &cleanup.workflow_run_id)
                            {
                                tracing::warn!(%error, "failed to clean workflow QA run");
                            }
                        }
                        return;
                    };
                    if actual.base.status != tasks::TaskStatus::Completed {
                        let reason = actual.outcome.error.clone().unwrap_or_else(|| {
                            format!(
                                "local_app_completion_not_published: task ended as {:?}",
                                actual.base.status
                            )
                        });
                        let payload = serde_json::json!({
                            "ok": false,
                            "error": reason,
                            "verified": false,
                            "status": format!("{:?}", actual.base.status).to_lowercase(),
                        })
                        .to_string();
                        let _ = registry
                            .output_manager
                            .replace_terminal_result_fail_closed(&output_file, &payload)
                            .await;
                    }
                    if publication_committed {
                        if let Some((host, publication)) = committed_publication {
                            host.emit_committed_workflow_qa_summary(&publication).await;
                        }
                    } else if let Some(cleanup) = cleanup {
                        if let Err(error) = cleanup
                            .host
                            .cleanup_workflow_qa_run(&cleanup.app_id, &cleanup.workflow_run_id)
                        {
                            tracing::warn!(%error, "failed to clean workflow QA run");
                        }
                    }
                    self.deliver_status(task_id, actual.base.status, actual.outcome.error.clone())
                        .await;
                    return;
                }
            }
        }
        let (outcome, status, _) = self
            .validate_local_app_completion(task_id, outcome, status)
            .await;
        let error = outcome.error.clone();
        // Keep the registry's ATOMIC payload+status publish (it closes the
        // outcome/status race a concurrent kill would otherwise win).
        tasks::handlers::TaskStatusSink::finish_workflow_terminal(
            &*self.registry,
            task_id,
            outcome,
            status,
        )
        .await;
        self.deliver_status(task_id, status, error).await;
    }

    async fn is_registered(&self, task_id: &str) -> bool {
        tasks::handlers::TaskStatusSink::is_registered(&*self.registry, task_id).await
    }

    async fn is_terminal(&self, task_id: &str) -> bool {
        tasks::handlers::TaskStatusSink::is_terminal(&*self.registry, task_id).await
    }
}

#[async_trait::async_trait]
impl tasks::handlers::local_workflow::WorkflowProgressSink for MobileWorkflowStatusSink {
    async fn emit_workflow_progress(
        &self,
        task_id: &str,
        run_id: &str,
        progress: tasks::handlers::local_workflow::WorkflowProgressUpdate,
    ) {
        let Some((origin_session_id, _)) = self.checkpoints.task_owner(task_id) else {
            self.checkpoints.buffer_event(
                task_id,
                PendingWorkflowEvent::Progress {
                    run_id: run_id.to_string(),
                    progress,
                },
            );
            return;
        };
        self.emit_progress_for_owner(task_id, run_id, origin_session_id, progress)
            .await;
    }
}

/// Late-bound [`lingxi_core::host::tool_invoker::ToolInvoker`] resolving the composition
/// cycle: the `LocalWorkflowHandler` is registered into the `TaskRegistry`
/// (needs `&mut` — BEFORE the registry is `Arc`-wrapped), yet must dispatch
/// tools through the parent's `Arc<ToolRegistry>`, which is assembled AFTER
/// the task registry exists (its `BuiltinToolContext` carries
/// `task_registry.clone()`). Constructed empty, filled exactly once with the
/// real `RegistryToolInvoker` after `tools` is built; no workflow can
/// dispatch a tool before the build returns. Mirror of the desktop
/// `DeferredToolInvoker`.
pub(crate) struct DeferredToolInvoker {
    inner: std::sync::OnceLock<Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker>>,
}

impl DeferredToolInvoker {
    pub(crate) fn new() -> Self {
        Self {
            inner: std::sync::OnceLock::new(),
        }
    }

    /// Fill the cell with the real invoker. A second call is a no-op (the
    /// first binding wins), matching the build-once semantics.
    pub(crate) fn set(&self, invoker: Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker>) {
        let _ = self.inner.set(invoker);
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::tool_invoker::ToolInvoker for DeferredToolInvoker {
    async fn invoke_detailed(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
    ) -> Result<
        lingxi_core::host::tool_invoker::ToolInvocationResult,
        lingxi_core::host::tool_invoker::ToolInvokerError,
    > {
        match self.inner.get() {
            Some(invoker) => {
                invoker
                    .invoke_detailed(name, input, ctx, workspace_lease_token)
                    .await
            }
            None => Err(lingxi_core::host::tool_invoker::ToolInvokerError::Internal(
                "DeferredToolInvoker: tool dispatch attempted before build() bound the registry"
                    .to_string(),
            )),
        }
    }

    async fn invoke(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        match self.inner.get() {
            Some(invoker) => invoker.invoke(name, input, ctx).await,
            None => Err(lingxi_core::host::tool_invoker::ToolInvokerError::Internal(
                "DeferredToolInvoker: tool dispatch attempted before build() bound the registry"
                    .to_string(),
            )),
        }
    }

    /// Forward the lease token instead of inheriting the trait's delegating
    /// default. This wrapper sits between the lease PRODUCER
    /// (`WorkspaceLeaseToolInvoker`) and the CONSUMER (`RegistryToolInvoker`,
    /// which folds the token into `PermissionCheckContext`), so the default —
    /// which drops the token and calls `invoke` — left
    /// `workspace_lease_token` permanently `None` in production: the lease
    /// ALLOW never fired, and neither did the paired `denies_host_owned_for_token`
    /// hard deny.
    async fn invoke_with_workspace_lease(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        match self.inner.get() {
            Some(invoker) => {
                invoker
                    .invoke_with_workspace_lease(name, input, ctx, workspace_lease_token)
                    .await
            }
            None => Err(lingxi_core::host::tool_invoker::ToolInvokerError::Internal(
                "DeferredToolInvoker: tool dispatch attempted before build() bound the registry"
                    .to_string(),
            )),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The mobile [`tool_workflow::WorkflowLauncher`]: resolves + validates the
/// script, mints/reuses the run id, persists the script for
/// re-runnability, and spawns the `LocalWorkflow` task through the mobile
/// `TaskRegistry`. Adapted from the desktop `TaskRegistryWorkflowLauncher`
/// with mobile path anchors (`std::fs` is fine here — every path is inside
/// the app sandbox).
pub(crate) struct MobileWorkflowLauncher {
    pub(crate) registry: Arc<tasks::registry::TaskRegistry>,
    /// Project cwd that owns the persisted session directory; fixed for the session.
    pub(crate) project_cwd: std::path::PathBuf,
    /// Profile root that owns `apps/<app_id>/workspace/.lingxi/app.manifest.json`.
    /// Local-app workflow persistence checks must read this host-materialized
    /// manifest rather than trusting the caller's expected collection list or
    /// the session cwd (which can point at another project during a resume).
    pub(crate) app_data_root: std::path::PathBuf,
    /// Live cwd shared with the session and sampled at each launch.
    pub(crate) current_cwd: Arc<std::sync::Mutex<std::path::PathBuf>>,
    /// The lingxi home (`<app_files_root>/.claude`), anchoring
    /// `transcriptDir = <projectDir>/<sessionId>/subagents/workflows/<runId>`.
    pub(crate) lingxi_home: std::path::PathBuf,
    /// The LIVE current-session uuid (bare uuid, no `sess:` prefix), read at
    /// launch time. Mobile retargets sessions inside ONE engine — New/Resume/
    /// Clear swap the id while the orchestrator and this launcher live on —
    /// so a boot-time snapshot would anchor every later workflow's transcript
    /// under a session the user has already left.
    pub(crate) session_uuid: Arc<std::sync::Mutex<String>>,
    /// Live provider-qualified session model selection.
    pub(crate) default_model_selection_provider:
        Arc<std::sync::OnceLock<agent::handle::DefaultModelSelectionProvider>>,
    pub(crate) checkpoints: Arc<MobileWorkflowCheckpointStore>,
    pub(crate) status_sink: Arc<MobileWorkflowStatusSink>,
    /// Same live registry written by the mobile PluginManager and read by the
    /// Workflow tool plus nested resolver.
    pub(crate) plugin_workflows: Arc<workflow::PluginWorkflowRegistry>,
}

/// Return whether a launch resolves to the Host-verified Local App plugin
/// build workflow. A fresh OR by-name-resumed launch must come from the
/// verified plugin registry (`verified_plugin_workflow`, which re-resolves
/// `spec.name` against the registry's own bytes regardless of resume state --
/// the same host-owned check either way). A `scriptPath` resume instead
/// proves the same workflow through host-owned checkpoint provenance plus an
/// exact script-hash match (`trusted_local_app_resume`). A custom workflow
/// that merely happens to reuse the same `meta.name` is not treated as a
/// Local App build.
fn is_mobile_local_app_builtin(
    spec: &tool_workflow::WorkflowLaunchSpec,
    trusted_local_app_resume: bool,
    verified_plugin_workflow: bool,
) -> bool {
    let no_script_path = spec
        .script_path
        .as_deref()
        .filter(|path| !path.is_empty())
        .is_none();
    let no_inline_override = spec
        .script
        .as_deref()
        .filter(|inline| !inline.is_empty())
        .is_none();
    // Phase 9's single workflow is supplied by the verified mobile Plugin
    // registry. Its fully-qualified name is the identity, while the script
    // itself retains the short `meta.name` required by the workflow parser.
    // Only the named, non-overridden form is accepted here; an inline script
    // cannot borrow Local App build authority by copying that name. Resuming
    // BY NAME (no `scriptPath`) is accepted on the same footing as a fresh
    // launch: `verified_plugin_workflow` re-reads the registry's current
    // bytes and compares them against the script this launch actually
    // resolved, which is exactly as strong a proof on a resume as on a fresh
    // launch -- unlike a `scriptPath` resume, it is never taken on `spec`'s
    // say-so alone.
    let named_plugin_builtin = no_script_path
        && no_inline_override
        && spec.name.as_deref()
            == Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID);
    (named_plugin_builtin && verified_plugin_workflow) || trusted_local_app_resume
}

/// A plugin-qualified name is not authority by itself: a project workflow can
/// shadow the same name in the normal saved-workflow precedence chain.  Only
/// the exact immutable script snapshot currently registered by the Host's
/// Plugin registry may receive Local App launch-context authority.
fn is_verified_plugin_workflow(
    spec: &tool_workflow::WorkflowLaunchSpec,
    script: &str,
    registry: &workflow::PluginWorkflowRegistry,
) -> bool {
    let Some(name) = spec.name.as_deref() else {
        return false;
    };
    if !crate::mobile::local_app_plugin_binding::is_plugin_workflow_id(name) {
        return false;
    }
    if spec
        .script_path
        .as_deref()
        .filter(|path| !path.is_empty())
        .is_some()
        || spec
            .script
            .as_deref()
            .filter(|inline| !inline.is_empty())
            .is_some()
    {
        return false;
    }
    registry
        .resolve(name)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|resolved| resolved == script)
}

/// Does this launch earn `LocalAppWorkflowTaskScope::for_mcp_authoring`?
///
/// Exactly two provenances, mirroring the build path's pair:
///   * a BY-NAME launch of the MCP-authoring plugin workflow whose script
///     bytes still match the Plugin registry (`verified_plugin_workflow`), or
///   * a `scriptPath` resume whose host-owned checkpoint provenance
///     (`expected_workflow_id`) says MCP authoring.
///
/// The `verified_plugin_workflow &&` on the first arm is load-bearing and is
/// the narrowing this function was extracted to pin: without it a project
/// workflow that merely takes the name in [`PLUGIN_MCP_AUTHORING_WORKFLOW_ID`]
/// in the saved-workflow precedence chain borrowed MCP-authoring scope, which
/// this call site is the only production grant of. Name is not authority; the
/// registry byte match, or the host's own resume record, is.
fn is_mcp_authoring_launch(
    spec_name: Option<&str>,
    verified_plugin_workflow: bool,
    expected_workflow_id: Option<&str>,
) -> bool {
    (verified_plugin_workflow
        && spec_name
            == Some(crate::mobile::local_app_plugin_binding::PLUGIN_MCP_AUTHORING_WORKFLOW_ID))
        || expected_workflow_id
            == Some(crate::mobile::local_app_plugin_binding::PLUGIN_MCP_AUTHORING_WORKFLOW_ID)
}

/// Return whether this launch is the Host-verified Local App use-test
/// workflow.  The plugin bytes or host checkpoint provenance are the proof;
/// neither a copied name nor an `app_id` in caller args can mint UseTest
/// authority.
fn is_use_test_launch(
    spec_name: Option<&str>,
    verified_plugin_workflow: bool,
    expected_workflow_id: Option<&str>,
) -> bool {
    (verified_plugin_workflow
        && spec_name == Some(crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID))
        || expected_workflow_id
            == Some(crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID)
}

/// External (model-supplied) argument keys the Host launch boundary accepts
/// for the plugin build workflow.
///
/// AUD-WF-03 / r2-tests-honesty-007: this is the SINGLE source of truth the
/// `matches!`-turned-`contains` arm in
/// [`validate_namespaced_local_app_external_args`] consumes AND the drift gate
/// in this file's tests consumes. It used to be an inline `matches!` pattern
/// with the same list re-typed inside the test, which meant a key ADDED at the
/// boundary drifted invisibly: the test's own copy never grew, so nothing ever
/// asked the script to declare it. Do not re-type these lists anywhere.
///
/// WP5: `name`/`brief` are the display name and one-line brief the user
/// already confirmed conversationally before this launch. They carry no
/// authority (the Host still owns profile, catalog and workspace identity) and
/// they are the ONLY way the confirmed wording can reach `LocalAppStageCreate`,
/// which is what the native create confirmation sheet renders and what
/// `LocalAppScaffold` commits. Without them the create flow falls back to the
/// `untitled` placeholder.
///
/// WP-MCP-intent: `mcp_intent` is the create-time MCP interview outcome the
/// user already gave conversationally (absent = never asked,
/// `{"status":"declined"}`, `{"status":"requested","capabilities":[...]}`). It
/// belongs here for the same reason `name`/`brief` do: the launch boundary
/// forwards ONLY the declared contract, so a key the script declares but this
/// list omits is rejected before the script runs — which does not lose the
/// answer, it fails the whole create launch.
pub(crate) const BUILD_EXTERNAL_ARG_KEYS: &[&str] = &[
    "operation",
    "app_id",
    "authoring_spec",
    "revision_prompt",
    "quality_level",
    "name",
    "brief",
    "mcp_intent",
];

/// External argument keys accepted for the plugin use-test workflow.
/// Same single-source-of-truth contract as [`BUILD_EXTERNAL_ARG_KEYS`].
pub(crate) const USE_TEST_EXTERNAL_ARG_KEYS: &[&str] =
    &["app_id", "scope", "scenarios", "quality_level"];

/// External argument keys accepted for the plugin MCP-authoring workflow.
/// Same single-source-of-truth contract as [`BUILD_EXTERNAL_ARG_KEYS`].
pub(crate) const MCP_AUTHORING_EXTERNAL_ARG_KEYS: &[&str] = &["app_id", "user_goal"];

// These schemas are Plugin-owned inputs. Embed the checked-in JSON at build
// time so a workflow launch never reads a workspace-provided schema or lets a
// caller replace the contract after Plugin verification.
const AUTHORING_SPEC_SCHEMA_JSON: &str =
    include_str!("../../../plugins/lingxi-local-app/schemas/authoring-spec.schema.json");
const QA_REPORT_SCHEMA_JSON: &str =
    include_str!("../../../plugins/lingxi-local-app/schemas/qa-report.schema.json");
const USE_TEST_REPORT_SCHEMA_JSON: &str =
    include_str!("../../../plugins/lingxi-local-app/schemas/use-test-report.schema.json");
const MCP_PROPOSAL_SCHEMA_JSON: &str =
    include_str!("../../../plugins/lingxi-local-app/schemas/mcp-proposal.schema.json");
const WORKFLOW_AGENT_RESULTS_SCHEMA_JSON: &str =
    include_str!("../../../plugins/lingxi-local-app/schemas/workflow-agent-results.schema.json");

fn checked_in_local_app_schemas() -> Result<Value, tool_workflow::WorkflowLaunchError> {
    let parse = |name: &str, source: &str| {
        serde_json::from_str::<Value>(source).map_err(|error| {
            tool_workflow::WorkflowLaunchError(format!(
                "cannot parse checked-in Local App {name} schema: {error}"
            ))
        })
    };
    let workflow_agent_results =
        parse("workflow_agent_results", WORKFLOW_AGENT_RESULTS_SCHEMA_JSON)?;
    let workflow_defs = workflow_agent_results
        .get("$defs")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            tool_workflow::WorkflowLaunchError(
                "checked-in workflow-agent-results schema has no $defs".into(),
            )
        })?;
    let role_schema = |name: &str| {
        let mut schema = workflow_defs.get(name).cloned().ok_or_else(|| {
            tool_workflow::WorkflowLaunchError(format!(
                "checked-in workflow-agent-results schema has no {name} definition"
            ))
        })?;
        if let Some(object) = schema.as_object_mut() {
            object.insert(
                "$schema".into(),
                workflow_agent_results
                    .get("$schema")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            object.insert("$defs".into(), Value::Object(workflow_defs.clone()));
        }
        Ok(schema)
    };
    Ok(serde_json::json!({
        "authoring_spec": parse("authoring_spec", AUTHORING_SPEC_SCHEMA_JSON)?,
        "operator_result": role_schema("operator_result")?,
        "qa_review": role_schema("qa_review")?,
        "qa_finalize": role_schema("qa_finalize")?,
        "mcp_promoter": role_schema("mcp_promoter")?,
        "qa_report": parse("qa_report", QA_REPORT_SCHEMA_JSON)?,
        "use_test_report": parse("use_test_report", USE_TEST_REPORT_SCHEMA_JSON)?,
        "mcp_proposal": parse("mcp_proposal", MCP_PROPOSAL_SCHEMA_JSON)?,
    }))
}

fn authoring_spec_from_args(
    object: &serde_json::Map<String, Value>,
) -> Result<Option<Value>, tool_workflow::WorkflowLaunchError> {
    let Some(value) = object.get("authoring_spec") else {
        return Ok(None);
    };
    let candidate = value.as_object().map_or_else(
        || {
            Err(tool_workflow::WorkflowLaunchError(
                "local-app authoring_spec must be a structured object; legacy string spec is not accepted"
                    .into(),
            ))
        },
        |_| Ok(value.clone()),
    )?;
    // The generic matcher checks shape, not cross-field invariants such as
    // one presentation per target. Return those errors to the caller before
    // creating a background task so it can correct the initial spec.
    let typed: local_apps::AppAuthoringSpec =
        serde_json::from_value(candidate.clone()).map_err(|error| {
            tool_workflow::WorkflowLaunchError(format!(
                "local-app authoring_spec is invalid: {error}"
            ))
        })?;
    typed.validate().map_err(|error| {
        tool_workflow::WorkflowLaunchError(format!("local-app authoring_spec is invalid: {error}"))
    })?;
    let schema = serde_json::from_str::<Value>(AUTHORING_SPEC_SCHEMA_JSON).map_err(|error| {
        tool_workflow::WorkflowLaunchError(format!("cannot parse authoring spec schema: {error}"))
    })?;
    if !local_apps::value_matches_schema(&candidate, &schema) {
        return Err(tool_workflow::WorkflowLaunchError(
            "local-app build workflow authoring spec does not satisfy the checked-in Plugin schema"
                .into(),
        ));
    }
    Ok(Some(candidate))
}

/// Return whether a Host-validated update changes any user-visible authoring
/// surface.  This is deliberately derived from the active successful build
/// contract and the incoming typed AuthoringSpec; a model-provided boolean is
/// never an authority source for the workflow's QA/design branch.
fn update_ui_impact(
    active_contract: &local_apps::AppAuthoringContract,
    incoming: &Value,
) -> Result<bool, tool_workflow::WorkflowLaunchError> {
    let incoming: local_apps::AppAuthoringSpec =
        serde_json::from_value(incoming.clone()).map_err(|error| {
            tool_workflow::WorkflowLaunchError(format!(
                "local-app update authoring spec is not a Host-valid AuthoringSpec: {error}"
            ))
        })?;
    incoming.validate().map_err(|error| {
        tool_workflow::WorkflowLaunchError(format!(
            "local-app update authoring spec is not a Host-valid AuthoringSpec: {error}"
        ))
    })?;
    Ok(active_contract.spec.ui != incoming.ui
        || active_contract.spec.targets != incoming.targets
        || active_contract.spec.design != incoming.design)
}

/// Reject caller fields outside the narrow public workflow contract before
/// the Host adds its private context.  Authority-bearing keys are accepted
/// nowhere on the public launch surface: they are rejected here and only
/// later re-injected by the Host. Every other unexpected field fails at the
/// launch boundary instead of reaching a Plugin script with an ambiguous
/// source of truth.
fn validate_namespaced_local_app_external_args(
    spec: &tool_workflow::WorkflowLaunchSpec,
) -> Result<(), tool_workflow::WorkflowLaunchError> {
    let Some(name) = spec.name.as_deref() else {
        return Ok(());
    };
    let Some(object) = spec.args.as_ref().and_then(serde_json::Value::as_object) else {
        return Ok(());
    };
    for key in object.keys() {
        let allowed = match name {
            name if name == crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID => {
                BUILD_EXTERNAL_ARG_KEYS.contains(&key.as_str())
            }
            name if name
                == crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID =>
            {
                USE_TEST_EXTERNAL_ARG_KEYS.contains(&key.as_str())
            }
            name if name
                == crate::mobile::local_app_plugin_binding::PLUGIN_MCP_AUTHORING_WORKFLOW_ID =>
            {
                MCP_AUTHORING_EXTERNAL_ARG_KEYS.contains(&key.as_str())
            }
            _ => true,
        };
        if !allowed {
            return Err(tool_workflow::WorkflowLaunchError(format!(
                "{name}: unknown external field {key:?}; Host accepts only the declared workflow contract"
            )));
        }
    }
    Ok(())
}

/// Resolve which Plugin-owned Local App workflow a `scriptPath` resume is
/// really re-entering, from host-recorded provenance only.
///
/// r4-workflow-runtime-05: this used to answer for
/// `PLUGIN_BUILD_WORKFLOW_ID` alone, which left the use-test and
/// MCP-authoring workflows with NO host-owned identity on the one launch
/// shape the Workflow tool's own resume hint produces (`{scriptPath,
/// resumeFromRunId}`, no `name`). Both their arg sanitizer and their
/// `host_context` enrichment key off that identity, so a resume of either
/// script reached QuickJS with no `host_context` at all and threw on its
/// first context statement.
///
/// The marker is what carries the trust, not the id: `script_is_verbatim_builtin`
/// is recorded `true` only when the launch resolved to the Plugin registry's
/// own bytes (`verified_plugin_workflow`), and the hash must still match the
/// script this launch resolved. A custom script that merely copies a
/// plugin-qualified `meta.name` records `false` and resolves to `None` here.
fn local_app_resume_resolution_for_record(
    record: &WorkflowProvenanceRecord,
    script: &str,
) -> Option<LocalAppResumeResolution> {
    let exact_hash = record.script_sha256 == sha256_hex(script.as_bytes());
    if !crate::mobile::local_app_plugin_binding::is_plugin_workflow_id(&record.workflow_id) {
        return None;
    }
    if record.script_is_verbatim_builtin == Some(true) && exact_hash {
        return Some(LocalAppResumeResolution {
            provenance: LocalAppResumeProvenance::TrustedPlugin,
            workflow_id: Some(record.workflow_id.clone()),
        });
    }
    if record.workflow_id == crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID
        && record.script_is_verbatim_builtin == Some(false)
        && exact_hash
    {
        return Some(LocalAppResumeResolution {
            provenance: LocalAppResumeProvenance::Custom,
            workflow_id: None,
        });
    }
    None
}

#[cfg(test)]
fn local_app_resume_provenance_for_launch(
    checkpoints: &MobileWorkflowCheckpointStore,
    session_uuid: &str,
    run_id: &str,
    script_path: &std::path::Path,
    script: &str,
) -> Option<LocalAppResumeProvenance> {
    local_app_resume_resolution_for_launch(checkpoints, session_uuid, run_id, script_path, script)
        .map(|resolution| resolution.provenance)
}

fn local_app_resume_resolution_for_launch(
    checkpoints: &MobileWorkflowCheckpointStore,
    session_uuid: &str,
    run_id: &str,
    script_path: &std::path::Path,
    script: &str,
) -> Option<LocalAppResumeResolution> {
    match checkpoints.read_provenance_sidecar(session_uuid, run_id, script_path) {
        WorkflowProvenanceLookup::Valid(record) => {
            local_app_resume_resolution_for_record(&record, script)
        }
        WorkflowProvenanceLookup::Invalid { .. } => None,
        WorkflowProvenanceLookup::Missing => checkpoints
            .local_app_resume_record(session_uuid, run_id, script_path)
            .and_then(|record| local_app_resume_resolution_for_record(&record, script)),
    }
}

/// Replace the caller-provided persistence contract with the IDs from the
/// materialized app manifest. The caller's value is intentionally ignored,
/// including `[]`: an empty caller list must not turn off the native
/// round-trip gate for a manifest that declares writable collections.
#[cfg(test)]
fn apply_materialized_local_app_collections(
    app_data_root: &std::path::Path,
    spec: &mut tool_workflow::WorkflowLaunchSpec,
    script: &str,
) -> Result<(), tool_workflow::WorkflowLaunchError> {
    apply_materialized_local_app_collections_with_provenance(app_data_root, spec, script, false)
}

#[cfg(test)]
fn apply_materialized_local_app_collections_with_provenance(
    app_data_root: &std::path::Path,
    spec: &mut tool_workflow::WorkflowLaunchSpec,
    script: &str,
    trusted_local_app_resume: bool,
) -> Result<(), tool_workflow::WorkflowLaunchError> {
    let verified_plugin_workflow = spec.name.as_deref()
        == Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID)
        && spec
            .script_path
            .as_deref()
            .filter(|path| !path.is_empty())
            .is_none()
        && spec
            .script
            .as_deref()
            .filter(|inline| !inline.is_empty())
            .is_none();
    let expected_workflow_id = (verified_plugin_workflow
        || (trusted_local_app_resume
            && spec.name.as_deref()
                == Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID)))
    .then_some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID);
    apply_materialized_local_app_collections_with_identity(
        app_data_root,
        spec,
        script,
        trusted_local_app_resume,
        expected_workflow_id,
        verified_plugin_workflow,
    )
    // Both thin wrappers exist for callers that only care about the args
    // rewrite; the minted scope is returned by the `_with_identity` form the
    // launcher calls.
    .map(|_scope| ())
}

/// Rewrite the launch args, and -- when this launch really is one of this
/// Host's own Local App build workflows against an app this Host resolved --
/// mint the [`tasks::scope::LocalAppWorkflowTaskScope`] that authorizes it.
///
/// `Ok(None)` is the answer for every launch that is not a Local App build:
/// the function returns before the app lookup, and an unscoped task row is
/// authority-free (no workspace lease, no App delete block). There is
/// deliberately no path from a caller-supplied name to a `Some`.
fn apply_materialized_local_app_collections_with_identity(
    app_data_root: &std::path::Path,
    spec: &mut tool_workflow::WorkflowLaunchSpec,
    _script: &str,
    trusted_local_app_resume: bool,
    expected_workflow_id: Option<&str>,
    verified_plugin_workflow: bool,
) -> Result<Option<tasks::scope::LocalAppWorkflowTaskScope>, tool_workflow::WorkflowLaunchError> {
    if let Some(expected_workflow_id) = expected_workflow_id {
        if expected_workflow_id != crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID
        {
            return Err(tool_workflow::WorkflowLaunchError(
                "cannot resume a local-app workflow with a different plugin workflow id"
                    .to_string(),
            ));
        }
    } else if trusted_local_app_resume {
        return Err(tool_workflow::WorkflowLaunchError(
            "cannot resume a local-app workflow without a trusted plugin workflow id".to_string(),
        ));
    }
    if !is_mobile_local_app_builtin(spec, trusted_local_app_resume, verified_plugin_workflow) {
        // Not one of this Host's Local App build workflows: no args rewrite,
        // and -- crucially -- no scope. A custom workflow that merely reuses a
        // real workflow's `meta.name` lands here, because the predicate above
        // requires either the exact plugin registry entry or trusted
        // host-owned resume provenance, never the caller's name alone.
        return Ok(None);
    }

    let launched_workflow_id = expected_workflow_id
        .map(str::to_string)
        .or_else(|| {
            spec.name
                .as_deref()
                .filter(|name| {
                    *name == crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID
                })
                .map(str::to_string)
        })
        .ok_or_else(|| {
            tool_workflow::WorkflowLaunchError(
                "cannot identify the local-app plugin workflow being launched".to_string(),
            )
        })?
        .to_string();

    let is_plugin_workflow = launched_workflow_id
        == crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID
        && (verified_plugin_workflow || trusted_local_app_resume);

    let args = spec.args.as_mut().ok_or_else(|| {
        tool_workflow::WorkflowLaunchError(
            "local-app build workflow requires args.app_id and a materialized manifest".to_string(),
        )
    })?;
    let object = args.as_object_mut().ok_or_else(|| {
        tool_workflow::WorkflowLaunchError(
            "local-app build workflow args must be an object containing app_id".to_string(),
        )
    })?;
    let app_id = object
        .get("app_id")
        .and_then(serde_json::Value::as_str)
        .filter(|app_id| !app_id.trim().is_empty())
        .ok_or_else(|| {
            tool_workflow::WorkflowLaunchError(
                "local-app build workflow requires a non-empty args.app_id".to_string(),
            )
        })?
        .to_string();
    let operation = object
        .get("operation")
        .and_then(serde_json::Value::as_str)
        .filter(|value| matches!(*value, "create" | "update" | "verify"))
        .ok_or_else(|| {
            tool_workflow::WorkflowLaunchError(
                "local-app build workflow requires args.operation to be one of create, update, \
                 verify"
                    .to_string(),
            )
        })?
        .to_string();
    let layout = local_apps::AppLayout::new(app_data_root, &app_id).map_err(|error| {
        tool_workflow::WorkflowLaunchError(format!(
            "cannot resolve local-app manifest for app {app_id:?}: {error}"
        ))
    })?;
    // Keep workflow routing behind the same authoritative gate as the build
    // path.  The workflow receives an app id from the model, so loading a
    // manifest alone is not enough: a torn shell can otherwise look like a
    // valid local app, and a caller-controlled binding could select a family
    // that this host cannot actually resolve.  `detect_build_target` checks
    // the record mirror's `scaffolded` bit, requires the manifest surface and
    // binding as a consistent pair, and resolves the binding against the exact
    // published catalog (including its contract hash and availability).
    let build_target =
        crate::mobile::local_apps_build::detect_build_target(&layout).map_err(|error| {
            tool_workflow::WorkflowLaunchError(format!(
                "cannot validate local-app runtime profile for app {app_id:?}: {error}"
            ))
        })?;
    let manifest = local_apps::load_manifest(&layout).map_err(|error| {
        tool_workflow::WorkflowLaunchError(format!(
            "cannot load materialized local-app manifest for app {app_id:?}: {error}"
        ))
    })?;
    let binding = manifest.runtime_profile.as_ref().ok_or_else(|| {
        tool_workflow::WorkflowLaunchError(format!(
            "local-app build workflow requires app {app_id:?} to have a persisted runtime profile"
        ))
    })?;
    if manifest.dependency_snapshot.is_none() {
        return Err(tool_workflow::WorkflowLaunchError(format!(
            "local-app build workflow requires app {app_id:?} to have a verified dependency snapshot"
        )));
    }
    let active_authoring_contract =
        crate::mobile::local_apps_build::active_authoring_contract(&layout).map_err(|error| {
            tool_workflow::WorkflowLaunchError(format!(
                "cannot load active Local App authoring contract: {error}"
            ))
        })?;
    let active_authoring_contract_sha256 =
        crate::mobile::local_apps_build::active_build_authoring_contract_sha256(&layout).map_err(
            |error| {
                tool_workflow::WorkflowLaunchError(format!(
                    "cannot load active Local App authoring contract digest: {error}"
                ))
            },
        )?;
    // The build receipt is the selector for the last successful Host build.
    // Keep its exact digest separate from the contract payload: the payload
    // intentionally has no digest field, while Plugin QA/update logic must
    // bind to the immutable contract selected by that receipt.
    // The Host consumes a typed handle rather than picking or comparing a
    // workflow name itself: the binding resolves the build target's
    // authorized workflow and enforces it against the caller's selection,
    // keeping the pre-Phase -1 refusal semantics (same app id, family, and
    // both workflow ids named on mismatch) behind a composition seam instead
    // of an inline string comparison.
    let plugin_binding =
        crate::mobile::local_app_plugin_binding::LocalAppPluginBinding::resolve(build_target);
    let binding_family = binding.family;
    let binding_revision = binding.revision;
    let binding_contract_sha256 = binding.contract_sha256.clone();
    plugin_binding.enforce(&app_id, binding_family, &launched_workflow_id)?;
    // Everything above is what makes this `app_id` trustworthy enough to mint
    // authority from, and it is why the scope is minted HERE rather than at
    // the spawn call below. By this line the Host has established, from state
    // it owns rather than from anything the caller said:
    //
    // * the script really is this Host's bundled Local App build workflow --
    //   `is_mobile_local_app_builtin` compares BYTES against
    //   `tool_workflow::BUILTIN_WORKFLOWS`, or accepts a resume only on
    //   host-owned provenance, so a forged `meta.name` never reaches here;
    // * the app exists and is a real, fully scaffolded Local App --
    //   `AppLayout::new` + `detect_build_target` check the record mirror's
    //   `scaffolded` bit and resolve the manifest/binding pair against the
    //   published catalog, and the manifest carries a verified dependency
    //   snapshot;
    // * this app is pinned to exactly this workflow -- `enforce` refuses any
    //   other caller-selected workflow id for the resolved build target.
    //
    // The `app_id` string does originate in `args`, but it is not TAKEN from
    // args: it is the key the whole resolution above succeeded on, so a forged
    // value can only name an app that genuinely exists, is genuinely
    // scaffolded, and is genuinely pinned to the workflow whose verbatim
    // bundled bytes are running. Naming another app there does not hand the
    // forger that app's authority; it hands them a build of that app, which is
    // the same thing the tool would have done anyway.
    let scope = tasks::scope::LocalAppWorkflowTaskScope::for_build(&app_id).map_err(|error| {
        tool_workflow::WorkflowLaunchError(format!(
            "cannot authorize local-app build workflow: {error}"
        ))
    })?;
    let runtime_profile = serde_json::json!({
        "family": binding_family.as_str(),
        "revision": binding_revision,
        "contract_sha256": binding_contract_sha256,
        "surface": binding_family.surface().as_str(),
    });
    let collection_ids = manifest
        .collections
        .iter()
        .map(|collection| serde_json::Value::String(collection.id.clone()))
        .collect();
    object.insert("runtime_profile".to_string(), runtime_profile.clone());
    let expected_writable_collections = serde_json::Value::Array(collection_ids);
    object.insert(
        "expected_writable_collections".to_string(),
        expected_writable_collections.clone(),
    );
    if is_plugin_workflow {
        let active_authoring_contract = active_authoring_contract.as_ref().ok_or_else(|| {
            tool_workflow::WorkflowLaunchError(format!(
                "local-app {operation} workflow requires an active Host authoring contract"
            ))
        })?;
        let active_authoring_contract_sha256 = active_authoring_contract_sha256
            .as_deref()
            .ok_or_else(|| {
                tool_workflow::WorkflowLaunchError(format!(
                    "local-app {operation} workflow requires an active Host authoring contract digest"
                ))
            })?;
        let invocation_capability = format!("mcpv_{}", uuid::Uuid::new_v4().simple());
        let authoring_spec = authoring_spec_from_args(object)?;
        if operation == "update" && authoring_spec.is_none() {
            return Err(tool_workflow::WorkflowLaunchError(
                "local-app update workflow requires a new full AuthoringSpec; freeform revision_prompt alone is not authoritative"
                    .into(),
            ));
        }
        if operation == "verify" && authoring_spec.is_some() {
            return Err(tool_workflow::WorkflowLaunchError(
                "local-app verify workflow accepts only the Host effective authoring contract"
                    .into(),
            ));
        }
        let update_ui_impact = if operation == "update" {
            let incoming = authoring_spec.as_ref().ok_or_else(|| {
                tool_workflow::WorkflowLaunchError(
                    "local-app update workflow requires a full Host-valid AuthoringSpec".into(),
                )
            })?;
            update_ui_impact(active_authoring_contract, incoming)?
        } else {
            false
        };
        let schemas = checked_in_local_app_schemas()?;
        let catalog =
            crate::mobile::local_app_template_catalog::catalog_view().map_err(|error| {
                tool_workflow::WorkflowLaunchError(format!(
                    "cannot read verified template catalog: {error}"
                ))
            })?;
        object.insert(
            "host_context".into(),
            serde_json::json!({
                "source": "verified_host",
                "operation": operation,
                "app_id": app_id,
                "runtime_profile": runtime_profile,
                "authoring_contract": serde_json::to_value(active_authoring_contract)
                    .unwrap_or(Value::Null),
                "authoring_contract_sha256": active_authoring_contract_sha256,
                "authoring_spec": authoring_spec.unwrap_or(Value::Null),
                "update_ui_impact": update_ui_impact,
                "invocation_capability": invocation_capability,
                "schemas": schemas.clone(),
                "template_catalog": {
                    "catalog_digest": catalog.catalog_digest,
                    "available_template_ids": catalog.templates.iter().map(|entry| entry.template_id.clone()).collect::<Vec<_>>(),
                },
                "expected_writable_collections": expected_writable_collections,
                "dependency_snapshot": {"verified": true},
                "active_catalog": manifest.active_mcp_catalog.clone(),
                "staging": {"isolated": true, "final_publish": false}
            }),
        );
    }
    Ok(Some(scope))
}

/// Read a host-created app record mirror without exposing a second persistence
/// API to workflow scripts. The JSON shape is checked by the workflow launch
/// boundary before a create run receives authority.
fn layout_for_app_record(
    app_data_root: &std::path::Path,
    app_id: &str,
) -> Result<serde_json::Value, tool_workflow::WorkflowLaunchError> {
    let layout = local_apps::AppLayout::new(app_data_root, app_id).map_err(|error| {
        tool_workflow::WorkflowLaunchError(format!(
            "cannot resolve local-app app {app_id:?}: {error}"
        ))
    })?;
    let path = layout
        .root()
        .join(layout.workspace_rel())
        .join(".lingxi")
        .join("app.json");
    let bytes = std::fs::read(&path).map_err(|error| {
        tool_workflow::WorkflowLaunchError(format!(
            "cannot read local-app record mirror {}: {error}",
            path.display()
        ))
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        tool_workflow::WorkflowLaunchError(format!(
            "cannot parse local-app record mirror {}: {error}",
            path.display()
        ))
    })
}

/// Re-mint Local App build authority for one restart-recovered checkpoint
/// (`MobileWorkflowCheckpointStore::adopt_session`), re-resolving from state
/// the Host owns rather than trusting anything the checkpoint itself
/// recorded. Companion to
/// [`apply_materialized_local_app_collections_with_identity`], which does the
/// equivalent job for a LIVE launch; see
/// [`tasks::registry::AdoptedWorkflow`]'s doc comment for the residual this
/// closes.
///
/// Unlike the old built-in transition path, Phase 9 trusts only the plugin
/// workflow id plus the host-recorded exact script hash/marker pair. A
/// checkpoint that fails re-validation (wrong workflow id, edited script,
/// missing marker, forged app id, or unscaffolded app) yields `None`.
///
/// `args_json`'s `app_id` is used only as a HINT for which app to resolve --
/// the same role it plays in
/// [`apply_materialized_local_app_collections_with_identity`] -- never as
/// authority by itself. A forged `app_id` naming an unrelated or
/// non-scaffolded app, or one not actually pinned to the host-verified
/// `workflow_id`, fails the resolve below and yields `None`, same as any
/// other unverifiable checkpoint.
fn resolve_adopted_local_app_build_scope(
    app_data_root: &std::path::Path,
    workflow_id: &str,
    script_bytes: &[u8],
    script_sha256: Option<&str>,
    script_is_verbatim_builtin: Option<bool>,
    args_json: Option<&str>,
) -> Option<tasks::scope::LocalAppWorkflowTaskScope> {
    if workflow_id != crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID {
        return None;
    }
    if script_is_verbatim_builtin != Some(true) {
        return None;
    }
    let expected_hash = script_sha256.filter(|hash| is_sha256_hex(hash))?;
    if sha256_hex(script_bytes) != expected_hash {
        return None;
    }
    let args_value: serde_json::Value = serde_json::from_str(args_json?).ok()?;
    let app_id = args_value
        .get("app_id")
        .and_then(serde_json::Value::as_str)?;
    if app_id.trim().is_empty() {
        return None;
    }
    let layout = local_apps::AppLayout::new(app_data_root, app_id).ok()?;
    let build_target = crate::mobile::local_apps_build::detect_build_target(&layout).ok()?;
    let manifest = local_apps::load_manifest(&layout).ok()?;
    let binding = manifest.runtime_profile.as_ref()?;
    manifest.dependency_snapshot.as_ref()?;
    let plugin_binding =
        crate::mobile::local_app_plugin_binding::LocalAppPluginBinding::resolve(build_target);
    plugin_binding
        .enforce(app_id, binding.family, workflow_id)
        .ok()?;
    tasks::scope::LocalAppWorkflowTaskScope::for_build(app_id).ok()
}

/// Re-mint the Host scope for a restart-recovered verified Local App
/// workflow. Build and UseTest share the same authenticated checkpoint proof
/// (verbatim plugin bytes, exact hash, real scaffold/profile/dependencies),
/// but receive their distinct typed purposes. Custom checkpoints and forged
/// app ids therefore remain unscoped.
fn resolve_adopted_local_app_scope(
    app_data_root: &std::path::Path,
    workflow_id: &str,
    script_bytes: &[u8],
    script_sha256: Option<&str>,
    script_is_verbatim_builtin: Option<bool>,
    args_json: Option<&str>,
) -> Option<tasks::scope::LocalAppWorkflowTaskScope> {
    if workflow_id == crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID {
        return resolve_adopted_local_app_build_scope(
            app_data_root,
            workflow_id,
            script_bytes,
            script_sha256,
            script_is_verbatim_builtin,
            args_json,
        );
    }
    if workflow_id != crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID
        || script_is_verbatim_builtin != Some(true)
    {
        return None;
    }
    let expected_hash = script_sha256.filter(|hash| is_sha256_hex(hash))?;
    if sha256_hex(script_bytes) != expected_hash {
        return None;
    }
    let args_value: serde_json::Value = serde_json::from_str(args_json?).ok()?;
    let app_id = args_value
        .get("app_id")
        .and_then(serde_json::Value::as_str)
        .filter(|app_id| !app_id.trim().is_empty())?;
    let layout = local_apps::AppLayout::new(app_data_root, app_id).ok()?;
    let _build_target = crate::mobile::local_apps_build::detect_build_target(&layout).ok()?;
    let manifest = local_apps::load_manifest(&layout).ok()?;
    manifest.runtime_profile.as_ref()?;
    manifest.dependency_snapshot.as_ref()?;
    // UseTest is only meaningful against the successful build-selected
    // contract; this also prevents a torn/scaffold-only app from borrowing
    // the QA gate during restart adoption.
    let active_digest =
        crate::mobile::local_apps_build::active_build_authoring_contract_sha256(&layout)
            .ok()
            .flatten()?;
    if active_digest.is_empty() {
        return None;
    }
    crate::mobile::local_apps_build::active_authoring_contract(&layout)
        .ok()
        .flatten()?;
    tasks::scope::LocalAppWorkflowTaskScope::for_use_test(app_id).ok()
}

#[async_trait::async_trait]
impl tool_workflow::WorkflowLauncher for MobileWorkflowLauncher {
    async fn launch(
        &self,
        mut spec: tool_workflow::WorkflowLaunchSpec,
    ) -> Result<tool_workflow::WorkflowLaunched, tool_workflow::WorkflowLaunchError> {
        let cwd = self
            .current_cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let abs = |p: &str| -> std::path::PathBuf {
            let path = std::path::Path::new(p);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            }
        };
        let script = tool_workflow::resolve_script_at(
            &cwd,
            &spec,
            |p| std::fs::read_to_string(abs(p)),
            Some(self.plugin_workflows.as_ref()),
        )?;
        let verified_plugin_workflow =
            is_verified_plugin_workflow(&spec, &script, self.plugin_workflows.as_ref());
        if verified_plugin_workflow {
            validate_namespaced_local_app_external_args(&spec)?;
        }
        // Reject a malformed `meta` block at the tool boundary; the byte-exact
        // message surfaces to the model as the tool error (desktop parity).
        workflow::validate_meta(&script).map_err(|e| {
            let msg = match e {
                workflow::WorkflowError::Script(m) => m,
                other => other.to_string(),
            };
            tool_workflow::WorkflowLaunchError(msg)
        })?;
        // Determinism gate: an INLINE `script` may not use
        // Date.now()/Math.random()/new Date() (breaks resume);
        // author-controlled `scriptPath`/`name` files are exempt.
        let is_inline = spec.script.as_deref().is_some_and(|s| !s.is_empty())
            && spec
                .script_path
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_none();
        if is_inline {
            if let Err(workflow::WorkflowError::Script(m)) = workflow::check_determinism(&script) {
                return Err(tool_workflow::WorkflowLaunchError(m));
            }
        }
        if let Err(error) = workflow::validate_body(&script) {
            let error = match error {
                workflow::WorkflowError::Engine(message)
                | workflow::WorkflowError::Script(message) => message,
            };
            let run_id = tool_workflow::mint_run_id(spec.resume_from_run_id.as_deref());
            let workflow_name = workflow::meta_string_value(&script, "name");
            let summary = workflow::meta_string_value(&script, "description");
            return Ok(tool_workflow::WorkflowLaunched {
                task_id: tasks::generate_task_id(tasks::TaskType::LocalWorkflow),
                run_id: Some(run_id),
                workflow_name,
                summary,
                error: Some(error),
                ..Default::default()
            });
        }
        // One launch belongs to exactly one session. Capture the live session
        // before consulting resume provenance so a concurrent retarget cannot
        // split its task row, checkpoint, and transcript directory across two
        // conversations.
        let session_uuid = spec
            .session_uuid
            .take()
            .filter(|session| !session.is_empty())
            .or_else(|| self.session_uuid.lock().ok().map(|guard| guard.clone()))
            .unwrap_or_default();
        // Resume gate (errorCode 3): a `resumeFromRunId` naming a
        // STILL-RUNNING workflow is rejected — two runs sharing a run id
        // would race on the same journal.
        if let Some(rid) = spec.resume_from_run_id.as_deref().filter(|s| !s.is_empty()) {
            // The tool schema advertises `^wf_[a-z0-9-]{6,}$`, but nothing in
            // the workspace validates JSON-Schema `pattern` — and this id
            // becomes a FILENAME (the persisted script below, and the journal
            // in the task handler). An unchecked `../…` or absolute value
            // would write outside the scratch dir, e.g. over the workspace's
            // host-managed `lib/lingxi-bridge.js`.
            if !tool_workflow::is_valid_run_id(rid) {
                return Err(tool_workflow::WorkflowLaunchError(format!(
                    "resumeFromRunId {rid:?} is not a workflow run id (expected wf_ followed by \
                     at least 6 lowercase alphanumerics or dashes)"
                )));
            }
            if let Some(task_id) = self.registry.find_running_workflow_by_run_id(rid).await {
                return Err(tool_workflow::WorkflowLaunchError(format!(
                    "Workflow {rid} is still running (task {task_id}). Stop it first with \
                     TaskStop({{taskId: \"{task_id}\"}}) before resuming."
                )));
            }
        }
        // Mobile is the host authority for local-app persistence. Rewrite the
        // task-local list before serializing args into the TaskRegistry so the
        // QuickJS workflow can never receive a caller-supplied `[]` for an app
        // whose materialized manifest declares collections. Current bundled
        // bytes identify the built-in even after a terminal checkpoint has
        // been cleaned up. A non-exact resume is classified from the
        // host-owned transcript sidecar first, then its legacy checkpoint,
        // and finally the same-session scratch path when no durable row
        // remains. These checks bind the workflow id, run id, script path,
        // session, recorded script identity, and provenance marker. A
        // markerless legacy script is rejected before it can execute. Non-local
        // and direct custom workflows remain byte-for-byte unchanged.
        let resume_resolution = match (
            spec.resume_from_run_id
                .as_deref()
                .filter(|run_id| !run_id.is_empty()),
            spec.script_path
                .as_deref()
                .filter(|path| !path.is_empty())
                .map(abs),
        ) {
            (Some(run_id), Some(script_path)) => local_app_resume_resolution_for_launch(
                &self.checkpoints,
                &session_uuid,
                run_id,
                &script_path,
                &script,
            ),
            (Some(run_id), None) if verified_plugin_workflow => {
                let persisted = self
                    .checkpoints
                    .persisted_workflow_script_path(&session_uuid, run_id);
                local_app_resume_resolution_for_launch(
                    &self.checkpoints,
                    &session_uuid,
                    run_id,
                    &persisted,
                    &script,
                )
                .filter(|resolution| {
                    resolution.provenance == LocalAppResumeProvenance::TrustedPlugin
                        && resolution.workflow_id.as_deref() == spec.name.as_deref()
                })
            }
            _ => None,
        };
        // The host-owned answer to "WHICH plugin workflow is this resume
        // re-entering", now resolvable for all three plugin workflows rather
        // than only the build one (r4-workflow-runtime-05). Only ever `Some`
        // for a `TrustedPlugin` resolution.
        let expected_workflow_id = resume_resolution
            .as_ref()
            .and_then(|resolution| resolution.workflow_id.as_deref());
        // `trusted_local_app_resume` is BUILD-specific: it feeds
        // `is_mobile_local_app_builtin`, which grants the build path's
        // args rewrite and `LocalAppWorkflowTaskScope::for_build`. A use-test
        // or MCP-authoring resume must NOT flip it on -- that would hand the
        // build workflow's authority to a different script.
        let trusted_local_app_resume = matches!(
            resume_resolution
                .as_ref()
                .map(|resolution| resolution.provenance),
            Some(LocalAppResumeProvenance::TrustedPlugin)
        ) && expected_workflow_id
            == Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID);
        // What the build-path seam is allowed to see: it refuses any expected
        // id other than the build one by design, so a use-test/MCP-authoring
        // resume reaches it as "not a build launch" (`None`) instead of as a
        // launch error.
        let expected_build_workflow_id = expected_workflow_id
            .filter(|id| *id == crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID);
        // Mint the run id before enrichment so any Host-injected run-scoped
        // state is bound to the same id the task receives. Host context is
        // injected before the args are serialized into TaskRegistry.
        let run_id = tool_workflow::mint_run_id(spec.resume_from_run_id.as_deref());
        sanitize_namespaced_local_app_args(&mut spec, expected_workflow_id);
        // The Host's own answer to "is this a Local App build, and of which
        // app": minted from the resolved app/binding, never from `spec.args`
        // or the workflow name. `None` for every other launch, which leaves
        // the task row authority-free (no workspace lease, no delete block).
        // A fresh launch proves this through `spec.name`; a `scriptPath`
        // resume -- the exact shape the tool's own resume hint produces,
        // `{scriptPath, resumeFromRunId}` with no `name` -- proves it instead
        // through `expected_workflow_id`, the host-owned checkpoint
        // provenance resolved above. Either must inject the same key.
        let is_build_workflow_launch = spec.name.as_deref().is_some_and(|name| {
            name == crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID
        }) || expected_workflow_id
            == Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID);
        if is_build_workflow_launch {
            if let Some(object) = spec.args.as_mut().and_then(Value::as_object_mut) {
                object.insert("workflow_run_id".into(), Value::String(run_id.clone()));
            }
        }
        let mut local_app_scope = apply_materialized_local_app_collections_with_identity(
            &self.app_data_root,
            &mut spec,
            &script,
            trusted_local_app_resume,
            expected_build_workflow_id,
            verified_plugin_workflow,
        )?;
        // The create path receives the run id as a Host-injected field only;
        // it is not part of the external Workflow tool contract.
        if is_build_workflow_launch {
            if let Some(context) = spec
                .args
                .as_mut()
                .and_then(Value::as_object_mut)
                .and_then(|object| object.get_mut("host_context"))
                .and_then(Value::as_object_mut)
            {
                context.insert("workflow_run_id".into(), Value::String(run_id.clone()));
            }
        }
        // A by-name launch proves the plugin identity through
        // `verified_plugin_workflow` (registry bytes re-read and compared); a
        // `scriptPath` resume proves it through `expected_workflow_id`, the
        // host-owned checkpoint provenance. Both must reach the enrichment,
        // or the resumed script runs with no `host_context` at all.
        enrich_persisted_plugin_workflow_context(
            &self.app_data_root,
            &mut spec,
            &run_id,
            verified_plugin_workflow,
            expected_workflow_id,
        )?;
        let is_mcp_authoring_launch = is_mcp_authoring_launch(
            spec.name.as_deref(),
            verified_plugin_workflow,
            expected_workflow_id,
        );
        let is_use_test_launch = is_use_test_launch(
            spec.name.as_deref(),
            verified_plugin_workflow,
            expected_workflow_id,
        );
        if local_app_scope.is_none() && (is_mcp_authoring_launch || is_use_test_launch) {
            if let Some(app_id) = spec
                .args
                .as_ref()
                .and_then(Value::as_object)
                .and_then(|object| object.get("app_id"))
                .and_then(Value::as_str)
            {
                local_app_scope = Some(if is_use_test_launch {
                    tasks::scope::LocalAppWorkflowTaskScope::for_use_test(app_id).map_err(
                        |error| {
                            tool_workflow::WorkflowLaunchError(format!(
                                "cannot authorize local-app use-test workflow: {error}"
                            ))
                        },
                    )?
                } else {
                    tasks::scope::LocalAppWorkflowTaskScope::for_mcp_authoring(app_id).map_err(
                        |error| {
                            tool_workflow::WorkflowLaunchError(format!(
                                "cannot authorize local-app MCP authoring workflow: {error}"
                            ))
                        },
                    )?
                });
            }
        }
        let workflow_name = workflow::meta_string_value(&script, "name");
        let summary = workflow::meta_string_value(&script, "description");
        let task_description = summary
            .clone()
            .unwrap_or_else(|| "Dynamic workflow".to_string());
        // Reserve before the first run-id-derived filesystem write. The async
        // block below collects every later error so the reservation is always
        // released exactly once.
        let reservation = self
            .registry
            .try_reserve_workflow_run_id(&run_id)
            .await
            .map_err(|error| tool_workflow::WorkflowLaunchError(error.to_string()))?;
        let launch_result = async {
            let subagents = orchestrator::transcript_paths::subagents_dir(
                &self.lingxi_home,
                &self.project_cwd.to_string_lossy(),
                &session_uuid,
            );
            // Persist the script so it is editable + re-runnable via `scriptPath`.
            // A `scriptPath` input is already on disk → returned as-is; an
            // inline/`name` script is written under the session-owned workflow dir.
            let script_path = if let Some(p) = spec.script_path.as_deref().filter(|s| !s.is_empty())
            {
                abs(p).to_str().map(str::to_string).ok_or_else(|| {
                    tool_workflow::WorkflowLaunchError(
                        "workflow script path is not valid UTF-8".to_string(),
                    )
                })?
            } else {
                let session_dir = subagents.parent().ok_or_else(|| {
                    tool_workflow::WorkflowLaunchError(
                        "cannot derive workflow session directory".to_string(),
                    )
                })?;
                let dir = session_dir.join("workflows");
                let file = dir.join(format!("{run_id}.js"));
                std::fs::create_dir_all(&dir).map_err(|error| {
                    tool_workflow::WorkflowLaunchError(format!(
                        "cannot create workflow script directory '{}': {error}",
                        dir.display()
                    ))
                })?;
                std::fs::write(&file, &script).map_err(|error| {
                    tool_workflow::WorkflowLaunchError(format!(
                        "cannot persist workflow script '{}': {error}",
                        file.display()
                    ))
                })?;
                file.to_str().map(str::to_string).ok_or_else(|| {
                    tool_workflow::WorkflowLaunchError(
                        "workflow script path is not valid UTF-8".to_string(),
                    )
                })?
            };
            let transcript_dir = { subagents.join("workflows").join(&run_id) };
            std::fs::create_dir_all(&transcript_dir).map_err(|error| {
                tool_workflow::WorkflowLaunchError(format!(
                    "cannot create workflow transcript directory '{}': {error}",
                    transcript_dir.display()
                ))
            })?;
            let journal_path = transcript_dir.join("journal.jsonl");
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&journal_path)
                .map_err(|error| {
                    tool_workflow::WorkflowLaunchError(format!(
                        "cannot create workflow journal '{}': {error}",
                        journal_path.display()
                    ))
                })?;
            let transcript_dir_wire = transcript_dir.to_str().map(str::to_string);
            let has_script_path = spec
                .script_path
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_some();
            let has_name = spec.name.as_deref().filter(|s| !s.is_empty()).is_some();
            let named_source = spec
                .name
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(|name| {
                    tool_workflow::workflow_source_for_name(
                        &cwd,
                        name,
                        Some(self.plugin_workflows.as_ref()),
                    )
                });
            // "This launch ran the Plugin's OWN bytes." For the build
            // workflow that is `is_mobile_local_app_builtin`; for the
            // use-test and MCP-authoring workflows it is
            // `verified_plugin_workflow`, which is the same registry-bytes
            // comparison for the other two ids (r4-workflow-runtime-05).
            // Recording it for all three is what lets a later `scriptPath`
            // resume of use-test/mcp-authoring be told apart from a custom
            // script that merely copied a plugin-qualified `meta.name` --
            // see `local_app_resume_resolution_for_record`.
            let script_is_verbatim_builtin = is_mobile_local_app_builtin(
                &spec,
                trusted_local_app_resume,
                verified_plugin_workflow,
            ) || verified_plugin_workflow;
            let (invocation_mode, workflow_source) = if has_script_path {
                ("scriptPath".to_string(), "scriptPath".to_string())
            } else if has_name {
                (
                    "named".to_string(),
                    if named_source.is_some() {
                        named_source.unwrap_or("custom").to_string()
                    } else {
                        "custom".to_string()
                    },
                )
            } else {
                ("inline".to_string(), "inline".to_string())
            };
            let script_sha256 = sha256_hex(script.as_bytes());
            let namespaced_plugin_workflow = verified_plugin_workflow
                .then(|| spec.name.as_deref().expect("verified Plugin workflow name"))
                .map(str::to_string);
            let workflow_id = namespaced_plugin_workflow
                .or_else(|| workflow_name.clone().filter(|name| !name.is_empty()))
                .or_else(|| spec.name.clone())
                .unwrap_or_default();
            // Fresh local-app launches leave a durable provenance record beside
            // the journal. Resumes never rewrite this record: a mismatched
            // caller path must not be able to replace a trusted record with a
            // new custom identity.
            if spec
                .resume_from_run_id
                .as_deref()
                .filter(|run_id| !run_id.is_empty())
                .is_none()
                && !session_uuid.is_empty()
                && verified_plugin_workflow
            {
                self.checkpoints
                    .write_provenance_sidecar(
                        &session_uuid,
                        &run_id,
                        std::path::Path::new(&script_path),
                        &script,
                        &workflow_id,
                        script_is_verbatim_builtin,
                    )
                    .map_err(|error| {
                        tool_workflow::WorkflowLaunchError(format!(
                            "cannot persist workflow provenance: {error}"
                        ))
                    })?;
            }
            let task_id = self
                .registry
                .spawn(
                    tasks::TaskType::LocalWorkflow,
                    tasks::TaskSpawnInput::LocalWorkflow {
                        session_uuid: Some(session_uuid.clone()),
                        workflow_id: workflow_id.clone(),
                        script,
                        resume_from_run_id: spec.resume_from_run_id.clone(),
                        args: spec
                            .args
                            .as_ref()
                            .map(|v| serde_json::to_string(v).unwrap_or_default()),
                        run_id: Some(run_id.clone()),
                        invocation_mode: Some(invocation_mode),
                        workflow_source: Some(workflow_source),
                        script_is_verbatim_builtin: Some(script_is_verbatim_builtin),
                        transcript_subdir: Some(transcript_dir.clone()),
                        launched_from_subagent: spec.launched_from_subagent,
                        tool_use_id: spec.tool_use_id.clone(),
                        creator_teammate_name: spec.creator_teammate_name.clone(),
                        creator_team_name: spec.creator_team_name.clone(),
                        creator_agent_id: spec
                            .creator_agent_id
                            .as_deref()
                            .and_then(lingxi_core::types::AgentId::parse_prefixed),
                        scope: local_app_scope,
                    },
                    task_description,
                )
                .await
                .map_err(|e| tool_workflow::WorkflowLaunchError(e.to_string()))?;
            self.registry
                .set_workflow_resume_metadata(&task_id, script_path.clone(), transcript_dir.clone())
                .await
                .map_err(|error| tool_workflow::WorkflowLaunchError(error.to_string()))?;
            let args_json = spec
                .args
                .as_ref()
                .map(|value| serde_json::to_string(value).unwrap_or_default());
            let pending_events = match self.checkpoints.upsert(
                &session_uuid,
                WorkflowCheckpoint {
                    task_id: task_id.clone(),
                    workflow_run_id: run_id.clone(),
                    workflow_id: workflow_id.clone(),
                    script_path: script_path.clone(),
                    script_sha256: Some(script_sha256),
                    script_is_verbatim_builtin: Some(script_is_verbatim_builtin),
                    args_json,
                    description: summary
                        .clone()
                        .or_else(|| workflow_name.clone())
                        .unwrap_or_else(|| "Workflow".to_string()),
                    start_time: Some(unix_time_ms()),
                    transcript_dir: transcript_dir.to_string_lossy().into_owned(),
                },
            ) {
                Ok(pending_events) => pending_events,
                Err(error) => {
                    let _ = self.registry.kill(&task_id).await;
                    return Err(tool_workflow::WorkflowLaunchError(format!(
                        "cannot persist workflow checkpoint: {error}"
                    )));
                }
            };
            self.status_sink
                .flush_pending_events(&task_id, pending_events)
                .await;
            if let Some(state) = self.registry.get(&task_id).await {
                if state.base().status.is_terminal() {
                    self.checkpoints.remove_task(&task_id);
                }
            }
            if spec.resume_from_run_id.is_some() {
                self.registry
                    .remove_paused_workflow_by_run_id(&session_uuid, &run_id)
                    .await;
            }
            Ok(tool_workflow::WorkflowLaunched {
                task_id,
                run_id: Some(run_id.clone()),
                script_path: Some(script_path),
                workflow_name,
                summary,
                transcript_dir: transcript_dir_wire,
                error: None,
            })
        }
        .await;
        drop(reservation);
        launch_result
    }
}

/// Add the same Host-owned identity envelope to the Plugin's standalone
/// use-test and MCP-authoring workflows. Unlike create, both are only valid
/// for an already scaffolded app, so the persisted manifest/profile and
/// dependency snapshot are fail-closed prerequisites.
///
/// r4-workflow-runtime-05: identity comes from EITHER of the two host-owned
/// proofs, never from `spec.name` alone.
///
/// * a fresh or by-name-resumed launch proves it with
///   `verified_plugin_workflow` — `is_verified_plugin_workflow` re-reads the
///   Plugin registry's current bytes and compares them to the script this
///   launch resolved;
/// * a `scriptPath` resume — the shape the Workflow tool's own resume hint
///   produces, `{scriptPath, resumeFromRunId}` with NO `name` — proves it with
///   `resumed_workflow_id`, the checkpoint-provenance answer from
///   [`local_app_resume_resolution_for_record`] (host-written sidecar, verbatim
///   marker, exact script hash).
///
/// Gating on `spec.name` alone is what made a `scriptPath` resume of either
/// script return here at the first line: no `host_context`, no
/// `workflow_run_id`, no `runtime_profile`, so the script threw on its first
/// context statement. Note the consequence of fixing it: a resume whose args
/// no longer carry `app_id` now fails at the launch boundary with a message
/// naming what is missing, exactly as the sibling build path already does,
/// instead of failing inside QuickJS.
fn enrich_persisted_plugin_workflow_context(
    app_data_root: &std::path::Path,
    spec: &mut tool_workflow::WorkflowLaunchSpec,
    run_id: &str,
    verified_plugin_workflow: bool,
    resumed_workflow_id: Option<&str>,
) -> Result<(), tool_workflow::WorkflowLaunchError> {
    let name = verified_plugin_workflow
        .then_some(spec.name.as_deref())
        .flatten()
        .or(resumed_workflow_id);
    let Some(name) = name else {
        return Ok(());
    };
    if !matches!(
        name,
        crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID
            | crate::mobile::local_app_plugin_binding::PLUGIN_MCP_AUTHORING_WORKFLOW_ID
    ) {
        return Ok(());
    }
    // Own the id: it may borrow `spec.name`, and the rest of this function
    // takes `spec.args` mutably.
    let name = name.to_string();
    let args = spec.args.as_mut().ok_or_else(|| {
        tool_workflow::WorkflowLaunchError(format!("{name} requires Host-enriched args"))
    })?;
    let object = args.as_object_mut().ok_or_else(|| {
        tool_workflow::WorkflowLaunchError(format!("{name} args must be an object"))
    })?;
    let app_id = object
        .get("app_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| tool_workflow::WorkflowLaunchError(format!("{name} requires app_id")))?
        .to_string();
    let layout = local_apps::AppLayout::new(app_data_root, &app_id).map_err(|error| {
        tool_workflow::WorkflowLaunchError(format!("cannot resolve app {app_id:?}: {error}"))
    })?;
    let manifest = local_apps::load_manifest(&layout).map_err(|error| {
        tool_workflow::WorkflowLaunchError(format!("cannot load persisted app profile: {error}"))
    })?;
    // Resolve the Host-owned scaffold/profile pair before any standalone
    // plugin workflow can mint an app scope. A valid-looking app_id and
    // manifest alone must not authorize a custom/torn shell.
    let _build_target =
        crate::mobile::local_apps_build::detect_build_target(&layout).map_err(|error| {
            tool_workflow::WorkflowLaunchError(format!(
                "cannot validate persisted Local App runtime profile: {error}"
            ))
        })?;
    let active_authoring_contract =
        crate::mobile::local_apps_build::active_authoring_contract(&layout).map_err(|error| {
            tool_workflow::WorkflowLaunchError(format!(
                "cannot load active Local App authoring contract: {error}"
            ))
        })?;
    let active_authoring_contract_sha256 =
        crate::mobile::local_apps_build::active_build_authoring_contract_sha256(&layout).map_err(
            |error| {
                tool_workflow::WorkflowLaunchError(format!(
                    "cannot load active Local App authoring contract digest: {error}"
                ))
            },
        )?;
    let catalog = crate::mobile::local_app_template_catalog::catalog_view().map_err(|error| {
        tool_workflow::WorkflowLaunchError(format!(
            "cannot read verified template catalog: {error}"
        ))
    })?;
    let binding = manifest.runtime_profile.as_ref();
    let runtime_profile = binding.map(|binding| {
        serde_json::json!({
            "family": binding.family.as_str(),
            "revision": binding.revision,
            "contract_sha256": binding.contract_sha256,
            "surface": binding.family.surface().as_str(),
        })
    });
    let expected_writable_collections = Value::Array(
        manifest
            .collections
            .iter()
            .map(|collection| Value::String(collection.id.clone()))
            .collect(),
    );
    let schemas = checked_in_local_app_schemas()?;
    object.insert("workflow_run_id".into(), Value::String(run_id.into()));
    if let Some(runtime_profile) = runtime_profile.clone() {
        object.insert("runtime_profile".into(), runtime_profile);
    }
    object.insert(
        "expected_writable_collections".into(),
        expected_writable_collections.clone(),
    );
    let invocation_capability = format!("mcpv_{}", uuid::Uuid::new_v4().simple());
    if name == crate::mobile::local_app_plugin_binding::PLUGIN_MCP_AUTHORING_WORKFLOW_ID {
        object.insert(
            "host_context".into(),
            serde_json::json!({
                "source": "verified_host",
                "operation": if manifest.active_mcp_catalog.is_some() { "revise" } else { "initial" },
                "app_id": app_id,
                "workflow_run_id": run_id,
                "invocation_capability": invocation_capability,
                "schemas": schemas.clone(),
                "runtime_profile": runtime_profile.unwrap_or(Value::Null),
                "authoring_contract": active_authoring_contract
                    .as_ref()
                    .map(|contract| serde_json::to_value(contract).unwrap_or(Value::Null))
                    .unwrap_or(Value::Null),
                "authoring_contract_sha256": active_authoring_contract_sha256,
                "template_catalog": {
                    "catalog_digest": catalog.catalog_digest,
                    "available_template_ids": catalog.templates.iter().map(|entry| entry.template_id.clone()).collect::<Vec<_>>(),
                },
                "expected_writable_collections": expected_writable_collections,
                "dependency_snapshot": {"verified": manifest.dependency_snapshot.is_some()},
                "active_catalog": manifest.active_mcp_catalog.clone(),
            }),
        );
        return Ok(());
    }
    let active_authoring_contract_sha256 =
        active_authoring_contract_sha256.as_deref().ok_or_else(|| {
            tool_workflow::WorkflowLaunchError(format!(
                "{name} requires an active Host authoring contract digest"
            ))
        })?;
    let Some(binding) = binding else {
        return Err(tool_workflow::WorkflowLaunchError(format!(
            "{name} requires a persisted runtime profile"
        )));
    };
    if manifest.dependency_snapshot.is_none() {
        return Err(tool_workflow::WorkflowLaunchError(format!(
            "{name} requires a verified dependency snapshot"
        )));
    }
    let active_authoring_contract = active_authoring_contract.as_ref().ok_or_else(|| {
        tool_workflow::WorkflowLaunchError(format!(
            "{name} requires an active Host authoring contract"
        ))
    })?;
    object.insert(
        "host_context".into(),
        serde_json::json!({
            "source": "verified_host",
            "operation": "verify",
            "app_id": app_id,
            "workflow_run_id": run_id,
            "schemas": schemas,
            "runtime_profile": {
                "family": binding.family.as_str(),
                "revision": binding.revision,
                "contract_sha256": binding.contract_sha256,
                "surface": binding.family.surface().as_str(),
            },
            "authoring_contract": serde_json::to_value(active_authoring_contract)
                .unwrap_or(Value::Null),
            "authoring_contract_sha256": active_authoring_contract_sha256,
            "template_catalog": {
                "catalog_digest": catalog.catalog_digest,
                "available_template_ids": catalog.templates.iter().map(|entry| entry.template_id.clone()).collect::<Vec<_>>(),
            },
            "expected_writable_collections": object.get("expected_writable_collections").cloned().unwrap_or(Value::Array(Vec::new())),
            "dependency_snapshot": {"verified": true},
        }),
    );
    Ok(())
}

/// Remove all authority-bearing fields from caller args before a namespaced
/// Local App workflow is enriched, on every launch shape this sanitizer can
/// currently recognize as such a workflow. The workflow receives these
/// values only from this launch boundary; allowing a caller-provided
/// `runtime_profile`, template handle, collection list or context to
/// survive on a recognized shape would turn the Plugin script into its own
/// authority source.
///
/// `spec.name` proves this on a fresh launch, but a `scriptPath` resume --
/// the shape the Workflow tool's own resume hint produces -- carries no
/// `name` at all. `expected_workflow_id` is the launcher's host-owned
/// checkpoint-provenance answer for that case. r4-workflow-runtime-05
/// extended `local_app_resume_resolution_for_record` to resolve all three
/// plugin workflow ids, so this sanitizer now fires on a `scriptPath` resume
/// of the use-test and mcp-authoring workflows too, not only the build one.
///
/// `validate_namespaced_local_app_external_args` still keys off `spec.name`
/// and therefore still does not run on ANY `scriptPath` resume (build
/// included) -- that is the pre-existing shape, not a use-test-specific gap:
/// it rejects UNKNOWN caller keys, whereas the authority-bearing keys it
/// would care about are unconditionally stripped here and re-injected by the
/// Host below.
fn sanitize_namespaced_local_app_args(
    spec: &mut tool_workflow::WorkflowLaunchSpec,
    expected_workflow_id: Option<&str>,
) {
    let is_local_app_workflow = spec
        .name
        .as_deref()
        .is_some_and(crate::mobile::local_app_plugin_binding::is_plugin_workflow_id)
        || expected_workflow_id
            .is_some_and(crate::mobile::local_app_plugin_binding::is_plugin_workflow_id);
    if !is_local_app_workflow {
        return;
    }
    let Some(object) = spec.args.as_mut().and_then(Value::as_object_mut) else {
        return;
    };
    for key in [
        "host_context",
        "runtime_profile",
        "expected_writable_collections",
        "selector_capability",
        "validated_selection_handle",
        "template_selection",
        "workflow_run_id",
        "authoring_contract",
        "schemas",
        "dependency_snapshot",
        "template_catalog",
        "active_catalog",
        "invocation_capability",
        "update_ui_impact",
        "staging",
    ] {
        object.remove(key);
    }
}

#[cfg(test)]
mod plugin_args_tests {
    use super::*;

    #[test]
    fn terminal_qa_strategy_comes_only_from_authenticated_workflow_args() {
        assert_eq!(
            workflow_verification_strategy(None).unwrap(),
            local_apps::QaVerificationStrategy::Balanced
        );
        assert_eq!(
            workflow_verification_strategy(Some(r#"{"app_id":"demo"}"#)).unwrap(),
            local_apps::QaVerificationStrategy::Balanced
        );
        assert_eq!(
            workflow_verification_strategy(Some(r#"{"app_id":"demo","quality_level":"thorough"}"#))
                .unwrap(),
            local_apps::QaVerificationStrategy::Thorough
        );
        assert!(workflow_verification_strategy(Some(
            r#"{"app_id":"demo","quality_level":"forged"}"#
        ))
        .is_err());
        assert!(workflow_verification_strategy(Some("not-json")).is_err());
    }

    fn valid_authoring_spec_fixture() -> Value {
        serde_json::json!({
            "product": {
                "goal": "track chores",
                "tasks": ["add a chore"],
                "external_integrations": []
            },
            "targets": [{"id": "primary", "os": "ios", "form_factor": "iphone"}],
            "ui": {
                "structure": ["list"],
                "theme": {"mode": "system", "accent": "blue"},
                "style": {"direction": "calm", "density": "comfortable"},
                "references": []
            },
            "design": {
                "presentations": [{"target_id": "primary", "presentation": "single list", "navigation": "push detail"}],
                "tokens": {"accent": "blue"},
                "states": {"loading": "spinner", "empty": "no chores", "error": "retry", "success": "saved", "permission": "explain"},
                "inputs": {"pointer_touch": ["tap"], "keyboard_mouse": ["enter"], "back": "navigate back", "reduced_motion": "remove transitions"},
                "canvas": null
            },
            "acceptance_checks": [{
                "id": "primary-action",
                "target_ids": ["primary"],
                "required": true,
                "preconditions": ["list loaded"],
                "steps": ["tap add"],
                "expected": "chore appears",
                "evidence": ["inspect", "ui_action"]
            }]
        })
    }

    #[test]
    fn update_ui_impact_is_derived_from_active_and_incoming_specs() {
        let spec_value = valid_authoring_spec_fixture();
        let spec: local_apps::AppAuthoringSpec =
            serde_json::from_value(spec_value.clone()).expect("fixture spec");
        let active = local_apps::AppAuthoringContract {
            version: local_apps::AUTHORING_SCHEMA_VERSION,
            revision: 1,
            app_id: "impact123".into(),
            runtime_profile: crate::mobile::local_app_runtime_profiles::current_binding_for_family(
                local_apps::AppRuntimeProfile::ReactDom,
            )
            .expect("runtime profile"),
            spec: spec.clone(),
        };
        assert!(!update_ui_impact(&active, &spec_value).expect("same spec"));

        let mut ui_changed = spec_value.clone();
        ui_changed["ui"]["structure"] = serde_json::json!(["grid"]);
        assert!(update_ui_impact(&active, &ui_changed).expect("ui change"));

        let mut targets_changed = spec_value.clone();
        targets_changed["targets"][0]["form_factor"] = serde_json::json!("ipad");
        assert!(update_ui_impact(&active, &targets_changed).expect("target change"));

        let mut design_changed = spec_value.clone();
        design_changed["design"]["tokens"]["accent"] = serde_json::json!("green");
        assert!(update_ui_impact(&active, &design_changed).expect("design change"));

        let mut product_only = spec_value.clone();
        product_only["product"]["goal"] = serde_json::json!("track household chores");
        assert!(!update_ui_impact(&active, &product_only).expect("product-only change"));
    }

    #[test]
    fn checked_in_schema_loader_exports_mcp_promoter_role() {
        let schemas = checked_in_local_app_schemas().expect("checked-in schemas");
        assert!(schemas
            .get("mcp_promoter")
            .and_then(Value::as_object)
            .is_some_and(|schema| schema.get("$defs").is_some()));
    }

    #[test]
    fn authoring_spec_requires_structured_schema_owned_intent() {
        let fixture = valid_authoring_spec_fixture();
        let args = serde_json::json!({"authoring_spec": fixture});
        let parsed = authoring_spec_from_args(args.as_object().unwrap())
            .expect("valid fixture")
            .expect("structured spec");
        assert_eq!(parsed, args["authoring_spec"]);

        let legacy = serde_json::json!({"authoring_spec": "track chores"});
        assert!(authoring_spec_from_args(legacy.as_object().unwrap()).is_err());

        let forged = serde_json::json!({
            "authoring_spec": {
                "product": fixture["product"].clone(),
                "targets": fixture["targets"].clone(),
                "ui": fixture["ui"].clone(),
                "design": fixture["design"].clone(),
                "acceptance_checks": fixture["acceptance_checks"].clone(),
                "runtime_profile": {"family": "babylon_3d"}
            }
        });
        assert!(authoring_spec_from_args(forged.as_object().unwrap()).is_err());
    }

    #[test]
    fn authoring_spec_rejects_schema_shaped_semantic_errors_before_launch() {
        let valid = valid_authoring_spec_fixture();
        let mut duplicate = valid.clone();
        let mut second_screen = duplicate["design"]["presentations"][0].clone();
        second_screen["presentation"] = serde_json::json!("settings screen");
        duplicate["design"]["presentations"]
            .as_array_mut()
            .unwrap()
            .push(second_screen);
        let mut unknown = valid.clone();
        unknown["design"]["presentations"][0]["target_id"] = serde_json::json!("missing");
        let mut whitespace = valid.clone();
        whitespace["design"]["states"]["loading"] = serde_json::json!("   ");
        let mut optional_only = valid.clone();
        optional_only["acceptance_checks"][0]["required"] = serde_json::json!(false);
        let schema: Value = serde_json::from_str(AUTHORING_SPEC_SCHEMA_JSON).unwrap();
        for (candidate, reason) in [
            (duplicate, "duplicate presentation for target"),
            (unknown, "unknown target"),
            (whitespace, "design.states.loading"),
            (optional_only, "at least one required acceptance check"),
        ] {
            assert!(
                local_apps::value_matches_schema(&candidate, &schema),
                "fixture must reproduce the old schema-only gap"
            );
            let args = serde_json::json!({"authoring_spec": candidate});
            let error = authoring_spec_from_args(args.as_object().unwrap())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("authoring_spec is invalid") && error.contains(reason),
                "{error}"
            );
        }
    }

    #[test]
    fn namespaced_local_app_authority_fields_are_host_owned() {
        let mut spec = tool_workflow::WorkflowLaunchSpec {
            name: Some("lingxi-local-app:local-app-build".into()),
            args: Some(serde_json::json!({
                "operation": "update",
                "app_id": "aaaa1111",
                "runtime_profile": {"family": "babylon_3d"},
                "host_context": {"source": "caller"},
                "validated_selection_handle": "vsel_forged",
                "expected_writable_collections": ["forged"],
                "workflow_run_id": "wf_forged1",
                "schemas": {"qa_report": {"type": "string"}},
                "dependency_snapshot": {"verified": false},
                "template_catalog": {"catalog_digest": "forged"},
                "active_catalog": {"catalog_sha256": "forged"},
                "invocation_capability": "forged",
                "update_ui_impact": true,
                "staging": {"isolated": false}
            })),
            ..Default::default()
        };
        sanitize_namespaced_local_app_args(&mut spec, None);
        let object = spec.args.expect("args");
        assert_eq!(
            object.get("operation").and_then(Value::as_str),
            Some("update")
        );
        assert_eq!(
            object.get("app_id").and_then(Value::as_str),
            Some("aaaa1111")
        );
        for field in [
            "runtime_profile",
            "host_context",
            "validated_selection_handle",
            "expected_writable_collections",
            "workflow_run_id",
            "schemas",
            "dependency_snapshot",
            "template_catalog",
            "active_catalog",
            "invocation_capability",
            "update_ui_impact",
            "staging",
        ] {
            assert!(object.get(field).is_none(), "{field} must be stripped");
        }
    }

    #[test]
    fn project_shadow_cannot_borrow_namespaced_plugin_authority() {
        let registry = workflow::PluginWorkflowRegistry::new();
        registry.register(vec![workflow::PluginWorkflowEntry {
            name: "lingxi-local-app:local-app-build".into(),
            script_path: std::path::PathBuf::from("/plugin/local-app-build.js"),
        }]);
        let spec = tool_workflow::WorkflowLaunchSpec {
            name: Some("lingxi-local-app:local-app-build".into()),
            ..Default::default()
        };
        assert!(!is_verified_plugin_workflow(
            &spec,
            "export const meta = { name: 'local-app-build' }; return forged;",
            &registry,
        ));
        assert!(!is_mobile_local_app_builtin(&spec, false, false,));
    }

    /// Pins BOTH halves of `is_mcp_authoring_launch`, the sole production
    /// grant of `LocalAppWorkflowTaskScope::for_mcp_authoring`. The second
    /// case is the narrowing that previously shipped unpinned: before it, a
    /// by-name launch of a project workflow that had simply taken the name
    /// `lingxi-local-app:local-app-mcp-authoring` in the saved-workflow
    /// precedence chain — bytes not matching the Plugin registry, so
    /// `verified_plugin_workflow = false` — still received MCP-authoring
    /// scope. If that `&&` is deleted, case 2 goes red.
    #[test]
    fn mcp_authoring_scope_needs_verified_bytes_or_host_owned_resume() {
        let mcp = crate::mobile::local_app_plugin_binding::PLUGIN_MCP_AUTHORING_WORKFLOW_ID;
        let build = crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID;
        // 1. by-name launch, registry bytes matched -> granted.
        assert!(
            is_mcp_authoring_launch(Some(mcp), true, None),
            "a by-name MCP-authoring launch whose script matched the plugin registry bytes \
             must receive MCP-authoring scope"
        );
        // 2. THE NARROWING: same name, bytes did NOT match -> refused.
        assert!(
            !is_mcp_authoring_launch(Some(mcp), false, None),
            "a project workflow shadowing the MCP-authoring plugin name must NOT borrow \
             MCP-authoring scope on the strength of its name alone"
        );
        // 3. scriptPath resume: no name at all, host-owned checkpoint
        //    provenance says MCP authoring -> granted.
        assert!(
            is_mcp_authoring_launch(None, false, Some(mcp)),
            "a scriptPath resume whose host-owned checkpoint records the MCP-authoring \
             workflow must still receive MCP-authoring scope"
        );
        // 4. the sibling plugin workflows must not leak into this scope, by
        //    either route.
        assert!(!is_mcp_authoring_launch(Some(build), true, None));
        assert!(!is_mcp_authoring_launch(None, false, Some(build)));
        assert!(!is_mcp_authoring_launch(None, true, None));
    }

    /// r4-workflow-runtime-02: a resume BY NAME (`resumeFromRunId` set,
    /// `name` set, no `scriptPath`) re-resolves the script from the plugin
    /// registry the same way a fresh by-name launch does -- so a byte match
    /// (`verified_plugin_workflow = true`) is exactly as strong a proof of
    /// provenance on a resume as on a fresh launch. `is_resume` alone must
    /// not defeat that host-owned bytes match.
    #[test]
    fn resume_by_name_with_verified_plugin_bytes_is_still_a_local_app_builtin() {
        let spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            resume_from_run_id: Some("wf_resumebyname1".into()),
            ..Default::default()
        };
        assert!(
            is_mobile_local_app_builtin(&spec, false, true),
            "a resume by name whose re-resolved script matched the verified plugin registry \
             bytes (verified_plugin_workflow=true) must still be treated as the Local App \
             build builtin"
        );
    }

    #[test]
    fn namespaced_plugin_rejects_renderer_and_workspace_overrides_at_launch() {
        let spec = tool_workflow::WorkflowLaunchSpec {
            name: Some("lingxi-local-app:local-app-build".into()),
            args: Some(serde_json::json!({
                "operation": "create",
                "app_id": "aaaa1111",
                "renderer": "canvas",
                "workspace_path": "/tmp/forged"
            })),
            ..Default::default()
        };
        let error = validate_namespaced_local_app_external_args(&spec)
            .expect_err("renderer/workspace overrides must not reach the Plugin");
        assert!(error.to_string().contains("renderer"));
    }

    #[test]
    fn namespaced_plugin_rejects_authority_override_fields_at_launch() {
        let spec = tool_workflow::WorkflowLaunchSpec {
            name: Some("lingxi-local-app:local-app-build".into()),
            args: Some(serde_json::json!({
                "operation": "create",
                "app_id": "aaaa1111",
                "validated_selection_handle": "vsel_forged",
            })),
            ..Default::default()
        };
        let error = validate_namespaced_local_app_external_args(&spec)
            .expect_err("Host-owned fields must be rejected at the public launch boundary");
        assert!(error.to_string().contains("validated_selection_handle"));
    }

    /// r2-tests-honesty-007 / r3-workflow-runtime-02: the two surviving plugin
    /// workflows. The `local-app-build.js` pair retired with that workflow;
    /// `validate_namespaced_local_app_external_args`
    /// declares a separate accepted-key set for each of the plugin
    /// workflow ids; each script maintains its own declared list in a
    /// different language, so each pair can drift independently of the
    /// others. Checked bidirectionally, and
    /// the Host side of each pair is the exact `*_EXTERNAL_ARG_KEYS` slice the
    /// launch boundary itself consumes (AUD-WF-03) rather than a copy.
    #[test]
    fn use_test_and_mcp_authoring_script_external_contracts_match_the_host() {
        fn parse_js_string_array(script: &str, declaration_prefix: &str) -> Vec<String> {
            let declaration = script
                .lines()
                .find(|line| line.starts_with(declaration_prefix))
                .unwrap_or_else(|| {
                    panic!("script must declare a line starting {declaration_prefix:?}")
                });
            declaration
                .split_once('[')
                .and_then(|(_, rest)| rest.split_once(']'))
                .expect("declaration must be an array literal")
                .0
                .split(',')
                .map(|entry| entry.trim().trim_matches('\'').to_string())
                .filter(|entry| !entry.is_empty())
                .collect()
        }
        let cases: [(&str, &str, &str, &[&str]); 2] = [
            (
                crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID,
                include_str!("../../../plugins/lingxi-local-app/workflows/local-app-use-test.js"),
                "const ALLOWED =",
                USE_TEST_EXTERNAL_ARG_KEYS,
            ),
            (
                crate::mobile::local_app_plugin_binding::PLUGIN_MCP_AUTHORING_WORKFLOW_ID,
                include_str!(
                    "../../../plugins/lingxi-local-app/workflows/local-app-mcp-authoring.js"
                ),
                "const EXTERNAL_KEYS =",
                MCP_AUTHORING_EXTERNAL_ARG_KEYS,
            ),
        ];
        for (workflow_id, script, declaration_prefix, host_accepted) in cases {
            let declared = parse_js_string_array(script, declaration_prefix);
            assert!(
                !declared.is_empty(),
                "sanity: parsing {declaration_prefix:?} out of {workflow_id}'s script produced \
                 an EMPTY list, so every check below would pass vacuously"
            );
            // script -> Host: every key the script declares external must be
            // accepted at the launch boundary (except the workflow's own
            // Host-injected internal keys, which legitimately are NOT
            // external and must stay out of this forward check).
            for key in &declared {
                if key == "host_context"
                    || key == "workflow_run_id"
                    || key == "runtime_profile"
                    || key == "expected_writable_collections"
                {
                    continue;
                }
                let spec = tool_workflow::WorkflowLaunchSpec {
                    name: Some(workflow_id.into()),
                    args: Some(serde_json::json!({ key: "value" })),
                    ..Default::default()
                };
                validate_namespaced_local_app_external_args(&spec).unwrap_or_else(|error| {
                    panic!(
                        "{workflow_id}'s script declares external field {key:?} but the Host \
                         launch boundary rejects it, so it can never reach the script: {error}"
                    )
                });
            }
            // Host -> script: every key the Host's launch boundary accepts
            // for this workflow must be in the script's own declared list,
            // or the script's unknown-field check throws on it.
            assert!(
                !host_accepted.is_empty(),
                "sanity: {workflow_id}'s Host-accepted key slice is EMPTY, so the \
                 Host -> script direction would pass vacuously"
            );
            for key in host_accepted {
                assert!(
                    declared.iter().any(|d| d == key),
                    "the Host launch boundary accepts external field {key:?} for {workflow_id}, \
                     but its script's declared external-key list omits it: {declared:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod run_id_tests {
    use std::sync::Arc;

    use tool_workflow::is_valid_run_id;

    use crate::mobile::test_support::FakeListener;

    fn stamp_profile(
        manifest: &mut local_apps::AppManifest,
        family: local_apps::AppRuntimeProfile,
    ) {
        let binding = crate::mobile::local_app_runtime_profiles::current_binding_for_family(family)
            .expect("published runtime profile");
        manifest.surface = Some(family.surface());
        manifest.runtime_profile = Some(binding.clone());
        manifest.template_origin = Some(local_apps::AppTemplateOrigin {
            plugin_id: local_apps::AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
            plugin_version: "builtin".into(),
            template_id: format!(
                "{}-r{}",
                family.as_str().replace('_', "-"),
                binding.revision
            ),
            template_sha256: binding.contract_sha256.clone(),
        });
        manifest.dependency_snapshot = Some(local_apps::AppDependencySnapshot {
            requested_sha256: "0".repeat(64),
            package_sha256: "1".repeat(64),
            lockfile_sha256: "2".repeat(64),
            dependency_tree_sha256: "3".repeat(64),
            sbom_sha256: "4".repeat(64),
            toolchain_key: crate::mobile::local_app_runtime_profiles::RUNTIME_PROFILE_TOOLCHAIN_KEY
                .to_string(),
            verified_profile_contract_sha256: binding.contract_sha256,
        });
    }

    fn fixture_authoring_spec(
        family: local_apps::AppRuntimeProfile,
    ) -> local_apps::AppAuthoringSpec {
        let mut spec: local_apps::AppAuthoringSpec = serde_json::from_str(include_str!(
            "../../../local-apps/tests/fixtures/authoring-spec.valid-null-canvas.json"
        ))
        .expect("checked-in authoring fixture");
        if family != local_apps::AppRuntimeProfile::ReactDom {
            spec.design.canvas = Some(local_apps::AppCanvasDesign {
                scene: "A deterministic fixture scene".into(),
                phases: vec!["ready".into()],
                controls: vec!["tap".into()],
                hud: "Fixture status".into(),
            });
        }
        spec
    }

    fn fixture_authoring_spec_value(family: local_apps::AppRuntimeProfile) -> serde_json::Value {
        serde_json::to_value(fixture_authoring_spec(family)).expect("authoring fixture JSON")
    }

    /// Install a genuine immutable authoring document and select it through
    /// the active build receipt, matching the production source of truth.
    /// Tests that exercise persisted update/verify/use-test launches must not
    /// synthesize an authoring contract directly in their input args.
    fn install_active_authoring_contract(
        layout: &local_apps::AppLayout,
        manifest: &local_apps::AppManifest,
    ) {
        let binding = manifest
            .runtime_profile
            .clone()
            .expect("profiled fixture has a runtime binding");
        let contract = local_apps::AppAuthoringContract {
            version: local_apps::AUTHORING_SCHEMA_VERSION,
            revision: 1,
            app_id: layout.app_id().to_string(),
            runtime_profile: binding,
            spec: fixture_authoring_spec(
                manifest
                    .runtime_profile
                    .as_ref()
                    .expect("profiled fixture")
                    .family,
            ),
        };
        let digest = local_apps::save_authoring_contract(layout, &contract)
            .expect("save immutable authoring fixture");
        let build_root = layout.root().join(layout.build_rel(false));
        std::fs::create_dir_all(&build_root).expect("build fixture directory");
        let receipt = serde_json::json!({
            "version": 3,
            "buildId": "a".repeat(64),
            "buildKey": "workflow-support-fixture",
            "runtimeContractSha256": manifest.runtime_contract_hash().expect("runtime contract hash"),
            "dependencySnapshotSha256": manifest.dependency_snapshot_hash().expect("dependency snapshot hash"),
            "outputSha256": "b".repeat(64),
            "authoringContractSha256": digest,
        });
        std::fs::write(
            build_root.join("build.json"),
            serde_json::to_vec_pretty(&receipt).expect("serialize build fixture"),
        )
        .expect("write build fixture");
    }

    fn stamp_record_mirror(layout: &local_apps::AppLayout, scaffolded: bool) {
        let mut record = local_apps::AppState::create_with_git(
            layout.app_id().to_string(),
            "Fixture".to_string(),
            "A workflow-support fixture".to_string(),
            None,
            false,
            1_700_000_000_000,
        )
        .record;
        record.scaffolded = scaffolded;
        let relative = local_apps::storage::metadata_rel(layout.app_id());
        let path = layout.root().join(relative);
        std::fs::create_dir_all(path.parent().expect("metadata parent")).expect("metadata dir");
        let mut body = serde_json::to_vec_pretty(&local_apps::storage::AppMetadataFile {
            schema_version: local_apps::APPS_SCHEMA_VERSION,
            app: record,
        })
        .expect("metadata json");
        body.push(b'\n');
        std::fs::write(path, body).expect("metadata mirror");
    }

    fn plugin_build_script() -> &'static str {
        "export const meta = { name: 'local-app-build', description: 'Plugin local app build' };\nreturn 1;\n"
    }

    fn plugin_use_test_script() -> &'static str {
        "export const meta = { name: 'local-app-use-test', description: 'Plugin local app use test' };\nreturn 1;\n"
    }

    #[tokio::test]
    async fn checkpoint_round_trip_adopts_a_paused_workflow() {
        let root = tempfile::tempdir().expect("tempdir");
        let home = root.path().join(".claude");
        let session = "00000000-0000-0000-0000-000000000001";
        let store = Arc::new(super::MobileWorkflowCheckpointStore::new(
            home,
            root.path().to_path_buf(),
        ));
        let transcript_dir = store
            .session_dir(session)
            .join("subagents")
            .join("workflows")
            .join("wf_abcdef");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::write(transcript_dir.join("journal.jsonl"), "").unwrap();
        let script_path = root.path().join("build.js");
        let script = b"return 1;";
        std::fs::write(&script_path, script).unwrap();
        store
            .upsert(
                session,
                super::WorkflowCheckpoint {
                    task_id: "wabc12345".into(),
                    workflow_run_id: "wf_abcdef".into(),
                    workflow_id: crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID
                        .into(),
                    script_path: script_path.to_string_lossy().into_owned(),
                    script_sha256: Some(super::sha256_hex(script)),
                    script_is_verbatim_builtin: Some(true),
                    args_json: Some(r#"{"app_id":"demo"}"#.into()),
                    description: "Build local app".into(),
                    start_time: Some(1234),
                    transcript_dir: transcript_dir.to_string_lossy().into_owned(),
                },
            )
            .unwrap();

        let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix_minimal::PosixFileSystem::new(root.path().to_path_buf()),
        );
        let output = Arc::new(tasks::output_manager::TaskOutputManager::new(
            root.path().join("task-output"),
            fs.clone(),
        ));
        std::fs::create_dir_all(root.path().join("task-output")).unwrap();
        let registry = tasks::registry::TaskRegistry::new(
            Arc::new(platform_posix_minimal::PosixRuntime::new()),
            fs,
            output,
        );
        store.adopt_session(session, &registry, root.path()).await;

        let state = registry.get("wabc12345").await.expect("adopted task");
        assert_eq!(state.base().status, tasks::TaskStatus::Paused);
        let tasks::state::TaskState::LocalWorkflow(workflow) = state else {
            panic!("expected workflow")
        };
        assert_eq!(workflow.run_id.as_deref(), Some("wf_abcdef"));
        assert_eq!(
            workflow.workflow_id,
            crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID
        );
        assert_eq!(workflow.script_path.as_deref(), script_path.to_str());

        store.remove_task("wabc12345");
        assert!(
            !store.path(session).exists(),
            "terminal cleanup removes the last handoff file"
        );
    }

    #[test]
    fn adopt_json_omits_absent_fields_and_preserves_other_handoffs() {
        let root = tempfile::tempdir().expect("tempdir");
        let session = "00000000-0000-0000-0000-000000000002";
        let store = super::MobileWorkflowCheckpointStore::new(
            root.path().join(".claude"),
            root.path().to_path_buf(),
        );
        store
            .upsert(
                session,
                super::WorkflowCheckpoint {
                    task_id: "wabc12345".into(),
                    workflow_run_id: "wf_abcdef".into(),
                    workflow_id: crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID
                        .into(),
                    script_path: "/workspace/build.js".into(),
                    script_sha256: None,
                    script_is_verbatim_builtin: None,
                    args_json: None,
                    description: "Build local app".into(),
                    start_time: None,
                    transcript_dir: "/workspace/transcript".into(),
                },
            )
            .expect("upsert checkpoint");

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.path(session)).expect("read adopt file"))
                .expect("parse adopt file");
        let workflow = &value["workflows"][0];
        assert_eq!(
            workflow["workflowId"],
            crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID
        );
        assert!(workflow.get("scriptSha256").is_none());
        assert!(workflow.get("argsJson").is_none());
        assert!(workflow.get("startTime").is_none());

        let mut file = store.read(session);
        file.shells.push(serde_json::json!({ "taskId": "b123" }));
        store.write(session, file).expect("add shell handoff");
        store.remove_task("wabc12345");

        let value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(store.path(session)).expect("shell handoff must remain"),
        )
        .expect("parse preserved adopt file");
        assert_eq!(value["workflows"], serde_json::json!([]));
        assert_eq!(value["shells"], serde_json::json!([{ "taskId": "b123" }]));
    }

    /// The minted shape is accepted; every escape shape a resume id could
    /// carry into `dir.join(format!("{run_id}.js"))` is refused.
    #[test]
    fn run_id_validation_refuses_path_escapes() {
        assert!(is_valid_run_id("wf_1a2b3c4d-0ff"));
        assert!(is_valid_run_id("wf_abcdef"));

        for bad in [
            "",
            "wf_",
            "wf_abc",
            "../../lib/lingxi-bridge",
            "wf_../../lib/lingxi-bridge",
            "/tmp/anywhere",
            "wf_/tmp/anywhere",
            "wf_ABCDEF",
            "wf_abc def",
            "wf_abc.def",
        ] {
            assert!(!is_valid_run_id(bad), "must refuse {bad:?}");
        }
    }

    #[test]
    fn local_app_builtin_overwrites_expected_collections_from_materialized_manifest() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = local_apps::AppLayout::new(root.path(), "demo1234").expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app("demo1234", "Demo");
        stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::ReactDom);
        manifest.collections.push(local_apps::DataCollectionSchema {
            id: "items".into(),
            name: "Items".into(),
            fields: Vec::new(),
        });
        local_apps::save_manifest(&layout, &manifest).expect("manifest");
        install_active_authoring_contract(&layout, &manifest);
        stamp_record_mirror(&layout, true);

        let script = plugin_build_script();
        let mut spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            args: Some(serde_json::json!({
                "operation": "update",
                "app_id": "demo1234",
                "authoring_spec": fixture_authoring_spec_value(local_apps::AppRuntimeProfile::ReactDom),
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };

        super::apply_materialized_local_app_collections(root.path(), &mut spec, script)
            .expect("manifest collection ids should be authoritative");

        assert_eq!(
            spec.args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!(["items"]))
        );
        assert_eq!(
            spec.args
                .as_ref()
                .and_then(|args| args.pointer("/runtime_profile/family"))
                .and_then(serde_json::Value::as_str),
            Some("react_dom")
        );
    }

    /// §19.3: workflow launch context is injected only by the Host, and a
    /// caller-supplied value is rejected or overridden. `object.insert`
    /// silently overwrites an existing key, which is easy to break with a
    /// refactor (`entry().or_insert_with(...)`, or an early return that
    /// skips the insert) while every other test in this module still stays
    /// green, because none of them put a hostile value in the args first.
    ///
    /// This does not just check that the caller's value is GONE — a Host
    /// that injected garbage would also make it gone. It independently
    /// recomputes the app's PINNED binding via
    /// `local_app_runtime_profiles::current_binding_for_family` (the same
    /// authority `stamp_profile` used to seed the manifest) and asserts the
    /// injected object equals it field-by-field.
    #[test]
    fn caller_supplied_runtime_profile_is_overridden_by_the_host() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = local_apps::AppLayout::new(root.path(), "demo1234").expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app("demo1234", "Demo");
        stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::ReactDom);
        manifest.collections.push(local_apps::DataCollectionSchema {
            id: "items".into(),
            name: "Items".into(),
            fields: Vec::new(),
        });
        local_apps::save_manifest(&layout, &manifest).expect("manifest");
        install_active_authoring_contract(&layout, &manifest);
        stamp_record_mirror(&layout, true);

        // A hostile caller-supplied runtime_profile: a different family, a
        // revision far beyond anything published, and a contract hash that
        // cannot correspond to any real catalog entry. If any of this
        // survives, a caller could point the workflow at a
        // collection/persistence contract the Host never verified.
        let mut spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            args: Some(serde_json::json!({
                "operation": "update",
                "app_id": "demo1234",
                "authoring_spec": fixture_authoring_spec_value(local_apps::AppRuntimeProfile::ReactDom),
                "runtime_profile": {
                    "family": "three_3d",
                    "revision": 999,
                    "contract_sha256": "attacker-supplied-not-a-real-sha256",
                },
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };

        super::apply_materialized_local_app_collections(
            root.path(),
            &mut spec,
            plugin_build_script(),
        )
        .expect("materialized manifest should still resolve through a hostile args block");

        let pinned = crate::mobile::local_app_runtime_profiles::current_binding_for_family(
            local_apps::AppRuntimeProfile::ReactDom,
        )
        .expect("published runtime profile");

        let injected = spec
            .args
            .as_ref()
            .and_then(|args| args.get("runtime_profile"))
            .expect("host must inject runtime_profile");
        assert_eq!(
            injected.get("family").and_then(serde_json::Value::as_str),
            Some(pinned.family.as_str()),
            "family must equal the app's PINNED family, not merely differ from the caller's: {injected}"
        );
        assert_eq!(
            injected.get("revision").and_then(serde_json::Value::as_u64),
            Some(u64::from(pinned.revision)),
            "revision must equal the app's PINNED revision, not merely differ from the caller's: {injected}"
        );
        assert_eq!(
            injected
                .get("contract_sha256")
                .and_then(serde_json::Value::as_str),
            Some(pinned.contract_sha256.as_str()),
            "contract_sha256 must equal the app's PINNED contract, not merely differ from the caller's: {injected}"
        );

        // The caller's hostile values must specifically be gone too, not
        // just "replaced by some other value that happens to equal pinned".
        assert_ne!(
            injected.get("family").and_then(serde_json::Value::as_str),
            Some("three_3d"),
            "caller-supplied family must not survive"
        );
        assert_ne!(
            injected.get("revision").and_then(serde_json::Value::as_u64),
            Some(999),
            "caller-supplied revision must not survive"
        );
        assert_ne!(
            injected
                .get("contract_sha256")
                .and_then(serde_json::Value::as_str),
            Some("attacker-supplied-not-a-real-sha256"),
            "caller-supplied contract_sha256 must not survive"
        );
    }

    /// §19.3, the `expected_writable_collections` half: a caller-supplied
    /// collection id naming something the manifest never declared must be
    /// replaced wholesale, not merged with the Host's list.
    #[test]
    fn caller_supplied_expected_writable_collections_is_overridden_by_the_host() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = local_apps::AppLayout::new(root.path(), "demo1234").expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app("demo1234", "Demo");
        stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::ReactDom);
        manifest.collections.push(local_apps::DataCollectionSchema {
            id: "items".into(),
            name: "Items".into(),
            fields: Vec::new(),
        });
        local_apps::save_manifest(&layout, &manifest).expect("manifest");
        install_active_authoring_contract(&layout, &manifest);
        stamp_record_mirror(&layout, true);

        // A hostile caller-supplied collection the manifest never declared.
        // If it survives, the workflow could be granted write access to a
        // collection the Host never authorized — worse than the `[]` case
        // the existing overwrite test covers, which never asserted a
        // non-empty caller value loses.
        let mut spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            args: Some(serde_json::json!({
                "operation": "update",
                "app_id": "demo1234",
                "authoring_spec": fixture_authoring_spec_value(local_apps::AppRuntimeProfile::ReactDom),
                "expected_writable_collections": ["attacker_secrets"],
            })),
            ..Default::default()
        };

        super::apply_materialized_local_app_collections(
            root.path(),
            &mut spec,
            plugin_build_script(),
        )
        .expect("materialized manifest should still resolve through a hostile args block");

        assert_eq!(
            spec.args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!(["items"])),
            "the Host must overwrite the caller's collection list with the manifest's, not merge it"
        );
    }

    /// §19.3, the key-SET half. The two tests above each pin ONE Host-injected
    /// key against a hostile caller value. Nothing pins the *set*, and that is
    /// the gap Phase 2 walks into: the canvas script branches on shape, so
    /// `surface` (or the next context key like it) gets added to these args,
    /// and the natural spelling for an author who thinks "the caller may have
    /// already computed it" is `object.entry(k).or_insert_with(...)`. That
    /// form honours a caller-supplied value, the whole suite stays green
    /// because no existing test puts that key in the args first, and a
    /// caller-controlled value reaches the workflow script.
    ///
    /// ## How the Host-owned / caller-owned line is drawn here
    ///
    /// This test does NOT classify keys by name, because a name list cannot
    /// know about a key that does not exist yet. It snapshots the caller's
    /// args, runs the seam, and treats exactly the keys the seam ADDED as
    /// Host-injected. Consequences:
    ///
    /// - a caller-owned key can never trip this, whatever it is named — it is
    ///   in the snapshot, so it is never in the delta. The caller args below
    ///   deliberately carry the full caller-owned surface the workflow scripts
    ///   document (`tools/workflow/src/local_app_workflow_core.js:13`:
    ///   `app_id`, `spec`, `strategy`, `complexity`, `revision_prompt`, plus
    ///   `model`, which is a Host DEFAULT the caller legitimately wins) so
    ///   that property is exercised, not just asserted.
    /// - the two known Host-injected keys are deliberately ABSENT from the
    ///   caller args, so they land in the delta and are pinned by name. That
    ///   they stay Host-owned even when the caller DOES supply them is what
    ///   the two override tests above prove; this test proves no THIRD key
    ///   joined them unnoticed.
    ///
    /// So: adding a Host-injected key here fails loudly and names it, and the
    /// only way to make it pass is to add the key to `expected` — at which
    /// point the reviewer of that diff is looking straight at the two
    /// override tests it must be accompanied by.
    #[test]
    fn host_injected_arg_keys_are_exactly_the_expected_set() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = local_apps::AppLayout::new(root.path(), "demo1234").expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app("demo1234", "Demo");
        stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::ReactDom);
        manifest.collections.push(local_apps::DataCollectionSchema {
            id: "items".into(),
            name: "Items".into(),
            fields: Vec::new(),
        });
        local_apps::save_manifest(&layout, &manifest).expect("manifest");
        install_active_authoring_contract(&layout, &manifest);
        stamp_record_mirror(&layout, true);

        let caller_args = serde_json::json!({
            "operation": "update",
            "app_id": "demo1234",
            "authoring_spec": fixture_authoring_spec_value(local_apps::AppRuntimeProfile::ReactDom),
            "revision_prompt": "a caller-authored revision prompt",
            "strategy": "balanced",
            "complexity": {"screens": 2},
            "model": "a-caller-chosen-model",
        });
        let caller_keys: std::collections::BTreeSet<String> = caller_args
            .as_object()
            .expect("caller args object")
            .keys()
            .cloned()
            .collect();
        let mut spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            args: Some(caller_args),
            ..Default::default()
        };

        super::apply_materialized_local_app_collections(
            root.path(),
            &mut spec,
            plugin_build_script(),
        )
        .expect("materialized manifest should resolve");

        let after = spec
            .args
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .expect("args survive the seam as an object");
        let injected: std::collections::BTreeSet<String> = after
            .keys()
            .filter(|key| !caller_keys.contains(*key))
            .cloned()
            .collect();
        let expected: std::collections::BTreeSet<String> = [
            "expected_writable_collections",
            "host_context",
            "runtime_profile",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        let unexpected: Vec<&String> = injected.difference(&expected).collect();
        assert!(
            unexpected.is_empty(),
            "`apply_materialized_local_app_collections` -- this SEAM only, not the launcher \
             that calls it -- injected workflow arg key(s) {unexpected:?} that §19.3 has no \
             caller-override test for. Every Host-injected key needs a companion test \
             proving a hostile caller value loses (see \
             `caller_supplied_runtime_profile_is_overridden_by_the_host` above) and an \
             `object.insert` — NOT `entry().or_insert_with`, which honours the caller. \
             Add that test, then list the key in `expected` here. (A key injected by `launch` \
             ITSELF rather than by this seam -- e.g. `workflow_run_id` -- is out of reach of \
             this gate; see \
             `launch_injects_workflow_run_id_and_exactly_the_expected_launcher_keys` below, \
             which covers the launcher's local-app BUILD path ONLY. `launch`'s other \
             injection site, `enrich_persisted_plugin_workflow_context` \
             (workflow_support.rs:2023), writes these same key names for the use-test and \
             mcp-authoring workflows and still has NO key-set gate at all.) Seam-injected \
             keys seen: {injected:?}"
        );
        let missing: Vec<&String> = expected.difference(&injected).collect();
        assert!(
            missing.is_empty(),
            "this seam stopped injecting the §19.3 key(s) {missing:?}; the workflow script \
             would then run on whatever the caller supplied. This gate covers only \
             `apply_materialized_local_app_collections`, not `launch` -- see \
             `launch_injects_workflow_run_id_and_exactly_the_expected_launcher_keys` below for \
             the launcher's local-app BUILD path; the launcher's other injection site, \
             `enrich_persisted_plugin_workflow_context` (workflow_support.rs:2023), has no \
             key-set gate at all. Seam-injected keys seen: {injected:?}"
        );

        // Listing a key in `expected` must not be a way to silence this gate.
        // Re-run the seam with a sentinel already sitting on EVERY expected
        // Host key: an `object.insert` key comes back with the same value it
        // had when the caller supplied nothing, while an
        // `entry().or_insert_with` key comes back holding the sentinel. So a
        // key added to `expected` without an accompanying override test fails
        // here instead of passing quietly.
        let mut hostile = serde_json::Map::new();
        for key in caller_keys.iter() {
            hostile.insert(
                key.clone(),
                after.get(key).cloned().expect("caller key survives"),
            );
        }
        for key in expected.iter() {
            hostile.insert(key.clone(), serde_json::json!("caller-sentinel"));
        }
        let mut hostile_spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            args: Some(serde_json::Value::Object(hostile)),
            ..Default::default()
        };
        super::apply_materialized_local_app_collections(
            root.path(),
            &mut hostile_spec,
            plugin_build_script(),
        )
        .expect("materialized manifest should resolve through a hostile args block");
        let hostile_after = hostile_spec
            .args
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .expect("args survive the seam as an object");
        for key in expected.iter() {
            if key == "host_context" {
                let normalize = |value: &serde_json::Value| {
                    let mut value = value.clone();
                    value
                        .as_object_mut()
                        .expect("host_context is an object")
                        .remove("invocation_capability");
                    value
                };
                let actual = hostile_after.get(key).expect("host context injected");
                let capability = actual
                    .get("invocation_capability")
                    .and_then(serde_json::Value::as_str)
                    .expect("host invocation capability");
                assert!(capability.starts_with("mcpv_"));
                assert_eq!(normalize(actual), normalize(after.get(key).unwrap()));
                continue;
            }
            assert_eq!(
                hostile_after.get(key),
                after.get(key),
                "the Host key {key:?} is listed as Host-injected but did not OVERRIDE the \
                 caller's value — it is written with `entry().or_insert_with` (or an \
                 equivalent) instead of `object.insert`, so a caller controls it"
            );
        }

        // The caller-owned keys are untouched — this gate must not be
        // mistaken for "the Host owns the whole args map".
        assert_eq!(
            after.get("model").and_then(serde_json::Value::as_str),
            Some("a-caller-chosen-model"),
            "`model` is a Host default the caller legitimately wins"
        );
        assert_eq!(
            after.get("strategy").and_then(serde_json::Value::as_str),
            Some("balanced"),
            "`strategy` is caller-owned by design"
        );
    }

    #[test]
    fn resumed_local_app_builtin_uses_manifest_but_non_local_workflows_do_not() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = local_apps::AppLayout::new(root.path(), "demo1234").expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app("demo1234", "Demo");
        stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::Canvas2d);
        manifest.collections.push(local_apps::DataCollectionSchema {
            id: "progress".into(),
            name: "Progress".into(),
            fields: Vec::new(),
        });
        local_apps::save_manifest(&layout, &manifest).expect("manifest");
        install_active_authoring_contract(&layout, &manifest);
        stamp_record_mirror(&layout, true);

        let mut resumed = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            script_path: Some("persisted-workflow.js".into()),
            resume_from_run_id: Some("wf_resume1".into()),
            args: Some(serde_json::json!({
                "operation": "update",
                "app_id": "demo1234",
                "authoring_spec": fixture_authoring_spec_value(local_apps::AppRuntimeProfile::Canvas2d),
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };
        super::apply_materialized_local_app_collections_with_identity(
            root.path(),
            &mut resumed,
            plugin_build_script(),
            true,
            Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID),
            false,
        )
        .expect("trusted resumed plugin workflow should use manifest ids");
        assert_eq!(
            resumed
                .args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!(["progress"]))
        );

        let non_local_descriptor = tool_workflow::BUILTIN_WORKFLOWS
            .get("deep-research")
            .expect("deep-research built-in");
        let mut custom = tool_workflow::WorkflowLaunchSpec {
            name: Some("deep-research".into()),
            args: Some(serde_json::json!({
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };
        super::apply_materialized_local_app_collections(
            root.path(),
            &mut custom,
            non_local_descriptor.script,
        )
        .expect("non-local workflows should not read app manifests");
        assert_eq!(
            custom
                .args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!([]))
        );
    }

    #[test]
    fn local_app_builtin_fails_closed_when_manifest_is_missing() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            args: Some(serde_json::json!({
                "operation": "update",
                "app_id": "demo1234",
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };
        let error = super::apply_materialized_local_app_collections(
            root.path(),
            &mut spec,
            plugin_build_script(),
        )
        .expect_err("missing manifest must not trust caller's empty list");
        assert!(
            error.to_string().contains("record mirror"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn local_app_workflow_requires_scaffolded_record_and_complete_manifest() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = local_apps::AppLayout::new(root.path(), "demo1234").expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app("demo1234", "Demo");
        stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::ReactDom);
        local_apps::save_manifest(&layout, &manifest).expect("manifest");

        let mut spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            args: Some(serde_json::json!({
                "operation": "update",
                "app_id": "demo1234",
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };

        // A manifest/profile pair must not bypass the independent record
        // commit point. This models a crash after the manifest was written but
        // before `record.scaffolded` was persisted.
        stamp_record_mirror(&layout, false);
        let error = super::apply_materialized_local_app_collections(
            root.path(),
            &mut spec,
            plugin_build_script(),
        )
        .expect_err("an unscaffolded record must not launch a local-app workflow");
        assert!(
            error.to_string().contains("never scaffolded"),
            "unexpected error: {error}"
        );

        // The record alone is not enough either: a scaffold commit without
        // its dependency snapshot is still a partial, non-runnable app.
        stamp_record_mirror(&layout, true);
        let mut partial_manifest = manifest;
        partial_manifest.dependency_snapshot = None;
        local_apps::save_manifest(&layout, &partial_manifest).expect("partial manifest");
        let error = super::apply_materialized_local_app_collections(
            root.path(),
            &mut spec,
            plugin_build_script(),
        )
        .expect_err("a partial manifest must not launch a local-app workflow");
        assert!(
            error.to_string().contains("dependency snapshot"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn local_app_workflow_rejects_corrupt_runtime_contract_hash() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = local_apps::AppLayout::new(root.path(), "demo1234").expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app("demo1234", "Demo");
        stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::ReactDom);
        manifest
            .runtime_profile
            .as_mut()
            .expect("profile binding")
            .contract_sha256 = "0".repeat(64);
        manifest
            .dependency_snapshot
            .as_mut()
            .expect("dependency snapshot")
            .verified_profile_contract_sha256 = "0".repeat(64);
        // `AppManifest::validate` now requires
        // `templateOrigin.templateSha256 == runtimeProfile.contractSha256`
        // (local-apps/src/manifest.rs), so corrupting only the binding no
        // longer produces a manifest that can be written OR read back. Corrupt
        // both to the same value: that is still a manifest whose pinned
        // contract digest is not one this host publishes, which is exactly the
        // state this test exists to prove the launch boundary refuses --
        // `contract_for_binding` compares the binding against the PUBLISHED
        // contract, which neither of these two fields can satisfy.
        manifest
            .template_origin
            .as_mut()
            .expect("template origin")
            .template_sha256 = "0".repeat(64);
        local_apps::save_manifest(&layout, &manifest).expect("manifest");
        stamp_record_mirror(&layout, true);

        let mut spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            args: Some(serde_json::json!({"operation": "update", "app_id": "demo1234"})),
            ..Default::default()
        };
        let error = super::apply_materialized_local_app_collections(
            root.path(),
            &mut spec,
            plugin_build_script(),
        )
        .expect_err("a corrupt profile hash must fail closed");
        assert!(
            error.to_string().contains("expects contract"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn local_app_workflow_rejects_unavailable_babylon_profile() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = local_apps::AppLayout::new(root.path(), "demo1234").expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app("demo1234", "Demo");
        manifest.surface = Some(local_apps::AppSurface::Canvas);
        manifest.runtime_profile = Some(local_apps::AppRuntimeProfileBinding {
            family: local_apps::AppRuntimeProfile::Babylon3d,
            revision: 1,
            // The catalog intentionally does not publish a Babylon contract
            // until the real-device spike succeeds; any well-formed digest
            // must still be rejected as unavailable, not selected as a route.
            contract_sha256: "a".repeat(64),
        });
        manifest.dependency_snapshot = Some(local_apps::AppDependencySnapshot {
            requested_sha256: "0".repeat(64),
            package_sha256: "1".repeat(64),
            lockfile_sha256: "2".repeat(64),
            dependency_tree_sha256: "3".repeat(64),
            sbom_sha256: "4".repeat(64),
            toolchain_key: crate::mobile::local_app_runtime_profiles::RUNTIME_PROFILE_TOOLCHAIN_KEY
                .to_string(),
            verified_profile_contract_sha256: "a".repeat(64),
        });
        manifest.template_origin = Some(local_apps::AppTemplateOrigin {
            plugin_id: local_apps::AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
            plugin_version: "builtin".into(),
            template_id: "babylon-3d-r4".into(),
            template_sha256: "a".repeat(64),
        });
        local_apps::save_manifest(&layout, &manifest).expect("manifest");
        stamp_record_mirror(&layout, true);

        let mut spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            args: Some(serde_json::json!({"operation": "update", "app_id": "demo1234"})),
            ..Default::default()
        };
        let error = super::apply_materialized_local_app_collections(
            root.path(),
            &mut spec,
            plugin_build_script(),
        )
        .expect_err("gated Babylon must not launch");
        assert!(
            error
                .to_string()
                .contains("not published in this host build"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn local_app_workflow_routes_each_published_profile_to_the_plugin_workflow() {
        for family in [
            local_apps::AppRuntimeProfile::ReactDom,
            local_apps::AppRuntimeProfile::Canvas2d,
            local_apps::AppRuntimeProfile::Three3d,
            local_apps::AppRuntimeProfile::Phaser2d,
        ] {
            let root = tempfile::tempdir().expect("tempdir");
            let layout = local_apps::AppLayout::new(root.path(), "demo1234").expect("layout");
            let mut manifest = local_apps::AppManifest::for_new_app("demo1234", "Demo");
            stamp_profile(&mut manifest, family);
            local_apps::save_manifest(&layout, &manifest).expect("manifest");
            install_active_authoring_contract(&layout, &manifest);
            stamp_record_mirror(&layout, true);

            let mut spec = tool_workflow::WorkflowLaunchSpec {
                name: Some(
                    crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into(),
                ),
                args: Some(serde_json::json!({
                    "operation": "update",
                    "app_id": "demo1234",
                    "authoring_spec": fixture_authoring_spec_value(family),
                })),
                ..Default::default()
            };
            super::apply_materialized_local_app_collections(
                root.path(),
                &mut spec,
                plugin_build_script(),
            )
            .unwrap_or_else(|error| {
                panic!("{family} should route to the plugin workflow: {error}")
            });
            assert_eq!(
                spec.args
                    .as_ref()
                    .and_then(|args| args.pointer("/runtime_profile/family"))
                    .and_then(serde_json::Value::as_str),
                Some(family.as_str()),
                "runtime profile must remain visible to the routed specialist"
            );
        }
    }

    // Characterization tests for the build-target → required-workflow
    // REFUSAL (plan v3 Phase -1, P-1.1). Before this change nothing in the
    // workspace pinned this behaviour — a grep for `refusing caller-selected
    // workflow` and `required_workflow_id` had zero hits outside the
    // production site. P-1.3 replaces the inline match these tests exercise
    // with typed routing through `local_app_plugin_binding`; these tests must
    // keep passing unchanged across that replacement, since they assert on
    // the launcher's observable behaviour (Err vs Ok, and what the error
    // names), not on how the routing is implemented.

    #[test]
    fn named_local_app_with_custom_inline_script_is_not_a_builtin() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            script: Some("export const meta = { name: 'custom' }; return 1".into()),
            args: Some(serde_json::json!({
                "app_id": "demo1234",
                "expected_writable_collections": ["caller_choice"],
            })),
            ..Default::default()
        };
        let script = spec.script.clone().expect("inline script");

        super::apply_materialized_local_app_collections(root.path(), &mut spec, &script)
            .expect("custom inline workflow must not read a missing manifest");

        assert_eq!(
            spec.args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!(["caller_choice"]))
        );

        let custom_path = root.path().join("custom-local-app.js");
        let custom_path_script = "export const meta = { name: 'local-app-build' };\nreturn 2\n";
        std::fs::write(&custom_path, custom_path_script).expect("custom script path");
        let mut path_spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            script_path: Some(custom_path.to_string_lossy().into_owned()),
            args: Some(serde_json::json!({
                "app_id": "demo1234",
                "expected_writable_collections": ["caller_choice"],
            })),
            ..Default::default()
        };
        super::apply_materialized_local_app_collections(
            root.path(),
            &mut path_spec,
            custom_path_script,
        )
        .expect("custom same-meta script path must not read a missing manifest");
        assert_eq!(
            path_spec
                .args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!(["caller_choice"]))
        );
    }

    /// r4-workflow-runtime-08: a launch with no `args.operation` used to be
    /// silently treated as `update` for the Host's OWN create/update fork
    /// (workflow_support.rs's `.unwrap_or("update")`), before the launch
    /// ever reaches the script's own `!['create','update','verify'].includes(...)`
    /// check. Against an unscaffolded shell that meant a confusing
    /// "requires a persisted runtime profile" error instead of the operation
    /// contract error the script would have given. The Host must now name
    /// the same contract itself instead of guessing.
    #[test]
    fn missing_operation_is_rejected_by_the_host_instead_of_defaulting_to_update() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = local_apps::AppLayout::new(root.path(), "demo1234").expect("layout");
        // An unscaffolded shell: exactly the case where the old silent
        // default produced the WRONG (profile) error instead of the
        // operation contract error.
        stamp_record_mirror(&layout, false);
        let mut spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            args: Some(serde_json::json!({ "app_id": "demo1234" })),
            ..Default::default()
        };
        let error = super::apply_materialized_local_app_collections(
            root.path(),
            &mut spec,
            plugin_build_script(),
        )
        .expect_err("a launch with no operation must not silently proceed as `update`");
        assert!(
            error.to_string().contains("operation"),
            "missing `operation` must be reported as an operation-contract error, not the \
             unrelated \"requires app {{app_id}} to have a persisted runtime profile\" error a \
             silent `update` default produces against an unscaffolded shell: {error}"
        );
    }

    #[test]
    fn trusted_plugin_local_app_resume_uses_checkpoint_provenance() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = local_apps::AppLayout::new(root.path(), "demo1234").expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app("demo1234", "Demo");
        stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::ReactDom);
        manifest.collections.push(local_apps::DataCollectionSchema {
            id: "progress".into(),
            name: "Progress".into(),
            fields: Vec::new(),
        });
        local_apps::save_manifest(&layout, &manifest).expect("manifest");
        install_active_authoring_contract(&layout, &manifest);
        stamp_record_mirror(&layout, true);

        let session = "00000000-0000-0000-0000-000000000003";
        let run_id = "wf_plugin1";
        let script_path = root.path().join("plugin-local-app.js");
        let trusted_script = plugin_build_script();
        std::fs::write(&script_path, trusted_script).expect("plugin script");
        let checkpoints = super::MobileWorkflowCheckpointStore::new(
            root.path().join(".claude"),
            root.path().to_path_buf(),
        );
        checkpoints
            .upsert(
                session,
                super::WorkflowCheckpoint {
                    task_id: "wplugin01".into(),
                    workflow_run_id: run_id.into(),
                    workflow_id: crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID
                        .into(),
                    script_path: script_path.to_string_lossy().into_owned(),
                    script_sha256: Some(super::sha256_hex(trusted_script.as_bytes())),
                    script_is_verbatim_builtin: Some(true),
                    args_json: Some(
                        serde_json::json!({
                            "app_id": "demo1234",
                            "expected_writable_collections": [],
                        })
                        .to_string(),
                    ),
                    description: "Plugin local app build".into(),
                    start_time: None,
                    transcript_dir: root
                        .path()
                        .join("transcript")
                        .to_string_lossy()
                        .into_owned(),
                },
            )
            .expect("checkpoint");

        let mut trusted = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            script_path: Some(script_path.to_string_lossy().into_owned()),
            resume_from_run_id: Some(run_id.into()),
            session_uuid: Some(session.into()),
            args: Some(serde_json::json!({
                "operation": "update",
                "app_id": "demo1234",
                "authoring_spec": fixture_authoring_spec_value(local_apps::AppRuntimeProfile::ReactDom),
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };
        assert!(checkpoints.is_trusted_local_app_resume(
            session,
            run_id,
            &script_path,
            trusted_script
        ));
        assert_eq!(
            checkpoints.local_app_resume_provenance(session, run_id, &script_path, trusted_script),
            Some(super::LocalAppResumeProvenance::TrustedPlugin)
        );
        super::apply_materialized_local_app_collections_with_provenance(
            root.path(),
            &mut trusted,
            trusted_script,
            true,
        )
        .expect("trusted plugin resume should use manifest ids");
        assert_eq!(
            trusted
                .args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!(["progress"]))
        );
        let trusted_context = trusted
            .args
            .as_ref()
            .and_then(|args| args.get("host_context"))
            .expect("trusted plugin resume must restore Host-owned workflow context");
        assert_eq!(
            trusted_context
                .get("source")
                .and_then(serde_json::Value::as_str),
            Some("verified_host")
        );
        assert_eq!(
            trusted_context
                .get("runtime_profile")
                .and_then(|profile| profile.get("family"))
                .and_then(serde_json::Value::as_str),
            Some("react_dom")
        );

        let custom_run_id = "wf_custom1";
        let custom_path = root.path().join("custom-local-app.js");
        let custom_script = "export const meta = { name: 'local-app-build' };\nreturn 'custom'\n";
        std::fs::write(&custom_path, custom_script).expect("custom script");
        checkpoints
            .upsert(
                session,
                super::WorkflowCheckpoint {
                    task_id: "wcustom1".into(),
                    workflow_run_id: custom_run_id.into(),
                    workflow_id: crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID
                        .into(),
                    script_path: custom_path.to_string_lossy().into_owned(),
                    script_sha256: Some(super::sha256_hex(custom_script.as_bytes())),
                    script_is_verbatim_builtin: Some(false),
                    args_json: None,
                    description: "Custom local app workflow".into(),
                    start_time: None,
                    transcript_dir: root
                        .path()
                        .join("custom-transcript")
                        .to_string_lossy()
                        .into_owned(),
                },
            )
            .expect("custom checkpoint");
        assert_eq!(
            checkpoints.local_app_resume_provenance(
                session,
                custom_run_id,
                &custom_path,
                custom_script
            ),
            Some(super::LocalAppResumeProvenance::Custom)
        );
        let mut custom_resume = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            script_path: Some(custom_path.to_string_lossy().into_owned()),
            resume_from_run_id: Some(custom_run_id.into()),
            session_uuid: Some(session.into()),
            args: Some(serde_json::json!({
                "app_id": "demo1234",
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };
        super::apply_materialized_local_app_collections_with_provenance(
            root.path(),
            &mut custom_resume,
            custom_script,
            false,
        )
        .expect("marker=false custom resume should remain custom");
        assert_eq!(
            custom_resume
                .args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!([]))
        );

        let mut untrusted = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            script_path: Some(script_path.to_string_lossy().into_owned()),
            resume_from_run_id: Some(run_id.into()),
            session_uuid: Some(session.into()),
            args: Some(serde_json::json!({
                "app_id": "demo1234",
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };
        let no_checkpoint = super::MobileWorkflowCheckpointStore::new(
            root.path().join("missing-checkpoint-home"),
            root.path().to_path_buf(),
        );
        assert!(!no_checkpoint.is_trusted_local_app_resume(
            session,
            run_id,
            &script_path,
            trusted_script
        ));
        super::apply_materialized_local_app_collections_with_provenance(
            root.path(),
            &mut untrusted,
            trusted_script,
            false,
        )
        .expect("untrusted resume should remain a custom workflow");
        assert_eq!(
            untrusted
                .args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!([]))
        );

        let wrong_path = root.path().join("wrong-path.js");
        std::fs::write(&wrong_path, trusted_script).expect("wrong-path script");
        assert!(!checkpoints.is_trusted_local_app_resume(
            session,
            run_id,
            &wrong_path,
            trusted_script
        ));
        let mut wrong_path_spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            script_path: Some(wrong_path.to_string_lossy().into_owned()),
            resume_from_run_id: Some(run_id.into()),
            session_uuid: Some(session.into()),
            args: Some(serde_json::json!({
                "app_id": "demo1234",
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };
        super::apply_materialized_local_app_collections_with_provenance(
            root.path(),
            &mut wrong_path_spec,
            trusted_script,
            false,
        )
        .expect("a run id must not authorize a different script path");
        assert_eq!(
            wrong_path_spec
                .args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!([]))
        );

        let current_path = root.path().join("current-path.js");
        std::fs::write(&current_path, trusted_script).expect("current script");
        let mut exact_bytes_untrusted = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            script_path: Some(current_path.to_string_lossy().into_owned()),
            resume_from_run_id: Some(run_id.into()),
            session_uuid: Some(session.into()),
            args: Some(serde_json::json!({
                "app_id": "demo1234",
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };
        super::apply_materialized_local_app_collections_with_provenance(
            root.path(),
            &mut exact_bytes_untrusted,
            trusted_script,
            false,
        )
        .expect("exact bytes without provenance remain custom");
        assert_eq!(
            exact_bytes_untrusted
                .args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!([]))
        );
    }

    #[test]
    fn cold_start_provenance_sidecar_requires_exact_plugin_identity() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = local_apps::AppLayout::new(root.path(), "demo1234").expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app("demo1234", "Demo");
        stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::ReactDom);
        manifest.collections.push(local_apps::DataCollectionSchema {
            id: "progress".into(),
            name: "Progress".into(),
            fields: Vec::new(),
        });
        local_apps::save_manifest(&layout, &manifest).expect("manifest");

        let session = "00000000-0000-0000-0000-000000000004";
        let store = super::MobileWorkflowCheckpointStore::new(
            root.path().join(".claude"),
            root.path().to_path_buf(),
        );
        let cold_store = || {
            super::MobileWorkflowCheckpointStore::new(
                root.path().join(".claude"),
                root.path().to_path_buf(),
            )
        };

        let trusted_run_id = "wf_trusted-cold1";
        let trusted_path = store.persisted_workflow_script_path(session, trusted_run_id);
        std::fs::create_dir_all(trusted_path.parent().expect("trusted parent"))
            .expect("legacy script directory");
        let trusted_script = plugin_build_script();
        std::fs::write(&trusted_path, trusted_script).expect("trusted script");
        store
            .write_provenance_sidecar(
                session,
                trusted_run_id,
                &trusted_path,
                trusted_script,
                crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID,
                true,
            )
            .expect("trusted provenance sidecar");
        assert_eq!(
            super::local_app_resume_provenance_for_launch(
                &cold_store(),
                session,
                trusted_run_id,
                &trusted_path,
                trusted_script,
            ),
            Some(super::LocalAppResumeProvenance::TrustedPlugin)
        );

        // A new custom same-meta workflow records marker=false in the
        // transcript sidecar. A fresh process can therefore resume it without
        // inheriting the local-app manifest gate.
        let custom_run_id = "wf_custom-cold1";
        let custom_path = store.persisted_workflow_script_path(session, custom_run_id);
        let custom_script =
            "export const meta = { name: 'local-app-build' };\nreturn 'custom body'\n";
        std::fs::write(&custom_path, custom_script).expect("custom script");
        store
            .write_provenance_sidecar(
                session,
                custom_run_id,
                &custom_path,
                custom_script,
                crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID,
                false,
            )
            .expect("custom provenance sidecar");
        let mut custom_spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            script_path: Some(custom_path.to_string_lossy().into_owned()),
            resume_from_run_id: Some(custom_run_id.into()),
            session_uuid: Some(session.into()),
            args: Some(serde_json::json!({
                "app_id": "demo1234",
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };
        assert_eq!(
            super::local_app_resume_provenance_for_launch(
                &cold_store(),
                session,
                custom_run_id,
                &custom_path,
                custom_script,
            ),
            Some(super::LocalAppResumeProvenance::Custom)
        );
        super::apply_materialized_local_app_collections_with_provenance(
            root.path(),
            &mut custom_spec,
            custom_script,
            false,
        )
        .expect("marker=false custom sidecar should remain custom");
        assert_eq!(
            custom_spec
                .args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!([]))
        );

        let provenance_path = store.provenance_path(session, custom_run_id);
        let mut tampered_value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&provenance_path).expect("read custom provenance"),
        )
        .expect("parse custom provenance");
        tampered_value["workflowId"] = serde_json::json!("custom-workflow");
        std::fs::write(
            &provenance_path,
            serde_json::to_vec_pretty(&tampered_value).expect("serialize tampered identity"),
        )
        .expect("tamper workflow identity");
        assert_eq!(
            super::local_app_resume_provenance_for_launch(
                &cold_store(),
                session,
                custom_run_id,
                &custom_path,
                custom_script,
            ),
            None
        );
        let mut tampered_identity = custom_spec.clone();
        super::apply_materialized_local_app_collections_with_provenance(
            root.path(),
            &mut tampered_identity,
            custom_script,
            false,
        )
        .expect("tampered local-app identity must remain untrusted");

        tampered_value["workflowId"] =
            serde_json::json!(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID);
        tampered_value["scriptSha256"] =
            serde_json::json!(super::sha256_hex(trusted_script.as_bytes()));
        std::fs::write(
            &provenance_path,
            serde_json::to_vec_pretty(&tampered_value).expect("serialize tampered hash"),
        )
        .expect("tamper script hash");
        assert_eq!(
            super::local_app_resume_provenance_for_launch(
                &cold_store(),
                session,
                custom_run_id,
                &custom_path,
                custom_script,
            ),
            None
        );

        let different_path = root.path().join("different.js");
        std::fs::write(&different_path, custom_script).expect("different path script");
        let mut different_path_spec = tool_workflow::WorkflowLaunchSpec {
            name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
            script_path: Some(different_path.to_string_lossy().into_owned()),
            resume_from_run_id: Some(custom_run_id.into()),
            session_uuid: Some(session.into()),
            args: Some(serde_json::json!({
                "app_id": "demo1234",
                "expected_writable_collections": [],
            })),
            ..Default::default()
        };
        assert_eq!(
            super::local_app_resume_provenance_for_launch(
                &cold_store(),
                session,
                custom_run_id,
                &different_path,
                custom_script,
            ),
            None
        );
        super::apply_materialized_local_app_collections_with_provenance(
            root.path(),
            &mut different_path_spec,
            custom_script,
            false,
        )
        .expect("different caller path must stay untrusted");

        // No sidecar plus a custom body/meta at a non-host path remains a
        // direct custom workflow, even when its args happen to contain app_id.
        let ordinary_path = root.path().join("ordinary-custom.js");
        let ordinary_script = "export const meta = { name: 'custom-workflow' };\nreturn 1\n";
        std::fs::write(&ordinary_path, ordinary_script).expect("ordinary custom script");
        let mut ordinary_spec = tool_workflow::WorkflowLaunchSpec {
            script_path: Some(ordinary_path.to_string_lossy().into_owned()),
            resume_from_run_id: Some("wf_ordinary1".into()),
            session_uuid: Some(session.into()),
            args: Some(serde_json::json!({
                "app_id": "demo1234",
                "expected_writable_collections": ["caller_choice"],
            })),
            ..Default::default()
        };
        assert_eq!(
            super::local_app_resume_provenance_for_launch(
                &cold_store(),
                session,
                "wf_ordinary1",
                &ordinary_path,
                ordinary_script,
            ),
            None
        );
        super::apply_materialized_local_app_collections_with_provenance(
            root.path(),
            &mut ordinary_spec,
            ordinary_script,
            false,
        )
        .expect("ordinary custom resume should remain unchanged");
        assert_eq!(
            ordinary_spec
                .args
                .as_ref()
                .and_then(|args| args.get("expected_writable_collections")),
            Some(&serde_json::json!(["caller_choice"]))
        );
    }

    /// r3-failure-paths-07: a workflow that FAILS must reach the client with
    /// the engine's reason attached, not as a bare task id.
    ///
    /// Both halves are asserted: the terminal payload lands in the registry
    /// (so the `TaskList` row a later session refresh pulls carries `error`),
    /// and the pushed `TaskStatusChanged` carries the same reason. The
    /// completed case is the vacuity guard — it proves the assertion below is
    /// reading a field that is genuinely `None` for a non-failure, i.e. the
    /// test cannot pass by always finding a reason.
    #[tokio::test]
    async fn failed_workflow_terminal_carries_the_reason_to_the_client() {
        use tasks::handlers::TaskStatusSink as _;

        let listener = Arc::new(FakeListener::default());
        let root = tempfile::tempdir().expect("tempdir");
        let checkpoints = Arc::new(super::MobileWorkflowCheckpointStore::new(
            root.path().join(".claude"),
            root.path().to_path_buf(),
        ));
        let active_session = Arc::new(std::sync::Mutex::new("session-a".to_string()));
        let sink = super::MobileWorkflowStatusSink::new(
            listener.clone(),
            checkpoints,
            active_session.clone(),
        );
        let _ = sink
            .checkpoints
            .track_task_owner("w12345678", "session-a", "wf_failing");
        let _ = sink
            .checkpoints
            .track_task_owner("w87654321", "session-a", "wf_ok");

        sink.finish_workflow_terminal(
            "w12345678",
            lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                error: Some("step 2 `build` exited 1".to_string()),
                ..Default::default()
            },
            tasks::TaskStatus::Failed,
        )
        .await;
        sink.finish_workflow_terminal(
            "w87654321",
            lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                result: Some("done".to_string()),
                ..Default::default()
            },
            tasks::TaskStatus::Completed,
        )
        .await;

        let events = listener.received.lock().await.clone();
        let statuses: Vec<(String, Option<String>)> = events
            .iter()
            .filter_map(|event| match event {
                client::protocol::events::ClientEvent::TaskStatusChanged {
                    task_id, error, ..
                } => Some((task_id.clone(), error.clone())),
                _ => None,
            })
            .collect();
        // Vacuity guard: without BOTH transitions on the wire the assertions
        // below would pass over an empty list.
        assert_eq!(
            statuses.len(),
            2,
            "expected both terminal transitions, got {statuses:?}"
        );
        assert_eq!(
            statuses[0],
            (
                "w12345678".to_string(),
                Some("step 2 `build` exited 1".to_string())
            ),
            "the failed transition must name the reason"
        );
        assert_eq!(
            statuses[1],
            ("w87654321".to_string(), None),
            "a completed transition must not carry a reason"
        );
    }

    #[tokio::test]
    async fn workflow_progress_sink_calls_structured_listener() {
        let listener = Arc::new(FakeListener::default());
        let root = tempfile::tempdir().expect("tempdir");
        let checkpoints = Arc::new(super::MobileWorkflowCheckpointStore::new(
            root.path().join(".claude"),
            root.path().to_path_buf(),
        ));
        let active_session = Arc::new(std::sync::Mutex::new("session-a".to_string()));
        let sink = super::MobileWorkflowStatusSink::new(
            listener.clone(),
            checkpoints,
            active_session.clone(),
        );

        let progress = tasks::handlers::local_workflow::WorkflowProgressUpdate {
            kind: "workflow_agent".to_string(),
            index: 2,
            title: None,
            message: None,
            label: Some("Design".to_string()),
            phase_index: Some(1),
            phase_title: Some("Design".to_string()),
            agent_id: Some("agent:123".to_string()),
            agent_type: Some("workflow-subagent".to_string()),
            model: Some("gpt-5.4".to_string()),
            fallback_model: None,
            state: Some("running".to_string()),
            error: None,
            tool_use_id: Some("workflow_agent_2_agent:123".to_string()),
            queued_at_ms: Some(10),
            started_at_ms: Some(20),
            last_progress_at_ms: Some(30),
            attempt: Some(1),
            last_attempt_reason: None,
            tokens: Some(40),
            tool_calls: Some(3),
            last_tool_name: Some("Read".to_string()),
            last_tool_summary: Some("Read".to_string()),
            prompt_preview: Some("design screen".to_string()),
        };

        // The worker can emit before launch() has persisted the checkpoint.
        // That event must be retained and replayed once ownership is known.
        tasks::handlers::local_workflow::WorkflowProgressSink::emit_workflow_progress(
            &sink,
            "task_123",
            "wf_abcdef",
            progress.clone(),
        )
        .await;
        assert!(listener.workflow_progress.lock().await.is_empty());

        let pending = sink
            .checkpoints
            .track_task_owner("task_123", "session-a", "wf_abcdef");
        sink.flush_pending_events("task_123", pending).await;

        let events = listener.workflow_progress.lock().await.clone();
        assert!(events
            .iter()
            .any(|(session_id, task_id, run_id, progress)| {
                session_id == "session-a"
                    && task_id == "task_123"
                    && run_id == "wf_abcdef"
                    && progress.kind == "workflow_agent"
                    && progress.index == 2
                    && progress.label.as_deref() == Some("Design")
                    && progress.agent_type.as_deref() == Some("workflow-subagent")
                    && progress.tokens == Some(40)
            }));

        *active_session.lock().unwrap() = "session-b".to_string();
        tasks::handlers::local_workflow::WorkflowProgressSink::emit_workflow_progress(
            &sink,
            "task_123",
            "wf_abcdef",
            tasks::handlers::local_workflow::WorkflowProgressUpdate {
                kind: "workflow_agent".to_string(),
                index: 3,
                title: None,
                message: None,
                label: None,
                phase_index: None,
                phase_title: None,
                agent_id: None,
                agent_type: None,
                model: None,
                fallback_model: None,
                state: Some("running".to_string()),
                error: None,
                tool_use_id: None,
                queued_at_ms: None,
                started_at_ms: None,
                last_progress_at_ms: None,
                attempt: None,
                last_attempt_reason: None,
                tokens: None,
                tool_calls: None,
                last_tool_name: None,
                last_tool_summary: None,
                prompt_preview: None,
            },
        )
        .await;
        assert_eq!(listener.workflow_progress.lock().await.len(), 1);
    }

    // ── P-1.7 R1: the Host mints the scope, and a genuine build blocks delete ──

    /// A stub `LocalWorkflow` handler.
    ///
    /// `TaskRegistry::spawn` needs a handler to dispatch to, but nothing in
    /// these two tests is about running JavaScript: the registry still builds
    /// the real task row from the real spawn input (`state_for_spawn`), which
    /// is where the launcher's `scope` lands. The stub never completes its
    /// task, so the row stays non-terminal -- which IS the "build still in
    /// flight" state the App delete guard exists for, held still instead of
    /// raced against a real QuickJS run.
    struct StubWorkflowHandler;

    #[async_trait::async_trait]
    impl tasks::task_trait::Task for StubWorkflowHandler {
        fn name(&self) -> &str {
            "stub_local_workflow"
        }

        fn task_type(&self) -> tasks::TaskType {
            tasks::TaskType::LocalWorkflow
        }

        async fn spawn(
            &self,
            _input: tasks::TaskSpawnInput,
            _ctx: tasks::task_trait::TaskContext,
        ) -> Result<tasks::task_trait::TaskHandle, tasks::task_trait::TaskError> {
            Ok(tasks::task_trait::TaskHandle::new(
                tasks::generate_task_id(tasks::TaskType::LocalWorkflow),
                None,
            ))
        }

        async fn kill(
            &self,
            _task_id: &str,
            _ctx: tasks::task_trait::TaskContext,
        ) -> Result<(), tasks::task_trait::TaskError> {
            Ok(())
        }
    }

    /// Materialize a real, fully scaffolded Local App under `root` -- manifest
    /// with a published runtime profile and a verified dependency snapshot,
    /// plus the record mirror's `scaffolded` bit. This is the on-disk state
    /// the launch seam resolves the app id against; without it the seam
    /// refuses and mints nothing.
    fn scaffold_local_app(
        root: &std::path::Path,
        app_id: &str,
        family: local_apps::AppRuntimeProfile,
    ) {
        let layout = local_apps::AppLayout::new(root, app_id).expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app(app_id, "Fixture");
        stamp_profile(&mut manifest, family);
        local_apps::save_manifest(&layout, &manifest).expect("manifest");
        install_active_authoring_contract(&layout, &manifest);
        stamp_record_mirror(&layout, true);
    }

    fn scope_test_registry() -> Arc<tasks::registry::TaskRegistry> {
        let output_dir = std::env::temp_dir().join(format!(
            "lingxi-p17-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&output_dir).expect("task output dir");
        let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix_minimal::PosixFileSystem::new(output_dir.clone()),
        );
        let mut registry = tasks::registry::TaskRegistry::new(
            Arc::new(platform_posix_minimal::PosixRuntime::new()),
            fs.clone(),
            Arc::new(tasks::output_manager::TaskOutputManager::new(
                output_dir, fs,
            )),
        );
        registry.register_handler(
            tasks::TaskType::LocalWorkflow,
            Arc::new(StubWorkflowHandler),
        );
        Arc::new(registry)
    }

    async fn register_terminal_test_workflow(
        registry: &Arc<tasks::registry::TaskRegistry>,
        scope: Option<tasks::scope::LocalAppWorkflowTaskScope>,
        args: serde_json::Value,
    ) -> String {
        let task_id = registry
            .spawn(
                tasks::TaskType::LocalWorkflow,
                tasks::TaskSpawnInput::LocalWorkflow {
                    session_uuid: Some("session-terminal".into()),
                    workflow_id: "terminal-test".into(),
                    script: "return null".into(),
                    resume_from_run_id: None,
                    args: Some(args.to_string()),
                    run_id: Some("wf_terminal-test".into()),
                    invocation_mode: Some("named".into()),
                    workflow_source: Some("plugin".into()),
                    script_is_verbatim_builtin: Some(true),
                    transcript_subdir: None,
                    launched_from_subagent: false,
                    tool_use_id: None,
                    creator_teammate_name: None,
                    creator_team_name: None,
                    creator_agent_id: None,
                    scope,
                },
                "terminal test".into(),
            )
            .await
            .expect("register terminal test workflow");
        registry
            .output_manager
            .allocate(&task_id)
            .await
            .expect("stub handler does not allocate its spool");
        task_id
    }

    fn terminal_test_sink(
        root: &std::path::Path,
        listener: Arc<FakeListener>,
        registry: Arc<tasks::registry::TaskRegistry>,
        task_id: &str,
    ) -> super::MobileWorkflowStatusSink {
        let checkpoints = Arc::new(super::MobileWorkflowCheckpointStore::new(
            root.join(".claude"),
            root.to_path_buf(),
        ));
        let sink = super::MobileWorkflowStatusSink::new(
            listener,
            checkpoints,
            Arc::new(std::sync::Mutex::new("session-terminal".into())),
        );
        sink.bind(registry);
        let _ = sink
            .checkpoints
            .track_task_owner(task_id, "session-terminal", "wf_terminal-test");
        sink
    }

    fn terminal_events(
        events: &[client::protocol::events::ClientEvent],
        task_id: &str,
    ) -> Vec<(String, Option<String>)> {
        events
            .iter()
            .filter_map(|event| match event {
                client::protocol::events::ClientEvent::TaskStatusChanged {
                    task_id: seen,
                    status,
                    error,
                    ..
                } if seen == task_id => Some((format!("{status:?}"), error.clone())),
                _ => None,
            })
            .collect()
    }

    fn seed_terminal_qa_session(root: &std::path::Path, app_id: &str, qa_handle: &str) {
        let layout = local_apps::AppLayout::new(root, app_id).expect("layout");
        let manifest = local_apps::load_manifest(&layout).expect("manifest");
        let authoring_contract_sha256 =
            crate::mobile::local_apps_build::active_build_authoring_contract_sha256(&layout)
                .expect("active build receipt")
                .expect("active authoring contract digest");
        let runtime_profile = manifest.runtime_profile.clone().expect("runtime profile");
        let dependency_snapshot_sha256 = manifest
            .dependency_snapshot_hash()
            .expect("dependency snapshot digest");
        local_apps::qa_begin(
            &layout,
            local_apps::QaIdentity {
                app_id: app_id.into(),
                workflow_run_id: "wf_terminal-test".into(),
                qa_handle: qa_handle.into(),
                build_id: "a".repeat(64),
                runtime_profile,
                verification_strategy: local_apps::QaVerificationStrategy::Balanced,
                dependency_snapshot_sha256,
                authoring_contract_sha256,
                manifest_revision: manifest.revision.max(1),
                runtime_generation: 1,
            },
            vec!["scenario-1".into()],
            vec!["primary".into()],
            vec![],
            1,
        )
        .expect("seed in-flight QA session");
    }

    #[tokio::test]
    async fn local_app_ok_false_remains_a_completed_logical_result() {
        use tasks::handlers::TaskStatusSink as _;

        let root = tempfile::tempdir().expect("tempdir");
        let registry = scope_test_registry();
        let task_id = register_terminal_test_workflow(
            &registry,
            Some(tasks::scope::LocalAppWorkflowTaskScope::for_build("qa-app").expect("scope")),
            serde_json::json!({"app_id":"qa-app","quality_level":"balanced"}),
        )
        .await;
        let raw = r#"{"ok":false,"status":"verification_failed"}"#;
        registry
            .output_manager
            .append(&registry.output_manager.path_for(&task_id).unwrap(), raw)
            .await
            .unwrap();
        let listener = Arc::new(FakeListener::default());
        let sink = terminal_test_sink(root.path(), listener.clone(), registry.clone(), &task_id);

        sink.finish_workflow_terminal(
            &task_id,
            lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                result: Some(raw.into()),
                ..Default::default()
            },
            tasks::TaskStatus::Completed,
        )
        .await;

        let tasks::state::TaskState::LocalWorkflow(actual) = registry.get(&task_id).await.unwrap()
        else {
            panic!("expected workflow")
        };
        assert_eq!(actual.base.status, tasks::TaskStatus::Completed);
        assert_eq!(actual.outcome.result.as_deref(), Some(raw));
        assert_eq!(
            registry
                .output_manager
                .read(
                    &registry.output_manager.path_for(&task_id).unwrap(),
                    tasks::output_manager::OutputOptions::default(),
                )
                .await
                .unwrap()
                .content,
            raw
        );
        assert_eq!(
            terminal_events(&listener.received.lock().await, &task_id),
            vec![("Completed".into(), None)]
        );
    }

    #[tokio::test]
    async fn local_app_missing_result_fails_closed_and_cleans_every_session_for_the_run() {
        use tasks::handlers::TaskStatusSink as _;

        let root = tempfile::tempdir().expect("tempdir");
        let app_id = "qa-app";
        scaffold_local_app(root.path(), app_id, local_apps::AppRuntimeProfile::ReactDom);
        let layout = local_apps::AppLayout::new(root.path(), app_id).expect("layout");
        let mut manifest = local_apps::load_manifest(&layout).expect("manifest");
        manifest.revision = 1;
        local_apps::save_manifest(&layout, &manifest).expect("revisioned QA manifest");
        let handles = [
            "qa_00000000000000000000000000000000",
            "qa_11111111111111111111111111111111",
        ];
        for handle in handles {
            seed_terminal_qa_session(root.path(), app_id, handle);
        }

        let registry = scope_test_registry();
        let task_id = register_terminal_test_workflow(
            &registry,
            Some(tasks::scope::LocalAppWorkflowTaskScope::for_use_test(app_id).expect("scope")),
            serde_json::json!({"app_id":app_id,"quality_level":"balanced"}),
        )
        .await;
        let listener = Arc::new(FakeListener::default());
        let sink = terminal_test_sink(root.path(), listener.clone(), registry.clone(), &task_id);
        let host = crate::mobile::local_apps_wire::broker_with_client_sink(
            root.path().to_path_buf(),
            client::adapter::ListenerSink::arc(listener.clone()),
            None,
            false,
            None,
        );
        sink.attach_local_apps_host(Arc::downgrade(&host))
            .expect("attach host");

        sink.finish_workflow_terminal(
            &task_id,
            lingxi_core::host::task_registry::WorkflowTerminalOutcome::default(),
            tasks::TaskStatus::Completed,
        )
        .await;

        let tasks::state::TaskState::LocalWorkflow(actual) = registry.get(&task_id).await.unwrap()
        else {
            panic!("expected workflow")
        };
        assert_eq!(actual.base.status, tasks::TaskStatus::Failed);
        assert!(actual
            .outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("verified workflow returned no result")));
        for handle in handles {
            assert!(
                local_apps::load_qa_session(&layout, handle).is_err(),
                "terminal cleanup must remove every raw QA session for the authenticated run"
            );
        }
        assert_eq!(
            terminal_events(&listener.received.lock().await, &task_id),
            vec![("Failed".into(), actual.outcome.error.clone(),)]
        );
    }

    #[tokio::test]
    async fn fake_local_app_qa_receipt_fails_registry_spool_and_client_together() {
        use tasks::handlers::TaskStatusSink as _;

        let root = tempfile::tempdir().expect("tempdir");
        let registry = scope_test_registry();
        let task_id = register_terminal_test_workflow(
            &registry,
            Some(tasks::scope::LocalAppWorkflowTaskScope::for_use_test("qa-app").expect("scope")),
            serde_json::json!({"app_id":"qa-app","quality_level":"thorough"}),
        )
        .await;
        let listener = Arc::new(FakeListener::default());
        let sink = terminal_test_sink(root.path(), listener.clone(), registry.clone(), &task_id);
        let host = crate::mobile::local_apps_wire::broker_with_client_sink(
            root.path().to_path_buf(),
            client::adapter::ListenerSink::arc(listener.clone()),
            None,
            false,
            None,
        );
        sink.attach_local_apps_host(Arc::downgrade(&host))
            .expect("attach host");

        sink.finish_workflow_terminal(
            &task_id,
            lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                result: Some(r#"{"ok":true,"receipt_id":"receipt_fake"}"#.into()),
                ..Default::default()
            },
            tasks::TaskStatus::Completed,
        )
        .await;

        let tasks::state::TaskState::LocalWorkflow(actual) = registry.get(&task_id).await.unwrap()
        else {
            panic!("expected workflow")
        };
        assert_eq!(actual.base.status, tasks::TaskStatus::Failed);
        assert!(actual.outcome.result.is_none());
        assert!(actual.outcome.error.as_deref().is_some_and(|error| {
            error.contains("local_app_completion_unverified")
                && !error.contains("terminal spool replacement failed")
        }));
        let spool = registry
            .output_manager
            .read(
                &registry.output_manager.path_for(&task_id).unwrap(),
                tasks::output_manager::OutputOptions::default(),
            )
            .await
            .unwrap();
        let spool: serde_json::Value = serde_json::from_str(&spool.content).unwrap();
        assert_eq!(spool["ok"], false);
        assert_eq!(spool["verified"], false);
        let events = terminal_events(&listener.received.lock().await, &task_id);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "Failed");
        assert!(events[0]
            .1
            .as_deref()
            .is_some_and(|error| { error.contains("local_app_completion_unverified") }));
    }

    #[tokio::test]
    async fn killed_local_app_callback_never_emits_candidate_completed() {
        use tasks::handlers::TaskStatusSink as _;

        let root = tempfile::tempdir().expect("tempdir");
        let registry = scope_test_registry();
        let task_id = register_terminal_test_workflow(
            &registry,
            Some(tasks::scope::LocalAppWorkflowTaskScope::for_build("qa-app").expect("scope")),
            serde_json::json!({"app_id":"qa-app"}),
        )
        .await;
        registry
            .set_status(&task_id, tasks::TaskStatus::Killed)
            .await
            .expect("kill wins");
        let listener = Arc::new(FakeListener::default());
        let sink = terminal_test_sink(root.path(), listener.clone(), registry.clone(), &task_id);

        sink.finish_workflow_terminal(
            &task_id,
            lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                result: Some(r#"{"ok":true,"receipt_id":"receipt_late"}"#.into()),
                ..Default::default()
            },
            tasks::TaskStatus::Completed,
        )
        .await;

        assert_eq!(
            registry.get(&task_id).await.unwrap().base().status,
            tasks::TaskStatus::Killed
        );
        let events = terminal_events(&listener.received.lock().await, &task_id);
        assert_eq!(events, vec![("Cancelled".into(), None)]);
        assert!(!events.iter().any(|(status, _)| status == "Completed"));
        let spool = registry
            .output_manager
            .read(
                &registry.output_manager.path_for(&task_id).unwrap(),
                tasks::output_manager::OutputOptions::default(),
            )
            .await
            .unwrap();
        let spool: serde_json::Value = serde_json::from_str(&spool.content).unwrap();
        assert_eq!(spool["ok"], false);
        assert_eq!(spool["status"], "killed");
    }

    #[tokio::test]
    async fn terminal_spool_failure_is_reported_as_failed_not_completed() {
        use tasks::handlers::TaskStatusSink as _;

        let root = tempfile::tempdir().expect("tempdir");
        let registry = scope_test_registry();
        let task_id = register_terminal_test_workflow(
            &registry,
            Some(tasks::scope::LocalAppWorkflowTaskScope::for_build("qa-app").expect("scope")),
            serde_json::json!({"app_id":"qa-app"}),
        )
        .await;
        let output_root = registry.output_manager.output_dir().to_path_buf();
        let moved_root = output_root.with_extension("moved");
        std::fs::rename(&output_root, &moved_root).expect("move pinned output root");
        std::fs::create_dir(&output_root).expect("replace output root identity");
        let listener = Arc::new(FakeListener::default());
        let sink = terminal_test_sink(root.path(), listener.clone(), registry.clone(), &task_id);

        sink.finish_workflow_terminal(
            &task_id,
            lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                result: Some(r#"{"ok":true,"receipt_id":"receipt_fake"}"#.into()),
                ..Default::default()
            },
            tasks::TaskStatus::Completed,
        )
        .await;

        let tasks::state::TaskState::LocalWorkflow(actual) = registry.get(&task_id).await.unwrap()
        else {
            panic!("expected workflow")
        };
        assert_eq!(actual.base.status, tasks::TaskStatus::Failed);
        assert!(actual
            .outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("terminal spool replacement failed")));
        let events = terminal_events(&listener.received.lock().await, &task_id);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "Failed");
        assert!(events[0]
            .1
            .as_deref()
            .is_some_and(|error| error.contains("terminal spool replacement failed")));
    }

    #[tokio::test]
    async fn unscoped_workflow_terminal_behavior_is_unchanged() {
        use tasks::handlers::TaskStatusSink as _;

        let root = tempfile::tempdir().expect("tempdir");
        let registry = scope_test_registry();
        let task_id = register_terminal_test_workflow(
            &registry,
            None,
            serde_json::json!({"app_id":"looks-like-an-app"}),
        )
        .await;
        let listener = Arc::new(FakeListener::default());
        let sink = terminal_test_sink(root.path(), listener.clone(), registry.clone(), &task_id);
        let result = "ordinary custom workflow result";

        sink.finish_workflow_terminal(
            &task_id,
            lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                result: Some(result.into()),
                ..Default::default()
            },
            tasks::TaskStatus::Completed,
        )
        .await;

        let tasks::state::TaskState::LocalWorkflow(actual) = registry.get(&task_id).await.unwrap()
        else {
            panic!("expected workflow")
        };
        assert_eq!(actual.base.status, tasks::TaskStatus::Completed);
        assert_eq!(actual.outcome.result.as_deref(), Some(result));
        assert_eq!(
            terminal_events(&listener.received.lock().await, &task_id),
            vec![("Completed".into(), None)]
        );
    }

    fn scope_test_launcher(
        root: &std::path::Path,
        registry: Arc<tasks::registry::TaskRegistry>,
    ) -> super::MobileWorkflowLauncher {
        let lingxi_home = root.join(".claude");
        let checkpoints = Arc::new(super::MobileWorkflowCheckpointStore::new(
            lingxi_home.clone(),
            root.to_path_buf(),
        ));
        let session_uuid = Arc::new(std::sync::Mutex::new("session-scope".to_string()));
        let status_sink = Arc::new(super::MobileWorkflowStatusSink::new(
            Arc::new(FakeListener::default()),
            checkpoints.clone(),
            session_uuid.clone(),
        ));
        status_sink.bind(registry.clone());
        let plugin_workflows = workflow::PluginWorkflowRegistry::new();
        let plugin_script_path = root.join("plugin-local-app-build.js");
        std::fs::write(&plugin_script_path, plugin_build_script()).expect("plugin workflow script");
        let plugin_use_test_script_path = root.join("plugin-local-app-use-test.js");
        std::fs::write(&plugin_use_test_script_path, plugin_use_test_script())
            .expect("plugin use-test workflow script");
        plugin_workflows.register(vec![
            workflow::PluginWorkflowEntry {
                name: crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into(),
                script_path: plugin_script_path,
            },
            workflow::PluginWorkflowEntry {
                name: crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID.into(),
                script_path: plugin_use_test_script_path,
            },
        ]);
        super::MobileWorkflowLauncher {
            registry,
            project_cwd: root.to_path_buf(),
            app_data_root: root.to_path_buf(),
            current_cwd: Arc::new(std::sync::Mutex::new(root.to_path_buf())),
            lingxi_home,
            session_uuid,
            default_model_selection_provider: Arc::new(std::sync::OnceLock::new()),
            checkpoints,
            status_sink,
            plugin_workflows: Arc::new(plugin_workflows),
        }
    }

    /// THE REGRESSION. A genuine, in-flight Local App build must block its
    /// app's delete, end to end: the Host resolves the app and mints a
    /// `LocalAppWorkflowTaskScope` at the launch seam, the launcher puts it on
    /// `TaskSpawnInput::LocalWorkflow`, `state_for_spawn` copies it onto the
    /// task row, and `find_nonterminal_local_app_workflows` finds the row.
    ///
    /// Every link is production code; only the task HANDLER is a stub, and it
    /// is a stub in the direction that cannot help the assertion (it neither
    /// sees nor sets the scope -- the registry does).
    ///
    /// Delete `scope: local_app_scope` from the launcher's spawn input (i.e.
    /// go back to a Host that mints nothing, which is the state this test was
    /// written against) and this goes red with the app unprotected, while
    /// every §8.1 forgery test stays green -- that asymmetry is the whole
    /// point, and it is why "everything is `None`" is not a safe default.
    #[tokio::test]
    async fn a_genuine_in_flight_local_app_build_blocks_that_apps_delete() {
        for (family, app_id) in [
            (local_apps::AppRuntimeProfile::ReactDom, "demo1234"),
            (local_apps::AppRuntimeProfile::Canvas2d, "canvas1234"),
        ] {
            let root = tempfile::tempdir().expect("tempdir");
            scaffold_local_app(root.path(), app_id, family);

            let registry = scope_test_registry();
            let launcher = scope_test_launcher(root.path(), registry.clone());

            let launched = {
                use tool_workflow::WorkflowLauncher as _;
                launcher
                    .launch(tool_workflow::WorkflowLaunchSpec {
                        name: Some(
                            crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID
                                .into(),
                        ),
                        args: Some(serde_json::json!({
                            "operation": "update",
                            "app_id": app_id,
                            "authoring_spec": fixture_authoring_spec_value(family),
                        })),
                        session_uuid: Some("session-scope".into()),
                        ..Default::default()
                    })
                    .await
                    .unwrap_or_else(|error| panic!("plugin build workflow must launch: {error}"))
            };

            assert_eq!(
                registry.find_nonterminal_local_app_workflows(app_id).await,
                vec![launched.task_id.clone()],
                "a genuine in-flight build must block its own app's delete"
            );
            assert!(
                registry
                    .find_nonterminal_local_app_workflows("some-other-app")
                    .await
                    .is_empty(),
                "and must block ONLY its own app's delete"
            );

            // The Host's verdict itself, not only its consequence: the seam mints
            // a scope for the app it RESOLVED, with the purpose the Host chose --
            // so the same run also takes that app's exclusive workspace lease.
            let mut genuine = tool_workflow::WorkflowLaunchSpec {
                name: Some(
                    crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into(),
                ),
                args: Some(serde_json::json!({
                    "operation": "update",
                    "app_id": app_id,
                    "authoring_spec": fixture_authoring_spec_value(family),
                })),
                ..Default::default()
            };
            let minted = super::apply_materialized_local_app_collections_with_identity(
                root.path(),
                &mut genuine,
                plugin_build_script(),
                false,
                Some(crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID),
                true,
            )
            .expect("the Host resolves this app")
            .expect("a genuine local-app build gets a scope");
            assert_eq!(minted.app_id(), app_id);
            assert!(
                minted.requires_workspace_lease(),
                "a build scope is what takes the app's exclusive workspace lease"
            );
        }
    }

    /// The real Plugin use-test launch must mint a UseTest scope and carry it
    /// through the registry row. A custom script that copies the short meta
    /// name remains unscoped, so an app id in caller args cannot borrow the QA
    /// gate or the delete guard.
    #[tokio::test]
    async fn a_verified_use_test_launch_gets_use_test_scope_but_custom_name_does_not() {
        use tool_workflow::WorkflowLauncher as _;

        let root = tempfile::tempdir().expect("tempdir");
        let app_id = "usetest123";
        scaffold_local_app(root.path(), app_id, local_apps::AppRuntimeProfile::ReactDom);
        let registry = scope_test_registry();
        let launcher = scope_test_launcher(root.path(), registry.clone());

        let launched = launcher
            .launch(tool_workflow::WorkflowLaunchSpec {
                name: Some(
                    crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID.into(),
                ),
                args: Some(serde_json::json!({
                    "app_id": app_id,
                    "scope": "acceptance",
                    "scenarios": [],
                    "quality_level": "balanced",
                })),
                session_uuid: Some("session-scope".into()),
                ..Default::default()
            })
            .await
            .expect("verified use-test workflow must launch");
        let row = registry
            .get(&launched.task_id)
            .await
            .expect("verified use-test row");
        let tasks::state::TaskState::LocalWorkflow(state) = row else {
            panic!("expected LocalWorkflow row")
        };
        let scope = state.scope.as_ref().expect("verified use-test scope");
        assert_eq!(scope.app_id(), app_id);
        assert_eq!(
            scope.purpose(),
            tasks::scope::LocalAppWorkflowPurpose::UseTest
        );
        assert!(!scope.requires_workspace_lease());
        assert_eq!(
            registry.find_nonterminal_local_app_workflows(app_id).await,
            vec![launched.task_id.clone()]
        );

        let custom = launcher
            .launch(tool_workflow::WorkflowLaunchSpec {
                name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID.into()),
                script: Some(
                    "export const meta = { name: 'local-app-use-test', description: 'Custom lookalike' };\nreturn 1;\n".into(),
                ),
                args: Some(serde_json::json!({"app_id": app_id})),
                session_uuid: Some("session-scope".into()),
                ..Default::default()
            })
            .await
            .expect("custom workflow should still launch");
        let custom_row = registry.get(&custom.task_id).await.expect("custom row");
        let tasks::state::TaskState::LocalWorkflow(custom_state) = custom_row else {
            panic!("expected custom LocalWorkflow row")
        };
        assert!(custom_state.scope.is_none());
    }

    #[tokio::test]
    async fn verified_use_test_launch_reaches_host_terminal_gate_but_custom_script_does_not() {
        use tasks::handlers::TaskStatusSink as _;
        use tool_workflow::WorkflowLauncher as _;

        let root = tempfile::tempdir().expect("tempdir");
        let app_id = "usetestgate";
        scaffold_local_app(root.path(), app_id, local_apps::AppRuntimeProfile::ReactDom);
        let registry = scope_test_registry();
        let launcher = scope_test_launcher(root.path(), registry.clone());
        let host = crate::mobile::local_apps_wire::broker_with_client_sink(
            root.path().to_path_buf(),
            client::adapter::ListenerSink::arc(Arc::new(FakeListener::default())),
            None,
            false,
            None,
        );
        launcher
            .status_sink
            .attach_local_apps_host(Arc::downgrade(&host))
            .expect("attach Host terminal validator");

        let genuine = launcher
            .launch(tool_workflow::WorkflowLaunchSpec {
                name: Some(
                    crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID.into(),
                ),
                args: Some(serde_json::json!({
                    "app_id": app_id,
                    "scope": "acceptance",
                    "scenarios": [],
                    "quality_level": "balanced",
                })),
                session_uuid: Some("session-scope".into()),
                ..Default::default()
            })
            .await
            .expect("verified use-test launch");
        registry
            .output_manager
            .allocate(&genuine.task_id)
            .await
            .expect("stub workflow spool");
        let forged_result = r#"{"ok":true,"receipt_id":"receipt_fake"}"#;
        launcher
            .status_sink
            .finish_workflow_terminal(
                &genuine.task_id,
                lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                    result: Some(forged_result.into()),
                    ..Default::default()
                },
                tasks::TaskStatus::Completed,
            )
            .await;
        let tasks::state::TaskState::LocalWorkflow(genuine_state) =
            registry.get(&genuine.task_id).await.expect("genuine task")
        else {
            panic!("expected genuine LocalWorkflow row")
        };
        assert_eq!(genuine_state.base.status, tasks::TaskStatus::Failed);
        assert!(genuine_state.outcome.error.as_deref().is_some_and(|error| {
            error.contains("local_app_completion_unverified")
                && !error.contains("terminal spool replacement failed")
        }));

        let custom = launcher
            .launch(tool_workflow::WorkflowLaunchSpec {
                name: Some(crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID.into()),
                script: Some(
                    "export const meta = { name: 'local-app-use-test', description: 'Custom lookalike' };\nreturn 1;\n".into(),
                ),
                args: Some(serde_json::json!({"app_id": app_id})),
                session_uuid: Some("session-scope".into()),
                ..Default::default()
            })
            .await
            .expect("same-name custom workflow launch");
        registry
            .output_manager
            .allocate(&custom.task_id)
            .await
            .expect("custom stub workflow spool");
        launcher
            .status_sink
            .finish_workflow_terminal(
                &custom.task_id,
                lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                    result: Some(forged_result.into()),
                    ..Default::default()
                },
                tasks::TaskStatus::Completed,
            )
            .await;
        let tasks::state::TaskState::LocalWorkflow(custom_state) =
            registry.get(&custom.task_id).await.expect("custom task")
        else {
            panic!("expected custom LocalWorkflow row")
        };
        assert!(custom_state.scope.is_none());
        assert_eq!(custom_state.base.status, tasks::TaskStatus::Completed);
        assert_eq!(custom_state.outcome.result.as_deref(), Some(forged_result));
    }

    /// r2-tests-honesty-006's launcher-level companion to
    /// `host_injected_arg_keys_are_exactly_the_expected_set` above. That gate
    /// only reaches `apply_materialized_local_app_collections` -- the SEAM --
    /// and cannot see either of the two places `launch` itself injects
    /// `workflow_run_id` (workflow_support.rs:1711, top-level, before the
    /// seam runs; and :1732, inside `host_context`, after the seam runs --
    /// the ONLY source of `host_context.workflow_run_id` on the `update`
    /// path, since the seam's own `host_context` literal for `update` never
    /// mentions the key), nor the create branch (:1303-1331), which returns
    /// before the seam's `runtime_profile` / `expected_writable_collections`
    /// inserts are reached at all.
    ///
    /// This drives the real `launch` end to end for both `operation`s and
    /// pins, at the launcher layer:
    /// - both `workflow_run_id` injections land the SAME id the workflow
    ///   actually ran under (`launched.run_id`), read back off the persisted
    ///   task row rather than off `spec` -- the row is what the workflow
    ///   script and any resume actually see;
    /// - the injected-key delta across the WHOLE launch path -- `launch`
    ///   itself PLUS the seam it calls -- is EXACTLY the expected set per
    ///   operation, using the same caller-key-snapshot-then-delta method as
    ///   the seam-level gate; each expected key is carried with the layer
    ///   that really writes it, so a key added or dropped fails loudly, by
    ///   name, and against the RIGHT layer instead of silently passing the
    ///   seam-level gate that cannot see the launcher.
    ///
    /// Scope, deliberately not over-claimed: this covers the launcher's
    /// local-app BUILD path (`PLUGIN_BUILD_WORKFLOW_ID`) only. `launch`'s
    /// other injection site, `enrich_persisted_plugin_workflow_context`
    /// (workflow_support.rs:2023), writes the same four key names for the
    /// use-test and mcp-authoring workflows and still has no key-set gate.
    ///
    /// The `create` operation is deliberately absent: the unified create
    /// launch was retired in favour of plan approval + `LocalAppPrepare` + a
    /// direct `LocalAppBuild`, so only `update` (and its resume) still reaches
    /// this launch path.
    #[tokio::test]
    async fn launch_injects_workflow_run_id_and_exactly_the_expected_launcher_keys() {
        use tool_workflow::WorkflowLauncher as _;

        for operation in ["update"] {
            let root = tempfile::tempdir().expect("tempdir");
            let app_id = "demo1234";
            let layout = local_apps::AppLayout::new(root.path(), app_id).expect("layout");
            let mut manifest = local_apps::AppManifest::for_new_app(app_id, "Fixture");
            stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::ReactDom);
            local_apps::save_manifest(&layout, &manifest).expect("manifest");
            install_active_authoring_contract(&layout, &manifest);
            stamp_record_mirror(&layout, true);

            let registry = scope_test_registry();
            let launcher = scope_test_launcher(root.path(), registry.clone());

            let caller_args = serde_json::json!({
                "app_id": app_id,
                "operation": operation,
                "authoring_spec": fixture_authoring_spec_value(local_apps::AppRuntimeProfile::ReactDom),
            });
            let caller_keys: std::collections::BTreeSet<String> = caller_args
                .as_object()
                .expect("caller args object")
                .keys()
                .cloned()
                .collect();

            let launched = launcher
                .launch(tool_workflow::WorkflowLaunchSpec {
                    name: Some(
                        crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into(),
                    ),
                    args: Some(caller_args),
                    session_uuid: Some("session-scope".into()),
                    ..Default::default()
                })
                .await
                .unwrap_or_else(|error| {
                    panic!("plugin build workflow must launch for operation {operation:?}: {error}")
                });

            let row = registry
                .get(&launched.task_id)
                .await
                .expect("the launch really did create a task row");
            let tasks::state::TaskState::LocalWorkflow(state) = row else {
                panic!("operation {operation:?}: expected a LocalWorkflow task row")
            };
            let args_json = state.args.as_deref().unwrap_or_else(|| {
                panic!("operation {operation:?}: the launcher must persist args onto the task row")
            });
            let after: serde_json::Value =
                serde_json::from_str(args_json).unwrap_or_else(|error| {
                    panic!("operation {operation:?}: persisted args are not valid json: {error}")
                });
            let after_object = after.as_object().unwrap_or_else(|| {
                panic!("operation {operation:?}: persisted args are not an object")
            });

            assert_eq!(
                after_object
                    .get("workflow_run_id")
                    .and_then(serde_json::Value::as_str),
                launched.run_id.as_deref(),
                "operation {operation:?}: the top-level `workflow_run_id` `launch` injects at \
                 workflow_support.rs:1711 must be the SAME id the launch actually ran under \
                 (persisted args: {after_object:?})"
            );
            assert_eq!(
                after_object
                    .get("host_context")
                    .and_then(|value| value.get("workflow_run_id"))
                    .and_then(serde_json::Value::as_str),
                launched.run_id.as_deref(),
                "operation {operation:?}: `host_context.workflow_run_id` is absent or is not \
                 the id this launch ran under. `launch` at workflow_support.rs:1732 is its ONLY \
                 source on the `update` path -- the seam's update-path `host_context` literal \
                 (:1448-1462) never mentions the key. If that injection is dropped the workflow \
                 script silently loses the run id from `host_context` (persisted host_context: \
                 {:?})",
                after_object.get("host_context")
            );
            if operation == "update" {
                let expected_digest =
                    crate::mobile::local_apps_build::active_build_authoring_contract_sha256(
                        &layout,
                    )
                    .expect("active build receipt")
                    .expect("active authoring contract digest");
                assert_eq!(
                    after_object
                        .get("host_context")
                        .and_then(|value| value.get("authoring_contract_sha256"))
                        .and_then(serde_json::Value::as_str),
                    Some(expected_digest.as_str()),
                    "update host context must carry the digest selected by the active successful build receipt"
                );
            }

            let injected: std::collections::BTreeSet<String> = after_object
                .keys()
                .filter(|key| !caller_keys.contains(*key))
                .cloned()
                .collect();
            // Verified against the source, not assumed. This delta spans BOTH
            // layers of the launch path -- `launch` itself and the seam
            // `apply_materialized_local_app_collections_with_identity` it
            // calls -- so every expected key is carried together with the
            // layer that really writes it, and a dropped key is reported
            // against that layer instead of always being blamed on `launch`.
            // (The retired `create` launch returned before the seam's
            // `runtime_profile` / `expected_writable_collections` inserts were
            // reached; only `update` still flows through both layers.)
            let expected_by_layer: std::collections::BTreeMap<String, &'static str> = vec![
                (
                    "workflow_run_id",
                    "`launch` itself, workflow_support.rs:1711",
                ),
                (
                    "runtime_profile",
                    "the seam `apply_materialized_local_app_collections_with_identity`, \
                     workflow_support.rs:1433",
                ),
                (
                    "expected_writable_collections",
                    "the seam, workflow_support.rs:1435",
                ),
                (
                    "host_context",
                    "the seam, workflow_support.rs:1446; its `workflow_run_id` member \
                     has no source other than `launch` at :1732",
                ),
            ]
            .into_iter()
            .map(|(key, layer)| (key.to_string(), layer))
            .collect();
            let expected: std::collections::BTreeSet<String> =
                expected_by_layer.keys().cloned().collect();

            let unexpected: Vec<&String> = injected.difference(&expected).collect();
            assert!(
                unexpected.is_empty(),
                "operation {operation:?}: the launch path -- `launch` itself AND the seam \
                 `apply_materialized_local_app_collections_with_identity` it calls -- injected \
                 workflow arg key(s) {unexpected:?} this test does not account for; a further \
                 injection at either layer needs its own coverage here, not silent acceptance \
                 (seen: {injected:?})"
            );
            let missing: Vec<(&String, &&'static str)> = expected_by_layer
                .iter()
                .filter(|(key, _)| !injected.contains(*key))
                .collect();
            assert!(
                missing.is_empty(),
                "operation {operation:?}: the launch path stopped injecting the key(s) \
                 {missing:?}, each listed with the layer that writes it -- on this operation \
                 not all of them come from `launch` itself (seen: {injected:?})"
            );
        }
    }

    #[test]
    fn host_context_uses_the_active_build_digest_across_contract_revisions() {
        let root = tempfile::tempdir().expect("tempdir");
        let app_id = "digest123";
        let layout = local_apps::AppLayout::new(root.path(), app_id).expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app(app_id, "Digest Fixture");
        stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::ReactDom);
        local_apps::save_manifest(&layout, &manifest).expect("manifest");
        install_active_authoring_contract(&layout, &manifest);
        stamp_record_mirror(&layout, true);

        let launch_context_digest = |spec_value: serde_json::Value| {
            let mut spec = tool_workflow::WorkflowLaunchSpec {
                name: Some(
                    crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into(),
                ),
                args: Some(serde_json::json!({
                    "operation": "update",
                    "app_id": app_id,
                    "authoring_spec": spec_value,
                })),
                ..Default::default()
            };
            super::apply_materialized_local_app_collections(
                root.path(),
                &mut spec,
                plugin_build_script(),
            )
            .expect("verified update launch");
            spec.args
                .as_ref()
                .and_then(|args| args.pointer("/host_context/authoring_contract_sha256"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .expect("Host digest")
        };

        let old_digest =
            crate::mobile::local_apps_build::active_build_authoring_contract_sha256(&layout)
                .expect("old receipt")
                .expect("old digest");
        let old_seen = launch_context_digest(fixture_authoring_spec_value(
            local_apps::AppRuntimeProfile::ReactDom,
        ));
        assert_eq!(old_seen, old_digest);

        let mut current =
            local_apps::load_authoring_contract(&layout, &old_digest).expect("old contract");
        current.revision += 1;
        let current_digest =
            local_apps::save_authoring_contract(&layout, &current).expect("current contract");
        assert_ne!(current_digest, old_digest);

        // A newer immutable contract file does not become active until the
        // successful-build receipt selects its digest.
        let still_old = launch_context_digest(fixture_authoring_spec_value(
            local_apps::AppRuntimeProfile::ReactDom,
        ));
        assert_eq!(still_old, old_digest);

        let receipt_path = layout
            .root()
            .join(layout.build_rel(false))
            .join("build.json");
        let mut receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receipt_path).expect("receipt"))
                .expect("receipt json");
        receipt["authoringContractSha256"] = serde_json::Value::String(current_digest.clone());
        std::fs::write(
            &receipt_path,
            serde_json::to_vec_pretty(&receipt).expect("receipt bytes"),
        )
        .expect("select current contract");
        let current_seen = launch_context_digest(fixture_authoring_spec_value(
            local_apps::AppRuntimeProfile::ReactDom,
        ));
        assert_eq!(current_seen, current_digest);
    }

    /// r1-workflow-runtime-03 / r1-e2e-trace-005 / r1-backlog-workflow-runtime-04:
    /// the Workflow tool's OWN resume hint (workflow_support.rs's sibling
    /// `tools/workflow/src/lib.rs:1497`) tells the model to resume with
    /// `{scriptPath, resumeFromRunId}` alone -- no `name`. Both places
    /// `launch` injects `workflow_run_id` (:1709 top-level, :1724 into
    /// `host_context`) were gated on `spec.name == Some(build id)`, so a
    /// resume shaped exactly like that hint got neither injection and the
    /// script's `HOST_CONTEXT_MISSING_RUN` guard would fire on the very next
    /// run. This drives a real `update` resume end to end through
    /// host-owned checkpoint provenance (never `spec.name`) and asserts both
    /// injections land the run id the launch actually ran under.
    /// The retired create resume (`named_resume_of_create_reuses_verified_selector_capability`)
    /// is gone with the unified create launch, which plan approval +
    /// `LocalAppPrepare` replaced.
    #[tokio::test]
    async fn scriptpath_resume_of_update_injects_workflow_run_id_without_a_name() {
        assert_resume_context(false).await;
    }

    #[tokio::test]
    async fn named_resume_of_update_preserves_verified_provenance() {
        assert_resume_context(true).await;
    }

    async fn assert_resume_context(named: bool) {
        use tool_workflow::WorkflowLauncher as _;

        let root = tempfile::tempdir().expect("tempdir");
        let app_id = "resume123";
        let layout = local_apps::AppLayout::new(root.path(), app_id).expect("layout");
        let mut manifest = local_apps::AppManifest::for_new_app(app_id, "Resume Fixture");
        stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::ReactDom);
        local_apps::save_manifest(&layout, &manifest).expect("manifest");
        install_active_authoring_contract(&layout, &manifest);
        stamp_record_mirror(&layout, true);

        let registry = scope_test_registry();
        let launcher = scope_test_launcher(root.path(), registry.clone());

        // Host-owned checkpoint provenance: the same bytes as the Host's
        // verified plugin registry entry, recorded under this run id/session
        // by a PRIOR launch. This is what `trusted_local_app_resume` proves --
        // never `spec.name`, which the resume hint never sends.
        let session = "session-scope";
        let run_id = "wf_resumecase1";
        let script_path = if named {
            launcher
                .checkpoints
                .persisted_workflow_script_path(session, run_id)
        } else {
            root.path().join("resumed-local-app-build.js")
        };
        std::fs::create_dir_all(script_path.parent().unwrap()).unwrap();
        let trusted_script = plugin_build_script();
        std::fs::write(&script_path, trusted_script).expect("resumed script file");
        launcher
            .checkpoints
            .upsert(
                session,
                super::WorkflowCheckpoint {
                    task_id: "tresume01".into(),
                    workflow_run_id: run_id.into(),
                    workflow_id: crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into(),
                    script_path: script_path.to_string_lossy().into_owned(),
                    script_sha256: Some(super::sha256_hex(trusted_script.as_bytes())),
                    script_is_verbatim_builtin: Some(true),
                    args_json: Some(
                        serde_json::json!({
                            "app_id": app_id,
                            "operation": "update",
                            "authoring_spec": fixture_authoring_spec_value(local_apps::AppRuntimeProfile::ReactDom),
                        })
                        .to_string(),
                    ),
                    description: "Resumed local app build".into(),
                    start_time: None,
                    transcript_dir: root
                        .path()
                        .join("transcript")
                        .to_string_lossy()
                        .into_owned(),
                },
            )
            .expect("checkpoint upsert");

        let launched = launcher
            .launch(tool_workflow::WorkflowLaunchSpec {
                name: named.then(|| crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into()),
                script_path: (!named).then(|| script_path.to_string_lossy().into_owned()),
                resume_from_run_id: Some(run_id.into()),
                session_uuid: Some(session.into()),
                args: Some(serde_json::json!({
                    "app_id": app_id,
                    "operation": "update",
                    "authoring_spec": fixture_authoring_spec_value(local_apps::AppRuntimeProfile::ReactDom),
                })),
                ..Default::default()
            })
            .await
            .unwrap_or_else(|error| {
                panic!("a scriptPath resume shaped like the tool's own hint must launch: {error}")
            });

        let row = registry
            .get(&launched.task_id)
            .await
            .expect("the resume really did create a task row");
        let tasks::state::TaskState::LocalWorkflow(state) = row else {
            panic!("expected a LocalWorkflow task row")
        };
        let args_json = state
            .args
            .as_deref()
            .expect("the launcher must persist args onto the task row");
        let after: serde_json::Value =
            serde_json::from_str(args_json).expect("persisted args are valid json");
        let after_object = after.as_object().expect("persisted args are an object");

        assert_eq!(
            after_object
                .get("workflow_run_id")
                .and_then(serde_json::Value::as_str),
            launched.run_id.as_deref(),
            "a scriptPath resume with no `name` must still receive the top-level \
             `workflow_run_id` `launch` injects at workflow_support.rs:1709/1711 -- gating \
             that injection on `spec.name` leaves it absent on exactly the resume shape the \
             tool's own hint text produces (persisted args: {after_object:?})"
        );
        assert_eq!(
            after_object
                .get("host_context")
                .and_then(|value| value.get("workflow_run_id"))
                .and_then(serde_json::Value::as_str),
            launched.run_id.as_deref(),
            "a scriptPath resume with no `name` must still receive \
             `host_context.workflow_run_id` (workflow_support.rs:1724/1732) -- without it the \
             resumed script's HOST_CONTEXT_MISSING_RUN guard fires on every update/verify \
             resume (persisted host_context: {:?})",
            after_object.get("host_context")
        );
    }

    /// r4-workflow-runtime-05: the SAME resume shape, for the OTHER two
    /// plugin workflows.
    ///
    /// `enrich_persisted_plugin_workflow_context` was gated twice on a
    /// by-name launch -- on `verified_plugin_workflow` (which
    /// `is_verified_plugin_workflow` returns `false` for whenever
    /// `spec.script_path` is non-empty) AND on `spec.name` -- while
    /// `local_app_resume_resolution_for_record` only ever resolved
    /// `PLUGIN_BUILD_WORKFLOW_ID`. Neither gate can be true on the resume
    /// shape the tool's own hint produces, so a `scriptPath` resume of
    /// use-test or mcp-authoring reached QuickJS with NO `host_context`, no
    /// `workflow_run_id` and no `runtime_profile`, and both scripts throw on
    /// their first context statement.
    ///
    /// The trust half is asserted in the same test, because it is what makes
    /// this safe: the workflow id in the checkpoint is NOT what grants the
    /// envelope. A row whose `script_is_verbatim_builtin` is `false` -- what
    /// a custom script that merely copied the plugin-qualified `meta.name`
    /// records -- must still resume with no `host_context`.
    #[tokio::test]
    async fn scriptpath_resume_of_use_test_and_mcp_authoring_gets_host_context() {
        use tool_workflow::WorkflowLauncher as _;

        for (workflow_id, app_id, run_id, meta_name, expected_operation) in [
            (
                crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID,
                "usetest1",
                "wf_usetestres1",
                "local-app-use-test",
                "verify",
            ),
            (
                crate::mobile::local_app_plugin_binding::PLUGIN_MCP_AUTHORING_WORKFLOW_ID,
                "mcpauth1",
                "wf_mcpauthres1",
                "local-app-mcp-authoring",
                "initial",
            ),
        ] {
            let root = tempfile::tempdir().expect("tempdir");
            let layout = local_apps::AppLayout::new(root.path(), app_id).expect("layout");
            let mut manifest = local_apps::AppManifest::for_new_app(app_id, "Resume Fixture");
            stamp_profile(&mut manifest, local_apps::AppRuntimeProfile::ReactDom);
            manifest.collections.push(local_apps::DataCollectionSchema {
                id: "progress".into(),
                name: "Progress".into(),
                fields: Vec::new(),
            });
            local_apps::save_manifest(&layout, &manifest).expect("manifest");
            install_active_authoring_contract(&layout, &manifest);
            stamp_record_mirror(&layout, true);

            let registry = scope_test_registry();
            let launcher = scope_test_launcher(root.path(), registry.clone());
            let session = "session-scope";
            let script = format!(
                "export const meta = {{ name: '{meta_name}', description: 'Plugin {meta_name}' \
                 }};\nreturn 1;\n"
            );
            let script_path = root.path().join(format!("resumed-{meta_name}.js"));
            std::fs::write(&script_path, &script).expect("resumed script file");

            // The host-owned provenance a PRIOR by-name launch of this plugin
            // workflow left behind: the plugin-qualified id plus the verbatim
            // marker plus the exact bytes.
            let trusted_checkpoint = |verbatim: bool| super::WorkflowCheckpoint {
                task_id: "tresume02".into(),
                workflow_run_id: run_id.into(),
                workflow_id: workflow_id.into(),
                script_path: script_path.to_string_lossy().into_owned(),
                script_sha256: Some(super::sha256_hex(script.as_bytes())),
                script_is_verbatim_builtin: Some(verbatim),
                args_json: Some(serde_json::json!({"app_id": app_id}).to_string()),
                description: "Resumed plugin workflow".into(),
                start_time: None,
                transcript_dir: root
                    .path()
                    .join("transcript")
                    .to_string_lossy()
                    .into_owned(),
            };
            launcher
                .checkpoints
                .upsert(session, trusted_checkpoint(true))
                .expect("checkpoint upsert");

            let resume_spec = || tool_workflow::WorkflowLaunchSpec {
                // Exactly the tool's own resume hint: no `name`.
                name: None,
                script_path: Some(script_path.to_string_lossy().into_owned()),
                resume_from_run_id: Some(run_id.into()),
                session_uuid: Some(session.into()),
                args: Some(serde_json::json!({ "app_id": app_id })),
                ..Default::default()
            };
            let launched = launcher.launch(resume_spec()).await.unwrap_or_else(|error| {
                panic!("{workflow_id}: a scriptPath resume shaped like the tool's own hint must launch: {error}")
            });
            let row = registry
                .get(&launched.task_id)
                .await
                .expect("the resume really did create a task row");
            let tasks::state::TaskState::LocalWorkflow(state) = row else {
                panic!("expected a LocalWorkflow task row")
            };
            let after: serde_json::Value = serde_json::from_str(
                state
                    .args
                    .as_deref()
                    .expect("the launcher must persist args onto the task row"),
            )
            .expect("persisted args are valid json");
            let context = after.get("host_context").unwrap_or_else(|| {
                panic!(
                    "{workflow_id}: a scriptPath resume must receive the Host-owned \
                     `host_context` envelope -- without it the script throws on its first \
                     context statement (persisted args: {after:?})"
                )
            });
            assert_eq!(
                context.get("source").and_then(serde_json::Value::as_str),
                Some("verified_host"),
                "{workflow_id}: the envelope must be the Host's own, got {context:?}"
            );
            assert_eq!(
                context.get("operation").and_then(serde_json::Value::as_str),
                Some(expected_operation),
                "{workflow_id}: got {context:?}"
            );
            assert_eq!(
                context.get("app_id").and_then(serde_json::Value::as_str),
                Some(app_id),
                "{workflow_id}: got {context:?}"
            );
            assert_eq!(
                context
                    .get("workflow_run_id")
                    .and_then(serde_json::Value::as_str),
                launched.run_id.as_deref(),
                "{workflow_id}: the envelope must carry the run id this launch actually ran \
                 under, got {context:?}"
            );
            assert_eq!(
                context
                    .get("runtime_profile")
                    .and_then(|profile| profile.get("family"))
                    .and_then(serde_json::Value::as_str),
                Some("react_dom"),
                "{workflow_id}: the persisted profile must reach the resumed script, got \
                 {context:?}"
            );
            assert_eq!(
                after.get("expected_writable_collections"),
                Some(&serde_json::json!(["progress"])),
                "{workflow_id}: the manifest's collection ids must reach the resumed script, \
                 got {after:?}"
            );

            // The negative half: same id, same bytes, same run id -- only the
            // verbatim marker flipped, which is what a custom script that
            // copied the plugin-qualified name records. No envelope.
            let forged_root = tempfile::tempdir().expect("tempdir");
            let forged_layout =
                local_apps::AppLayout::new(forged_root.path(), app_id).expect("layout");
            let mut forged_manifest = local_apps::AppManifest::for_new_app(app_id, "Forged");
            stamp_profile(
                &mut forged_manifest,
                local_apps::AppRuntimeProfile::ReactDom,
            );
            local_apps::save_manifest(&forged_layout, &forged_manifest).expect("manifest");
            stamp_record_mirror(&forged_layout, true);
            let forged_registry = scope_test_registry();
            let forged_launcher = scope_test_launcher(forged_root.path(), forged_registry.clone());
            let forged_script_path = forged_root.path().join(format!("forged-{meta_name}.js"));
            std::fs::write(&forged_script_path, &script).expect("forged script file");
            forged_launcher
                .checkpoints
                .upsert(
                    session,
                    super::WorkflowCheckpoint {
                        script_path: forged_script_path.to_string_lossy().into_owned(),
                        transcript_dir: forged_root
                            .path()
                            .join("transcript")
                            .to_string_lossy()
                            .into_owned(),
                        ..trusted_checkpoint(false)
                    },
                )
                .expect("checkpoint upsert");
            let forged = forged_launcher
                .launch(tool_workflow::WorkflowLaunchSpec {
                    name: None,
                    script_path: Some(forged_script_path.to_string_lossy().into_owned()),
                    resume_from_run_id: Some(run_id.into()),
                    session_uuid: Some(session.into()),
                    args: Some(serde_json::json!({ "app_id": app_id })),
                    ..Default::default()
                })
                .await
                .expect("a custom workflow still launches; it just gets no authority");
            let forged_row = forged_registry
                .get(&forged.task_id)
                .await
                .expect("the forged resume really did create a live task row");
            let tasks::state::TaskState::LocalWorkflow(forged_state) = forged_row else {
                panic!("expected a LocalWorkflow task row")
            };
            let forged_args: serde_json::Value = serde_json::from_str(
                forged_state
                    .args
                    .as_deref()
                    .expect("the launcher must persist args onto the task row"),
            )
            .expect("persisted args are valid json");
            assert!(
                forged_args.get("host_context").is_none(),
                "{workflow_id}: a checkpoint whose script_is_verbatim_builtin is false is what a \
                 custom script copying the plugin-qualified meta.name records -- it must NOT \
                 receive the Host envelope, got {forged_args:?}"
            );
        }
    }

    /// §8.1's other half, which the fix above must not trade away: a CUSTOM
    /// workflow that forges every caller-controlled input it has -- the
    /// launch `name`, the script's own `meta.name`, and `args.app_id` --
    /// still gets no scope, so it neither takes the workspace lease nor
    /// blocks the victim's delete.
    ///
    /// Both forgery shapes are exercised: an inline body under a forged name
    /// (an inline `script` beats the named built-in in `resolve_script_at`,
    /// so the name buys the forger nothing), and a `scriptPath` file. The
    /// assertion is deliberately NOT "the guard returned empty" alone -- an
    /// empty result would also be produced by a launch that failed outright,
    /// which would prove nothing. Each launch is asserted to have produced a
    /// live, non-terminal task row FIRST; the row exists and is running, and
    /// is still not allowed to block the victim.
    #[tokio::test]
    async fn a_forged_custom_workflow_gets_no_scope_and_cannot_block_the_victims_delete() {
        use tool_workflow::WorkflowLauncher as _;

        let root = tempfile::tempdir().expect("tempdir");
        // The victim is a REAL, fully scaffolded app -- so the only thing
        // missing from the forger's launch is the Host's own verdict, not the
        // app.
        scaffold_local_app(
            root.path(),
            "victim12",
            local_apps::AppRuntimeProfile::ReactDom,
        );

        let registry = scope_test_registry();
        let launcher = scope_test_launcher(root.path(), registry.clone());

        let forged_body =
            "export const meta = { name: 'local-app-build', description: 'not a build' }\n\
             return 1\n";
        let forged_path = root.path().join("forged.js");
        std::fs::write(&forged_path, forged_body).expect("write forged script");

        let inline = launcher
            .launch(tool_workflow::WorkflowLaunchSpec {
                // Forged launch name...
                name: Some(
                    crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into(),
                ),
                // ...forged `meta.name` inside a body that is not the
                // bundled built-in...
                script: Some(forged_body.into()),
                // ...and the victim's app id forged into caller args.
                args: Some(serde_json::json!({ "app_id": "victim12" })),
                session_uuid: Some("session-scope".into()),
                ..Default::default()
            })
            .await
            .expect("a custom workflow still launches; it just gets no authority");

        let by_path = launcher
            .launch(tool_workflow::WorkflowLaunchSpec {
                script_path: Some(forged_path.to_string_lossy().into_owned()),
                args: Some(serde_json::json!({ "app_id": "victim12" })),
                session_uuid: Some("session-scope".into()),
                ..Default::default()
            })
            .await
            .expect("a custom scriptPath workflow still launches");

        for launched in [&inline, &by_path] {
            let row = registry
                .get(&launched.task_id)
                .await
                .expect("the forged launch really did create a task row");
            assert!(
                !row.base().status.is_terminal(),
                "the forged row must be live when the guard is asked, or an \
                 empty guard result would prove nothing: {row:?}"
            );
        }

        assert!(
            registry
                .find_nonterminal_local_app_workflows("victim12")
                .await
                .is_empty(),
            "a forged custom workflow must not block the victim app's delete, \
             however convincing its name and args are"
        );

        // And the same verdict read directly off the seam, which covers the
        // OTHER half of "gets nothing": no scope at all means
        // `requires_workspace_lease` is false too, not merely that the delete
        // guard happened to skip it.
        for (label, mut spec) in [
            (
                "inline body under a forged name",
                tool_workflow::WorkflowLaunchSpec {
                    name: Some(
                        crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID.into(),
                    ),
                    script: Some(forged_body.into()),
                    args: Some(serde_json::json!({ "app_id": "victim12" })),
                    ..Default::default()
                },
            ),
            (
                "scriptPath body with a forged meta.name",
                tool_workflow::WorkflowLaunchSpec {
                    script_path: Some(forged_path.to_string_lossy().into_owned()),
                    args: Some(serde_json::json!({ "app_id": "victim12" })),
                    ..Default::default()
                },
            ),
        ] {
            let minted = super::apply_materialized_local_app_collections_with_identity(
                root.path(),
                &mut spec,
                forged_body,
                false,
                None,
                false,
            )
            .expect("a custom workflow is not refused here, only left unscoped");
            assert!(
                minted.is_none(),
                "{label}: a forged custom workflow must get NO scope -- no \
                 workspace lease and no delete block"
            );
        }
    }

    // ── P-1.12: re-mint scope at the restart-ADOPTION seam ──────────────

    /// Write a checkpoint through the real `upsert`/journal machinery so
    /// `adopt_session` sees exactly what a restart would find on disk, then
    /// adopt it. `script_bytes` is what a genuine build's persisted script
    /// would look like (bundled bytes) OR what a forged custom workflow's
    /// would look like (anything else) -- `resolve_adopted_local_app_build_scope`
    /// is supposed to tell those apart from the BYTES alone, never from
    /// `workflow_id`/`args`.
    async fn persist_and_adopt_checkpoint(
        checkpoints: &super::MobileWorkflowCheckpointStore,
        registry: &tasks::registry::TaskRegistry,
        app_data_root: &std::path::Path,
        session_uuid: &str,
        task_id: &str,
        run_id: &str,
        workflow_id: &str,
        app_id: &str,
        script_bytes: &[u8],
        script_is_verbatim_builtin: bool,
    ) {
        let transcript_dir = checkpoints
            .session_dir(session_uuid)
            .join("subagents")
            .join("workflows")
            .join(run_id);
        std::fs::create_dir_all(&transcript_dir).expect("transcript dir");
        std::fs::write(transcript_dir.join("journal.jsonl"), "").expect("journal");
        let script_path = app_data_root.join(format!("{run_id}.js"));
        std::fs::write(&script_path, script_bytes).expect("persisted script");
        checkpoints
            .upsert(
                session_uuid,
                super::WorkflowCheckpoint {
                    task_id: task_id.to_string(),
                    workflow_run_id: run_id.to_string(),
                    // A real build's name, and ALSO exactly what a forged
                    // checkpoint would carry (see `AdoptedWorkflow`'s doc
                    // comment) -- both tests below use the SAME
                    // `workflow_id` on purpose, so only the script bytes can
                    // be what tells them apart.
                    workflow_id: workflow_id.to_string(),
                    script_path: script_path.to_string_lossy().into_owned(),
                    script_sha256: Some(super::sha256_hex(script_bytes)),
                    script_is_verbatim_builtin: Some(script_is_verbatim_builtin),
                    args_json: Some(serde_json::json!({ "app_id": app_id }).to_string()),
                    description: "Build local app".into(),
                    start_time: Some(1_234),
                    transcript_dir: transcript_dir.to_string_lossy().into_owned(),
                },
            )
            .expect("persist checkpoint");
        checkpoints
            .adopt_session(session_uuid, registry, app_data_root)
            .await;
    }

    /// THE ADOPTION-SEAM REGRESSION (P-1.12 residual 1). A genuine Local App
    /// build's checkpoint -- persisted with the REAL bundled build script's
    /// bytes -- must come back from an engine restart still blocking its
    /// app's delete: `resolve_adopted_local_app_build_scope` re-derives a
    /// scope from the persisted script bytes + the real on-disk manifest, and
    /// `register_adopted_workflow_with_scope` stamps it on the adopted
    /// (`Paused`, non-terminal) row.
    ///
    /// Read back `checkpoint.workflow_id`/`args.app_id` instead of the real
    /// script bytes and this test still passes (a forged checkpoint can set
    /// those to the same values) while
    /// `an_adopted_forged_workflow_still_gets_no_scope` below goes red -- that
    /// asymmetry is the point of re-deriving from bytes, not from the
    /// checkpoint's own claims.
    #[tokio::test]
    async fn an_adopted_in_flight_build_still_blocks_its_apps_delete() {
        let root = tempfile::tempdir().expect("tempdir");
        let app_id = "resumedapp";
        scaffold_local_app(root.path(), app_id, local_apps::AppRuntimeProfile::ReactDom);

        let registry = scope_test_registry();
        let checkpoints = super::MobileWorkflowCheckpointStore::new(
            root.path().join(".claude"),
            root.path().to_path_buf(),
        );
        let session_uuid = "session-adopt-genuine";
        let real_script = plugin_build_script();

        persist_and_adopt_checkpoint(
            &checkpoints,
            &registry,
            root.path(),
            session_uuid,
            "wgenuine1",
            "wf_genuin1",
            crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID,
            app_id,
            real_script.as_bytes(),
            true,
        )
        .await;

        let state = registry
            .get("wgenuine1")
            .await
            .expect("adopted row must exist");
        assert!(
            !state.base().status.is_terminal(),
            "an adopted row must be non-terminal (Paused)"
        );
        assert_eq!(
            registry.find_nonterminal_local_app_workflows(app_id).await,
            vec!["wgenuine1".to_string()],
            "a genuine in-flight build recovered by adoption must still \
             block its own app's delete"
        );
        assert!(
            registry
                .find_nonterminal_local_app_workflows("some-other-app")
                .await
                .is_empty(),
            "and must block ONLY its own app's delete"
        );
    }

    /// Restart adoption must preserve the Host-authenticated UseTest
    /// purpose, while the same workflow id/app id with a custom script stays
    /// unscoped. This exercises the persisted checkpoint path rather than
    /// manually constructing a scope for the registry.
    #[tokio::test]
    async fn an_adopted_in_flight_use_test_gets_use_test_scope() {
        let root = tempfile::tempdir().expect("tempdir");
        let app_id = "adoptusetest";
        scaffold_local_app(root.path(), app_id, local_apps::AppRuntimeProfile::ReactDom);
        let registry = scope_test_registry();
        let checkpoints = super::MobileWorkflowCheckpointStore::new(
            root.path().join(".claude"),
            root.path().to_path_buf(),
        );
        persist_and_adopt_checkpoint(
            &checkpoints,
            &registry,
            root.path(),
            "session-adopt-use-test",
            "wuadopt01",
            "wf_useadopt1",
            crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID,
            app_id,
            plugin_use_test_script().as_bytes(),
            true,
        )
        .await;
        let state = registry
            .get("wuadopt01")
            .await
            .expect("adopted use-test row");
        let tasks::state::TaskState::LocalWorkflow(state) = state else {
            panic!("expected LocalWorkflow row")
        };
        let scope = state.scope.as_ref().expect("adopted UseTest scope");
        assert_eq!(scope.app_id(), app_id);
        assert_eq!(
            scope.purpose(),
            tasks::scope::LocalAppWorkflowPurpose::UseTest
        );
        assert_eq!(
            registry.find_nonterminal_local_app_workflows(app_id).await,
            vec!["wuadopt01".to_string()]
        );

        let forged_root = tempfile::tempdir().expect("forged tempdir");
        scaffold_local_app(
            forged_root.path(),
            app_id,
            local_apps::AppRuntimeProfile::ReactDom,
        );
        let forged_registry = scope_test_registry();
        let forged_checkpoints = super::MobileWorkflowCheckpointStore::new(
            forged_root.path().join(".claude"),
            forged_root.path().to_path_buf(),
        );
        persist_and_adopt_checkpoint(
            &forged_checkpoints,
            &forged_registry,
            forged_root.path(),
            "session-adopt-use-test",
            "wuforged1",
            "wf_useforged",
            crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID,
            app_id,
            b"export const meta = { name: 'local-app-use-test' }; return 1;",
            false,
        )
        .await;
        let forged_state = forged_registry
            .get("wuforged1")
            .await
            .expect("forged adopted row");
        let tasks::state::TaskState::LocalWorkflow(forged_state) = forged_state else {
            panic!("expected forged LocalWorkflow row")
        };
        assert!(forged_state.scope.is_none());
        assert!(forged_registry
            .find_nonterminal_local_app_workflows(app_id)
            .await
            .is_empty());
    }

    /// P-1.12 residual 1, the other half. A checkpoint that carries a real
    /// build workflow's `workflow_id` and a victim app's id in `args` --
    /// exactly as faithfully as a genuine build's checkpoint would, per
    /// `AdoptedWorkflow`'s doc comment -- but whose PERSISTED SCRIPT is a
    /// custom body (not the bundled bytes) must still get no scope on
    /// adoption, and so must not block the victim app's delete. This is the
    /// ⛔ constraint from the task brief made concrete: adoption must never
    /// derive authority from `workflow_id`/`args` alone.
    #[tokio::test]
    async fn an_adopted_forged_workflow_still_gets_no_scope() {
        let root = tempfile::tempdir().expect("tempdir");
        let victim_app_id = "victimapp1";
        // The victim is a REAL, fully scaffolded app -- so the only thing
        // missing from the forged checkpoint is the Host's own re-derived
        // verdict, not the app.
        scaffold_local_app(
            root.path(),
            victim_app_id,
            local_apps::AppRuntimeProfile::ReactDom,
        );

        let registry = scope_test_registry();
        let checkpoints = super::MobileWorkflowCheckpointStore::new(
            root.path().join(".claude"),
            root.path().to_path_buf(),
        );
        let session_uuid = "session-adopt-forged";
        let forged_body =
            "export const meta = { name: 'local-app-build', description: 'not a build' }\n\
             return 1\n";

        persist_and_adopt_checkpoint(
            &checkpoints,
            &registry,
            root.path(),
            session_uuid,
            "wforged12",
            "wf_forged1",
            crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID,
            victim_app_id,
            forged_body.as_bytes(),
            false,
        )
        .await;

        let state = registry
            .get("wforged12")
            .await
            .expect("adopted row must still exist even though it got no scope");
        assert!(
            !state.base().status.is_terminal(),
            "the adopted row must be live when the guard is asked, or an \
             empty guard result would prove nothing"
        );
        assert!(
            registry
                .find_nonterminal_local_app_workflows(victim_app_id)
                .await
                .is_empty(),
            "a forged workflow_id/args.app_id pair must never block another \
             app's delete just because it was adopted"
        );
    }
}

#[cfg(test)]
mod workspace_lease_forwarding_tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use lingxi_core::host::tool_invoker::{
        SubagentInvocationContext, ToolInvoker, ToolInvokerError,
    };

    /// Terminal invoker that records the lease token it was dispatched with.
    struct RecordingInvoker {
        seen: Arc<StdMutex<Option<Option<u64>>>>,
    }

    #[async_trait::async_trait]
    impl ToolInvoker for RecordingInvoker {
        async fn invoke_detailed(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: SubagentInvocationContext,
            workspace_lease_token: Option<u64>,
        ) -> Result<lingxi_core::host::tool_invoker::ToolInvocationResult, ToolInvokerError>
        {
            *self.seen.lock().unwrap() = Some(workspace_lease_token);
            Ok(lingxi_core::host::tool_invoker::ToolInvocationResult {
                is_error: true,
                data: serde_json::json!({"error": "contract validation failed"}),
                model_content: Some("contract validation failed".into()),
            })
        }

        async fn invoke(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: SubagentInvocationContext,
        ) -> Result<serde_json::Value, ToolInvokerError> {
            *self.seen.lock().unwrap() = Some(None);
            Ok(serde_json::json!({}))
        }

        async fn invoke_with_workspace_lease(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: SubagentInvocationContext,
            workspace_lease_token: Option<u64>,
        ) -> Result<serde_json::Value, ToolInvokerError> {
            *self.seen.lock().unwrap() = Some(workspace_lease_token);
            Ok(serde_json::json!({}))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[tokio::test]
    async fn deferred_invoker_preserves_error_content_and_workspace_lease() {
        let seen = Arc::new(StdMutex::new(None));
        let deferred = super::DeferredToolInvoker::new();
        deferred.set(Arc::new(RecordingInvoker { seen: seen.clone() }));
        let result = deferred
            .invoke_detailed(
                "LocalAppContract",
                serde_json::json!({}),
                bare_ctx(),
                Some(77),
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert_eq!(
            result.data,
            serde_json::json!({"error": "contract validation failed"})
        );
        assert_eq!(
            result.model_content.as_deref(),
            Some("contract validation failed")
        );
        assert_eq!(*seen.lock().unwrap(), Some(Some(77)));
    }

    fn bare_ctx() -> SubagentInvocationContext {
        SubagentInvocationContext {
            permission_pause_observer: None,
            parent_agent_id: None,
            origin_session_id: None,
            tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
            agent_name: None,
            team_name: None,
            is_async: false,
            is_non_interactive_session: false,
            can_show_permission_prompts: false,
            cwd: None,
            tool_use_id: None,
            assistant_message_id: None,
            depth: 0,
            observer: None,
            parent_model: None,
            parent_model_profile: None,
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        }
    }

    /// A lease token handed to the deferred invoker must reach the real
    /// invoker underneath it. The trait's delegating default drops it, which
    /// left `PermissionCheckContext.workspace_lease_token` permanently `None`
    /// in production: the lease ALLOW never fired, and the paired
    /// `denies_host_owned_for_token` hard deny never fired either.
    #[tokio::test]
    async fn deferred_invoker_forwards_the_workspace_lease_token() {
        let seen = Arc::new(StdMutex::new(None));
        let deferred = super::DeferredToolInvoker::new();
        deferred.set(Arc::new(RecordingInvoker { seen: seen.clone() }));

        deferred
            .invoke_with_workspace_lease("Read", serde_json::json!({}), bare_ctx(), Some(77))
            .await
            .expect("dispatch");

        assert_eq!(
            *seen.lock().unwrap(),
            Some(Some(77)),
            "the deferred invoker must forward the lease token, not swallow it"
        );
    }
}
