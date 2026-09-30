use crate::conversation::ConversationOrchestrator;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::turn_loop::dispatch_tool_uses_tracked;
use crate::OrchestratorConfig;
use async_trait::async_trait;
use hooks::events::HookEventType;
use lingxi_core::host::permission_gate::{
    PermissionDecision, PermissionGate, PermissionResolution,
};
use lingxi_core::host::tool_invoker::{SubagentInvocationContext, ToolInvoker};
use lingxi_core::types::{ContentBlock, HookId, ToolUseId};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_invoker_impl::RegistryToolInvoker;
use tool_api::tool_trait::{
    CoercedInput, DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};

/// A gate that RESOLVES to a plain allow (the `rule_source` under test) but
/// whose prompt transport always denies — so "the ask reached the prompt" is
/// observable as a deny in the tool_result.
struct PromptSpyGate {
    rule_source: Option<String>,
}

#[tokio::test]
async fn monitor_websocket_classifier_allow_does_not_ask_again() {
    use permission::classifier::{AutoModeClassifierVerdict, LoopPermissionClassifier};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Model(AtomicUsize);
    #[async_trait]
    impl LoopPermissionClassifier for Model {
        async fn classify(
            &self,
            _: &str,
            _: &Value,
            _: &[permission::host_context::HostContextRecord],
            _: &[String],
        ) -> AutoModeClassifierVerdict {
            self.0.fetch_add(1, Ordering::SeqCst);
            AutoModeClassifierVerdict::Allow {
                score: 1.0,
                reason: "Allowed by fast classifier".into(),
            }
        }
    }
    struct NoPrompt;
    #[async_trait]
    impl PermissionGate for NoPrompt {
        async fn check(&self, _: &str, _: &Value) -> PermissionDecision {
            panic!("approved websocket must not ask twice")
        }
    }
    struct Monitor(Arc<AtomicUsize>);
    #[async_trait]
    impl Tool for Monitor {
        fn name(&self) -> &str {
            "Monitor"
        }
        fn input_schema(&self) -> &Value {
            static SCHEMA: once_cell::sync::Lazy<Value> =
                once_cell::sync::Lazy::new(|| json!({"type":"object"}));
            &SCHEMA
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &Value) -> bool {
            true
        }
        fn is_read_only(&self, _: &Value) -> bool {
            false
        }
        async fn validate_input(
            &self,
            _: &Value,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &Value,
            _: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Ask {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "Monitor will open a WebSocket".into(),
                },
                prompt: permission::result::PermissionPrompt {
                    title: "Monitor".into(),
                    message: "Monitor will open a WebSocket".into(),
                    options: vec![],
                },
                pending_classifier_check: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
            String::new()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _: Value,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolCallResult::from_data(json!({"content":"ran"})))
        }
    }
    let classifier = Arc::new(Model(AtomicUsize::new(0)));
    let gate = permission::PolicyPermissionGate::new(
        Arc::new(permission::PermissionPolicy::new(
            permission::PermissionMode::Auto,
        )),
        Arc::new(NoPrompt),
    );
    assert!(gate
        .loop_classifier_handle()
        .set(classifier.clone())
        .is_ok());
    let called = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(Monitor(called.clone())) as Arc<dyn Tool>);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(gate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    let uses = vec![(
        ToolUseId::new(),
        "Monitor".into(),
        json!({"ws":{"url":"wss://events.example.com"}}),
        None,
    )];
    dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .unwrap();
    assert_eq!(classifier.0.load(Ordering::SeqCst), 1);
    assert_eq!(called.load(Ordering::SeqCst), 1);
}

#[async_trait]
impl PermissionGate for PromptSpyGate {
    async fn check(&self, _t: &str, _i: &Value) -> PermissionDecision {
        // Stands in for the interactive prompt. Reaching this proves the
        // tool's ask was routed to the transport instead of being
        // re-authorized (and re-allowed) from the rule layer.
        PermissionDecision::Deny {
            reason: "prompted-and-declined".into(),
        }
    }
    async fn resolve_detailed(&self, _t: &str, _i: &Value) -> PermissionResolution {
        PermissionResolution::Allow {
            rule_source: self.rule_source.clone(),
            classifier_approved: false,
        }
    }
}

