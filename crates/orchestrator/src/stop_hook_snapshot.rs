//! `Stop` / `SubagentStop` hook `background_tasks` + `session_crons` snapshot —
//! the production producers for the two arrays claude-code stamps onto every
//! main-loop `Stop` (and `SubagentStop`) hook payload when a tool-use context is
//! present.
//!
//! claude-code (`$Ee`, the Stop-hook firer):
//! ```text
//! let m = s ? { background_tasks: Lic(s.taskRegistry.all()), session_crons: Mic() } : undefined;
//! …{ …, hook_event_name: "Stop", stop_hook_active, last_assistant_message, ...m }
//! ```
//! When the tool-use context `s` is present, `m` carries both arrays (possibly
//! empty `[]`); when absent, `m` is `undefined` and both keys are omitted. This
//! module ports the two pure builders `Lic` / `Mic` (+ their helpers `O1o` /
//! `wA` / `TUe`) and exposes a [`StopHookSnapshotProvider`] seam the orchestrator
//! consults at its `Stop` / `SubagentStop` firings to populate
//! [`hooks::HookContext::background_tasks`] / `session_crons`.
//!
//! The orchestrator names its data sources only through this narrow seam (no
//! dependency on the `tasks` or `cron` crates) — exactly the pattern
//! [`crate::RegistryTaskNotifications`] uses for terminal task notifications. The
//! concrete provider lives at the composition root (`engine-desktop`), which has
//! the live `TaskRegistry` + cron file and maps them through the pure builders
//! below.

use async_trait::async_trait;
use hooks::{HookBackgroundTask, HookSessionCron};
use lingxi_core::host::task_registry::TaskRecord;

/// claude-code `Ljo` — the per-field truncation cap (characters) applied to a
/// background task's `description` / `command` and a cron's `prompt`.
pub const SNAPSHOT_TRUNCATE_CHARS: usize = 1000;

/// Port of claude-code `TUe(s, Ljo)` — truncate `s` to at most
/// [`SNAPSHOT_TRUNCATE_CHARS`] **characters** (NOT bytes; JS `.slice` operates
/// on UTF-16 code units, but truncating on Rust `char` boundaries is the closest
/// faithful analogue for the prompts/descriptions seen here and never splits a
/// scalar value). A string already within the cap is returned unchanged.
#[must_use]
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => s[..byte_idx].to_string(),
        None => s.to_string(),
    }
}

/// Port of claude-code `O1o[type] ?? type` — map a task-type wire string to the
/// `background_tasks[].type` label claude-code emits, falling back to the raw
/// wire string for any type not in the map.
#[must_use]
fn type_label(task_type: &str) -> String {
    // O1o (byte-exact): {local_agent:"subagent", local_workflow:"workflow",
    // local_bash:"shell", monitor_mcp:"monitor", monitor_ws:"monitor",
    // mcp_task:"MCP task", in_process_teammate:"teammate", dream:"dream",
    // remote_agent:"cloud session"}.
    match task_type {
        "local_agent" => "subagent",
        "local_workflow" => "workflow",
        "local_bash" => "shell",
        "monitor_mcp" | "monitor_ws" => "monitor",
        "mcp_task" => "MCP task",
        "in_process_teammate" => "teammate",
        "dream" => "dream",
        "auto_mode_scan" => "auto-mode scan",
        "remote_agent" => "cloud session",
        "local_fusion" => "fusion",
        other => return other.to_string(),
    }
    .to_string()
}

/// Port of claude-code `wA(e)` — the `background_tasks` keep-filter: keep a task
/// iff its status is `running` or `pending` AND it is not an explicitly
/// non-backgrounded task (claude-code
/// `"isBackgrounded" in e && e.isBackgrounded === false` ⇒ drop). In the port
/// only `local_agent` carries an `is_backgrounded` field (`TaskRecord`'s
/// `is_backgrounded: Some(false)` ⇒ drop); every other task type leaves it
/// `None` and is never dropped by that clause.
#[must_use]
fn keep_task(rec: &TaskRecord) -> bool {
    if rec.status != "running" && rec.status != "pending" {
        return false;
    }
    if rec.is_backgrounded == Some(false) {
        return false;
    }
    true
}

