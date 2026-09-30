//! Tests for `streaming_executor.rs`, extracted from inline `#[cfg(test)]` blocks.

use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TOOL_CONCURRENCY_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn tool_concurrency_env_guard() -> std::sync::MutexGuard<'static, ()> {
        TOOL_CONCURRENCY_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn status_enum_roundtrips() {
        assert_eq!(ToolStatus::Queued, ToolStatus::Queued);
        assert_ne!(ToolStatus::Queued, ToolStatus::Yielded);
    }

    #[test]
    fn streaming_fallback_synthetic() {
        let block = synthetic_error_block(ToolUseId::new(), AbortReason::StreamingFallback);
        let ContentBlock::ToolResult { content, .. } = block else {
            panic!()
        };
        assert_eq!(
            content,
            "<tool_use_error>Error: Streaming fallback - tool execution discarded</tool_use_error>"
        );
    }

    // ============================================================================
    // Task 6: StreamingToolExecutor::add_tool tests
    // ============================================================================

    use crate::conversation::ConversationOrchestrator;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use crate::OrchestratorConfig;
    use async_trait::async_trait;
    use lingxi_core::types::MessageId;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tool_api::context::ToolUseContext;
    use tool_api::progress::ToolProgressSender;
    use tool_api::registry::ToolRegistry;
    use tool_api::tool_trait::{
        DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
        ValidationError,
    };

    /// Build an orchestrator with an EMPTY tool registry. Used to exercise the
    /// unknown-tool short-circuit path.
    fn orch_empty() -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
    }

    /// A minimal concurrency-safe tool for the "known tool" path test.
    struct SafeTool;

    #[async_trait]
    impl Tool for SafeTool {
        fn name(&self) -> &str {
            "SafeTool"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            "safe-tool".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult {
                data: json!({ "content": "ok" }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    struct StrictSchemaSafeTool;

    #[async_trait]
    impl Tool for StrictSchemaSafeTool {
        fn name(&self) -> &str {
            "StrictSchemaSafeTool"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| {
                    json!({
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "required": ["path"]
                    })
                });
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            "strict-schema-safe-tool".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult {
                data: json!({ "content": "ok" }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    /// Build an orchestrator whose registry contains a single `SafeTool`.
    fn orch_with_safe_tool() -> ConversationOrchestrator {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(SafeTool) as Arc<dyn Tool>);
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
    }

    fn orch_with_safe_and_strict_schema_tool() -> ConversationOrchestrator {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(SafeTool) as Arc<dyn Tool>);
        registry.register_builtin(Arc::new(StrictSchemaSafeTool) as Arc<dyn Tool>);
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
    }

    #[tokio::test]
    async fn add_unknown_tool_completes_immediately_with_wrapper() {
        let orch = orch_empty();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(
            ToolUseId::new(),
            "Nope".into(),
            json!({}),
            None,
            MessageId::new(),
        );
        let t = &exec.tools[0];
        assert_eq!(t.status, ToolStatus::Completed);
        assert!(t.is_concurrency_safe);
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = t.result.as_ref().unwrap()
        else {
            panic!("expected ToolResult block")
        };
        assert!(*is_error);
        assert_eq!(
            content,
            "<tool_use_error>Error: No such tool available: Nope</tool_use_error>"
        );
    }

    #[test]
    fn unknown_tool_suffix_is_empty_for_genuinely_unknown() {
        let tools = ToolRegistry::new();
        assert_eq!(unknown_tool_suffix("Nope", &tools, false, false), "");
    }

    #[test]
    fn unknown_tool_suffix_points_glob_and_grep_at_bash() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed::new("Bash")) as Arc<dyn Tool>);
        assert_eq!(
            unknown_tool_suffix("Glob", &tools, false, false),
            ". Glob is not available in this session \u{2014} find files with `find` via the Bash tool instead."
        );
        assert_eq!(
            unknown_tool_suffix("Grep", &tools, false, false),
            ". Grep is not available in this session \u{2014} search file contents with `grep` via the Bash tool instead."
        );
    }

    #[test]
    fn unknown_tool_suffix_glob_without_shell_is_disabled() {
        let tools = ToolRegistry::new();
        assert_eq!(
            unknown_tool_suffix("Glob", &tools, false, false),
            ". Glob is disabled for this session."
        );
    }

    #[test]
    fn unknown_tool_suffix_names_disconnected_mcp_server() {
        let tools = ToolRegistry::new();
        assert_eq!(
            unknown_tool_suffix("mcp__github__issue", &tools, false, false),
            ". Its MCP server 'github' has disconnected. Continue without this tool; it becomes callable again only if the server reconnects."
        );
        assert_eq!(
            unknown_tool_suffix("mcp__github__issue", &tools, true, false),
            ". Its MCP server 'github' is not available in this context. Continue without this tool."
        );
    }

    #[test]
    fn unknown_tool_suffix_subagent_restricted_catalog() {
        let tools = ToolRegistry::new();
        assert_eq!(
            unknown_tool_suffix("EnterPlanMode", &tools, true, false),
            ". EnterPlanMode is not available inside subagents. Complete the task with the tools provided and return findings to the orchestrator."
        );
        assert_eq!(
            unknown_tool_suffix("WaitForMcpServers", &tools, true, false),
            ". WaitForMcpServers is not available inside subagents. Complete the task with the tools provided and return findings to the orchestrator."
        );
        assert_eq!(
            unknown_tool_suffix("EnterPlanMode", &tools, false, false),
            ""
        );
    }

    #[test]
    fn unknown_tool_suffix_pending_mcp_points_at_wait_for_mcp_servers() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed::new("WaitForMcpServers")) as Arc<dyn Tool>);
        assert_eq!(
            unknown_tool_suffix("mcp__github__issue", &tools, false, false),
            ". The MCP server 'github' is still connecting. Call WaitForMcpServers to wait for it, then try again."
        );
        // Subagent skips `l5o` (`r?"":l5o`) and uses the disconnected arm.
        assert_eq!(
            unknown_tool_suffix("mcp__github__issue", &tools, true, false),
            ". Its MCP server 'github' is not available in this context. Continue without this tool."
        );
    }

    #[test]
    fn unknown_tool_suffix_coordinator_points_at_agent() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed::new("Agent")) as Arc<dyn Tool>);
        tools.register_builtin(Arc::new(SafeToolNamed::new("Read")) as Arc<dyn Tool>);
        assert_eq!(
            unknown_tool_suffix("Read", &tools, false, true),
            ". Read is not available to you as the coordinator \u{2014} run it from a worker via the Agent tool instead."
        );
        assert_eq!(
            unknown_tool_suffix("Read", &tools, false, false),
            ". Read is disabled for this session, in subagents as well as here."
        );
        tools.register_builtin(Arc::new(SafeToolNamed::new("Skill")) as Arc<dyn Tool>);
        assert_eq!(
            unknown_tool_suffix("Skill", &tools, false, true),
            ". Skill is disabled for this session, in subagents as well as here."
        );
    }

    #[test]
    fn unknown_tool_suffix_catalog_disabled_hides_glob_via_shell() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed::new("Bash")) as Arc<dyn Tool>);
        tools.register_builtin(Arc::new(SafeToolNamed::new("Glob")) as Arc<dyn Tool>);
        tools.set_session_tool_allowlist(&["Bash".to_string()]);
        assert_eq!(
            unknown_tool_suffix("Glob", &tools, false, false),
            ". Glob is disabled for this session, in subagents as well as here."
        );
    }

    #[test]
    fn unknown_tool_suffix_webfetch_in_catalog_is_disabled_not_artifact() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed::new("WebFetch")) as Arc<dyn Tool>);
        tools.register_builtin(Arc::new(SafeToolNamed::new("Agent")) as Arc<dyn Tool>);
        assert_eq!(
            unknown_tool_suffix("WebFetch", &tools, false, false),
            ". WebFetch is disabled for this session, in subagents as well as here."
        );
        assert_eq!(
            unknown_tool_suffix("WebFetch", &tools, false, true),
            ". WebFetch is not available to you as the coordinator \u{2014} run it from a worker via the Agent tool instead."
        );
    }

    #[test]
    fn unknown_tool_suffix_qbt_matches_canonical_alias() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed::new("Agent")) as Arc<dyn Tool>);
        tools.register_builtin(Arc::new(SafeToolNamed {
            name: "ListAgents",
            aliases: &["ListPeers"],
            ..SafeToolNamed::new("ListAgents")
        }) as Arc<dyn Tool>);
        tools.set_session_tool_allowlist(&["Agent".to_string()]);
        assert_eq!(
            unknown_tool_suffix("ListPeers", &tools, false, true),
            ". ListPeers is disabled for this session, in subagents as well as here."
        );
    }

    struct StubCoordinatorMode {
        enabled: bool,
    }

    impl lingxi_core::host::coordinator_mode::CoordinatorModeHandle for StubCoordinatorMode {
        fn is_enabled(&self) -> bool {
            self.enabled
        }
    }

    fn orch_with_named_tools(names: &[&'static str]) -> ConversationOrchestrator {
        let mut registry = ToolRegistry::new();
        for name in names {
            registry.register_builtin(Arc::new(SafeToolNamed::new(name)) as Arc<dyn Tool>);
        }
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
        .with_coordinator_simple_mode_for_test(false)
        .with_coordinator_pool_for_test(false, &[])
    }

    #[test]
    fn unknown_tool_suffix_for_uses_wired_coordinator_mode() {
        let orch = orch_with_named_tools(&["Agent", "Read"])
            .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: true }));
        assert_eq!(
            unknown_tool_suffix_for("Read", &orch),
            ". Read is not available to you as the coordinator \u{2014} run it from a worker via the Agent tool instead."
        );
    }

    #[tokio::test]
    async fn add_tool_disabled_coordinator_handle_dispatches_worker_tool() {
        let orch = orch_with_named_tools(&["Agent", "Read"])
            .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: false }));
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(
            ToolUseId::new(),
            "Read".into(),
            json!({}),
            None,
            MessageId::new(),
        );
        assert_eq!(exec.tools[0].status, ToolStatus::Queued);
    }

    #[tokio::test]
    async fn add_tool_simple_mode_keeps_pool_but_suppresses_worker_redirect() {
        let orch = orch_with_named_tools(&["Agent", "Read"])
            .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: true }))
            .with_coordinator_simple_mode_for_test(true);
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(
            ToolUseId::new(),
            "Read".into(),
            json!({}),
            None,
            MessageId::new(),
        );
        assert_eq!(exec.tools[0].status, ToolStatus::Completed);
        let ContentBlock::ToolResult { content, .. } = exec.tools[0].result.as_ref().unwrap()
        else {
            panic!("expected error")
        };
        assert!(content.contains("disabled for this session"));
        assert!(!content.contains("run it from a worker"));
    }

    #[tokio::test]
    async fn add_tool_coordinator_hidden_worker_tool_gets_y7e() {
        let orch = orch_with_named_tools(&["Agent", "Read"])
            .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: true }));
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(
            ToolUseId::new(),
            "Read".into(),
            json!({}),
            None,
            MessageId::new(),
        );
        let t = &exec.tools[0];
        assert_eq!(t.status, ToolStatus::Completed);
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = t.result.as_ref().unwrap()
        else {
            panic!("expected ToolResult block")
        };
        assert!(*is_error);
        assert_eq!(
            content,
            "<tool_use_error>Error: No such tool available: Read. Read is not available to you as the coordinator \u{2014} run it from a worker via the Agent tool instead.</tool_use_error>"
        );
    }

    #[tokio::test]
    async fn add_tool_coordinator_pool_dispatches_user_and_plan_controls() {
        let names = [
            "Agent",
            "SendMessage",
            "TaskStop",
            "AskUserQuestion",
            "EnterPlanMode",
            "ExitPlanMode",
            "subscribe_pr_activity",
            "unsubscribe_pr_activity",
        ];
        let orch = orch_with_named_tools(&names)
            .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: true }))
            .with_coordinator_pool_for_test(
                false,
                &["AskUserQuestion", "EnterPlanMode", "ExitPlanMode"],
            );
        let mut exec = StreamingToolExecutor::new(&orch);
        for name in names {
            exec.add_tool(
                ToolUseId::new(),
                name.into(),
                json!({}),
                None,
                MessageId::new(),
            );
            assert_eq!(
                exec.tools.last().unwrap().status,
                ToolStatus::Queued,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn add_tool_coordinator_qbt_tools_remain_available() {
        let names = [
            "Skill",
            "ListAgents",
            "Workflow",
            "ReadNotifications",
            "StructuredOutput",
        ];
        let orch = orch_with_named_tools(&names)
            .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: true }));
        let mut exec = StreamingToolExecutor::new(&orch);
        for name in names {
            exec.add_tool(
                ToolUseId::new(),
                name.into(),
                json!({}),
                None,
                MessageId::new(),
            );
            assert_eq!(exec.tools.last().unwrap().status, ToolStatus::Queued);
        }
    }

    #[test]
    fn coordinator_pool_metadata_and_extras_follow_idr() {
        let orch = orch_with_named_tools(&[]);
        assert!(orch
            .is_coordinator_pool_tool(&SafeToolNamed::new("mcp__github__subscribe_pr_activity")));
        let comms = SafeToolNamed {
            role: Some("comms"),
            ..SafeToolNamed::new("mcp__chat__send")
        };
        assert!(orch.is_coordinator_pool_tool(&comms));
        for name in [
            "AskUserQuestion",
            "ExitPlanMode",
            "SendUserMessage",
            "SendUserFile",
            "Bash",
        ] {
            assert!(
                !orch.is_coordinator_pool_tool(&SafeToolNamed::new(name)),
                "{name}"
            );
        }
        let orch = orch.with_coordinator_pool_for_test(true, &["AskUserQuestion", "V1", "Parent"]);
        for name in ["AskUserQuestion", "SendUserMessage", "SendUserFile"] {
            assert!(
                orch.is_coordinator_pool_tool(&SafeToolNamed::new(name)),
                "{name}"
            );
        }
        assert!(orch.is_coordinator_pool_tool(&SafeToolNamed {
            v1: Some("V1"),
            ..SafeToolNamed::new("split")
        }));
        assert!(orch.is_coordinator_pool_tool(&SafeToolNamed {
            parent: Some("Parent"),
            ..SafeToolNamed::new("split")
        }));
        assert!(!orch.is_coordinator_pool_tool(&SafeToolNamed {
            aliases: &["V1"],
            ..SafeToolNamed::new("split")
        }));
    }

    #[test]
    fn coordinator_redirect_requires_enabled_worker_and_agent_tools() {
        let mut orch =
            orch_with_named_tools(&["Agent", "Read", "AskUserQuestion", "ExitPlanMode", "Custom"])
                .with_coordinator_mode(Arc::new(StubCoordinatorMode { enabled: true }));
        for name in ["AskUserQuestion", "ExitPlanMode", "Custom"] {
            assert!(unknown_tool_suffix_for(name, &orch).contains("disabled for this session"));
        }
        for denied in ["Read", "Agent"] {
            *orch.tool_pool_denied_names.write().unwrap() = vec![denied.into()];
            assert!(unknown_tool_suffix_for("Read", &orch).contains("disabled for this session"));
        }
        orch.tool_pool_denied_names.write().unwrap().clear();
        *orch.main_agent_tool_names.write().unwrap() = Some(["Read".into()].into_iter().collect());
        assert!(unknown_tool_suffix_for("Read", &orch).contains("disabled for this session"));
        *orch.main_agent_tool_names.write().unwrap() = None;
        assert!(unknown_tool_suffix_for("Read", &orch).contains("run it from a worker"));
        Arc::get_mut(&mut orch.tools)
            .unwrap()
            .set_session_tool_allowlist(&["Agent".into()]);
        assert!(unknown_tool_suffix_for("Read", &orch).contains("disabled for this session"));
    }

    #[test]
    fn disabled_brief_uses_canonical_message_fallback() {
        let mut tools = ToolRegistry::new();
        tools.register_builtin(Arc::new(SafeToolNamed {
            aliases: &["Brief"],
            ..SafeToolNamed::new("SendUserMessage")
        }));
        assert_eq!(unknown_tool_suffix("Brief", &tools, false, true),
            ". Brief is not enabled in this session \u{2014} write your message as normal assistant text instead.");
    }

    struct SafeToolNamed {
        name: &'static str,
        aliases: &'static [&'static str],
        role: Option<&'static str>,
        v1: Option<&'static str>,
        parent: Option<&'static str>,
    }

    impl SafeToolNamed {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                aliases: &[],
                role: None,
                v1: None,
                parent: None,
            }
        }
    }

    #[async_trait]
    impl Tool for SafeToolNamed {
        fn name(&self) -> &str {
            self.name
        }
        fn aliases(&self) -> &[&str] {
            self.aliases
        }
        fn mcp_role(&self) -> Option<&str> {
            self.role
        }
        fn underlying_v1_tool_name(&self) -> Option<&str> {
            self.v1
        }
        fn family_parent_tool_name(&self) -> Option<&str> {
            self.parent
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            self.name.into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult {
                data: json!({ "content": "ok" }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    #[tokio::test]
    async fn add_known_concurrency_safe_tool_is_queued() {
        let orch = orch_with_safe_tool();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(
            ToolUseId::new(),
            "SafeTool".into(),
            json!({}),
            None,
            MessageId::new(),
        );
        let t = &exec.tools[0];
        assert_eq!(t.status, ToolStatus::Queued);
        assert!(t.is_concurrency_safe);
        assert!(t.result.is_none());
    }

    #[tokio::test]
    async fn malformed_input_is_concurrency_unsafe_and_queued_behind_running_safe_tool() {
        let orch = orch_with_safe_and_strict_schema_tool();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        exec.add_tool(
            ToolUseId::new(),
            "StrictSchemaSafeTool".into(),
            json!({}),
            None,
            a,
        );

        exec.process_queue();

        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        assert_eq!(exec.tools[1].status, ToolStatus::Queued);
        assert!(!exec.tools[1].is_concurrency_safe);
    }

    // ============================================================================
    // Task 7: can_execute + process_queue tests
    // ============================================================================

    use super::can_execute;

    #[test]
    fn can_execute_respects_concurrency_safety() {
        assert!(can_execute(&[], true));
        assert!(can_execute(&[], false)); // nothing running → ok
        assert!(can_execute(&[true, true], true)); // all safe + candidate safe → ok
        assert!(!can_execute(&[true], false)); // candidate unsafe, something running → no
        assert!(!can_execute(&[false], true)); // an unsafe tool running → no
        assert!(!can_execute(&[true, true], false)); // many safe running, unsafe candidate → no
    }

    /// A minimal concurrency-UNSAFE tool for ordering/barrier tests.
    struct UnsafeTool;

    #[async_trait]
    impl Tool for UnsafeTool {
        fn name(&self) -> &str {
            "UnsafeTool"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            false
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            false
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            "unsafe-tool".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult {
                data: json!({ "content": "ok" }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    /// Build an orchestrator with both SafeTool and UnsafeTool.
    fn orch_with_both_tools() -> ConversationOrchestrator {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(SafeTool) as Arc<dyn Tool>);
        registry.register_builtin(Arc::new(UnsafeTool) as Arc<dyn Tool>);
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
    }

    /// [safe, unsafe, safe]: first safe tool starts; the unsafe is a barrier;
    /// the third safe tool must NOT start out of order.
    #[tokio::test]
    async fn process_queue_starts_safe_tool_and_barriers_on_unsafe() {
        let orch = orch_with_both_tools();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        exec.add_tool(ToolUseId::new(), "UnsafeTool".into(), json!({}), None, a);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        exec.process_queue();
        // First safe starts; unsafe is a barrier (can't run while safe executes);
        // the third safe sits behind the barrier and must NOT start out of order.
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        assert_eq!(exec.tools[1].status, ToolStatus::Queued);
        assert_eq!(exec.tools[2].status, ToolStatus::Queued);
    }

    /// [safe, safe]: both concurrent-safe tools start.
    #[tokio::test]
    async fn process_queue_starts_all_safe_tools_concurrently() {
        let orch = orch_with_safe_tool();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        exec.process_queue();
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        assert_eq!(exec.tools[1].status, ToolStatus::Executing);
    }

    /// [unsafe, safe]: the unsafe tool starts first (nothing executing), then
    /// the following safe tool stays Queued (exclusive holds the barrier).
    #[tokio::test]
    async fn process_queue_unsafe_first_blocks_following_safe() {
        let orch = orch_with_both_tools();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(ToolUseId::new(), "UnsafeTool".into(), json!({}), None, a);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        exec.process_queue();
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        assert_eq!(exec.tools[1].status, ToolStatus::Queued);
    }

    /// [safe, safe, unsafe, safe]: both leading safe tools start; the unsafe is
    /// a barrier; the trailing safe stays Queued behind it.
    #[tokio::test]
    async fn process_queue_two_safe_then_unsafe_barrier() {
        let orch = orch_with_both_tools();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        exec.add_tool(ToolUseId::new(), "UnsafeTool".into(), json!({}), None, a);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        exec.process_queue();
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        assert_eq!(exec.tools[1].status, ToolStatus::Executing);
        assert_eq!(exec.tools[2].status, ToolStatus::Queued);
        assert_eq!(exec.tools[3].status, ToolStatus::Queued);
    }

    /// A SAFE queued tool blocked by an already-Executing UNSAFE tool does NOT
    /// barrier (only an unsafe queued tool barriers) — process_queue scans past
    /// it and starts nothing. Exercises the "keep scanning" continuation branch
    /// that requires pre-existing Executing state.
    #[tokio::test]
    async fn process_queue_safe_blocked_by_executing_unsafe_keeps_scanning() {
        let orch = orch_with_both_tools();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(ToolUseId::new(), "UnsafeTool".into(), json!({}), None, a);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        // Simulate the unsafe tool already running (as if a prior process_queue
        // started it and it has not completed yet).
        exec.tools[0].status = ToolStatus::Executing;
        exec.process_queue();
        // Both safe tools are blocked by the executing unsafe tool; neither
        // starts, and the safe ones do NOT barrier (scan continues past them).
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        assert_eq!(exec.tools[1].status, ToolStatus::Queued);
        assert_eq!(exec.tools[2].status, ToolStatus::Queued);
    }

    // ============================================================================
    // B6: concurrency cap (binary r1p / i1p) tests
    // ============================================================================

    use super::{
        max_tool_use_concurrency, max_tool_use_concurrency_from, DEFAULT_MAX_TOOL_USE_CONCURRENCY,
    };

    /// Binary `r1p`: `parseInt(env, 10) > 0 ? env : 10`.
    #[test]
    fn max_tool_use_concurrency_parse_matches_binary_r1p() {
        // Absent / empty / non-numeric → default 10.
        assert_eq!(max_tool_use_concurrency_from(None), 10);
        assert_eq!(max_tool_use_concurrency_from(Some("")), 10);
        assert_eq!(max_tool_use_concurrency_from(Some("abc")), 10);
        // Zero and negative → default (binary uses `e > 0`).
        assert_eq!(max_tool_use_concurrency_from(Some("0")), 10);
        assert_eq!(max_tool_use_concurrency_from(Some("-3")), 10);
        // Positive → that value.
        assert_eq!(max_tool_use_concurrency_from(Some("1")), 1);
        assert_eq!(max_tool_use_concurrency_from(Some("5")), 5);
        assert_eq!(max_tool_use_concurrency_from(Some("25")), 25);
        // parseInt leading-prefix semantics: "5x" → 5, "  7 " → 7.
        assert_eq!(max_tool_use_concurrency_from(Some("5x")), 5);
        assert_eq!(max_tool_use_concurrency_from(Some("  7 ")), 7);
        // Default constant is 10.
        assert_eq!(DEFAULT_MAX_TOOL_USE_CONCURRENCY, 10);
    }

    /// With 30 concurrency-safe tools, `process_queue` must start at most the
    /// default cap (10) simultaneously; the remaining 20 stay Queued. Mirrors
    /// the binary's `i1p` bounded merge (window = `r1p()` = 10).
    /// (Mutates env to clear any override → `--test-threads=1`.)
    #[tokio::test]
    async fn process_queue_caps_safe_tools_at_default_ten() {
        let _guard = tool_concurrency_env_guard();
        // Ensure no env override leaks in from the environment.
        std::env::remove_var("LINGXI_MAX_TOOL_USE_CONCURRENCY");
        assert_eq!(max_tool_use_concurrency(), DEFAULT_MAX_TOOL_USE_CONCURRENCY);

        let orch = orch_with_safe_tool();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        for _ in 0..30 {
            exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        }
        exec.process_queue();

        let executing = exec
            .tools
            .iter()
            .filter(|t| t.status == ToolStatus::Executing)
            .count();
        let queued = exec
            .tools
            .iter()
            .filter(|t| t.status == ToolStatus::Queued)
            .count();
        assert_eq!(
            executing, DEFAULT_MAX_TOOL_USE_CONCURRENCY,
            "no more than {DEFAULT_MAX_TOOL_USE_CONCURRENCY} safe tools may run at once"
        );
        assert_eq!(
            queued,
            30 - DEFAULT_MAX_TOOL_USE_CONCURRENCY,
            "the rest stay Queued"
        );
        // The first N (in received order) are the ones started.
        for i in 0..DEFAULT_MAX_TOOL_USE_CONCURRENCY {
            assert_eq!(
                exec.tools[i].status,
                ToolStatus::Executing,
                "tool {i} should run"
            );
        }
        for i in DEFAULT_MAX_TOOL_USE_CONCURRENCY..30 {
            assert_eq!(
                exec.tools[i].status,
                ToolStatus::Queued,
                "tool {i} should wait"
            );
        }
    }

    /// `LINGXI_MAX_TOOL_USE_CONCURRENCY` overrides the cap. With the env
    /// set to 3 and 10 safe tools queued, exactly 3 start.
    /// (Mutates env → `--test-threads=1`.)
    #[tokio::test]
    async fn process_queue_respects_env_concurrency_override() {
        let _guard = tool_concurrency_env_guard();
        std::env::set_var("LINGXI_MAX_TOOL_USE_CONCURRENCY", "3");
        // Guard so a panic/assert failure still clears the env for sibling tests.
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                std::env::remove_var("LINGXI_MAX_TOOL_USE_CONCURRENCY");
            }
        }
        let _clear = Clear;

        assert_eq!(max_tool_use_concurrency(), 3);

        let orch = orch_with_safe_tool();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        for _ in 0..10 {
            exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        }
        exec.process_queue();

        let executing = exec
            .tools
            .iter()
            .filter(|t| t.status == ToolStatus::Executing)
            .count();
        assert_eq!(executing, 3, "env override caps safe concurrency at 3");
        assert_eq!(
            exec.tools
                .iter()
                .filter(|t| t.status == ToolStatus::Queued)
                .count(),
            7,
            "remaining 7 stay Queued under the override"
        );
    }

    /// Releasing one in-flight safe tool (mark it Completed) frees a slot so the
    /// next queued safe tool starts on the following `process_queue` — the
    /// sliding-window behaviour of the binary's `i1p` merge.
    /// (Mutates env → `--test-threads=1`.)
    #[tokio::test]
    async fn process_queue_starts_next_safe_when_slot_frees() {
        let _guard = tool_concurrency_env_guard();
        std::env::set_var("LINGXI_MAX_TOOL_USE_CONCURRENCY", "2");
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                std::env::remove_var("LINGXI_MAX_TOOL_USE_CONCURRENCY");
            }
        }
        let _clear = Clear;

        let orch = orch_with_safe_tool();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        for _ in 0..4 {
            exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        }
        exec.process_queue();
        // Cap=2 → first two run, last two wait.
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        assert_eq!(exec.tools[1].status, ToolStatus::Executing);
        assert_eq!(exec.tools[2].status, ToolStatus::Queued);
        assert_eq!(exec.tools[3].status, ToolStatus::Queued);

        // Simulate tool[0] completing → frees one slot.
        exec.tools[0].status = ToolStatus::Completed;
        exec.process_queue();
        // Now exactly one more (tool[2]) starts; tool[3] still waits (slot full again).
        assert_eq!(
            exec.tools[2].status,
            ToolStatus::Executing,
            "freed slot starts next"
        );
        assert_eq!(
            exec.tools[3].status,
            ToolStatus::Queued,
            "still capped at 2 in-flight"
        );
        let executing = exec
            .tools
            .iter()
            .filter(|t| t.status == ToolStatus::Executing)
            .count();
        assert_eq!(executing, 2, "never more than 2 safe tools in flight");
    }

    #[tokio::test]
    async fn add_unknown_tool_sets_provider_id_on_result() {
        let orch = orch_empty();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(
            ToolUseId::new(),
            "Ghost".into(),
            json!({}),
            Some("prov-abc-123".into()),
            MessageId::new(),
        );
        let t = &exec.tools[0];
        let ContentBlock::ToolResult {
            provider_tool_use_id,
            ..
        } = t.result.as_ref().unwrap()
        else {
            panic!()
        };
        assert_eq!(provider_tool_use_id.as_deref(), Some("prov-abc-123"));
    }

    /// Part B: the UserInterrupted synthetic uses the BARE REJECT_MESSAGE with
    /// is_error: true, and is NOT `<tool_use_error>`-wrapped.
    #[test]
    fn user_interrupted_synthetic_is_bare_reject_message() {
        let block = synthetic_error_block(ToolUseId::new(), AbortReason::UserInterrupted);
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = block
        else {
            panic!()
        };
        assert!(is_error, "user-interrupted result must be an error");
        assert_eq!(content, REJECT_MESSAGE);
        assert!(
            !content.contains("<tool_use_error>"),
            "REJECT_MESSAGE must be bare (not tool_use_error-wrapped): {content}"
        );
    }

    #[test]
    fn tool_description_priority_and_truncation() {
        // command field, > 40 chars → truncate to 40 + ellipsis.
        let long = "x".repeat(45);
        let t = TrackedTool {
            id: ToolUseId::new(),
            name: "Bash".into(),
            input: json!({ "command": long }),
            provider_id: None,
            assistant_id: MessageId::new(),
            status: ToolStatus::Queued,
            is_concurrency_safe: false,
            result: None,
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
        };
        let desc = tool_description(&t);
        assert_eq!(desc, format!("Bash({}\u{2026})", "x".repeat(40)));

        // file_path fallback (no command), short → no truncation.
        let t = TrackedTool {
            id: ToolUseId::new(),
            name: "Read".into(),
            input: json!({ "file_path": "/tmp/a.txt" }),
            provider_id: None,
            assistant_id: MessageId::new(),
            status: ToolStatus::Queued,
            is_concurrency_safe: true,
            result: None,
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
        };
        assert_eq!(tool_description(&t), "Read(/tmp/a.txt)");

        // pattern fallback.
        let t = TrackedTool {
            id: ToolUseId::new(),
            name: "Grep".into(),
            input: json!({ "pattern": "foo" }),
            provider_id: None,
            assistant_id: MessageId::new(),
            status: ToolStatus::Queued,
            is_concurrency_safe: true,
            result: None,
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
        };
        assert_eq!(tool_description(&t), "Grep(foo)");

        // empty input → bare name.
        let t = TrackedTool {
            id: ToolUseId::new(),
            name: "SafeTool".into(),
            input: json!({}),
            provider_id: None,
            assistant_id: MessageId::new(),
            status: ToolStatus::Queued,
            is_concurrency_safe: true,
            result: None,
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
        };
        assert_eq!(tool_description(&t), "SafeTool");
    }

    // ============================================================================
    // Task 9: take_newly_completed + has_unfinished tests
    // ============================================================================

    /// Test 1: Two SafeTools driven to completion → take_newly_completed returns
    /// both in received order; a second call returns empty; statuses become Yielded.
    #[tokio::test]
    async fn take_newly_completed_returns_results_in_received_order() {
        let orch = orch_with_safe_tool();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        exec.process_queue();
        while !exec.inflight.is_empty() {
            exec.drain_one().await;
        }
        // Both should be Completed now.
        assert_eq!(exec.tools[0].status, ToolStatus::Completed);
        assert_eq!(exec.tools[1].status, ToolStatus::Completed);

        let results = exec.take_newly_completed();
        assert_eq!(results.len(), 2, "expected both tools drained");
        // Statuses should now be Yielded.
        assert_eq!(exec.tools[0].status, ToolStatus::Yielded);
        assert_eq!(exec.tools[1].status, ToolStatus::Yielded);

        // Second call returns empty (all already Yielded).
        let results2 = exec.take_newly_completed();
        assert!(results2.is_empty(), "second call must return empty");
    }

    /// Test 2: Unknown tool (already Completed at add_tool time) is immediately
    /// yielded by take_newly_completed without calling process_queue.
    #[tokio::test]
    async fn take_newly_completed_yields_unknown_tool_immediately() {
        let orch = orch_empty();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(
            ToolUseId::new(),
            "NoSuchTool".into(),
            json!({}),
            None,
            MessageId::new(),
        );
        // No process_queue, no drain_one — it's already Completed.
        assert_eq!(exec.tools[0].status, ToolStatus::Completed);

        let results = exec.take_newly_completed();
        assert_eq!(results.len(), 1, "unknown tool result should be drained");
        assert_eq!(exec.tools[0].status, ToolStatus::Yielded);

        let ContentBlock::ToolResult { is_error, .. } = &results[0].block else {
            panic!("expected ToolResult block")
        };
        assert!(*is_error, "unknown-tool block must be an error");
        assert_eq!(results[0].post_tool_batch_calls.len(), 1);
        assert_eq!(
            results[0].post_tool_batch_calls[0].tool_response,
            Some(json!(
                "<tool_use_error>Error: No such tool available: NoSuchTool</tool_use_error>"
            )),
            "PostToolBatch covers every assistant tool_use and reads the yielded result content"
        );
    }

    /// Test 3: Exclusive-barrier stop.
    /// tool[0] = Completed (safe), tool[1] = Executing+unsafe, tool[2] = Completed (safe).
    /// take_newly_completed must yield ONLY tool[0] and stop at tool[1].
    #[tokio::test]
    async fn take_newly_completed_stops_at_executing_exclusive_tool() {
        let orch = orch_with_safe_tool();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);

        // We build a contrived state by directly constructing TrackedTools.
        let id0 = ToolUseId::new();
        let id1 = ToolUseId::new();
        let id2 = ToolUseId::new();
        let result_block = ContentBlock::ToolResult {
            tool_use_id: id0.clone(),
            content: "done".into(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: None,
        };
        let result_block2 = ContentBlock::ToolResult {
            tool_use_id: id2.clone(),
            content: "also done".into(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: None,
        };

        exec.tools.push(TrackedTool {
            id: id0,
            name: "SafeTool".into(),
            input: json!({}),
            provider_id: None,
            assistant_id: a,
            status: ToolStatus::Completed,
            is_concurrency_safe: true,
            result: Some(result_block),
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
        });
        exec.tools.push(TrackedTool {
            id: id1,
            name: "UnsafeTool".into(),
            input: json!({}),
            provider_id: None,
            assistant_id: a,
            status: ToolStatus::Executing,
            is_concurrency_safe: false, // exclusive barrier
            result: None,
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
        });
        exec.tools.push(TrackedTool {
            id: id2,
            name: "SafeTool".into(),
            input: json!({}),
            provider_id: None,
            assistant_id: a,
            status: ToolStatus::Completed,
            is_concurrency_safe: true,
            result: Some(result_block2),
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
        });

        let results = exec.take_newly_completed();
        // Only tool[0] should be emitted; tool[1] is the barrier; tool[2] is skipped.
        assert_eq!(
            results.len(),
            1,
            "only the pre-barrier completed tool should be drained"
        );
        assert_eq!(exec.tools[0].status, ToolStatus::Yielded);
        // tool[1] still Executing (we don't touch it).
        assert_eq!(exec.tools[1].status, ToolStatus::Executing);
        // tool[2] still Completed (was NOT emitted past the barrier).
        assert_eq!(exec.tools[2].status, ToolStatus::Completed);
    }

    /// Complement of the barrier test: an Executing+SAFE tool does NOT stop the
    /// drain — a `Completed` tool AFTER it is still emitted (TS: only
    /// `executing && !isConcurrencySafe` breaks; a safe executing tool falls
    /// through and scanning continues).
    #[tokio::test]
    async fn take_newly_completed_skips_executing_safe_and_emits_later_completed() {
        let orch = orch_with_safe_tool();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        let id0 = ToolUseId::new();
        let id1 = ToolUseId::new();
        exec.tools.push(TrackedTool {
            id: id0,
            name: "SafeTool".into(),
            input: json!({}),
            provider_id: None,
            assistant_id: a,
            status: ToolStatus::Executing,
            is_concurrency_safe: true, // safe → NOT a barrier
            result: None,
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
        });
        exec.tools.push(TrackedTool {
            id: id1.clone(),
            name: "SafeTool".into(),
            input: json!({}),
            provider_id: None,
            assistant_id: a,
            status: ToolStatus::Completed,
            is_concurrency_safe: true,
            result: Some(ContentBlock::ToolResult {
                tool_use_id: id1,
                content: "done".into(),
                is_error: false,
                provider_tool_use_id: None,
                content_blocks: None,
            }),
            prevent_continuation: false,
            injected: Vec::new(),
            modifiers: Vec::new(),
            post_tool_batch_calls: Vec::new(),
        });

        let results = exec.take_newly_completed();
        // tool[0] (Executing+safe) is skipped but not a barrier; tool[1] emits.
        assert_eq!(results.len(), 1);
        assert_eq!(exec.tools[0].status, ToolStatus::Executing); // untouched
        assert_eq!(exec.tools[1].status, ToolStatus::Yielded);
    }

    // ============================================================================
    // DEFERRED-3: user-ESC granular interrupt (abort_reason_for + per-tool gating)
    // ============================================================================

    /// A concurrency-SAFE tool whose `interrupt_behavior()==Cancel`. Sleeps,
    /// racing its `ctx.cancel`; on cancel returns `Aborted` so the executor
    /// substitutes the synthetic. Mirrors a WebFetch/Agent-style Cancel tool.
    struct CancelBehaviorTool {
        name: &'static str,
        is_mcp: bool,
    }

    #[async_trait]
    impl Tool for CancelBehaviorTool {
        fn name(&self) -> &str {
            self.name
        }
        fn is_mcp(&self) -> bool {
            self.is_mcp
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn interrupt_behavior(
            &self,
            _input: &serde_json::Value,
        ) -> tool_api::tool_trait::InterruptBehavior {
            tool_api::tool_trait::InterruptBehavior::Cancel
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            "cancel-tool".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            let token = ctx.cancel.clone();
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    Ok(ToolCallResult {
                        data: json!({ "content": "cancel-tool-ran-to-end" }),
                        model_content: None,
                        new_messages: vec![],
                        context_modifier: None,
                        is_error: false,
                        mcp_meta: None,
                    })
                }
                () = async { match token { Some(t) => t.cancelled().await, None => std::future::pending().await } } => {
                    Err(ToolError::Aborted)
                }
            }
        }
    }

    fn orch_with_cancel_and_block_tools() -> ConversationOrchestrator {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CancelBehaviorTool {
            name: "CancelTool",
            is_mcp: false,
        }) as Arc<dyn Tool>);
        registry.register_builtin(Arc::new(CancelBehaviorTool {
            name: "McpCancelTool",
            is_mcp: true,
        }) as Arc<dyn Tool>);
        // SafeTool defaults to Block (no interrupt_behavior override).
        registry.register_builtin(Arc::new(SafeTool) as Arc<dyn Tool>);
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
    }

    /// `abort_reason_for`: a fired user-cancel token yields `UserInterrupted` for
    /// a Cancel-behavior tool and `None` for a Block-behavior tool (gating).
    #[tokio::test]
    async fn abort_reason_for_gates_on_interrupt_behavior() {
        let orch = orch_with_cancel_and_block_tools();
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new_with_user_cancel(&orch, user_cancel.clone());
        exec.add_tool(ToolUseId::new(), "CancelTool".into(), json!({}), None, a);
        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        // Not fired yet → no abort for either.
        assert_eq!(exec.abort_reason_for(0), None);
        assert_eq!(exec.abort_reason_for(1), None);
        user_cancel.cancel();
        assert_eq!(exec.abort_reason_for(0), Some(AbortReason::UserInterrupted));
        assert_eq!(
            exec.abort_reason_for(1),
            None,
            "Block tool is NOT interrupted"
        );
    }

    /// Queued Cancel-behavior tool under a fired user-cancel gets the bare
    /// REJECT_MESSAGE via `apply_abort_to_pending`; a queued Block tool runs.
    #[tokio::test]
    async fn apply_abort_to_pending_user_interrupt_rejects_cancel_tool_only() {
        let orch = orch_with_cancel_and_block_tools();
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new_with_user_cancel(&orch, user_cancel.clone());
        exec.add_tool(ToolUseId::new(), "CancelTool".into(), json!({}), None, a);
        user_cancel.cancel();
        exec.apply_abort_to_pending();
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = exec.tools[0].result.as_ref().unwrap()
        else {
            panic!()
        };
        assert!(*is_error);
        assert_eq!(content, REJECT_MESSAGE);
    }

    #[tokio::test]
    async fn interrupted_mcp_tool_uses_the_2_1_246_explicit_error() {
        let orch = orch_with_cancel_and_block_tools();
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let mut exec = StreamingToolExecutor::new_with_user_cancel(&orch, user_cancel.clone());
        exec.add_tool(
            ToolUseId::new(),
            "McpCancelTool".into(),
            json!({}),
            None,
            MessageId::new(),
        );
        user_cancel.cancel();
        exec.apply_abort_to_pending();

        let ContentBlock::ToolResult {
            content, is_error, ..
        } = exec.tools[0].result.as_ref().unwrap()
        else {
            panic!()
        };
        assert!(*is_error);
        assert_eq!(
            content,
            "Error: The tool call was interrupted before a result was received. It may or may not have completed on the server — verify before assuming it succeeded, and retry if needed."
        );
    }

    /// End-to-end through `run_to_completion`: an in-flight Cancel-behavior tool
    /// observes its `ctx.cancel` (parented to the user token) firing mid-flight,
    /// returns early, and `drain_one` substitutes the bare REJECT_MESSAGE.
    #[tokio::test]
    async fn in_flight_cancel_tool_user_interrupted_gets_reject_message() {
        let orch = orch_with_cancel_and_block_tools();
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new_with_user_cancel(&orch, user_cancel.clone());
        exec.add_tool(ToolUseId::new(), "CancelTool".into(), json!({}), None, a);
        // Start it, then fire the user cancel while it is in flight.
        exec.process_queue();
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        user_cancel.cancel();
        let results = exec.run_to_completion().await.unwrap();
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &results[0]
        else {
            panic!()
        };
        assert!(*is_error);
        assert_eq!(
            content, REJECT_MESSAGE,
            "in-flight Cancel tool must get the bare REJECT_MESSAGE on user interrupt"
        );
        assert!(!content.contains("cancel-tool-ran-to-end"));
    }

    /// discard → StreamingFallback propagation: `discard()` fires `tool_abort`,
    /// whose child reaches an in-flight tool's `ctx.cancel`. The tool returns
    /// early and `drain_one` substitutes the streaming-fallback synthetic onto
    /// its real outcome (the surviving non-user abort path, formerly exercised by
    /// the removed Bash sibling-error cascade tests).
    #[tokio::test]
    async fn discard_substitutes_streaming_fallback_on_in_flight_tool() {
        let orch = orch_with_cancel_and_block_tools();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(ToolUseId::new(), "CancelTool".into(), json!({}), None, a);
        // Start it, then discard the turn while it is in flight.
        exec.process_queue();
        assert_eq!(exec.tools[0].status, ToolStatus::Executing);
        exec.discard();
        let results = exec.run_to_completion().await.unwrap();
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &results[0]
        else {
            panic!()
        };
        assert!(*is_error);
        assert_eq!(
            content,
            "<tool_use_error>Error: Streaming fallback - tool execution discarded</tool_use_error>",
            "in-flight tool must get the streaming-fallback synthetic on discard"
        );
        assert!(!content.contains("cancel-tool-ran-to-end"));
    }

    /// Test 4: has_unfinished returns true when there are Queued/Executing tools,
    /// and false once all tools are Yielded.
    #[tokio::test]
    async fn has_unfinished_tracks_non_yielded_tools() {
        let orch = orch_with_safe_tool();
        let a = MessageId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        // No tools at all → nothing unfinished.
        assert!(
            !exec.has_unfinished(),
            "empty executor must have no unfinished tools"
        );

        exec.add_tool(ToolUseId::new(), "SafeTool".into(), json!({}), None, a);
        assert!(exec.has_unfinished(), "Queued tool means unfinished");

        exec.process_queue();
        assert!(exec.has_unfinished(), "Executing tool still unfinished");

        while !exec.inflight.is_empty() {
            exec.drain_one().await;
        }
        assert!(
            exec.has_unfinished(),
            "Completed but not yet Yielded still unfinished"
        );

        exec.take_newly_completed();
        assert!(!exec.has_unfinished(), "all Yielded → no unfinished tools");
    }
}
