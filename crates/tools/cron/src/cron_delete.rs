//! `CronDeleteTool` — cancel a scheduled cron job by id, removing durable jobs
//! from the project tasks file and session-only jobs from the live scheduler.
//!
//! 1:1 parity port of claude-code `CronDeleteTool.ts`. The model supplies the
//! job `id` returned by `CronCreate`; the tool read-modify-writes the tasks
//! file (dropping the matching task) and reports `Cancelled job <id>.`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use telemetry::pii::Verified;
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{CRON_DELETE_COMPLETED, CRON_DELETE_FAILED, CRON_DELETE_STARTED};
use telemetry::AnalyticsBus;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};

use std::path::Path;

use crate::schedule_cron::cron_file_path;

/// Tool name byte-lock.
pub const CRON_DELETE_TOOL_NAME: &str = "CronDelete";

/// Model-facing description (CronDeleteTool.ts `description()`). Byte-verified
/// against 2.1.206: the terse `CRON_DELETE_DESCRIPTION` form ("Cancel a
/// scheduled cron job by ID", 2 hits) is what the binary emits — the earlier
/// verbose "Cancel a cron job previously scheduled with CronCreate." seam had 0
/// hits in 206.
const CRON_DELETE_DESCRIPTION: &str = "Cancel a scheduled cron job by ID";

static SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "id": {
                "type": "string",
                "description": "Job ID returned by CronCreate."
            }
        },
        "required": ["id"]
    })
});

fn verified_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(Verified::assert_safe(s.to_string()).into_inner())
}

async fn emit_failed(bus: &Arc<AnalyticsBus>, kind: &str, duration_ms: u64) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert("error_kind".into(), verified_str(kind));
    md.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(duration_ms as i64),
    );
    bus.log_event(CRON_DELETE_FAILED, md).await;
}

/// Does a persisted task with `id` exist in the project's single tasks file?
async fn job_exists(fs: &dyn lingxi_core::host::FileSystem, project_root: &Path, id: &str) -> bool {
    match cron::tasks_file::read_tasks_body(fs, project_root).await {
        Ok(body) => cron::tasks_file::parse_tasks(&body)
            .tasks
            .iter()
            .any(|t| t.id == id),
        Err(_) => false,
    }
}

