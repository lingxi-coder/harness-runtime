use super::*;

struct NativeEffectsTool {
    modifier_applied: Arc<std::sync::atomic::AtomicBool>,
}

struct SignalAwareTool {
    cwd: std::path::PathBuf,
    received_cancellation: Arc<std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>>,
}

struct AsyncAgentTool {
    agent_id: String,
    observed_provenance:
        Arc<std::sync::Mutex<Vec<lingxi_core::host::subagent_spawn::AgentSpawnProvenance>>>,
}

struct AgentWaitRegistry {
    expected_agent_id: String,
    outcome: lingxi_core::host::task_registry::AgentTerminalWaitOutcome,
    observed_agent_ids: std::sync::Mutex<Vec<String>>,
    expected_timeout: Option<std::time::Duration>,
    wait_gate: Option<Arc<tokio::sync::Notify>>,
    wait_entered: Option<Arc<tokio::sync::Notify>>,
    observed_cancellation: std::sync::Mutex<Vec<tokio_util::sync::CancellationToken>>,
    rows: Vec<lingxi_core::host::task_registry::TaskRecord>,
}

#[derive(Clone, Copy, Default)]
struct WrapperShape {
    underlying_v1_tool_name: Option<&'static str>,
    entry_field_name: Option<&'static str>,
    per_entry_hook_inputs_is_function: bool,
    reassemble_is_function: bool,
}

struct ApiCatalogTool {
    name: &'static str,
    aliases: &'static [&'static str],
    marker: &'static str,
    schema: serde_json::Value,
    wrapper: WrapperShape,
    deny_permission: bool,
    permission_checks: Arc<std::sync::atomic::AtomicUsize>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    observed_inputs: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    observed_permission_messages:
        Arc<std::sync::Mutex<Vec<Vec<lingxi_core::types::ConversationMessage>>>>,
    observed_tool_use_ids: Arc<std::sync::Mutex<Vec<Option<lingxi_core::types::ToolUseId>>>>,
    observed_agent_ids: Arc<std::sync::Mutex<Vec<Option<lingxi_core::types::AgentId>>>>,
    on_schema: Option<Arc<dyn Fn() + Send + Sync>>,
    on_permission: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl ApiCatalogTool {
    fn new(name: &'static str, aliases: &'static [&'static str], marker: &'static str) -> Self {
        Self {
            name,
            aliases,
            marker,
            schema: serde_json::json!({"type":"object"}),
            wrapper: WrapperShape::default(),
            deny_permission: false,
            permission_checks: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            observed_inputs: Arc::new(std::sync::Mutex::new(Vec::new())),
            observed_permission_messages: Arc::new(std::sync::Mutex::new(Vec::new())),
            observed_tool_use_ids: Arc::new(std::sync::Mutex::new(Vec::new())),
            observed_agent_ids: Arc::new(std::sync::Mutex::new(Vec::new())),
            on_schema: None,
            on_permission: None,
        }
    }
}

#[async_trait::async_trait]
impl tool_api::Tool for ApiCatalogTool {
    fn name(&self) -> &str {
        self.name
    }

    fn aliases(&self) -> &[&str] {
        self.aliases
    }

    fn native_mod_tool_batch_wrapper_facts(
        &self,
    ) -> tool_api::tool_trait::NativeModToolBatchWrapperFacts<'_> {
        tool_api::tool_trait::NativeModToolBatchWrapperFacts {
            underlying_v1_tool_name: self.wrapper.underlying_v1_tool_name,
            entry_field_name: self.wrapper.entry_field_name,
            per_entry_hook_inputs_is_function: self.wrapper.per_entry_hook_inputs_is_function,
            reassemble_is_function: self.wrapper.reassemble_is_function,
        }
    }

    fn input_schema(&self) -> &serde_json::Value {
        if let Some(on_schema) = &self.on_schema {
            on_schema();
        }
        &self.schema
    }

    fn is_enabled(&self, _: &tool_api::ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
        false
    }

    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        false
    }

    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        context: &tool_api::ToolUseContext,
    ) -> permission::PermissionResult {
        self.permission_checks
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.observed_permission_messages
            .lock()
            .unwrap()
            .push(context.messages.clone());
        self.observed_tool_use_ids
            .lock()
            .unwrap()
            .push(context.tool_use_id.clone());
        self.observed_agent_ids
            .lock()
            .unwrap()
            .push(context.agent_id);
        if let Some(on_permission) = &self.on_permission {
            on_permission();
        }
        if self.deny_permission {
            return permission::PermissionResult::Deny {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "tool-owned denial".into(),
                },
                explanation: Some(format!("Permission to use {} has been denied.", self.name)),
                metadata: permission::result::PermissionMetadata::default(),
            };
        }
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "tool.call catalog resolver test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &serde_json::Value, _: &tool_api::DescriptionOptions) -> String {
        format!("{} test tool", self.name)
    }

    async fn prompt(&self, _: &tool_api::PromptOptions) -> String {
        format!("{} test tool", self.name)
    }

    async fn call(
        &self,
        input: serde_json::Value,
        _: tool_api::ToolUseContext,
        _: tool_api::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.observed_inputs.lock().unwrap().push(input.clone());
        Ok(tool_api::ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: serde_json::json!({"marker": self.marker, "input": input}),
            model_content: Some(self.marker.to_owned()),
            new_messages: Vec::new(),
            context_modifier: None,
            mcp_meta: None,
            is_error: false,
        })
    }
}

