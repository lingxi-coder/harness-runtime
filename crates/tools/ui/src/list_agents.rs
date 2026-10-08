//! `ListAgents` (alias `ListPeers`) — 2.1.232 `zy` / `SDd`.
//!
//! Lists the agents this process can `SendMessage` to. Cloud / Remote Control
//! rows are omitted (carve-out); the surviving clauses are the oracle's
//! (2.1.238 `YmS` @286282922, identical opening in 2.1.220).

use async_trait::async_trait;
use lingxi_core::host::task_registry::TaskRecord;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};
use tool_api::BuiltinToolContext;

/// Model-facing name (2.1.232 `zy`).
pub const LIST_AGENTS_TOOL_NAME: &str = "ListAgents";
/// Legacy alias (2.1.232 `SDd`).
pub const LIST_PEERS_TOOL_NAME: &str = "ListPeers";

const DESCRIPTION: &str = concat!(
    "Lists agents you can SendMessage to — in-process subagents you spawned, ",
    "other local Claude sessions on this machine. ",
    "Names are the address: send with `SendMessage({to: \"<name>\", message: \"...\"})`, ",
    "copying the name exactly as a row prints it. Append a row's ` [ref]` only when the ",
    "bare name is not enough — two rows share it, or an error asks you to disambiguate."
);

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "channel": {
                "type": "string",
                "description": "Not available in this build; leave unset."
            },
            "q": {
                "type": "string",
                "maxLength": 256,
                "description": "Not available in this build; leave unset."
            }
        }
    })
});

static OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "listing": { "type": "string", "description": "Formatted list of reachable agents" }
        },
        "required": ["listing"]
    })
});

/// `ListAgents` tool.
pub struct ListAgentsTool {
    ctx: BuiltinToolContext,
}

impl ListAgentsTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }

    async fn format_listing(&self, ctx: &ToolUseContext) -> String {
        let mut sections = Vec::new();

        if let Some(note) = self.self_note(ctx).await {
            sections.push(note);
        }

        let in_process = self.in_process_rows().await;
        if !in_process.is_empty() {
            sections.push(format!(
                "In-process agents ({}):\n{}",
                in_process.len(),
                in_process.join("\n")
            ));
        }

        let peer_rows = self.peer_rows();
        if !peer_rows.is_empty() {
            sections.push(format!(
                "Peer sessions ({}):\n{}",
                peer_rows.len(),
                peer_rows.join("\n")
            ));
        }

        if sections.is_empty() {
            "No reachable agents.".into()
        } else {
            sections.join("\n\n")
        }
    }

    async fn self_note(&self, ctx: &ToolUseContext) -> Option<String> {
        let name =
            lingxi_core::host::live_sessions::process_name().or_else(|| ctx.agent_name.clone())?;
        let self_id = lingxi_core::host::live_sessions::process_session_id().unwrap_or_default();
        let rref = session_ref(&self_id, 0);
        Some(format!(
            "This session is {name} [{rref}] — the name other sessions use to message it (it is not listed below; a message to it would be a message to yourself)."
        ))
    }

    async fn in_process_rows(&self) -> Vec<String> {
        let mut rows = Vec::new();
        let mut seen = std::collections::HashSet::new();

        if let Some(router) = &self.ctx.mailbox_router {
            let mut entries = router.named_recipients().await;
            entries.sort_by(|l, r| l.0.cmp(&r.0));
            for (name, agent_id) in entries {
                if !seen.insert(name.clone()) {
                    continue;
                }
                let status = if let Some(task_registry) = &self.ctx.task_registry {
                    match task_registry.get(&agent_id.to_string()).await {
                        Ok(Some(task)) => task_status(&task),
                        _ => "busy",
                    }
                } else {
                    "busy"
                };
                rows.push(format!("  {name}  ·  teammate  ·  {status}"));
            }
        }

        if let Some(registry) = &self.ctx.agent_name_registry {
            let mut entries = registry.list().await;
            entries.sort_by(|l, r| l.0.cmp(&r.0));
            for (name, agent_id) in entries {
                if !seen.insert(name.clone()) {
                    continue;
                }
                let status = if let Some(task_registry) = &self.ctx.task_registry {
                    match task_registry.get(&agent_id.to_string()).await {
                        Ok(Some(task)) => task_status(&task),
                        _ => "busy",
                    }
                } else {
                    "busy"
                };
                rows.push(format!("  {name}  ·  local_agent  ·  {status}"));
            }
        }
        rows
    }

    fn peer_rows(&self) -> Vec<String> {
        let Some(dir) = lingxi_core::host::live_sessions::process_dir() else {
            return Vec::new();
        };
        let self_id = lingxi_core::host::live_sessions::process_session_id();
        let Ok(live) = dir.list_live() else {
            return Vec::new();
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut rows = Vec::new();
        for rec in live {
            if self_id.as_deref().is_some_and(|id| rec.sid() == id) {
                continue;
            }
            let name = rec.display_name();
            let rref = session_ref(rec.sid(), rec.pid);
            let kind = rec.kind.as_deref().unwrap_or("session");
            let started = rec
                .started_at
                .map(|t| format!("started {} ago", rel_ms(now.saturating_sub(t))));
            let mut bits = vec![
                format!("{name} [{rref}]"),
                kind.to_string(),
                status_bits(rec.normalized_status(), rec.waiting_for.as_deref()),
            ];
            if let Some(started) = started {
                bits.push(started);
            }
            rows.push(format!("  {}", bits.join("  ·  ")));
        }
        rows
    }
}

