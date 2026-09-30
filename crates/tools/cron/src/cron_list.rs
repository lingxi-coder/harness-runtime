//! `CronListTool` — list durable jobs from the project tasks file plus
//! session-only jobs owned by the live scheduler.
//!
//! 1:1 parity port of claude-code `CronListTool.ts`. Returns a `jobs` array of
//! `{id, cron, humanSchedule, prompt, recurring?, durable?}` plus a flattened
//! result text for the model, one line per job.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Map, Value};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{CRON_LIST_COMPLETED, CRON_LIST_STARTED};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};

use crate::schedule_cron::cron_to_human;

/// Tool name byte-lock.
pub const CRON_LIST_TOOL_NAME: &str = "CronList";

/// Model-facing description (CronListTool.ts description()).
const CRON_LIST_DESCRIPTION: &str = "List scheduled cron jobs";

/// Empty `strictObject({})` schema — CronList takes no input.
static SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "properties": {},
        "required": []
    })
});

/// Truncate `s` to `max_width` columns, appending `…` if it was longer.
///
/// Port of claude-code `utils/truncate.ts` `truncateToWidth`.
///
fn truncate_to_width(s: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return "\u{2026}".to_string();
    }
    let available = max_width - 1;
    let mut used = 0;
    let mut result = String::new();
    for grapheme in s.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme);
        if used + width > available {
            break;
        }
        result.push_str(grapheme);
        used += width;
    }
    result.push('\u{2026}');
    result
}

/// Port of claude-code `utils/truncate.ts` `truncate(str, maxWidth, true)`
/// (single-line mode): cut at the first newline, else width-truncate.
fn truncate_single_line(s: &str, max_width: usize) -> String {
    if let Some(idx) = s.find('\n') {
        let head = &s[..idx];
        if UnicodeWidthStr::width(head) + 1 > max_width {
            return truncate_to_width(head, max_width);
        }
        return format!("{head}\u{2026}");
    }
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    truncate_to_width(s, max_width)
}

/// Read the single `<root>/.claude/scheduled_tasks.json` file into the `jobs`
/// shape, in FILE order (the oracle `bFe` does not sort). A missing /
/// unparseable file yields no jobs. Every persisted task is durable by
/// definition, so the `durable:false`
/// key (CronListTool.ts' `durable === false` spread, which only applies to the
/// separate in-memory session tasks) is never emitted here.
async fn read_durable_jobs(
    fs: &dyn lingxi_core::host::FileSystem,
    project_root: &Path,
) -> Vec<Value> {
    let body = match cron::tasks_file::read_tasks_body(fs, project_root).await {
        Ok(b) => b,
        Err(_) => return Vec::new(), // file absent → no jobs
    };
    let doc = cron::tasks_file::parse_tasks(&body);

    let jobs: Vec<Value> = doc
        .tasks
        .into_iter()
        .map(|t| {
            let mut obj = Map::new();
            obj.insert("id".into(), json!(t.id));
            obj.insert("cron".into(), json!(t.cron));
            obj.insert("humanSchedule".into(), json!(cron_to_human(&t.cron)));
            obj.insert("prompt".into(), json!(t.prompt));
            // Conditional spread (CronListTool.ts): `recurring` only when truthy.
            if t.recurring == Some(true) {
                obj.insert("recurring".into(), json!(true));
            }
            Value::Object(obj)
        })
        .collect();
    // Oracle `bFe` preserves the tasks file's order — NO id sort (the model-
    // facing `jobs` array and rendered lines follow file order).
    jobs
}

fn teammate_owner(ctx: &ToolUseContext) -> Option<String> {
    ctx.agent_name.as_ref().map(|name| {
        ctx.agent_id
            .map_or_else(|| name.clone(), |agent_id| agent_id.to_string())
    })
}

async fn read_all_jobs(
    tool_ctx: &tool_api::BuiltinToolContext,
    call_ctx: &ToolUseContext,
) -> Vec<Value> {
    let owner = teammate_owner(call_ctx);
    let mut jobs = if owner.is_none() {
        read_durable_jobs(tool_ctx.fs.as_ref(), &tool_ctx.session_cwd.project_root()).await
    } else {
        Vec::new()
    };
    if let Some(registry) = &tool_ctx.task_registry {
        if let Ok(session) = cron::session_jobs(registry).await {
            // Oracle `bFe` = `[...durableFromFile, ...sessionStore]` unsorted:
            // session jobs follow the durable file-order block, in session
            // CREATION order (the in-memory store is HashMap-backed here, so
            // sort by created_at — ties broken by id — to recover that order).
            let mut session: Vec<_> = session
                .into_iter()
                .filter(|task| {
                    owner
                        .as_deref()
                        .is_none_or(|owner| task.owner.as_deref() == Some(owner))
                })
                .collect();
            session.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            jobs.extend(session.into_iter().map(|task| {
                let mut obj = Map::new();
                obj.insert("id".into(), json!(task.id));
                obj.insert("cron".into(), json!(task.cron));
                obj.insert("humanSchedule".into(), json!(cron_to_human(&task.cron)));
                obj.insert("prompt".into(), json!(task.prompt));
                if task.recurring {
                    obj.insert("recurring".into(), json!(true));
                }
                obj.insert("durable".into(), json!(false));
                Value::Object(obj)
            }));
        }
    }
    // NO final merge sort — durable file-order followed by session creation-order.
    jobs
}