#[async_trait::async_trait]
impl tool_api::Tool for AsyncAgentTool {
    fn name(&self) -> &str {
        "Agent"
    }

    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| serde_json::json!({"type":"object"}));
        &SCHEMA
    }

    fn is_enabled(&self, _: &tool_api::ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
        false
    }

    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        false
    }

    fn interrupt_behavior(&self, _: &serde_json::Value) -> tool_api::InterruptBehavior {
        tool_api::InterruptBehavior::Cancel
    }

    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &tool_api::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "Native Agent projection test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &serde_json::Value, _: &tool_api::DescriptionOptions) -> String {
        "Launch an Agent for the projection test".into()
    }

    async fn prompt(&self, _: &tool_api::PromptOptions) -> String {
        "Agent projection test".into()
    }

    async fn call(
        &self,
        _: serde_json::Value,
        context: tool_api::ToolUseContext,
        _: tool_api::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        self.observed_provenance
            .lock()
            .unwrap()
            .push(context.agent_spawn_provenance);
        Ok(tool_api::ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: serde_json::json!({
                "status": "async_launched",
                "agentId": self.agent_id.clone(),
                "resolvedModel": "model-from-launch",
                "discardedLaunchField": "must not escape",
            }),
            model_content: Some("Agent launched".into()),
            new_messages: Vec::new(),
            context_modifier: None,
            mcp_meta: None,
            is_error: false,
        })
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::task_registry::TaskRegistryHandle for AgentWaitRegistry {
    async fn create(
        &self,
        _: lingxi_core::host::task_registry::TaskCreateInput,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Err(
            lingxi_core::host::task_registry::TaskRegistryError::Internal(
                "unused in Mod Agent projection test".into(),
            ),
        )
    }

    async fn get(
        &self,
        _: &str,
    ) -> Result<
        Option<lingxi_core::host::task_registry::TaskRecord>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Ok(None)
    }

    async fn output(
        &self,
        _: &str,
        _: Option<u64>,
    ) -> Result<
        lingxi_core::host::task_registry::TaskOutputChunk,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Err(
            lingxi_core::host::task_registry::TaskRegistryError::Internal(
                "unused in Mod Agent projection test".into(),
            ),
        )
    }

    async fn wait_for_agent_terminal(
        &self,
        raw_agent_id: &str,
        cancellation: tokio_util::sync::CancellationToken,
        timeout: Option<std::time::Duration>,
    ) -> Result<
        lingxi_core::host::task_registry::AgentTerminalWaitOutcome,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        assert_eq!(raw_agent_id, self.expected_agent_id);
        assert!(!cancellation.is_cancelled());
        assert_eq!(timeout, self.expected_timeout);
        self.observed_agent_ids
            .lock()
            .unwrap()
            .push(raw_agent_id.to_owned());
        self.observed_cancellation
            .lock()
            .unwrap()
            .push(cancellation);
        if let Some(entered) = &self.wait_entered {
            entered.notify_one();
        }
        if let Some(gate) = &self.wait_gate {
            gate.notified().await;
        }
        Ok(self.outcome.clone())
    }

    async fn list(
        &self,
        _: lingxi_core::host::task_registry::TaskListFilter,
    ) -> Result<
        Vec<lingxi_core::host::task_registry::TaskRecord>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Ok(self.rows.clone())
    }

    async fn update(
        &self,
        _: &str,
        _: lingxi_core::host::task_registry::TaskUpdatePatch,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Err(
            lingxi_core::host::task_registry::TaskRegistryError::Internal(
                "unused in Mod Agent projection test".into(),
            ),
        )
    }

    async fn set_status(
        &self,
        _: &str,
        _: &str,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Err(
            lingxi_core::host::task_registry::TaskRegistryError::Internal(
                "unused in Mod Agent projection test".into(),
            ),
        )
    }

    async fn kill(
        &self,
        _: &str,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Err(
            lingxi_core::host::task_registry::TaskRegistryError::Internal(
                "unused in Mod Agent projection test".into(),
            ),
        )
    }
}

#[async_trait::async_trait]
impl tool_api::Tool for SignalAwareTool {
    fn name(&self) -> &str {
        "Slow"
    }

    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| {
                serde_json::json!({
                    "type":"object",
                    "properties":{"path":{"type":"string"}},
                    "required":["path"]
                })
            });
        &SCHEMA
    }

    fn is_enabled(&self, _: &tool_api::ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
        false
    }

    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        false
    }

    fn interrupt_behavior(&self, _: &serde_json::Value) -> tool_api::InterruptBehavior {
        tool_api::InterruptBehavior::Cancel
    }

    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &tool_api::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "Cancellation test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &serde_json::Value, _: &tool_api::DescriptionOptions) -> String {
        "Waits for caller cancellation".into()
    }

    async fn prompt(&self, _: &tool_api::PromptOptions) -> String {
        "Cancellation test".into()
    }

    async fn call(
        &self,
        input: serde_json::Value,
        context: tool_api::ToolUseContext,
        _: tool_api::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        let marker = input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| tool_api::ToolError::InvalidInput("path required".into()))?;
        let cancellation = context
            .cancel
            .ok_or_else(|| tool_api::ToolError::Internal("cancel token missing".into()))?;
        *self.received_cancellation.lock().unwrap() = Some(cancellation.clone());
        std::fs::write(self.cwd.join(marker), "started")
            .map_err(|error| tool_api::ToolError::Io(error.to_string()))?;
        cancellation.cancelled().await;
        Err(tool_api::ToolError::Aborted)
    }
}

#[async_trait::async_trait]
impl tool_api::Tool for NativeEffectsTool {
    fn name(&self) -> &str {
        "Effects"
    }

    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| serde_json::json!({"type":"object"}));
        &SCHEMA
    }

    fn is_enabled(&self, _: &tool_api::ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
        false
    }

    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        false
    }

    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &tool_api::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "Mod API effect test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &serde_json::Value, _: &tool_api::DescriptionOptions) -> String {
        "Returns a raw payload and deferred effects".into()
    }

    async fn prompt(&self, _: &tool_api::PromptOptions) -> String {
        "Effect test".into()
    }

    fn result_ends_turn(&self, result: &tool_api::ToolCallResult) -> bool {
        !result.is_error
    }

    async fn call(
        &self,
        input: serde_json::Value,
        _: tool_api::ToolUseContext,
        _: tool_api::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        let modifier_applied = self.modifier_applied.clone();
        let injected = lingxi_core::types::ConversationMessage::user_meta(
            lingxi_core::types::MessageId::new(),
            "inner virtual message".into(),
        );
        Ok(tool_api::ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: serde_json::json!({"raw": "structured", "logical_error": input["logical_error"]}),
            model_content: Some(if input["logical_error"] == true {
                "logical error text".into()
            } else {
                "model-facing text".into()
            }),
            new_messages: vec![injected],
            context_modifier: Some(Box::new(move |context| {
                modifier_applied.store(true, std::sync::atomic::Ordering::SeqCst);
                context
            })),
            mcp_meta: Some(serde_json::json!({"server": "inner"})),
            is_error: input["logical_error"] == true,
        })
    }
}

fn permission_gate(
    tool_name: &str,
    behavior: permission::PermissionBehavior,
) -> Arc<dyn permission::PermissionGate> {
    Arc::new(permission::PolicyPermissionGate::new(
        Arc::new(permission::PermissionPolicy::from_rules(
            permission::PermissionMode::Default,
            vec![permission::PermissionRule {
                value: permission::PermissionRuleValue::from_rule_string(tool_name),
                behavior,
                source: permission::PermissionRuleSource::Session,
            }],
        )),
        Arc::new(super::NoOpPermissionGate),
    ))
}

