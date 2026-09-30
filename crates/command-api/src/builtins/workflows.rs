//! `/workflows` — browse running, paused, and completed dynamic workflows.
//!
//! The TUI intercepts this command and opens its interactive workflow picker.
//! This handler is the shared registry/headless projection used by mobile and
//! bridge clients: it reads the same [`lingxi_core::host::task_registry::TaskRegistryHandle`]
//! and renders the picker's snapshot as text.

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::task_registry::{TaskRegistryHandle, WorkflowRecord};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const DESCRIPTION: &str = "Browse running, paused, and completed workflows";
const TITLE: &str = "Dynamic workflows";
const EMPTY: &str = "No dynamic workflows in this session.";

/// Shared/headless `/workflows` handler.
pub struct WorkflowsHandler {
    tasks: Option<Arc<dyn TaskRegistryHandle>>,
}

impl WorkflowsHandler {
    /// Construct the fallback used before a host binds its task registry.
    #[must_use]
    pub fn new() -> Self {
        Self { tasks: None }
    }

    /// Construct a handler over the host's live task registry.
    #[must_use]
    pub fn with_registry(tasks: Arc<dyn TaskRegistryHandle>) -> Self {
        Self { tasks: Some(tasks) }
    }
}

impl Default for WorkflowsHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BuiltinCommandHandler for WorkflowsHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        let Some(tasks) = &self.tasks else {
            return CommandResult::Done {
                display: Some("/workflows is unavailable (no engine handle wired)".to_string()),
            };
        };

        // Match the TUI's snapshot semantics: a list failure falls back to the
        // empty state, and an individual spool failure only omits agent count.
        let mut records = tasks.list_workflows().await.unwrap_or_default();
        sort_newest_first(&mut records);
        let mut agent_counts = Vec::with_capacity(records.len());
        for record in &records {
            let count = tasks
                .output(&record.task_id, None)
                .await
                .map_or(0, |chunk| distinct_agent_count(&chunk.content));
            agent_counts.push(count);
        }

        CommandResult::Done {
            display: Some(render_workflows(
                &records,
                &agent_counts,
                now_epoch_millis(),
            )),
        }
    }

    fn name(&self) -> &str {
        "workflows"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }
}

fn now_epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn sort_newest_first(records: &mut [WorkflowRecord]) {
    records.sort_by(|a, b| {
        b.started_at_ms
            .cmp(&a.started_at_ms)
            .then_with(|| a.task_id.cmp(&b.task_id))
    });
}