fn session_ref(session_id: &str, pid: u32) -> String {
    let hex: String = session_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(8)
        .collect();
    if hex.len() >= 6 {
        hex.to_ascii_lowercase()
    } else {
        format!("{pid:08x}")
    }
}

fn rel_ms(ms: i64) -> String {
    if ms < 60_000 {
        format!("{}s", (ms / 1000).max(0))
    } else if ms < 3_600_000 {
        format!("{}m", ms / 60_000)
    } else {
        format!("{}h", ms / 3_600_000)
    }
}

fn task_status(task: &TaskRecord) -> &'static str {
    match task.status.as_str() {
        "completed" | "failed" | "killed" => "idle",
        "waiting" | "blocked" => "waiting",
        _ => "busy",
    }
}

fn status_bits(status: &str, waiting_for: Option<&str>) -> String {
    if status == "waiting" {
        if let Some(waiting_for) = waiting_for.filter(|s| !s.trim().is_empty()) {
            return format!("waiting  ·  waiting for {waiting_for}");
        }
    }
    status.to_string()
}

#[async_trait]
impl Tool for ListAgentsTool {
    fn name(&self) -> &str {
        LIST_AGENTS_TOOL_NAME
    }

    fn aliases(&self) -> &[&str] {
        &[LIST_PEERS_TOOL_NAME]
    }

    fn search_hint(&self) -> Option<&str> {
        Some("list agents you can SendMessage to")
    }