fn mod_tool_call_module(dir: &std::path::Path, source: &str) -> std::path::PathBuf {
    let module = dir.join("register.js");
    std::fs::write(&module, source).expect("write Mod module");
    module
}

#[tokio::test]
async fn mod_agent_api_launch_returns_before_fresh_infinite_settlement() {
    use lingxi_core::host::task_registry::{AgentTerminalSnapshot, AgentTerminalWaitOutcome};
    const AGENT_ID: &str = "raw-launched-agent";
    let dir = tempfile::tempdir().unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    let module = mod_tool_call_module(
        dir.path(),
        r#"
        export function register(on) {
          on('tool.call', {tool:'Caller'}, async $ => {
            const rows = await $.agent.list();
            if (!Array.isArray(rows) || rows.length !== 0) throw new Error('list shape');
            const child = await $.agent.spawn({prompt:'run this child'});
            if (child.agentId !== 'raw-launched-agent' || child.model !== 'model-from-launch') {
              throw new Error('launch projection');
            }
            return {result:child,text:'launch returned'};
          });
        }
    "#,
    );
    host.load("direct-agent", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let registry = Arc::new(AgentWaitRegistry {
        expected_agent_id: AGENT_ID.into(),
        outcome: AgentTerminalWaitOutcome::Completed(AgentTerminalSnapshot {
            task_id: "launched-task".into(),
            native_transcript_text: "child answer".into(),
            error: None,
        }),
        observed_agent_ids: std::sync::Mutex::new(Vec::new()),
        expected_timeout: None,
        wait_gate: Some(gate.clone()),
        wait_entered: Some(entered.clone()),
        observed_cancellation: std::sync::Mutex::new(Vec::new()),
        rows: Vec::new(),
    });
    let provenance = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(ApiCatalogTool::new("Caller", &[], "caller")));
    tools.register_builtin(Arc::new(AsyncAgentTool {
        agent_id: AGENT_ID.into(),
        observed_provenance: provenance.clone(),
    }));
    let output = Arc::new(crate::test_support::MockOutputStream::new());
    let orch = orchestrator_with_catalog(
        dir.path().to_path_buf(),
        Arc::new(tools),
        allow_tools_gate(&["Caller", "Agent"]),
        output,
        host.clone(),
        OrchestratorConfig::default(),
    )
    .with_task_registry(registry.clone())
    .with_mod_agent_name_registry(Arc::new(
        lingxi_core::host::agent_name_registry::InMemoryAgentNameRegistry::new(),
    ));
    let dispatched = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::turn_loop::dispatch_tool_uses_tracked_deferred(
            &orch,
            &[(
                ToolUseId::new(),
                "Caller".into(),
                serde_json::json!({}),
                None,
            )],
            None,
            None,
        ),
    )
    .await
    .expect("direct spawn must not wait for child termination")
    .unwrap();
    let (text, is_error) = super::schema_gate_tool_result(&dispatched.results[0]);
    assert!(!is_error, "{text}");
    // tool.call middleware maps its raw result to model content. Its `text`
    // field describes a core result and is not a replacement text channel.
    assert_eq!(
        text,
        r#"{"model":"model-from-launch","agentId":"raw-launched-agent"}"#
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let tokens = registry.observed_cancellation.lock().unwrap().clone();
    assert_eq!(tokens.len(), 1);
    assert!(
        !tokens[0].is_cancelled(),
        "API completion must not cancel BOt"
    );
    {
        let observed = provenance.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(
            observed[0].hook_caller,
            lingxi_core::host::task_registry::FieldPresence::Value(serde_json::json!(
                "direct-agent"
            ))
        );
    }
    drop(orch);
    host.unload("direct-agent").await.unwrap();
    assert!(
        !tokens[0].is_cancelled(),
        "detached settlement uses its own signal"
    );
    gate.notify_one();
    tokio::task::yield_now().await;
}

fn orchestrator_with_host(
    cwd: std::path::PathBuf,
    tool: Arc<dyn tool_api::Tool>,
    gate: Arc<dyn permission::PermissionGate>,
    output: Arc<crate::test_support::MockOutputStream>,
    host: Arc<hooks::mods::ModHost>,
) -> ConversationOrchestrator {
    let mut tools = ToolRegistry::new();
    tools.register_builtin(tool);
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(crate::test_support::MockApiClient::new(vec![])),
        Arc::new(tools),
        crate::test_support::noop_hook_executor(),
        gate,
        output,
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        cwd,
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)))
}

fn orchestrator_with_catalog(
    cwd: std::path::PathBuf,
    tools: Arc<tool_api::ToolRegistry>,
    gate: Arc<dyn permission::PermissionGate>,
    output: Arc<crate::test_support::MockOutputStream>,
    host: Arc<hooks::mods::ModHost>,
    config: OrchestratorConfig,
) -> ConversationOrchestrator {
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    ConversationOrchestrator::new(
        config,
        Arc::new(crate::test_support::MockApiClient::new(vec![])),
        tools,
        crate::test_support::noop_hook_executor(),
        gate,
        output,
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        cwd,
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)))
}

fn allow_tools_gate(names: &[&str]) -> Arc<dyn permission::PermissionGate> {
    Arc::new(permission::PolicyPermissionGate::new(
        Arc::new(permission::PermissionPolicy::from_rules(
            permission::PermissionMode::Default,
            names
                .iter()
                .map(|name| permission::PermissionRule {
                    value: permission::PermissionRuleValue::from_rule_string(name),
                    behavior: permission::PermissionBehavior::Allow,
                    source: permission::PermissionRuleSource::Session,
                })
                .collect::<Vec<_>>(),
        )),
        Arc::new(super::NoOpPermissionGate),
    ))
}

fn projects_consent_test_config() -> OrchestratorConfig {
    let mut config = OrchestratorConfig::default();
    config.mod_projects_consent = hooks::mods::ProjectsConsentFacts {
        default_host_sticky_latch: Some(true),
        projects_env: Some(false),
        session_mcp_signal: Some(false),
        feature_result: Some(hooks::mods::ProjectsFeatureResult {
            value: false,
            source: hooks::mods::ProjectsFeatureSource::Fallback,
        }),
        growthbook_used_non_default_host: Some(false),
    };
    config
}