struct FixedPermissionRequestHook {
    response: hooks::HookResponse,
}

#[async_trait]
impl hooks::BuiltinHookHandler for FixedPermissionRequestHook {
    async fn handle(
        &self,
        _event: &hooks::HookEvent,
        _ctx: &hooks::HookContext,
    ) -> hooks::HookResult {
        hooks::HookResult {
            outcome: hooks::HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            response: Some(self.response.clone()),
        }
    }

    fn id(&self) -> &str {
        "fixed-permission-request"
    }
}

struct UnusedHookHttp;

#[async_trait]
impl lingxi_core::host::HttpTransport for UnusedHookHttp {
    async fn request(
        &self,
        _req: lingxi_core::types::HttpRequest,
    ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
        Err(lingxi_core::host::HttpError::InvalidRequest(
            "unused".into(),
        ))
    }

    async fn stream_sse(
        &self,
        _req: lingxi_core::types::HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
        Err(lingxi_core::host::HttpError::InvalidRequest(
            "unused".into(),
        ))
    }
}

struct UnusedHookRuntime;

#[async_trait]
impl lingxi_core::host::RuntimeSpawner for UnusedHookRuntime {
    async fn spawn(
        &self,
        _name: &str,
        _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError> {
        Err(lingxi_core::host::RuntimeError::Internal("unused".into()))
    }

    async fn sleep(&self, _duration: std::time::Duration) {}

    async fn cancel(
        &self,
        _handle: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), lingxi_core::host::RuntimeError> {
        Ok(())
    }
}

fn permission_request_hook_executor(response: hooks::HookResponse) -> Arc<hooks::HookExecutorImpl> {
    let hook = hooks::HookDefinition {
        id: HookId::new(),
        name: "fixed-permission-request".into(),
        events: vec![HookEventType::PermissionRequest],
        if_condition: None,
        executor: hooks::HookExecutor::Builtin {
            handler_id: "fixed-permission-request".into(),
        },
        source: hooks::HookSource::Session,
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    };
    let mut registry = hooks::HookRegistry::new();
    registry.register(hook);
    let registry = Arc::new(tokio::sync::RwLock::new(registry));
    let mut executor = hooks::HookExecutorImpl::new(
        registry,
        Arc::new(UnusedHookHttp),
        Arc::new(UnusedHookRuntime),
    );
    executor.register_builtin(Arc::new(FixedPermissionRequestHook { response }));
    Arc::new(executor)
}

/// Injects the Read-side policy denial used by Workflow's scriptPath
/// permission check, while leaving the outer Workflow gate free to allow.
struct ReadDenyGate;

#[async_trait]
impl PermissionGate for ReadDenyGate {
    async fn check(&self, _t: &str, _i: &Value) -> PermissionDecision {
        PermissionDecision::Deny {
            reason: "prompted-and-declined".into(),
        }
    }
    async fn resolve_detailed(&self, _t: &str, _i: &Value) -> PermissionResolution {
        PermissionResolution::Deny {
            reason: "prompted-and-declined".into(),
            source: lingxi_core::host::permission_gate::PermissionDecisionSource::Rule,
            rule_source: Some("userSettings".into()),
            decision_reason_type: Some("rule".into()),
            decision_reason: None,
            behavior_ask: false,
            content_blocks: Vec::new(),
        }
    }
}

/// A tool with a STRICT schema (so an un-coerced alias key is rejected), an
/// optional `coerce_input` twin of Bash's `timeout_ms` rule, and an optional
/// `check_permissions` ask. Records the input `call` actually received.
struct SeamTool {
    coerce: bool,
    ask: bool,
    mcp: bool,
    workflow_read_ask: bool,
    requires_ui: bool,
    seen: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Tool for SeamTool {
    fn name(&self) -> &str {
        "Seam"
    }
    fn input_schema(&self) -> &Value {
        static SCHEMA: once_cell::sync::Lazy<Value> = once_cell::sync::Lazy::new(|| {
            json!({
                "type": "object",
                "properties": { "timeout": { "type": "number" } },
                "additionalProperties": false
            })
        });
        &SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn is_mcp(&self) -> bool {
        self.mcp
    }
    fn requires_user_interaction(&self) -> bool {
        self.requires_ui
    }
    fn max_result_size_chars(&self) -> usize {
        1024 * 1024
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn coerce_input(&self, input: &Value) -> Option<CoercedInput> {
        if !self.coerce {
            return None;
        }
        let obj = input.as_object()?;
        if !obj.contains_key("timeout_ms") || obj.contains_key("timeout") {
            return None;
        }
        let mut out = serde_json::Map::new();
        for (k, v) in obj {
            if k != "timeout_ms" {
                out.insert(k.clone(), v.clone());
            }
        }
        out.insert("timeout".into(), obj["timeout_ms"].clone());
        Some(CoercedInput {
            input: Value::Object(out),
            shape_class: "timeout_ms".into(),
        })
    }
    async fn validate_input(&self, _: &Value, _: &ToolUseContext) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &Value,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        if self.ask {
            return permission::PermissionResult::Ask {
                reason: permission::PermissionDecisionReason::SandboxOverride {
                    reason: permission::result::SandboxOverrideReason::DangerouslyDisableSandbox,
                },
                prompt: permission::result::PermissionPrompt {
                    title: "Seam".into(),
                    message: "Run outside of the sandbox".into(),
                    options: Vec::new(),
                },
                pending_classifier_check: None,
                metadata: permission::result::PermissionMetadata {
                    blocked_path: self
                        .workflow_read_ask
                        .then(|| "/tmp/workflow.js".to_string()),
                    ..permission::result::PermissionMetadata::default()
                },
            };
        }
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "seam".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        input: Value,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.seen.lock().unwrap().push(input);
        Ok(ToolCallResult::from_data(json!({ "content": "ran" })))
    }
}

/// Dispatch `{"timeout_ms": 5000}` at one `SeamTool` configuration and return
/// `(model text of the tool_result, inputs `call` saw)`.
async fn dispatch(tool: SeamTool, rule_source: Option<&str>) -> (String, Vec<Value>) {
    let seen = tool.seen.clone();
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(tool) as Arc<dyn Tool>);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(PromptSpyGate {
            rule_source: rule_source.map(str::to_string),
        }),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    let uses = vec![(
        ToolUseId::new(),
        "Seam".to_string(),
        json!({ "timeout_ms": 5000 }),
        None,
    )];
    let (blocks, ..) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch must succeed");
    let text = match &blocks[0] {
        ContentBlock::ToolResult { content, .. } => content.clone(),
        other => panic!("expected a tool_result, got {other:?}"),
    };
    let inputs = seen.lock().unwrap().clone();
    (text, inputs)
}

/// BASH-18 CALL SITE: `coerce_input` runs BEFORE the JSON-schema gate, and
/// the rewritten input is what `call` receives.
#[tokio::test]
async fn coerce_input_is_applied_before_schema_validation() {
    let (text, inputs) = dispatch(
        SeamTool {
            coerce: true,
            ask: false,
            mcp: false,
            workflow_read_ask: false,
            requires_ui: false,
            seen: Arc::new(Mutex::new(Vec::new())),
        },
        None,
    )
    .await;
    assert!(
        !text.contains("InputValidationError"),
        "the coerced input must clear the strict schema, got: {text}"
    );
    assert_eq!(inputs.len(), 1, "the tool must have run");
    assert_eq!(inputs[0], json!({ "timeout": 5000 }));
}

/// A/B TWIN — the same dispatch with `coerce_input` returning `None` is
/// REJECTED by the strict schema. Without this the test above would pass
/// even if the dispatcher never called the hook (the schema gate would have
/// to be lenient, and it is not).
#[tokio::test]
async fn without_the_hook_the_alias_key_fails_the_schema() {
    let (text, inputs) = dispatch(
        SeamTool {
            coerce: false,
            ask: false,
            mcp: false,
            workflow_read_ask: false,
            requires_ui: false,
            seen: Arc::new(Mutex::new(Vec::new())),
        },
        None,
    )
    .await;
    assert!(
        text.contains("InputValidationError"),
        "an un-coerced alias key must fail the strict schema, got: {text}"
    );
    assert!(inputs.is_empty(), "the tool must not have run");
}

/// BASH-10 CALL SITE: a tool `check_permissions` ASK escalates a NON-RULE
/// allow all the way to the prompt transport.
#[tokio::test]
async fn tool_check_permissions_ask_escalates_a_non_rule_allow() {
    let (text, inputs) = dispatch(
        SeamTool {
            coerce: true,
            ask: true,
            mcp: false,
            workflow_read_ask: false,
            requires_ui: false,
            seen: Arc::new(Mutex::new(Vec::new())),
        },
        None,
    )
    .await;
    assert!(
        text.contains("prompted-and-declined"),
        "the tool's ask must reach the prompt transport, got: {text}"
    );
    assert!(inputs.is_empty(), "a declined prompt must not run the tool");
}

/// Workflow's nested `Read` ASK is tool-owned for outer policy composition,
/// but it is still an ordinary configured permission request. A
/// `PermissionRequest` hook may approve it and rewrite the input; only MCP
/// ceilings and explicit requires-user-interaction asks are excluded from
/// this rescue path.
#[tokio::test]
async fn workflow_read_ask_permission_request_allow_rescues_with_rewrite() {
    let tool = SeamTool {
        coerce: true,
        ask: true,
        mcp: false,
        workflow_read_ask: true,
        requires_ui: false,
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let seen = tool.seen.clone();
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(tool) as Arc<dyn Tool>);
    let hook = hooks::HookResponse {
        decision: Some(hooks::HookDecision::Approve),
        updated_input: Some(json!({ "timeout": 7 })),
        ..hooks::HookResponse::default()
    };
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        permission_request_hook_executor(hook),
        Arc::new(PromptSpyGate { rule_source: None }),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    let uses = vec![(
        ToolUseId::new(),
        "Seam".to_string(),
        json!({ "timeout_ms": 5000 }),
        None,
    )];
    let (blocks, ..) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch must succeed after hook rescue");
    let text = match &blocks[0] {
        ContentBlock::ToolResult { content, .. } => content,
        other => panic!("expected a tool_result, got {other:?}"),
    };
    assert!(!text.contains("prompted-and-declined"), "got: {text}");
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[json!({ "timeout": 7 })],
        "PermissionRequest updatedInput must reach the rescued tool"
    );
}

/// A/B TWIN 1 — the same gate + input with the tool returning `Allow` runs
/// the tool. Proves the deny above came from the HOOK, not from the gate.
#[tokio::test]
async fn tool_check_permissions_allow_leaves_the_resolution_alone() {
    let (text, inputs) = dispatch(
        SeamTool {
            coerce: true,
            ask: false,
            mcp: false,
            workflow_read_ask: false,
            requires_ui: false,
            seen: Arc::new(Mutex::new(Vec::new())),
        },
        None,
    )
    .await;
    assert!(!text.contains("prompted-and-declined"), "got: {text}");
    assert_eq!(inputs.len(), 1, "an allowing hook must not block the tool");
}

/// A/B TWIN 2 — `!XXn(r.decisionReason)`: when the base allow came from a
/// permission RULE the oracle does NOT let the tool escalate, so the tool
/// still runs even though its hook asks.
#[tokio::test]
async fn a_rule_allow_suppresses_the_tool_ask() {
    let (text, inputs) = dispatch(
        SeamTool {
            coerce: true,
            ask: true,
            mcp: false,
            workflow_read_ask: false,
            requires_ui: false,
            seen: Arc::new(Mutex::new(Vec::new())),
        },
        Some("userSettings"),
    )
    .await;
    assert!(!text.contains("prompted-and-declined"), "got: {text}");
    assert_eq!(inputs.len(), 1, "a rule allow must bind over the tool ask");
}

/// MCP tool-owned ASK remains protected even when the outer policy
/// resolution is an explicit allow-rule.  The structured `is_mcp` marker,
/// not a wire-name prefix, selects this protected composition path.
#[tokio::test]
async fn mcp_tool_ask_overrides_explicit_allow_rule() {
    let (text, inputs) = dispatch(
        SeamTool {
            coerce: true,
            ask: true,
            mcp: true,
            workflow_read_ask: false,
            requires_ui: false,
            seen: Arc::new(Mutex::new(Vec::new())),
        },
        Some("userSettings"),
    )
    .await;
    assert!(text.contains("prompted-and-declined"), "got: {text}");
    assert!(inputs.is_empty(), "an MCP ceiling ask must not be bypassed");
}

/// Workflow's `scriptPath` check is a tool-local Read permission.  It must
/// still run when the outer Workflow policy resolves to an explicit allow;
/// otherwise a denied Read would be rescued by the Workflow allow rule.
#[tokio::test]
async fn workflow_script_path_read_deny_overrides_outer_allow_rule() {
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(
        tool_workflow::WorkflowTool::new(None).with_permission_gate(Arc::new(ReadDenyGate)),
    ) as Arc<dyn Tool>);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(PromptSpyGate {
            rule_source: Some("userSettings".into()),
        }),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    let uses = vec![(
        ToolUseId::new(),
        "Workflow".to_string(),
        json!({ "scriptPath": "denied.js" }),
        None,
    )];
    let (results, ..) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch must surface a tool_result deny");
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &results[0]
    else {
        panic!("expected a tool_result from Workflow permission denial");
    };
    assert!(*is_error);
    assert!(content.contains("prompted-and-declined"), "got: {content}");
}