/// Flatten the `jobs` array to the model-facing result text
/// (CronListTool.ts `mapToolResultToToolResultBlockParam`).
fn render_result(jobs: &[Value]) -> String {
    if jobs.is_empty() {
        return "No scheduled jobs.".to_string();
    }
    jobs.iter()
        .map(|j| {
            let id = j.get("id").and_then(Value::as_str).unwrap_or("");
            let human = j.get("humanSchedule").and_then(Value::as_str).unwrap_or("");
            let prompt = j.get("prompt").and_then(Value::as_str).unwrap_or("");
            let recurring_label = if j.get("recurring").and_then(Value::as_bool) == Some(true) {
                " (recurring)"
            } else {
                " (one-shot)"
            };
            let durable_label = if j.get("durable").and_then(Value::as_bool) == Some(false) {
                " [session-only]"
            } else {
                ""
            };
            // U+2014 EM DASH separator, matching the TS template literal " — ".
            format!(
                "{id} \u{2014} {human}{recurring_label}{durable_label}: {}",
                truncate_single_line(prompt, 80)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `CronListTool` — list scheduled cron jobs.
pub struct CronListTool {
    pub(crate) ctx: tool_api::BuiltinToolContext,
}

impl CronListTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for CronListTool {
    fn name(&self) -> &str {
        CRON_LIST_TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("list active cron jobs")
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
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_destructive(&self, _: &Value) -> bool {
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
                reason: "CronList reads .claude/scheduled_tasks.json".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        CRON_LIST_DESCRIPTION.into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        if !crate::schedule_cron::durable_enabled() {
            return "List all cron jobs scheduled via CronCreate in this session.".into();
        }
        // PARITY 2.1.263 `hbn(true)`.
        "List all cron jobs scheduled via CronCreate, both durable (.claude/scheduled_tasks.json) and session-only.".into()
    }

    async fn validate_input(
        &self,
        _input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }

    async fn call(
        &self,
        _input: Value,
        call_ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let bus = self.ctx.bus.clone();

        bus.log_event(CRON_LIST_STARTED, HashMap::new()).await;

        let jobs = read_all_jobs(&self.ctx, &call_ctx).await;
        let content = render_result(&jobs);

        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(started.elapsed().as_millis() as i64),
        );
        md.insert("job_count".into(), AnalyticsValue::Int(jobs.len() as i64));
        bus.log_event(CRON_LIST_COMPLETED, md).await;

        Ok(ToolCallResult {
            data: json!({
                "jobs": jobs,
                "content": content,
            }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
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

    /// Seed the single `<root>/.claude/scheduled_tasks.json` file with the given
    /// tasks (everything on disk is durable by definition — there is no on-disk
    /// `durable` field).
    async fn seed_tasks(root: &Path, tasks: Vec<cron::tasks_file::CronTask>) {
        let path = cron::tasks_file::session_scheduled_tasks_path(root);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let doc = cron::tasks_file::ScheduledTasks {
            tasks,
            ..Default::default()
        };
        tokio::fs::write(&path, cron::tasks_file::serialize_tasks(&doc))
            .await
            .unwrap();
    }

    /// Build one recurring `CronTask` for seeding.
    fn task(id: &str, cron: &str, prompt: &str, recurring: bool) -> cron::tasks_file::CronTask {
        cron::tasks_file::CronTask {
            creator: Default::default(),
            automation: None,
            id: id.into(),
            cron: cron.into(),
            prompt: prompt.into(),
            created_at: 0,
            last_fired_at: None,
            recurring: Some(recurring),
            permanent: None,
            expires_at: None,
            session_id: None,
        }
    }

    #[test]
    fn constants_locked() {
        assert_eq!(CRON_LIST_TOOL_NAME, "CronList");
    }

    #[test]
    fn truncate_single_line_behaviour() {
        // Short prompt is returned verbatim.
        assert_eq!(truncate_single_line("echo hi", 80), "echo hi");
        // Over 80 chars gets cut to 79 chars + ellipsis (= 80 total).
        let long = "a".repeat(100);
        let out = truncate_single_line(&long, 80);
        assert_eq!(out.chars().count(), 80);
        assert!(out.ends_with('\u{2026}'));
        assert_eq!(out.chars().filter(|c| *c == 'a').count(), 79);
        // Newline forces single-line truncation with a trailing ellipsis.
        assert_eq!(
            truncate_single_line("line one\nline two", 80),
            "line one\u{2026}"
        );
        // CJK occupies two terminal cells per glyph: 39 glyphs (78 cells) plus
        // the one-cell ellipsis fit in the 80-column result.
        let wide = "你".repeat(50);
        let out = truncate_single_line(&wide, 80);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 79);
        assert_eq!(out.chars().filter(|ch| *ch == '你').count(), 39);
        assert!(out.ends_with('\u{2026}'));

        // Advance by grapheme cluster, matching Intl.Segmenter: never leave a
        // dangling ZWJ/modifier when a complex emoji hits the width boundary.
        let family = "👨\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
        let clustered = format!("{}suffix", family.repeat(50));
        let out = truncate_single_line(&clustered, 8);
        assert!(out.ends_with('\u{2026}'));
        assert!(out
            .trim_end_matches('\u{2026}')
            .graphemes(true)
            .all(|grapheme| grapheme == family));
    }

    #[tokio::test]
    async fn empty_lists_no_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronListTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let out = tool
            .call(json!({}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(out.data["jobs"], json!([]));
        assert_eq!(out.data["content"], json!("No scheduled jobs."));
    }

    #[tokio::test]
    async fn lists_created_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        // The oracle preserves FILE order — no id sort — so these come back in
        // the order they appear on disk. Both are durable (on disk).
        seed_tasks(
            tmp.path(),
            vec![
                task("dbbbb1111", "0 9 * * *", "morning", true),
                task("daaaa0000", "*/5 * * * *", "ticker", true),
            ],
        )
        .await;

        let tool = CronListTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let out = tool
            .call(json!({}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        let jobs = out.data["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 2);
        // File order (NOT id-sorted): dbbbb1111 was written first.
        assert_eq!(jobs[0]["id"], json!("dbbbb1111"));
        assert_eq!(jobs[1]["id"], json!("daaaa0000"));
        // humanSchedule rendered.
        assert_eq!(jobs[0]["humanSchedule"], json!("Every day at 9:00 AM"));
        assert_eq!(jobs[1]["humanSchedule"], json!("Every 5 minutes"));
        // recurring present (true) on both. Every persisted task is durable, so
        // the `durable` key is never emitted.
        assert_eq!(jobs[0]["recurring"], json!(true));
        assert!(jobs[0].get("durable").is_none());
        assert_eq!(jobs[1]["recurring"], json!(true));
        assert!(jobs[1].get("durable").is_none());
    }

    #[tokio::test]
    async fn teammate_list_hides_jobs_it_does_not_own() {
        let tmp = tempfile::tempdir().unwrap();
        seed_tasks(
            tmp.path(),
            vec![task("abcdef12", "0 9 * * *", "leader job", true)],
        )
        .await;
        let tool = CronListTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let mut call_ctx = fresh_ctx();
        call_ctx.agent_name = Some("researcher".into());

        let out = tool
            .call(json!({}), call_ctx, fresh_tx())
            .await
            .expect("list");
        assert_eq!(out.data["jobs"], json!([]));
        assert_eq!(out.data["content"], json!("No scheduled jobs."));
    }

    #[tokio::test]
    async fn result_text_format() {
        let tmp = tempfile::tempdir().unwrap();
        // Recurring + one-shot, both durable (no [session-only] label on disk jobs).
        seed_tasks(
            tmp.path(),
            vec![
                task("daaaa0000", "*/5 * * * *", "ticker", true),
                task("dbbbb1111", "30 14 28 2 *", "remind me", false),
            ],
        )
        .await;

        let tool = CronListTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let out = tool
            .call(json!({}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        let content = out.data["content"].as_str().unwrap();
        let lines: Vec<&str> = content.split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "daaaa0000 \u{2014} Every 5 minutes (recurring): ticker"
        );
        assert_eq!(
            lines[1],
            "dbbbb1111 \u{2014} 30 14 28 2 * (one-shot): remind me"
        );
    }

    #[tokio::test]
    async fn result_text_truncates_long_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let long = "x".repeat(120);
        seed_tasks(
            tmp.path(),
            vec![task("daaaa0000", "*/5 * * * *", &long, true)],
        )
        .await;

        let tool = CronListTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let out = tool
            .call(json!({}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        let content = out.data["content"].as_str().unwrap();
        // 79 'x' + ellipsis.
        assert!(content.ends_with(&format!("{}\u{2026}", "x".repeat(79))));
        assert!(!content.contains(&"x".repeat(80)));
    }
}