async fn dispatch_preflight_case(
    orch: &ConversationOrchestrator,
    target: &str,
    consent: Option<serde_json::Value>,
) -> serde_json::Value {
    let outer_id = lingxi_core::types::ToolUseId::new();
    let mut input = serde_json::json!({"target": target});
    if let Some(consent) = consent {
        input["consent"] = consent;
        input["hasConsent"] = serde_json::Value::Bool(true);
    }
    let dispatched = crate::turn_loop::dispatch_tool_uses_tracked_deferred(
        orch,
        &[(outer_id.clone(), "Caller".into(), input, None)],
        None,
        None,
    )
    .await
    .expect("dispatch Mod preflight test");
    assert_eq!(dispatched.results.len(), 1);
    orch.transcript
        .tool_use_results
        .lock()
        .await
        .get(outer_id.as_str())
        .cloned()
        .expect("outer result should be recorded")
}

#[tokio::test]
async fn tool_call_api_preflight_matches_native_order_and_mx_catalog_filter() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = hooks::mods::ModHost::start(None).await.expect("Mod host");
    let module = mod_tool_call_module(
        dir.path(),
        r#"
        export function register(on) {
          on('tool.call', { tool: 'Caller' }, async ($, event) => {
            const input = { tool: event.target };
            if (Object.prototype.hasOwnProperty.call(event, 'consent')) input.consent = event.consent;
            try {
              const answer = await $.tool.call(input);
              return { result: answer.result, text: answer.text };
            } catch (error) {
              const message = error && typeof error.message === 'string' ? error.message : String(error);
              return { result: { error: message }, text: message };
            }
          });
        }
        "#,
    );
    host.load(
        "tool-call-preflight",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .expect("load preflight Mod");

    let caller = Arc::new(ApiCatalogTool::new("Caller", &[], "caller"));
    let web_fetch = Arc::new(ApiCatalogTool::new(
        "WebFetch",
        &["fetchAlias"],
        "web-fetch",
    ));
    let mut wrapped = ApiCatalogTool::new("BatchWrapped", &["wrappedAlias"], "wrapped");
    wrapped.wrapper = WrapperShape {
        underlying_v1_tool_name: Some("Read"),
        entry_field_name: Some("entries"),
        per_entry_hook_inputs_is_function: true,
        reassemble_is_function: true,
    };
    let wrapped = Arc::new(wrapped);
    let mut underlying_only =
        ApiCatalogTool::new("UnderlyingOnly", &["underlyingAlias"], "underlying");
    underlying_only.wrapper.underlying_v1_tool_name = Some("Read");
    let underlying_only = Arc::new(underlying_only);
    let mut partial = ApiCatalogTool::new("PartialWrapped", &["partialAlias"], "partial");
    partial.wrapper = WrapperShape {
        underlying_v1_tool_name: Some("Read"),
        entry_field_name: Some("entries"),
        per_entry_hook_inputs_is_function: true,
        reassemble_is_function: false,
    };
    let partial = Arc::new(partial);
    let mut read = ApiCatalogTool::new("Read", &["readAlias"], "read");
    read.wrapper.underlying_v1_tool_name = Some("Read");
    let read = Arc::new(read);

    let mut tools = tool_api::ToolRegistry::new();
    let catalog_tools: Vec<Arc<dyn tool_api::Tool>> = vec![
        caller.clone(),
        web_fetch.clone(),
        wrapped.clone(),
        underlying_only.clone(),
        partial.clone(),
        read.clone(),
    ];
    for tool in catalog_tools {
        tools.register_builtin(tool);
    }
    let gate = allow_tools_gate(&[
        "Caller",
        "WebFetch",
        "BatchWrapped",
        "UnderlyingOnly",
        "PartialWrapped",
        "Read",
    ]);
    let output = Arc::new(crate::test_support::MockOutputStream::new());
    let orch = orchestrator_with_catalog(
        dir.path().to_path_buf(),
        Arc::new(tools),
        gate,
        output,
        host,
        projects_consent_test_config(),
    );

    let malformed_consent =
        dispatch_preflight_case(&orch, "Missing", Some(serde_json::json!(7))).await;
    assert_eq!(
        malformed_consent["error"],
        "tool-call-preflight: $.tool.call: consent, when given, is a string",
        "consent type is checked before resolving the requested tool"
    );

    let unknown_in_projects =
        dispatch_preflight_case(&orch, "Missing", Some(serde_json::json!(""))).await;
    assert_eq!(
        unknown_in_projects["error"],
        "tool-call-preflight: $.tool.call: no tool named \"Missing\" in this session",
        "unknown tool is rejected before querying the Projects consent gate"
    );

    let projects_alias =
        dispatch_preflight_case(&orch, "fetchAlias", Some(serde_json::json!(""))).await;
    assert_eq!(
        projects_alias["error"],
        "tool-call-preflight: $.tool.call: WebFetch does not accept `consent` in a Projects session. Call it without `consent`, and the normal permission check will decide.",
        "Projects consent uses the resolved canonical tool name, even for an empty string"
    );

    let wrapped_alias =
        dispatch_preflight_case(&orch, "wrappedAlias", Some(serde_json::json!(""))).await;
    assert_eq!(
        wrapped_alias["error"],
        "tool-call-preflight: $.tool.call: no tool named \"wrappedAlias\" in this session",
        "the Mx-complete wrapper is hidden before alias resolution"
    );

    let wrong_case_alias = dispatch_preflight_case(&orch, "READALIAS", None).await;
    assert_eq!(
        wrong_case_alias["error"],
        "tool-call-preflight: $.tool.call: no tool named \"READALIAS\" in this session",
        "aliases are matched case-sensitively"
    );

    let underlying_result = dispatch_preflight_case(&orch, "underlyingAlias", None).await;
    assert_eq!(underlying_result["marker"], "underlying");
    let partial_result = dispatch_preflight_case(&orch, "partialAlias", None).await;
    assert_eq!(partial_result["marker"], "partial");

    for tool in [&caller, &web_fetch, &wrapped, &read] {
        assert_eq!(
            tool.permission_checks
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "preflight errors must not enter tool permission checks for {}",
            tool.name
        );
        assert_eq!(
            tool.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "preflight errors must not enter tool bodies for {}",
            tool.name
        );
    }
    assert_eq!(
        underlying_only
            .permission_checks
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "underlyingV1ToolName alone is not the four-fact Mx wrapper marker"
    );
    assert_eq!(
        underlying_only
            .calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        partial
            .permission_checks
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a partial wrapper shape remains in the active catalog"
    );
    assert_eq!(partial.calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Native's caller AbortSignal is composed only after these checks. Exercise
    // the orchestrator boundary with an already-cancelled token to ensure it
    // does not short-circuit catalog or Projects errors. The Hooks-owned API
    // parser separately checks malformed consent before this trait is called.
    let pre_aborted = tokio_util::sync::CancellationToken::new();
    pre_aborted.cancel();
    let unknown = match hooks::mods::ModSessionContext::prepare_tool_call(
        &orch,
        "tool-call-preflight",
        "Missing".into(),
        Some(String::new()),
        pre_aborted.clone(),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("an aborted caller still resolves the catalog before cancellation"),
    };
    assert!(unknown.to_string().contains("no tool named \"Missing\""));
    let projects = match hooks::mods::ModSessionContext::prepare_tool_call(
        &orch,
        "tool-call-preflight",
        "WebFetch".into(),
        Some(String::new()),
        pre_aborted,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("an aborted caller still receives the Native Projects refusal"),
    };
    assert!(projects
        .to_string()
        .contains("WebFetch does not accept `consent` in a Projects session"));
}

#[tokio::test]
async fn tool_call_api_keeps_the_prepared_arc_across_catalog_replacement() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = hooks::mods::ModHost::start(None).await.expect("Mod host");
    let module = mod_tool_call_module(
        dir.path(),
        r#"
        export function register(on) {
          on('tool.call', { tool: 'Caller' }, async ($, event) => {
            const answer = await $.tool.call({
              tool: 'readAlias',
              tool_use_id: 'caller-supplied-id',
              agentId: 'caller-supplied-agent',
              consent: '',
              value: 'before-shadow',
              $shadowed: { value: 'after-shadow' }
            });
            return { result: answer.result, text: answer.text };
          });
          on('tool.call', { tool: 'readAlias' }, async ($, event, next) => {
            await next(event);
            return await next(event);
          });
        }
        "#,
    );
    host.load(
        "tool-call-prepared-handle",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .expect("load prepared-handle Mod");

    let mut tool_registry = tool_api::ToolRegistry::new();
    let caller = Arc::new(ApiCatalogTool::new("Caller", &[], "caller"));
    tool_registry.register_builtin(caller.clone());
    let tools = Arc::new(tool_registry);
    let connection_id = lingxi_core::types::McpConnectionId::new();
    let mut replacement = ApiCatalogTool::new("Read", &["readAlias"], "replacement");
    replacement.schema = serde_json::json!({
        "type":"object",
        "properties":{"replacement_only":{"type":"string"}},
        "required":["replacement_only"]
    });
    let replacement = Arc::new(replacement);
    let registry_to_replace = tools.clone();
    let replacement_for_swap = replacement.clone();
    let connection_for_swap = connection_id;
    let mut original = ApiCatalogTool::new("Read", &["readAlias"], "original");
    let swapped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let swapped_for_schema = swapped.clone();
    original.on_schema = Some(Arc::new(move || {
        if swapped_for_schema.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let replacement: Arc<dyn tool_api::Tool> = replacement_for_swap.clone();
        registry_to_replace.register_mcp_tools(connection_for_swap, vec![replacement]);
    }));
    let original = Arc::new(original);
    let original_dyn: Arc<dyn tool_api::Tool> = original.clone();
    tools.register_mcp_tools(connection_id, vec![original_dyn]);

    let orch = orchestrator_with_catalog(
        dir.path().to_path_buf(),
        tools.clone(),
        allow_tools_gate(&["Caller", "Read"]),
        Arc::new(crate::test_support::MockOutputStream::new()),
        host,
        OrchestratorConfig::default(),
    );
    let outer_id = lingxi_core::types::ToolUseId::new();
    let dispatched = crate::turn_loop::dispatch_tool_uses_tracked_deferred(
        &orch,
        &[(
            outer_id.clone(),
            "Caller".into(),
            serde_json::Value::Object(serde_json::Map::new()),
            None,
        )],
        None,
        None,
    )
    .await
    .expect("dispatch alias call");

    assert_eq!(dispatched.results.len(), 1);
    let recorded = orch
        .transcript
        .tool_use_results
        .lock()
        .await
        .get(outer_id.as_str())
        .cloned()
        .expect("outer result should be recorded");
    assert_eq!(recorded["marker"], "original");
    assert_eq!(
        recorded["input"],
        serde_json::json!({"value":"after-shadow"})
    );
    assert!(swapped.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        original
            .permission_checks
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "both middleware next calls use the preflight Arc after schema lookup replaces the live catalog"
    );
    assert_eq!(original.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        replacement
            .permission_checks
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        replacement.calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        original.observed_inputs.lock().unwrap().as_slice(),
        &[
            serde_json::json!({"value":"after-shadow"}),
            serde_json::json!({"value":"after-shadow"})
        ],
        "$shadowed overwrites ordinary arguments after top-level reserved fields are removed"
    );
    let virtual_ids = original
        .observed_tool_use_ids
        .lock()
        .unwrap()
        .iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(virtual_ids.len(), 2);
    assert!(virtual_ids
        .iter()
        .all(|tool_use_id| tool_use_id.as_str().starts_with("toolu_plugin_")));
    assert_eq!(
        virtual_ids[0], virtual_ids[1],
        "Native mints one virtual tool id per API call and reuses it across next runs"
    );
    assert_ne!(
        virtual_ids[0].as_str(),
        "caller-supplied-id",
        "caller-provided tool_use_id must not replace the Native virtual id"
    );
    assert!(
        original
            .observed_agent_ids
            .lock()
            .unwrap()
            .iter()
            .all(Option::is_none),
        "caller-provided agentId must not become host agent identity"
    );
    assert!(original
        .observed_permission_messages
        .lock()
        .unwrap()
        .iter()
        .any(|messages| messages.iter().any(|message| matches!(
            message,
            lingxi_core::types::ConversationMessage::User { content, .. }
                if content.iter().any(|block| matches!(
                    block,
                    lingxi_core::types::ContentBlock::Text { text, .. } if text.is_empty()
                ))
        ))), "an explicitly supplied empty consent remains Some(\"\") and is passed to the permission context");
}

#[tokio::test]
async fn tool_call_api_projects_agent_terminal_results_from_native_transcript_facts() {
    use lingxi_core::host::task_registry::{
        AgentTerminalSnapshot, AgentTerminalWaitOutcome, AgentTerminalWaitReason,
    };

    const AGENT_ID: &str = "agent/raw-id";
    let dir = tempfile::tempdir().expect("temp dir");
    let host = hooks::mods::ModHost::start(None).await.expect("Mod host");
    let module = mod_tool_call_module(
        dir.path(),
        r#"
        export function register(on) {
          on('tool.call', { tool: 'Agent' }, async ($, event) => {
            const answer = await $.tool.call({ tool: 'Agent', prompt: 'run the child' });
            if (event.expectResult) {
              if (!answer.result || answer.result.agentId !== event.expectedAgentId) {
                throw new Error('Agent result identity was not projected');
              }
              if (answer.result.resolvedModel !== 'model-from-launch') {
                throw new Error('resolvedModel was not projected');
              }
              if (Object.keys(answer.result).sort().join(',') !== 'agentId,resolvedModel') {
                throw new Error('launch-only fields leaked into the Agent result');
              }
            } else if (!Object.prototype.hasOwnProperty.call(answer, 'result') || answer.result !== undefined) {
              throw new Error('Native FAt result must be an own property with undefined value');
            } else if (Object.keys(answer).join(',') !== 'result,text,isError') {
              throw new Error(`Native FAt property order changed: ${Object.keys(answer).join(',')}`);
            }
            if (answer.text !== event.expectedText || Boolean(answer.isError) !== event.expectedError) {
              throw new Error(`unexpected Agent projection: ${JSON.stringify(answer)}`);
            }
            if (event.expectedError) {
              // The direct API's FAt shape has no result. A tool.call event
              // middleware still needs to return its own valid result shape,
              // so this handler deliberately consumes the FAt value here.
              return { result: answer.text, text: answer.text };
            }
            return answer;
          });
        }
        "#,
    );
    host.load(
        "tool-call-agent-projection",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .expect("load Mod");

    let cases = [
        (
            "completed keeps whitespace and only LAt result keys",
            AgentTerminalWaitOutcome::Completed(AgentTerminalSnapshot {
                task_id: "task-completed".into(),
                native_transcript_text: "  final answer\n".into(),
                error: None,
            }),
            "  final answer\n",
            false,
            true,
        ),
        (
            "failed prefers even an empty task error over transcript",
            AgentTerminalWaitOutcome::Failed(AgentTerminalSnapshot {
                task_id: "task-failed".into(),
                native_transcript_text: "must not be used".into(),
                error: Some(String::new()),
            }),
            "tool-call-agent-projection: $.agent.spawn: the subagent failed",
            true,
            false,
        ),
        (
            "killed uses transcript when no error string exists",
            AgentTerminalWaitOutcome::Killed(AgentTerminalSnapshot {
                task_id: "task-killed".into(),
                native_transcript_text: "last child output".into(),
                error: None,
            }),
            "tool-call-agent-projection: $.agent.spawn: the subagent killed: last child output",
            true,
            false,
        ),
        (
            "startup timeout uses the Native FAt error text",
            AgentTerminalWaitOutcome::Interrupted {
                reason: AgentTerminalWaitReason::StartupTimeout,
                observed_task_id: None,
            },
            "tool-call-agent-projection: $.agent.spawn: no answer within 10 minutes",
            true,
            false,
        ),
        (
            "settle timeout after observing the task has the same Native text",
            AgentTerminalWaitOutcome::Interrupted {
                reason: AgentTerminalWaitReason::SettleTimeout,
                observed_task_id: Some("task-running".into()),
            },
            "tool-call-agent-projection: $.agent.spawn: no answer within 10 minutes",
            true,
            false,
        ),
        (
            "empty evicted transcript uses the Native record error",
            AgentTerminalWaitOutcome::Evicted {
                native_transcript_text: Some(String::new()),
            },
            "tool-call-agent-projection: $.agent.spawn: the subagent's record was evicted before its answer was read",
            true,
            false,
        ),
        (
            "evicted non-empty transcript still projects a successful Agent result",
            AgentTerminalWaitOutcome::Evicted {
                native_transcript_text: Some("last persisted assistant row".into()),
            },
            "last persisted assistant row",
            false,
            true,
        ),
    ];

    for (label, outcome, expected_text, expected_error, expect_result) in cases {
        let registry = Arc::new(AgentWaitRegistry {
            expected_agent_id: AGENT_ID.into(),
            outcome,
            observed_agent_ids: std::sync::Mutex::new(Vec::new()),
            expected_timeout: Some(std::time::Duration::from_secs(10 * 60)),
            wait_gate: None,
            wait_entered: None,
            observed_cancellation: std::sync::Mutex::new(Vec::new()),
            rows: Vec::new(),
        });
        let observed_provenance = Arc::new(std::sync::Mutex::new(Vec::new()));
        let output = Arc::new(crate::test_support::MockOutputStream::new());
        let orch = orchestrator_with_host(
            dir.path().to_path_buf(),
            Arc::new(AsyncAgentTool {
                agent_id: AGENT_ID.into(),
                observed_provenance: observed_provenance.clone(),
            }),
            permission_gate("Agent", permission::PermissionBehavior::Allow),
            output,
            host.clone(),
        )
        .with_task_registry(registry.clone());
        let outer_id = lingxi_core::types::ToolUseId::new();
        let dispatched = crate::turn_loop::dispatch_tool_uses_tracked_deferred(
            &orch,
            &[(
                outer_id,
                "Agent".into(),
                serde_json::json!({
                    "expectedText": expected_text,
                    "expectedError": expected_error,
                    "expectResult": expect_result,
                    "expectedAgentId": AGENT_ID,
                }),
                None,
            )],
            None,
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("{label}: {error}"));

        assert_eq!(dispatched.results.len(), 1, "{label}");
        let (content, is_error) = super::schema_gate_tool_result(&dispatched.results[0]);
        assert!(!is_error, "{label}");
        if expected_error {
            assert_eq!(content, expected_text, "{label}");
        }
        assert_eq!(
            registry.observed_agent_ids.lock().unwrap().as_slice(),
            &[AGENT_ID.to_owned()],
            "Native matches the raw Agent id without UUID parsing ({label})"
        );
        let provenance = observed_provenance.lock().unwrap();
        assert_eq!(provenance.len(), 1, "{label}");
        assert_eq!(
            provenance[0].hook_caller,
            lingxi_core::host::task_registry::FieldPresence::Value(serde_json::json!(
                "tool-call-agent-projection"
            )),
            "hook caller comes from the registered host API, not rewritten input ({label})"
        );
        assert_eq!(
            provenance[0].hook_origin,
            lingxi_core::host::task_registry::FieldPresence::Value(serde_json::json!([
                "tool-call-agent-projection"
            ])),
            "origin is the Native default chain ({label})"
        );
        drop(provenance);
        assert!(orch.session.lock().await.history.is_empty(), "{label}");
    }
}

#[tokio::test]
async fn tool_call_api_uses_permissioned_executor_without_publishing_virtual_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = hooks::mods::ModHost::start(None).await.expect("Mod host");
    let module = mod_tool_call_module(
        dir.path(),
        r#"
        export function register(on) {
          on('tool.call', { tool: 'Schemic' }, async ($, event) => {
            return await $.tool.call({ tool: 'Schemic', path: event.path, consent: event.consent });
          });
        }
        "#,
    );
    host.load("tool-call-api", dir.path(), &module, serde_json::json!({}))
        .await
        .expect("load Mod");

    let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output = Arc::new(crate::test_support::MockOutputStream::new());
    let orch = orchestrator_with_host(
        dir.path().to_path_buf(),
        Arc::new(super::SchemaCallTrackerTool {
            called: called.clone(),
            rejected_message_id: None,
            permission_checks: None,
        }),
        permission_gate("Schemic", permission::PermissionBehavior::Allow),
        output.clone(),
        host,
    );
    let outer_id = lingxi_core::types::ToolUseId::new();
    let dispatched = crate::turn_loop::dispatch_tool_uses_tracked_deferred(
        &orch,
        &[(
            outer_id.clone(),
            "Schemic".into(),
            serde_json::json!({
                "path": "/virtual",
                "consent": "The permission gate must still decide",
            }),
            None,
        )],
        None,
        None,
    )
    .await
    .expect("dispatch Mod-backed call");

    assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(dispatched.results.len(), 1);
    assert_eq!(dispatched.post_tool_batch_calls.len(), 1);
    assert!(dispatched.injected_messages.is_empty());
    assert!(dispatched.context_modifiers.is_empty());
    assert!(!dispatched.prevent_continuation);
    let stored_results = orch.transcript.tool_use_results.lock().await;
    assert_eq!(stored_results.len(), 1, "only the outer row is retained");
    assert_eq!(
        stored_results.get(outer_id.as_str()),
        Some(&serde_json::json!({"ok":true}))
    );
    drop(stored_results);

    let events = output.snapshot().await;
    let call_ids = events
        .iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(call_ids, vec![outer_id.clone()], "inner W1 call is virtual");
    let result_ids = events
        .iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ToolResult { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(result_ids, vec![outer_id]);
    assert!(orch.session.lock().await.history.is_empty());
}

#[tokio::test]
async fn tool_call_api_denial_stays_distinct_and_consent_does_not_bypass_permission() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = hooks::mods::ModHost::start(None).await.expect("Mod host");
    let module = mod_tool_call_module(
        dir.path(),
        r#"
        export function register(on) {
          on('tool.call', { tool: 'Schemic' }, async ($, event) => {
            const answer = await $.tool.call({ tool: 'Schemic', path: event.path, consent: event.consent });
            if (answer.deny !== 'Permission to use Schemic has been denied.') {
              throw new Error('tool.call API did not preserve its plain Native deny value');
            }
            return { result: answer.deny, text: answer.deny };
          });
        }
        "#,
    );
    host.load(
        "tool-call-denied",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .expect("load Mod");

    let mut denied_tool = ApiCatalogTool::new("Schemic", &[], "denied");
    denied_tool.deny_permission = true;
    let denied_tool = Arc::new(denied_tool);
    let output = Arc::new(crate::test_support::MockOutputStream::new());
    let orch = orchestrator_with_host(
        dir.path().to_path_buf(),
        denied_tool.clone(),
        permission_gate("Schemic", permission::PermissionBehavior::Allow),
        output.clone(),
        host,
    );
    let outer_id = lingxi_core::types::ToolUseId::new();
    let dispatched = crate::turn_loop::dispatch_tool_uses_tracked_deferred(
        &orch,
        &[(
            outer_id.clone(),
            "Schemic".into(),
            serde_json::json!({
                "path": "/virtual",
                "consent": "This is context, not permission",
            }),
            None,
        )],
        None,
        None,
    )
    .await
    .expect("dispatch denied Mod-backed call");

    assert_eq!(
        denied_tool
            .permission_checks
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "consent does not skip the tool-owned permission decision"
    );
    assert_eq!(
        denied_tool.calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    let block = dispatched.results.first().expect("denial result");
    let (content, is_error) = super::schema_gate_tool_result(block);
    assert!(!is_error, "the Mod consumes Native's API-level deny value");
    assert_eq!(content, "Permission to use Schemic has been denied.");
    assert_eq!(dispatched.post_tool_batch_calls.len(), 1);
    assert!(dispatched.injected_messages.is_empty());
    assert!(dispatched.context_modifiers.is_empty());

    let events = output.snapshot().await;
    let call_ids = events
        .iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(call_ids, vec![outer_id]);
    assert!(
        orch.permission_denials().await.is_empty(),
        "eU does not attach the main query's permission-denial sink to its virtual W1 call"
    );
}

#[tokio::test]
async fn tool_call_api_projects_only_native_promise_fields_and_drops_virtual_frames() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = hooks::mods::ModHost::start(None).await.expect("Mod host");
    let module = mod_tool_call_module(
        dir.path(),
        r#"
        export function register(on) {
          on('tool.call', { tool: 'Effects' }, async ($, event) => {
            const answer = await $.tool.call({ tool: 'Effects', logical_error: event.logical_error });
            const expectedText = event.logical_error ? 'logical error text' : 'model-facing text';
            if (answer.text !== expectedText || answer.result.raw !== 'structured') {
              throw new Error('raw result and model text were not preserved separately');
            }
            if (Boolean(answer.isError) !== event.logical_error) {
              throw new Error('logical isError flag was lost');
            }
            return { result: answer.result, text: answer.text, ...(answer.isError ? { isError: true } : {}) };
          });
        }
        "#,
    );
    host.load(
        "tool-call-effects",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .expect("load Mod");

    for logical_error in [false, true] {
        let modifier_applied = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let output = Arc::new(crate::test_support::MockOutputStream::new());
        let orch = orchestrator_with_host(
            dir.path().to_path_buf(),
            Arc::new(NativeEffectsTool {
                modifier_applied: modifier_applied.clone(),
            }),
            permission_gate("Effects", permission::PermissionBehavior::Allow),
            output.clone(),
            host.clone(),
        );
        let outer_id = lingxi_core::types::ToolUseId::new();
        let dispatched = crate::turn_loop::dispatch_tool_uses_tracked_deferred(
            &orch,
            &[(
                outer_id.clone(),
                "Effects".into(),
                serde_json::json!({
                    "logical_error": logical_error,
                }),
                None,
            )],
            None,
            None,
        )
        .await
        .expect("dispatch tool.call API");

        assert_eq!(dispatched.results.len(), 1);
        let (content, is_error) = super::schema_gate_tool_result(&dispatched.results[0]);
        assert!(
            content.contains("\"raw\":\"structured\""),
            "outer model text should map from the returned raw data: {content}"
        );
        assert!(
            !is_error,
            "inner $.tool.call Promise isError is not the regular outer DVt error bit"
        );
        assert_eq!(dispatched.post_tool_batch_calls.len(), 1);
        assert!(dispatched.injected_messages.is_empty());
        assert!(dispatched.context_modifiers.is_empty());
        assert!(!dispatched.prevent_continuation);
        assert!(!modifier_applied.load(std::sync::atomic::Ordering::SeqCst));
        assert!(orch.session.lock().await.history.is_empty());
        assert!(orch.transcript.tool_use_mcp_meta.lock().await.is_empty());
        assert!(orch
            .transcript
            .pending_tool_result_turn_end
            .lock()
            .await
            .is_empty());
        let results = orch.transcript.tool_use_results.lock().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results.get(outer_id.as_str()).unwrap()["raw"], "structured");
        drop(results);

        let events = output.snapshot().await;
        let calls = events
            .iter()
            .filter_map(|event| match event {
                lingxi_core::host::OutputEvent::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls, vec![outer_id]);
    }
}

#[tokio::test]
async fn tool_call_context_keeps_exact_utf16_in_reminder_and_queued_attachment() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = hooks::mods::ModHost::start(None).await.expect("Mod host");
    let module = mod_tool_call_module(
        dir.path(),
        r#"
        export function register(on) {
          on('tool.call', { tool: 'Read' }, async ($, event, next) => {
            const result = await next(event);
            return { ...result, context: ['ctx\uD800'] };
          });
        }
        "#,
    );
    host.load(
        "tool-call-utf16-context",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .expect("load Mod");
    let output = Arc::new(crate::test_support::MockOutputStream::new());
    let tool = Arc::new(ApiCatalogTool::new("Read", &[], "read result"));
    let orch = orchestrator_with_host(
        dir.path().to_path_buf(),
        tool,
        permission_gate("Read", permission::PermissionBehavior::Allow),
        output,
        host,
    );
    let tool_use_id = lingxi_core::types::ToolUseId::new();

    let dispatched = crate::turn_loop::dispatch_tool_uses_tracked_deferred(
        &orch,
        &[(
            tool_use_id.clone(),
            "Read".into(),
            serde_json::json!({}),
            None,
        )],
        None,
        None,
    )
    .await
    .expect("dispatch tool.call context");

    assert_eq!(dispatched.injected_messages.len(), 1);
    let (message, _) = &dispatched.injected_messages[0];
    let lingxi_core::types::ConversationMessage::User { content, .. } = message else {
        panic!("tool.call context should be a meta user reminder");
    };
    let lingxi_core::types::ContentBlock::TextJsUtf16 {
        utf16_code_units, ..
    } = &content[0]
    else {
        panic!("lone surrogate should remain typed in the model reminder");
    };
    let mut expected_message = "<system-reminder>\ntool.call hook additional context: ctx"
        .encode_utf16()
        .collect::<Vec<_>>();
    expected_message.push(0xd800);
    expected_message.extend("\n</system-reminder>".encode_utf16());
    assert_eq!(utf16_code_units, &expected_message);

    let queued = orch.take_queued_hook_attachments(&tool_use_id).await;
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0].0.string_units("/content/0"),
        Some(vec![0x0063, 0x0074, 0x0078, 0xd800])
    );
    assert!(queued[0].0.to_json_string().unwrap().contains(r"\ud800"));
}

#[tokio::test]
async fn dropping_outer_dispatch_cancels_virtual_tool_after_it_starts() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = hooks::mods::ModHost::start(None).await.expect("Mod host");
    let module = mod_tool_call_module(
        dir.path(),
        r#"
        export function register(on) {
          on('tool.call', { tool: 'Slow' }, async ($, event) => {
            return await $.tool.call({ tool: 'Slow', path: event.path });
          });
        }
        "#,
    );
    host.load(
        "tool-call-cancel",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .expect("load Mod");

    let received_cancellation = Arc::new(std::sync::Mutex::new(None));
    let output = Arc::new(crate::test_support::MockOutputStream::new());
    let orch = Arc::new(orchestrator_with_host(
        dir.path().to_path_buf(),
        Arc::new(SignalAwareTool {
            cwd: dir.path().to_path_buf(),
            received_cancellation: received_cancellation.clone(),
        }),
        permission_gate("Slow", permission::PermissionBehavior::Allow),
        output.clone(),
        host,
    ));
    let outer_id = lingxi_core::types::ToolUseId::new();
    let dispatch_orch = Arc::clone(&orch);
    let dispatch = tokio::spawn(async move {
        crate::turn_loop::dispatch_tool_uses_tracked_deferred(
            &dispatch_orch,
            &[(
                outer_id,
                "Slow".into(),
                serde_json::json!({"path": "started.marker"}),
                None,
            )],
            None,
            None,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !dir.path().join("started.marker").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("tool body should start before the outer dispatch is dropped");
    dispatch.abort();
    let join_error = match dispatch.await {
        Err(error) => error,
        Ok(_) => panic!("outer dispatch unexpectedly completed before cancellation"),
    };
    assert!(join_error.is_cancelled());
    assert!(dir.path().join("started.marker").exists());
    let received_cancellation = received_cancellation
        .lock()
        .unwrap()
        .clone()
        .expect("tool body captured its cancellation token before writing the start marker");
    assert!(
        received_cancellation.is_cancelled(),
        "dropping the Native outer hook dispatch must cancel the same token received by the running tool"
    );
    assert!(orch.session.lock().await.history.is_empty());
}