fn teammate_owner(ctx: &ToolUseContext) -> Option<String> {
    ctx.agent_name.as_ref().map(|name| {
        ctx.agent_id
            .map_or_else(|| name.clone(), |agent_id| agent_id.to_string())
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobLocation {
    Session,
    Durable,
    Foreign,
}

async fn locate_accessible_job(
    tool_ctx: &tool_api::BuiltinToolContext,
    call_ctx: &ToolUseContext,
    id: &str,
) -> Option<JobLocation> {
    let owner = teammate_owner(call_ctx);
    if let Some(registry) = &tool_ctx.task_registry {
        if let Ok(tasks) = cron::session_jobs(registry).await {
            if let Some(task) = tasks.into_iter().find(|task| task.id == id) {
                return Some(
                    if owner
                        .as_deref()
                        .is_none_or(|owner| task.owner.as_deref() == Some(owner))
                    {
                        JobLocation::Session
                    } else {
                        JobLocation::Foreign
                    },
                );
            }
        }
    }
    if job_exists(
        tool_ctx.fs.as_ref(),
        &tool_ctx.session_cwd.project_root(),
        id,
    )
    .await
    {
        Some(if owner.is_none() {
            JobLocation::Durable
        } else {
            JobLocation::Foreign
        })
    } else {
        None
    }
}

/// `CronDeleteTool` — cancel a scheduled cron job by id.
pub struct CronDeleteTool {
    pub(crate) ctx: tool_api::BuiltinToolContext,
}

impl CronDeleteTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for CronDeleteTool {
    fn name(&self) -> &str {
        CRON_DELETE_TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("cancel a scheduled cron job")
    }
    fn native_input_validation(&self) -> bool {
        true
    }
    fn input_schema(&self) -> &Value {
        &SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        // PARITY 2.1.263 `EC()`: `!CLAUDE_CODE_DISABLE_CRON && gate(tengu_kairos_cron, true)`
        // — the env kill switch hides the tool from the model.
        crate::cron_tools_enabled()
    }
    fn max_result_size_chars(&self) -> usize {
        100_000
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn get_path(&self, _: &Value) -> Option<std::path::PathBuf> {
        Some(cron::tasks_file::session_scheduled_tasks_path(
            &self.ctx.session_cwd.project_root(),
        ))
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _: &Value) -> bool {
        // CronDeleteTool.ts does not flag destructive; deleting a job descriptor
        // is treated as non-destructive (it is the user's own scheduled job).
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "CronDelete removes a cron job from .claude/scheduled_tasks.json".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        CRON_DELETE_DESCRIPTION.into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        if !crate::schedule_cron::durable_enabled() {
            return "Cancel a cron job previously scheduled with CronCreate. Removes it from the in-memory session store.".into();
        }
        // PARITY 2.1.263 `mbn(true)`.
        "Cancel a cron job previously scheduled with CronCreate. Removes it from .claude/scheduled_tasks.json (durable jobs) or the in-memory session store (session-only jobs).".into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        call_ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let id = input
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ValidationError("CronDelete: missing or non-string id".into()))?;

        match locate_accessible_job(&self.ctx, call_ctx, id).await {
            Some(JobLocation::Session | JobLocation::Durable) => {}
            Some(JobLocation::Foreign) => {
                return Err(ValidationError(format!(
                    "Cannot delete cron job '{id}': owned by another agent"
                )));
            }
            None => return Err(ValidationError(format!("No scheduled job with id '{id}'"))),
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        call_ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let bus = self.ctx.bus.clone();

        let id = match input.get("id").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                emit_failed(&bus, "missing_id", started.elapsed().as_millis() as u64).await;
                return Err(ToolError::InvalidInput(
                    "CronDelete: missing or non-string id".into(),
                ));
            }
        };

        let mut md: LogEventMetadata = HashMap::new();
        md.insert("id".into(), verified_str(&id));
        bus.log_event(CRON_DELETE_STARTED, md).await;

        let location = match locate_accessible_job(&self.ctx, &call_ctx, &id).await {
            Some(JobLocation::Foreign) => {
                emit_failed(&bus, "foreign_owner", started.elapsed().as_millis() as u64).await;
                return Err(ToolError::InvalidInput(format!(
                    "Cannot delete cron job '{id}': owned by another agent"
                )));
            }
            Some(location) => location,
            None => {
                emit_failed(&bus, "not_found", started.elapsed().as_millis() as u64).await;
                return Err(ToolError::InvalidInput(format!(
                    "No scheduled job with id '{id}'"
                )));
            }
        };

        if location == JobLocation::Session {
            let registry = self.ctx.task_registry.as_ref().ok_or_else(|| {
                ToolError::Internal("CronDelete: active scheduler disappeared".into())
            })?;
            let owner = teammate_owner(&call_ctx);
            let removed = cron::unregister_live_job(registry, &id, owner.as_deref())
                .await
                .map_err(|error| ToolError::Internal(format!("CronDelete: {error}")))?;
            if !removed {
                return Err(ToolError::InvalidInput(format!(
                    "No scheduled job with id '{id}'"
                )));
            }
            let mut md: LogEventMetadata = HashMap::new();
            md.insert(
                "duration_ms".into(),
                AnalyticsValue::Int(started.elapsed().as_millis() as i64),
            );
            bus.log_event(CRON_DELETE_COMPLETED, md).await;
            return Ok(cancelled_result(&id));
        }

        // Read-modify-write the single `{ "tasks": [...] }` file: drop the task
        // with the matching id and write the rest back. A missing file / missing
        // id surfaces the byte-exact "No scheduled job with id '<id>'" error.
        let path = cron_file_path(&self.ctx.session_cwd.project_root());
        let _process_guard = cron::lock_cron_file().await;
        let _file_guard = match cron::tasks_file::lock_scheduled_tasks(
            self.ctx.fs.as_ref(),
            &self.ctx.session_cwd.project_root(),
        )
        .await
        {
            Ok(guard) => guard,
            Err(e) => {
                emit_failed(&bus, "io_lock", started.elapsed().as_millis() as u64).await;
                return Err(ToolError::Io(format!(
                    "CronDelete: io error at {}: {e}",
                    path.display()
                )));
            }
        };
        let body = match cron::tasks_file::read_tasks_body(
            self.ctx.fs.as_ref(),
            &self.ctx.session_cwd.project_root(),
        )
        .await
        {
            Ok(b) => b,
            Err(_) => {
                emit_failed(&bus, "not_found", started.elapsed().as_millis() as u64).await;
                return Err(ToolError::InvalidInput(format!(
                    "No scheduled job with id '{id}'"
                )));
            }
        };
        let mut doc = cron::tasks_file::parse_tasks(&body);
        let before = doc.tasks.len();
        doc.tasks.retain(|t| t.id != id);
        if doc.tasks.len() == before {
            emit_failed(&bus, "not_found", started.elapsed().as_millis() as u64).await;
            return Err(ToolError::InvalidInput(format!(
                "No scheduled job with id '{id}'"
            )));
        }

        let updated = cron::tasks_file::serialize_tasks(&doc);
        if let Err(e) = cron::tasks_file::write_tasks_body(
            self.ctx.fs.as_ref(),
            &self.ctx.session_cwd.project_root(),
            &updated,
        )
        .await
        {
            emit_failed(&bus, "io_remove", started.elapsed().as_millis() as u64).await;
            return Err(ToolError::Io(format!(
                "CronDelete: io error at {}: {e}",
                path.display()
            )));
        }

        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(started.elapsed().as_millis() as i64),
        );
        bus.log_event(CRON_DELETE_COMPLETED, md).await;

        if let Some(registry) = &self.ctx.task_registry {
            let _ = cron::unregister_live_job(registry, &id, None).await;
        }

        Ok(cancelled_result(&id))
    }
}

fn cancelled_result(id: &str) -> ToolCallResult {
    ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
        data: json!({
            "id": id,
            "content": format!("Cancelled job {id}."),
        }),
        model_content: None,
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx_in};

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    /// Seed `<root>/.claude/scheduled_tasks.json` with the given task ids.
    async fn seed_ids(root: &Path, ids: &[&str]) {
        let path = cron_file_path(root);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let doc = cron::tasks_file::ScheduledTasks {
            tasks: ids
                .iter()
                .map(|id| cron::tasks_file::CronTask {
                    creator: Default::default(),
                    automation: None,
                    id: (*id).into(),
                    cron: "*/5 * * * *".into(),
                    prompt: "echo hi".into(),
                    created_at: 0,
                    last_fired_at: None,
                    recurring: Some(true),
                    permanent: None,
                    expires_at: None,
                    session_id: None,
                })
                .collect(),
            ..Default::default()
        };
        tokio::fs::write(&path, cron::tasks_file::serialize_tasks(&doc))
            .await
            .unwrap();
    }

    async fn read_ids(root: &Path) -> Vec<String> {
        let path = cron_file_path(root);
        let body = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        cron::tasks_file::parse_tasks(&body)
            .tasks
            .into_iter()
            .map(|t| t.id)
            .collect()
    }

    #[test]
    fn constants_locked() {
        assert_eq!(CRON_DELETE_TOOL_NAME, "CronDelete");
    }

    #[tokio::test]
    async fn delete_existing_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        // Two jobs; deleting one leaves the other in the same file.
        seed_ids(tmp.path(), &["d12345678", "dkeep0000"]).await;

        let tool = CronDeleteTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let out = tool
            .call(json!({"id": "d12345678"}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(out.data["id"], json!("d12345678"));
        assert_eq!(out.data["content"], json!("Cancelled job d12345678."));

        // Only the targeted job is gone; the file (and the other job) remains.
        assert_eq!(read_ids(tmp.path()).await, vec!["dkeep0000".to_string()]);
    }

    #[tokio::test]
    async fn delete_missing_errors_with_exact_message() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronDeleteTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let err = tool
            .call(json!({"id": "dnope0000"}), fresh_ctx(), fresh_tx())
            .await
            .expect_err("missing");
        match err {
            ToolError::InvalidInput(msg) => {
                assert_eq!(msg, "No scheduled job with id 'dnope0000'");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn validate_input_rejects_missing_job() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronDeleteTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let err = tool
            .validate_input(&json!({"id": "dmissing1"}), &fresh_ctx())
            .await
            .expect_err("missing");
        assert_eq!(err.0, "No scheduled job with id 'dmissing1'");
    }

    #[tokio::test]
    async fn teammate_cannot_delete_another_owners_durable_job() {
        let tmp = tempfile::tempdir().unwrap();
        seed_ids(tmp.path(), &["abcdef12"]).await;
        let tool = CronDeleteTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let mut call_ctx = fresh_ctx();
        call_ctx.agent_name = Some("researcher".into());

        let error = tool
            .validate_input(&json!({"id": "abcdef12"}), &call_ctx)
            .await
            .expect_err("teammate must not delete the leader's durable job");
        assert_eq!(
            error.0,
            "Cannot delete cron job 'abcdef12': owned by another agent"
        );
        let call_error = tool
            .call(json!({"id": "abcdef12"}), call_ctx, fresh_tx())
            .await
            .expect_err("call must enforce ownership without pre-validation too");
        assert!(format!("{call_error}")
            .contains("Cannot delete cron job 'abcdef12': owned by another agent"));
        assert_eq!(read_ids(tmp.path()).await, vec!["abcdef12"]);
    }
}