    fn user_facing_name(&self) -> Option<&str> {
        Some("ListAgents")
    }

    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&OUTPUT_SCHEMA)
    }

    fn max_result_size_chars(&self) -> usize {
        100_000
    }

    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        lingxi_core::host::live_sessions::cross_session_messaging_enabled()
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "ListAgents is read-only".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        DESCRIPTION.into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        DESCRIPTION.into()
    }

    async fn call(
        &self,
        _input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let listing = self.format_listing(&ctx).await;
        Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: json!({ "listing": listing }),
            model_content: Some(listing),
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
    use async_trait::async_trait;
    use lingxi_core::host::agent_name_registry::{AgentNameRegistry, InMemoryAgentNameRegistry};
    use lingxi_core::host::task_registry::{
        TaskCreateInput, TaskListFilter, TaskOutputChunk, TaskRegistryError, TaskRegistryHandle,
        TaskUpdatePatch,
    };
    use mobile_linux_api::ProcessOutput;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    // Delegates to the crate-level lock: these globals are per-process, so a
    // file-local mutex would only serialize this file against itself.
    fn process_lock() -> &'static Mutex<()> {
        crate::process_globals_lock()
    }

    #[derive(Default)]
    struct StubTaskRegistry {
        tasks: Mutex<HashMap<String, TaskRecord>>,
    }

    #[async_trait]
    impl TaskRegistryHandle for StubTaskRegistry {
        async fn create(&self, _input: TaskCreateInput) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!("not used")
        }

        async fn get(&self, id: &str) -> Result<Option<TaskRecord>, TaskRegistryError> {
            Ok(self.tasks.lock().unwrap().get(id).cloned())
        }

        async fn list(
            &self,
            _filter: TaskListFilter,
        ) -> Result<Vec<TaskRecord>, TaskRegistryError> {
            Ok(self.tasks.lock().unwrap().values().cloned().collect())
        }

        async fn update(
            &self,
            _id: &str,
            _patch: TaskUpdatePatch,
        ) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!("not used")
        }

        async fn set_status(
            &self,
            _id: &str,
            _status: &str,
        ) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!("not used")
        }

        async fn kill(&self, _id: &str) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!("not used")
        }

        async fn output(
            &self,
            _id: &str,
            _offset: Option<u64>,
        ) -> Result<TaskOutputChunk, TaskRegistryError> {
            unreachable!("not used")
        }
    }

    #[test]
    fn names_are_byte_exact() {
        assert_eq!(LIST_AGENTS_TOOL_NAME, "ListAgents");
        assert_eq!(LIST_PEERS_TOOL_NAME, "ListPeers");
    }

    /// 2.1.238 `YmS` (@286282922, identical opening clause in 2.1.220): the
    /// description LEADS with the in-process-subagents clause. Trimming the
    /// cloud / Remote-Control clauses is the accepted LingXi carve-out;
    /// INVERTING the in-process clause (the port used to say those agents are
    /// "addressed by the name they were spawned with, not this list") is not.
    #[test]
    fn description_keeps_the_oracle_in_process_clause() {
        assert!(
            DESCRIPTION.starts_with(
                "Lists agents you can SendMessage to \u{2014} in-process subagents you spawned, other local Claude sessions on this machine. "
            ),
            "opening clause diverged; got: {}",
            DESCRIPTION
        );
        assert!(
            !DESCRIPTION.contains("not this list"),
            "the inverted in-process clause must not come back"
        );
        assert!(DESCRIPTION.contains(
            "Names are the address: send with `SendMessage({to: \"<name>\", message: \"...\"})`, copying the name exactly as a row prints it."
        ));
        assert!(DESCRIPTION.ends_with(
            "Append a row's ` [ref]` only when the bare name is not enough \u{2014} two rows share it, or an error asks you to disambiguate."
        ));
    }

    #[tokio::test]
    async fn empty_listing_without_any_sources() {
        let _g = process_lock().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::TempDir::new().unwrap();
        lingxi_core::host::live_sessions::set_process_dir(
            lingxi_core::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions")),
        );
        lingxi_core::host::live_sessions::set_process_session_id("self-session");
        lingxi_core::host::live_sessions::set_process_name("lead");

        let tool = ListAgentsTool::new(shell_test_ctx(dummy_out()));
        let result = tool
            .call(json!({}), fresh_ctx(), fresh_tx())
            .await
            .expect("list succeeds");
        assert_eq!(
            result.model_content.as_deref(),
            Some("This session is lead [00000000] — the name other sessions use to message it (it is not listed below; a message to it would be a message to yourself).")
        );
    }

    #[tokio::test]
    async fn listing_includes_self_in_process_and_peer_statuses() {
        let _g = process_lock().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::TempDir::new().unwrap();
        let dir =
            lingxi_core::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions"));
        lingxi_core::host::live_sessions::set_process_dir(dir.clone());
        lingxi_core::host::live_sessions::set_process_session_id("self-session");
        lingxi_core::host::live_sessions::set_process_name("lead");

        std::fs::create_dir_all(dir.root()).unwrap();
        let peer_path = dir.root().join("222.json");
        std::fs::write(
            &peer_path,
            serde_json::to_vec(&serde_json::json!({
                "pid": 222u32,
                "sessionId": "abcdef12-3456-7890-abcd-ef1234567890",
                "name": "peer",
                "kind": "interactive",
                "startedAt": 0,
                "status": "waiting",
                "waitingFor": "permission prompt"
            }))
            .unwrap(),
        )
        .unwrap();

        let agent_registry = Arc::new(InMemoryAgentNameRegistry::new());
        let agent_id = lingxi_core::types::AgentId::new();
        agent_registry.register("worker-a", agent_id).await;
        let task_registry = Arc::new(StubTaskRegistry::default());
        task_registry.tasks.lock().unwrap().insert(
            agent_id.to_string(),
            TaskRecord {
                task_id: "t1".into(),
                task_type: "local_agent".into(),
                status: "running".into(),
                description: "agent".into(),
                agent_facts: None,
                ..Default::default()
            },
        );

        let mut builtin = shell_test_ctx(dummy_out());
        builtin.agent_name_registry = Some(agent_registry);
        builtin.task_registry = Some(task_registry);
        let tool = ListAgentsTool::new(builtin);

        let result = tool
            .call(json!({}), fresh_ctx(), fresh_tx())
            .await
            .expect("list succeeds");
        let listing = result.model_content.unwrap();
        assert!(listing.contains("This session is lead [00000000]"));
        assert!(listing.contains("worker-a  ·  local_agent  ·  busy"));
        assert!(listing.contains(
            "peer [abcdef12]  ·  interactive  ·  waiting  ·  waiting for permission prompt"
        ));
    }
}