fn distinct_agent_count(spool: &str) -> usize {
    spool
        .lines()
        .filter_map(|line| line.strip_prefix("[workflow_agent] "))
        .filter_map(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .map(|event| {
            event
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        })
        .collect::<HashSet<_>>()
        .len()
}

fn render_workflows(records: &[WorkflowRecord], agent_counts: &[usize], now_ms: u64) -> String {
    if records.is_empty() {
        return format!("{TITLE}\n\n{EMPTY}");
    }

    let running = records
        .iter()
        .filter(|record| record.status == "running")
        .count();
    let paused = records
        .iter()
        .filter(|record| record.status == "paused")
        .count();
    let completed = records.len() - running - paused;
    let mut summary = Vec::with_capacity(3);
    if running > 0 {
        summary.push(format!("{running} running"));
    }
    if completed > 0 {
        summary.push(format!("{completed} completed"));
    }
    if paused > 0 {
        summary.push(format!("{paused} paused"));
    }

    let mut lines = vec![TITLE.to_string(), summary.join(" · "), String::new()];
    for (index, record) in records.iter().enumerate() {
        let glyph = match record.status.as_str() {
            "completed" => "✔",
            "failed" | "killed" => "✘",
            "paused" => "Ⅱ",
            _ => "⟳",
        };
        let mut meta = Vec::with_capacity(2);
        let agents = agent_counts.get(index).copied().unwrap_or(0);
        if agents > 0 {
            let noun = if agents == 1 { "agent" } else { "agents" };
            meta.push(format!("{agents} {noun}"));
        }
        if let Some(elapsed) = elapsed(record, now_ms) {
            meta.push(elapsed);
        }

        let suffix = if meta.is_empty() {
            String::new()
        } else {
            format!("  {}", meta.join(" · "))
        };
        lines.push(format!("{glyph} {}{suffix}", display_name(record)));
        if record.status == "paused" {
            if let (Some(script_path), Some(run_id)) =
                (record.script_path.as_deref(), record.run_id.as_deref())
            {
                let script_path = script_path.replace('\\', "\\\\").replace('\'', "\\'");
                let args = record
                    .args
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .map(|value| format!(", args: {value}"))
                    .unwrap_or_default();
                lines.push(format!(
                    "  Resume: Workflow({{scriptPath: '{script_path}', resumeFromRunId: '{run_id}'{args}}})"
                ));
            }
        }
    }
    lines.join("\n")
}

fn display_name(record: &WorkflowRecord) -> String {
    let raw = if !record.name.is_empty() {
        record.name.as_str()
    } else if !record.description.is_empty() {
        record.description.as_str()
    } else {
        "Dynamic workflow"
    };
    if raw.chars().count() > 50 {
        format!("{}…", raw.chars().take(49).collect::<String>())
    } else {
        raw.to_string()
    }
}

fn elapsed(record: &WorkflowRecord, now_ms: u64) -> Option<String> {
    let started = record.started_at_ms?;
    let ended = record.ended_at_ms.unwrap_or(now_ms);
    let seconds = ended.saturating_sub(started) / 1_000;
    Some(if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        task_id: &str,
        name: &str,
        status: &str,
        started_at_ms: u64,
        ended_at_ms: Option<u64>,
    ) -> WorkflowRecord {
        WorkflowRecord {
            task_id: task_id.to_string(),
            name: name.to_string(),
            status: status.to_string(),
            started_at_ms: Some(started_at_ms),
            ended_at_ms,
            ..WorkflowRecord::default()
        }
    }

    #[test]
    fn empty_snapshot_matches_tui_copy() {
        assert_eq!(
            render_workflows(&[], &[], 0),
            "Dynamic workflows\n\nNo dynamic workflows in this session."
        );
    }

    #[test]
    fn snapshot_is_newest_first_with_tui_summary_and_row_meta() {
        let mut records = vec![
            record("w-old", "Audit", "completed", 1_000, Some(84_000)),
            record("w-new", "Build", "running", 10_000, None),
        ];
        sort_newest_first(&mut records);
        let rendered = render_workflows(&records, &[2, 1], 55_000);
        assert_eq!(
            rendered,
            "Dynamic workflows\n1 running · 1 completed\n\n⟳ Build  2 agents · 45s\n✔ Audit  1 agent · 1m 23s"
        );
    }

    #[test]
    fn paused_snapshot_exposes_the_explicit_resume_call() {
        let mut paused = record("w-pause", "Build", "paused", 10_000, None);
        paused.run_id = Some("wf_abcdef".into());
        paused.script_path = Some("/workspace/build.js".into());
        paused.args = Some(r#"{"app_id":"demo"}"#.into());

        assert_eq!(
            render_workflows(&[paused], &[1], 55_000),
            concat!(
                "Dynamic workflows\n1 paused\n\n",
                "Ⅱ Build  1 agent · 45s\n",
                "  Resume: Workflow({scriptPath: '/workspace/build.js', ",
                "resumeFromRunId: 'wf_abcdef', args: {\"app_id\":\"demo\"}})"
            )
        );
    }

    #[test]
    fn agent_count_collapses_lifecycle_events_by_workflow_index() {
        let spool = concat!(
            "[workflow_agent] {\"index\":0,\"state\":\"start\"}\n",
            "[workflow_agent] {\"index\":0,\"state\":\"done\"}\n",
            "malformed\n",
            "[workflow_agent] {\"index\":1,\"state\":\"start\"}\n"
        );
        assert_eq!(distinct_agent_count(spool), 2);
    }

    #[tokio::test]
    async fn unwired_handler_returns_the_tui_unavailable_message() {
        let result = WorkflowsHandler::new()
            .handle(&ParsedSlashCommand {
                name: "workflows".to_string(),
                raw_args: String::new(),
                positional_args: Vec::new(),
            })
            .await;
        match result {
            CommandResult::Done { display } => assert_eq!(
                display.as_deref(),
                Some("/workflows is unavailable (no engine handle wired)")
            ),
            other => panic!("unexpected result: {other:?}"),
        }
    }
}