/// Port of claude-code `Lic(taskRegistry.all())` — build the `background_tasks`
/// array. Each kept task (see [`keep_task`]) maps to `{id, type, status,
/// description}` plus the per-type extras claude-code's `switch (n.type)` sets:
/// `local_bash` → `command`; `local_agent` → `agent_type`; `monitor_mcp` /
/// `mcp_task` → `server`, `tool`; `local_workflow` → `name`. `description`,
/// `command` and the cron `prompt` are truncated to
/// [`SNAPSHOT_TRUNCATE_CHARS`].
///
/// The per-type extras are sourced from the already-projected [`TaskRecord`]
/// fields (`command` / `agent_type` / `server` / `tool` / `name`), which the
/// concrete `TaskRegistryHandle` populates from each task variant. Input order
/// is preserved (claude iterates `Object.values(registry)` once).
#[must_use]
pub fn build_background_tasks(records: &[TaskRecord]) -> Vec<HookBackgroundTask> {
    records
        .iter()
        .filter(|r| keep_task(r))
        .map(|r| HookBackgroundTask {
            is_idle: r.is_idle,
            id: r.task_id.clone(),
            r#type: type_label(&r.task_type),
            status: r.status.clone(),
            description: truncate_chars(&r.description, SNAPSHOT_TRUNCATE_CHARS),
            // claude-code truncates the shell command too (`TUe(n.command, Ljo)`).
            command: r
                .command
                .as_deref()
                .map(|c| truncate_chars(c, SNAPSHOT_TRUNCATE_CHARS)),
            agent_type: r.agent_type.clone(),
            server: r.server.clone(),
            tool: r.tool.clone(),
            name: r.name.clone(),
        })
        .collect()
}

/// Neutral cron input for [`build_session_crons`] — the orchestrator-local
/// mirror of the fields claude-code `Mic` reads off a session cron task, so the
/// orchestrator never names the `cron` crate. The composition root maps each
/// on-disk `cron::tasks_file::CronTask` into this shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSnapshotInput {
    /// Cron job id (claude `t.id`).
    pub id: String,
    /// 5-field cron expression (claude `t.cron`).
    pub cron: String,
    /// `recurring ?? false` source (claude `t.recurring`).
    pub recurring: Option<bool>,
    /// Prompt enqueued at each fire (claude `t.prompt`).
    pub prompt: String,
}

/// Port of claude-code `Mic(Cv())` — build the `session_crons` array:
/// `{id, schedule ← cron, recurring ← recurring ?? false, prompt ← TUe(prompt,
/// Ljo)}` for each cron task, in source order.
#[must_use]
pub fn build_session_crons(crons: &[CronSnapshotInput]) -> Vec<HookSessionCron> {
    crons
        .iter()
        .map(|c| HookSessionCron {
            id: c.id.clone(),
            schedule: c.cron.clone(),
            recurring: c.recurring.unwrap_or(false),
            prompt: truncate_chars(&c.prompt, SNAPSHOT_TRUNCATE_CHARS),
        })
        .collect()
}

/// Source of the `Stop` / `SubagentStop` `background_tasks` + `session_crons`
/// snapshot. Injected at the composition root over the live task registry + cron
/// file. The orchestrator consults it ONLY at its `Stop` / `SubagentStop`
/// firings (mirroring claude-code's `s` gate), so other lifecycle hooks
/// (`UserPromptSubmit`, expansion, …) keep both fields `None` and omit the keys.
#[async_trait]
pub trait StopHookSnapshotProvider: Send + Sync {
    /// The current `background_tasks` array (claude `Lic(taskRegistry.all())`).
    /// Returns `Some(vec)` whenever a snapshot is available (possibly empty `[]`,
    /// which claude STILL emits when the tool-use context is present); `None`
    /// only if the source is unavailable.
    async fn background_tasks(&self) -> Vec<HookBackgroundTask>;

    /// Internal goal timing metadata captured with the background task snapshot.
    /// Timestamp values are Unix epoch milliseconds; they never enter hook JSON.
    /// Providers without timing data preserve their existing snapshot behavior.
    async fn background_tasks_with_start_times(
        &self,
    ) -> (
        Vec<HookBackgroundTask>,
        std::collections::HashMap<String, u64>,
    ) {
        (
            self.background_tasks().await,
            std::collections::HashMap::new(),
        )
    }

    /// The current `session_crons` array (claude `Mic()`), in source order.
    async fn session_crons(&self) -> Vec<HookSessionCron>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn rec(task_type: &str, status: &str, description: &str) -> TaskRecord {
        TaskRecord {
            task_id: format!("{task_type}-1"),
            task_type: task_type.into(),
            status: status.into(),
            description: description.into(),
            agent_facts: None,
            ..Default::default()
        }
    }

    #[test]
    fn teammate_idle_metadata_is_preserved_without_changing_hook_wire() {
        let mut teammate = rec("in_process_teammate", "running", "work");
        let active = build_background_tasks(&[teammate.clone()]);
        teammate.is_idle = true;
        let idle = build_background_tasks(&[teammate]);
        assert!(idle[0].is_idle);
        assert_eq!(
            serde_json::to_value(&active).unwrap(),
            serde_json::to_value(&idle).unwrap()
        );
    }

    #[test]
    fn type_label_maps_every_claude_kind_byte_exact() {
        assert_eq!(type_label("local_agent"), "subagent");
        assert_eq!(type_label("local_workflow"), "workflow");
        assert_eq!(type_label("local_bash"), "shell");
        assert_eq!(type_label("monitor_mcp"), "monitor");
        assert_eq!(type_label("monitor_ws"), "monitor");
        assert_eq!(type_label("mcp_task"), "MCP task");
        assert_eq!(type_label("in_process_teammate"), "teammate");
        assert_eq!(type_label("dream"), "dream");
        assert_eq!(type_label("remote_agent"), "cloud session");
        assert_eq!(type_label("local_fusion"), "fusion");
        // O1o[type] ?? type — unknown kind falls back to the raw wire string.
        assert_eq!(type_label("something_new"), "something_new");
    }