/// The same Workflow Read deny must bind on the subagent invoker.  The
/// production Workflow tool owns the nested Read check; the outer gate's
/// Allow cannot rescue it because tool-local permission is evaluated first.
#[tokio::test]
async fn workflow_script_path_read_deny_overrides_subagent_allow() {
    let inner_gate = Arc::new(ReadDenyGate);
    let workflow = tool_workflow::WorkflowTool::new(None).with_permission_gate(inner_gate);
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(workflow) as Arc<dyn Tool>);
    let invoker =
        RegistryToolInvoker::new(Arc::new(registry)).with_gate(Arc::new(NoOpPermissionGate));
    let ctx = SubagentInvocationContext {
        permission_pause_observer: None,
        parent_agent_id: None,
        origin_session_id: None,
        tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
        agent_name: Some("researcher".into()),
        team_name: Some("alpha".into()),
        is_async: false,
        is_non_interactive_session: false,
        can_show_permission_prompts: true,
        cwd: None,
        tool_use_id: Some("toolu_workflow_subagent".into()),
        assistant_message_id: None,
        depth: 0,
        observer: None,
        parent_model: None,
        parent_model_profile: None,
        mode_override: None,
        request_source: None,
        frozen_command_denies: Vec::new(),
    };
    let error = invoker
        .invoke("Workflow", json!({ "scriptPath": "denied.js" }), ctx)
        .await
        .expect_err("nested Read denial must stop subagent Workflow");
    assert!(
        matches!(error, lingxi_core::host::tool_invoker::ToolInvokerError::Internal(ref reason) if reason == "prompted-and-declined")
    );
}