    #[test]
    fn wa_filter_drops_non_running_pending_and_backgrounded_false() {
        // running / pending kept.
        assert!(keep_task(&rec("local_bash", "running", "d")));
        assert!(keep_task(&rec("local_bash", "pending", "d")));
        // terminal statuses dropped.
        assert!(!keep_task(&rec("local_bash", "completed", "d")));
        assert!(!keep_task(&rec("local_bash", "failed", "d")));
        assert!(!keep_task(&rec("local_bash", "killed", "d")));
        // local_agent with is_backgrounded == Some(false) dropped even if running.
        let mut backgrounded_false = rec("local_agent", "running", "d");
        backgrounded_false.is_backgrounded = Some(false);
        assert!(!keep_task(&backgrounded_false));
        // is_backgrounded == Some(true) kept.
        let mut backgrounded_true = rec("local_agent", "running", "d");
        backgrounded_true.is_backgrounded = Some(true);
        assert!(keep_task(&backgrounded_true));
    }

    #[test]
    fn lic_per_type_extras_and_label() {
        let bash = TaskRecord {
            task_id: "b1".into(),
            task_type: "local_bash".into(),
            status: "running".into(),
            description: "build it".into(),
            command: Some("cargo build".into()),
            agent_facts: None,
            ..Default::default()
        };
        let agent = TaskRecord {
            task_id: "a1".into(),
            task_type: "local_agent".into(),
            status: "running".into(),
            description: "explore".into(),
            agent_type: Some("general-purpose".into()),
            forked_skill_name: None,
            is_backgrounded: Some(true),
            agent_facts: None,
            ..Default::default()
        };
        let monitor = TaskRecord {
            task_id: "m1".into(),
            task_type: "monitor_mcp".into(),
            status: "pending".into(),
            description: "watch".into(),
            server: Some("srv".into()),
            agent_facts: None,
            ..Default::default()
        };
        let workflow = TaskRecord {
            task_id: "w1".into(),
            task_type: "local_workflow".into(),
            status: "running".into(),
            description: "flow".into(),
            name: Some("my-wf".into()),
            agent_facts: None,
            ..Default::default()
        };
        let out = build_background_tasks(&[bash, agent, monitor, workflow]);
        // Serialize each to JSON to lock the key order + per-type extras (Lic).
        assert_eq!(
            serde_json::to_string(&out[0]).unwrap(),
            r#"{"id":"b1","type":"shell","status":"running","description":"build it","command":"cargo build"}"#
        );
        assert_eq!(
            serde_json::to_string(&out[1]).unwrap(),
            r#"{"id":"a1","type":"subagent","status":"running","description":"explore","agent_type":"general-purpose"}"#
        );
        assert_eq!(
            serde_json::to_string(&out[2]).unwrap(),
            r#"{"id":"m1","type":"monitor","status":"pending","description":"watch","server":"srv"}"#
        );
        assert_eq!(
            serde_json::to_string(&out[3]).unwrap(),
            r#"{"id":"w1","type":"workflow","status":"running","description":"flow","name":"my-wf"}"#
        );
    }

    #[test]
    fn lic_truncates_description_and_command_to_1000_chars() {
        let long = "x".repeat(1500);
        let r = TaskRecord {
            task_id: "b1".into(),
            task_type: "local_bash".into(),
            status: "running".into(),
            description: long.clone(),
            command: Some(long.clone()),
            agent_facts: None,
            ..Default::default()
        };
        let out = build_background_tasks(&[r]);
        assert_eq!(out[0].description.chars().count(), 1000);
        assert_eq!(out[0].command.as_deref().unwrap().chars().count(), 1000);
    }

    #[test]
    fn mic_maps_crons_with_recurring_default_and_truncation() {
        let long = "p".repeat(1200);
        let crons = vec![
            CronSnapshotInput {
                id: "c1".into(),
                cron: "0 9 * * *".into(),
                recurring: Some(true),
                prompt: "morning".into(),
            },
            CronSnapshotInput {
                id: "c2".into(),
                cron: "* * * * *".into(),
                recurring: None,
                prompt: long,
            },
        ];
        let out = build_session_crons(&crons);
        assert_eq!(
            serde_json::to_string(&out[0]).unwrap(),
            r#"{"id":"c1","schedule":"0 9 * * *","recurring":true,"prompt":"morning"}"#
        );
        // recurring None → false; prompt truncated to 1000 chars.
        assert!(!out[1].recurring);
        assert_eq!(out[1].prompt.chars().count(), 1000);
        assert_eq!(out[1].schedule, "* * * * *");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        // multi-byte chars must not be split mid-scalar.
        let s = "é".repeat(1500);
        let t = truncate_chars(&s, 1000);
        assert_eq!(t.chars().count(), 1000);
        assert!(t.is_char_boundary(t.len()));
    }
}
