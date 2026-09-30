//! Permission hooks fired from the turn loop's tool-dispatch chokepoint
//! (`turn_loop.rs`) around the permission gate.
//!
//! Parity with claude-code (HOOK.3 issues 2/3 — SOURCE-GATED):
//! - `PermissionRequest` (`runPermissionRequestHooksForHeadlessAgent`) fires ONLY
//!   when the gate is ABOUT TO ASK (`resolve_detailed` → `Ask`), BEFORE the prompt;
//!   a hook 'allow' RESCUES the call (resolved via `check_after_hook_allow`), a
//!   hook 'deny' denies. An outright ALLOW, or a rule/mode DENY, is already
//!   resolved → `PermissionRequest` does NOT fire for it.
//! - `PermissionDenied` (`executePermissionDeniedHooks`, `toolExecution.ts:1075`)
//!   fires ONLY on an auto-mode CLASSIFIER deny — NOT on a rule/mode/plan deny.
//!   The auto-mode classifier is wired in this build, so classifier-source denies
//!   can trigger the retry meta path by default.
//!
//! Scenarios:
//! 1. A gate that ALLOWS fires NEITHER event; the tool runs.
//! 2. A CLASSIFIER-source deny fires `PermissionDenied` (and NOT `PermissionRequest`).
//! 3. A rule/mode (non-classifier) deny fires NEITHER event; the tool is denied.
//! 4. A `PreToolUse` hook 'allow' (which bypasses the gate) fires NEITHER event.
//! 5. No permission hooks registered → strict no-ops; the turn completes.
use llm_runtime::ContentBlock as LlmContentBlock;

use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookDecision, HookOutcome, HookResponse, HookResult};
use hooks::HookExecutorImpl;
use lingxi_core::host::permission_gate::{
    PermissionDecision, PermissionDecisionSource, PermissionGate, PermissionResolution,
};
use lingxi_core::host::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{
    ContentBlock, ConversationMessage, HookId, HttpRequest, HttpResponse, ImageSource, ToolUseId,
};
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

// ---- unused HTTP / Runtime stubs (Builtin hooks never touch them) ----
struct UnusedHttp;
#[async_trait]
impl HttpTransport for UnusedHttp {
    async fn request(&self, _req: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
    async fn stream_sse(
        &self,
        _req: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
}
struct UnusedRuntime;
#[async_trait]
impl RuntimeSpawner for UnusedRuntime {
    async fn spawn(
        &self,
        _name: &str,
        _task: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, RuntimeError> {
        Err(RuntimeError::Internal("unused".into()))
    }
    async fn sleep(&self, _d: Duration) {}
    async fn cancel(
        &self,
        _h: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

// ---- Permission gate that always DENIES with a fixed reason (Unspecified
// source — models a rule/mode deny, which must NOT fire PermissionDenied) ----
struct DenyGate {
    reason: &'static str,
}
#[async_trait]
impl PermissionGate for DenyGate {
    async fn check(&self, _name: &str, _input: &serde_json::Value) -> PermissionDecision {
        PermissionDecision::Deny {
            reason: self.reason.into(),
        }
    }
}

// ---- Permission gate whose deny is sourced to the auto-mode CLASSIFIER (the
// only deny source that fires the PermissionDenied hook) ----
struct ClassifierDenyGate {
    reason: &'static str,
}
#[async_trait]
impl PermissionGate for ClassifierDenyGate {
    async fn check(&self, _name: &str, _input: &serde_json::Value) -> PermissionDecision {
        PermissionDecision::Deny {
            reason: self.reason.into(),
        }
    }
    async fn resolve_detailed(
        &self,
        _name: &str,
        _input: &serde_json::Value,
    ) -> PermissionResolution {
        PermissionResolution::Deny {
            reason: self.reason.into(),
            source: PermissionDecisionSource::Classifier,
            rule_source: None,
            decision_reason_type: None,
            decision_reason: None,
            behavior_ask: false,
            content_blocks: Vec::new(),
        }
    }
}

// ---- Gate whose deny is an `ask`-behavior rejection that supplies image
// contentBlocks (`toolExecution.ts:1040`). DORMANT in the real build — no real
// gate produces this — so it exercises the top-level-image deny plumbing. ----
struct AskRejectGate {
    reason: &'static str,
}
#[async_trait]
impl PermissionGate for AskRejectGate {
    async fn check(&self, _name: &str, _input: &serde_json::Value) -> PermissionDecision {
        PermissionDecision::Deny {
            reason: self.reason.into(),
        }
    }
    async fn resolve_detailed(
        &self,
        _name: &str,
        _input: &serde_json::Value,
    ) -> PermissionResolution {
        PermissionResolution::Deny {
            reason: self.reason.into(),
            source: PermissionDecisionSource::Unspecified,
            rule_source: None,
            decision_reason_type: None,
            decision_reason: None,
            behavior_ask: true,
            content_blocks: vec![ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "AQID".into(),
                },
            }],
        }
    }
}

// ---- A `PermissionDenied` hook that returns `{retry: true}` ----
struct PermDeniedRetryHook;
#[async_trait]
impl BuiltinHookHandler for PermDeniedRetryHook {
    fn id(&self) -> &str {
        "perm-denied-retry"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            response: Some(HookResponse {
                retry: Some(true),
                ..Default::default()
            }),
        }
    }
}

// ---- A simple tool that always succeeds ----
struct EchoTool;
#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "Echo"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({"type": "object"}));
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
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
        _ctx: &tool_api::context::ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        "echo".into()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: tool_api::context::ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: json!({ "content": format!("ran with {input}") }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

// ---- Recording hooks ----

#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenRequest {
    tool_name: String,
    tool_input: serde_json::Value,
    reason: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenDenied {
    tool_name: String,
    tool_input: serde_json::Value,
    reason: String,
}

#[derive(Default)]
struct PermLog {
    requests: Vec<SeenRequest>,
    denials: Vec<SeenDenied>,
}

/// A single handler subscribed to BOTH events; the orchestrator routes each
/// event to it via the registry (one builtin definition per event type).
struct PermRecorder {
    log: Arc<Mutex<PermLog>>,
}
#[async_trait]
impl BuiltinHookHandler for PermRecorder {
    fn id(&self) -> &str {
        "record-permission"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        match event {
            HookEvent::PermissionRequest {
                tool_name,
                tool_input,
                reason,
            } => self.log.lock().unwrap().requests.push(SeenRequest {
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                reason: reason.clone(),
            }),
            HookEvent::PermissionDenied {
                tool_name,
                tool_input,
                reason,
                ..
            } => self.log.lock().unwrap().denials.push(SeenDenied {
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                reason: reason.clone(),
            }),
            _ => {}
        }
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: None,
        }
    }
}

/// A `PreToolUse` hook that emits `additionalContext` (the model-facing channel)
/// but makes NO permission decision — so the tool proceeds to the permission
/// gate. claude-code pushes this context to `resultingMessages` in the pre-hook
/// phase (`toolExecution.ts:846`) BEFORE the gate, so it must surface even when
/// the gate later DENIES the tool.
struct PreContextHook;
#[async_trait]
impl BuiltinHookHandler for PreContextHook {
    fn id(&self) -> &str {
        "pre-context"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            response: Some(HookResponse {
                additional_context: Some("DENY-CTX".into()),
                ..Default::default()
            }),
        }
    }
}

/// A `PreToolUse` hook that 'allow's, bypassing the permission gate entirely.
struct PreAllowHook;
#[async_trait]
impl BuiltinHookHandler for PreAllowHook {
    fn id(&self) -> &str {
        "pre-allow"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: Some(HookResponse {
                decision: Some(HookDecision::Approve),
                ..Default::default()
            }),
        }
    }
}

/// A `PreToolUse` hook that returns `permissionDecision:"ask"` (parses to
/// `HookDecision::Ask`) — forcing the interactive prompt even over an allow rule.
struct PreAskHook;
#[async_trait]
impl BuiltinHookHandler for PreAskHook {
    fn id(&self) -> &str {
        "pre-ask"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: Some(HookResponse {
                decision: Some(HookDecision::Ask),
                ..Default::default()
            }),
        }
    }
}

fn builtin_hook(name: &str, handler_id: &str, event_type: HookEventType) -> HookDefinition {
    HookDefinition {
        id: HookId::new(),
        name: name.into(),
        events: vec![event_type],
        if_condition: None,
        executor: DefHookExecutor::Builtin {
            handler_id: handler_id.into(),
        },
        source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    }
}

/// Registry + executor with the permission recorder subscribed to BOTH
/// `PermissionRequest` and `PermissionDenied`.
async fn exec_permission_recorder(log: Arc<Mutex<PermLog>>) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    {
        let mut r = registry.write().await;
        r.register(builtin_hook(
            "req",
            "record-permission",
            HookEventType::PermissionRequest,
        ));
        r.register(builtin_hook(
            "denied",
            "record-permission",
            HookEventType::PermissionDenied,
        ));
    }
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(PermRecorder { log }));
    Arc::new(exec)
}

fn orch_with_gate(
    api: Arc<MockApiClient>,
    hooks: Arc<HookExecutorImpl>,
    tools: ToolRegistry,
    gate: Arc<dyn PermissionGate>,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(tools),
        hooks,
        gate,
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

fn orch_with_gate_config(
    api: Arc<MockApiClient>,
    hooks: Arc<HookExecutorImpl>,
    tools: ToolRegistry,
    gate: Arc<dyn PermissionGate>,
    config: OrchestratorConfig,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        config,
        api,
        Arc::new(tools),
        hooks,
        gate,
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

fn two_turn_api(
    tool_use_id: ToolUseId,
    tool_name: &str,
    input: serde_json::Value,
) -> Arc<MockApiClient> {
    Arc::new(MockApiClient::new(vec![
        mock_message_response(
            vec![LlmContentBlock::ToolCall {
                id: tool_use_id.to_string(),
                name: tool_name.into(),
                input,
            }],
            Some("tool_use"),
        ),
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "done".into(),
                cache_control: None,
            }],
            Some("end_turn"),
        ),
    ]))
}

#[tokio::test]
async fn allow_gate_fires_neither_permission_hook() {
    // An outright ALLOW (NoOp gate → resolve_detailed = Allow) is already
    // resolved: it is NOT an about-to-ask, so PermissionRequest does NOT fire,
    // and it is not a deny, so PermissionDenied does NOT fire. The tool runs.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "Echo", json!({ "x": 1 }));
    let log = Arc::new(Mutex::new(PermLog::default()));
    let hooks = exec_permission_recorder(log.clone()).await;
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool));
    // The default NoOpPermissionGate always allows.
    let orch = orch_with_gate(api, hooks, tools, Arc::new(NoOpPermissionGate));

    let outcome = orch.run_turn("run echo").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let log = log.lock().unwrap();
    assert!(
        log.requests.is_empty(),
        "an allowed tool is not an about-to-ask → no PermissionRequest: {:?}",
        log.requests
    );
    assert!(
        log.denials.is_empty(),
        "an allowed tool must NOT fire PermissionDenied: {:?}",
        log.denials
    );
}

#[tokio::test]
async fn classifier_deny_fires_permission_denied_not_request() {
    // An auto-mode CLASSIFIER deny fires PermissionDenied (claude-code
    // `toolExecution.ts:1075`), carrying the reason — and NOT PermissionRequest
    // (it is a resolved deny, not an about-to-ask).
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "Echo", json!({ "cmd": "rm -rf /" }));
    let log = Arc::new(Mutex::new(PermLog::default()));
    let hooks = exec_permission_recorder(log.clone()).await;
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool));
    let gate = Arc::new(ClassifierDenyGate {
        reason: "classifier blocked it",
    });
    let orch = orch_with_gate(api, hooks, tools, gate);

    let outcome = orch.run_turn("run echo").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let log = log.lock().unwrap();
    assert!(
        log.requests.is_empty(),
        "a resolved deny does NOT fire PermissionRequest: {:?}",
        log.requests
    );
    assert_eq!(
        log.denials.len(),
        1,
        "a classifier deny fires PermissionDenied once: {:?}",
        log.denials
    );
    assert_eq!(log.denials[0].tool_name, "Echo");
    assert_eq!(log.denials[0].tool_input, json!({ "cmd": "rm -rf /" }));
    assert_eq!(log.denials[0].reason, "classifier blocked it");
}

#[tokio::test]
async fn rule_mode_deny_fires_neither_permission_hook() {
    // A rule/mode (non-classifier, Unspecified-source) deny fires NEITHER hook —
    // claude-code does NOT fire PermissionDenied on a rule/mode deny. The tool is
    // still denied and the turn completes.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "Echo", json!({ "cmd": "rm -rf /" }));
    let log = Arc::new(Mutex::new(PermLog::default()));
    let hooks = exec_permission_recorder(log.clone()).await;
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool));
    let gate = Arc::new(DenyGate {
        reason: "policy forbids it",
    });
    let orch = orch_with_gate(api, hooks, tools, gate);

    let outcome = orch.run_turn("run echo").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let log = log.lock().unwrap();
    assert!(
        log.requests.is_empty() && log.denials.is_empty(),
        "a rule/mode deny fires no permission hooks: req={:?} denied={:?}",
        log.requests,
        log.denials
    );
}

#[tokio::test]
async fn pre_hook_allow_bypasses_gate_and_fires_neither() {
    // A PreToolUse hook 'allow' skips the permission gate entirely, so NEITHER
    // PermissionRequest nor PermissionDenied fires — even against a deny-gate.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "Echo", json!({}));
    let log = Arc::new(Mutex::new(PermLog::default()));

    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    {
        let mut r = registry.write().await;
        r.register(builtin_hook("pre", "pre-allow", HookEventType::PreToolUse));
        r.register(builtin_hook(
            "req",
            "record-permission",
            HookEventType::PermissionRequest,
        ));
        r.register(builtin_hook(
            "denied",
            "record-permission",
            HookEventType::PermissionDenied,
        ));
    }
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(PreAllowHook));
    exec.register_builtin(Arc::new(PermRecorder { log: log.clone() }));
    let hooks = Arc::new(exec);

    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool));
    // A deny-gate that would deny IF consulted — it must NOT be consulted.
    let gate = Arc::new(DenyGate {
        reason: "would deny",
    });
    let orch = orch_with_gate(api, hooks, tools, gate);

    let outcome = orch.run_turn("run echo").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let log = log.lock().unwrap();
    assert!(
        log.requests.is_empty() && log.denials.is_empty(),
        "a PreToolUse 'allow' bypasses the gate → no permission events: req={:?} denied={:?}",
        log.requests,
        log.denials
    );
}

#[tokio::test]
async fn pre_hook_ask_forces_prompt_over_allow_rule() {
    // R-D3: a PreToolUse hook `permissionDecision:"ask"` (HookDecision::Ask) forces
    // the about-to-ask path EVEN when the gate would ALLOW (NoOpPermissionGate →
    // resolve_detailed = Allow). Contrast `allow_gate_fires_neither_permission_hook`
    // (same gate, NO ask hook) where PermissionRequest does NOT fire. Here the Allow
    // resolution is upgraded to Ask, so PermissionRequest FIRES before the prompt —
    // claude-code `permissionBehavior="ask"` (azn ~205721920; deny > ask > allow, so
    // it overrides an allow rule but a deny rule / plan mode would still bind).
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "Echo", json!({ "x": 1 }));
    let log = Arc::new(Mutex::new(PermLog::default()));

    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    {
        let mut r = registry.write().await;
        r.register(builtin_hook("pre", "pre-ask", HookEventType::PreToolUse));
        r.register(builtin_hook(
            "req",
            "record-permission",
            HookEventType::PermissionRequest,
        ));
        r.register(builtin_hook(
            "denied",
            "record-permission",
            HookEventType::PermissionDenied,
        ));
    }
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(PreAskHook));
    exec.register_builtin(Arc::new(PermRecorder { log: log.clone() }));
    let hooks = Arc::new(exec);

    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool));
    // NoOpPermissionGate would ALLOW (resolve_detailed = Allow); the hook ask must
    // override it and force the prompt path.
    let orch = orch_with_gate(api, hooks, tools, Arc::new(NoOpPermissionGate));

    let outcome = orch.run_turn("run echo").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let log = log.lock().unwrap();
    assert_eq!(
        log.requests.len(),
        1,
        "a hook 'ask' forces the about-to-ask path even over an allow rule → PermissionRequest fires once: {:?}",
        log.requests
    );
    assert!(
        log.denials.is_empty(),
        "an ask is not a deny → PermissionDenied must not fire: {:?}",
        log.denials
    );
}

#[tokio::test]
async fn pre_tool_additional_context_surfaces_even_when_denied() {
    // HOOK.1 + Fix B (claude.ts `toolExecution.ts:846`): a PreToolUse hook's
    // `additionalContext` is pushed to `resultingMessages` in the PRE-hook phase,
    // BEFORE the permission gate runs. So even when the gate later DENIES the
    // tool, that context must STILL surface — as its own `<system-reminder>`
    // meta user message, NOT folded into the deny error tool_result. This locks
    // the deny-arm emit in `dispatch_tool_uses_tracked`.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "Echo", json!({ "x": 1 }));
    let log = Arc::new(Mutex::new(PermLog::default()));

    // Register the additionalContext-emitting PreToolUse hook PLUS the permission
    // recorders (so we can also confirm a rule/mode deny fires no permission hook).
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    {
        let mut r = registry.write().await;
        r.register(builtin_hook(
            "pre-ctx",
            "pre-context",
            HookEventType::PreToolUse,
        ));
        r.register(builtin_hook(
            "req",
            "record-permission",
            HookEventType::PermissionRequest,
        ));
        r.register(builtin_hook(
            "denied",
            "record-permission",
            HookEventType::PermissionDenied,
        ));
    }
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(PreContextHook));
    exec.register_builtin(Arc::new(PermRecorder { log: log.clone() }));
    let hooks = Arc::new(exec);

    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool));
    // A rule/mode (Unspecified-source) deny — denies the tool but fires no
    // PermissionDenied hook (so the additionalContext is the ONLY injected msg).
    let gate = Arc::new(DenyGate {
        reason: "policy forbids it",
    });
    let orch = orch_with_gate(api, hooks, tools, gate);

    let outcome = orch.run_turn("run echo").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let session = orch.session();
    let s = session.lock().await;

    // (a) the deny error tool_result is present and carries the deny reason —
    //     and the additionalContext is NOT folded into it.
    let mut saw_deny_result = false;
    for m in &s.history {
        if let ConversationMessage::User { content, .. } = m {
            for b in content {
                if let ContentBlock::ToolResult {
                    content,
                    is_error,
                    tool_use_id: tu,
                    ..
                } = b
                {
                    if *tu == tool_use_id {
                        saw_deny_result = true;
                        assert!(*is_error, "the deny tool_result must be is_error");
                        // claude-code sends the deny reason VERBATIM as the
                        // tool_result content (no "Permission denied: " wrapper).
                        assert!(
                            content.contains("policy forbids it"),
                            "deny tool_result carries the deny reason verbatim: {content:?}"
                        );
                        assert!(
                            !content.contains("DENY-CTX"),
                            "additionalContext must NOT be folded into the deny tool_result: {content:?}"
                        );
                    }
                }
            }
        }
    }
    assert!(saw_deny_result, "a deny error tool_result must be present");

    // (b) the additionalContext surfaces as a SEPARATE meta user message, with
    //     the exact `<system-reminder>` wrap used elsewhere, sitting DIRECTLY
    //     after the tool_result user message (post-hoist order).
    let ctx_text =
        "<system-reminder>\nPreToolUse:Echo hook additional context: DENY-CTX\n</system-reminder>";
    let ctx_pos = s
        .history
        .iter()
        .position(|m| {
            matches!(m, ConversationMessage::User { content, .. }
                if content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == ctx_text)))
        })
        .expect("standalone additionalContext message present even though the tool was denied");

    let before = &s.history[ctx_pos - 1];
    match before {
        ConversationMessage::User { content, .. } => assert!(
            content.iter().any(
                |b| matches!(b, ContentBlock::ToolResult { tool_use_id: tu, .. } if *tu == tool_use_id)
            ),
            "the context message must sit directly after the deny tool_result message"
        ),
        other => panic!("expected a tool_result User message before the context one, got {other:?}"),
    }

    // (c) the context message is tagged with the dispatching tool's tool_use_id.
    let ctx_id = s.history[ctx_pos].id();
    assert_eq!(
        s.injected_message_sources.get(&ctx_id),
        Some(&tool_use_id),
        "context message id maps to the dispatching tool's tool_use_id"
    );

    // (d) a rule/mode deny fires NEITHER permission hook (sanity: the only
    //     injected message is the additionalContext, not a hook side-effect).
    let log = log.lock().unwrap();
    assert!(
        log.requests.is_empty() && log.denials.is_empty(),
        "a rule/mode deny fires no permission hooks: req={:?} denied={:?}",
        log.requests,
        log.denials
    );
}

#[tokio::test]
async fn no_permission_hooks_registered_is_noop() {
    // No PermissionRequest/Denied hooks: a deny-gate still produces the error
    // result and the turn completes (byte-identical to before the fires).
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "Echo", json!({}));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    let hooks = Arc::new(HookExecutorImpl::new(
        registry,
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    ));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool));
    let gate = Arc::new(DenyGate { reason: "nope" });
    let orch = orch_with_gate(api.clone(), hooks, tools, gate);

    let outcome = orch.run_turn("run echo").await.expect("turn ok");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "no-hook deny case must still complete the turn: {outcome:?}"
    );
    assert_eq!(
        api.captured_msgs().await.len(),
        2,
        "the loop still reaches the terminating turn (2 API calls)"
    );
}

/// Helper: find the User message that carries the deny tool_result for `tu` and
/// return its content blocks.
fn deny_result_blocks<'a>(
    history: &'a [ConversationMessage],
    tu: &ToolUseId,
) -> Option<&'a Vec<ContentBlock>> {
    history.iter().find_map(|m| {
        match m {
        ConversationMessage::User { content, .. }
            if content.iter().any(|b| matches!(
                b,
                ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == tu
            )) =>
        {
            Some(content)
        }
        _ => None,
    }
    })
}

#[tokio::test]
async fn ask_behavior_deny_appends_image_blocks_at_top_level() {
    // An `ask`-behavior rejection that supplies contentBlocks
    // (`toolExecution.ts:1039-1046`): the deny user message is
    // [text tool_result(is_error), image] — the image rides at the TOP LEVEL of
    // the user message, alongside (NOT inside) the tool_result.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "Echo", json!({ "x": 1 }));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    let hooks = Arc::new(HookExecutorImpl::new(
        registry,
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    ));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool));
    let gate = Arc::new(AskRejectGate {
        reason: "declined at prompt",
    });
    let orch = orch_with_gate(api, hooks, tools, gate);

    let outcome = orch.run_turn("run echo").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let session = orch.session();
    let s = session.lock().await;
    let content = deny_result_blocks(&s.history, &tool_use_id)
        .expect("a deny tool_result user message must be present");

    // First block: the text-only tool_result carrying the deny reason.
    match &content[0] {
        ContentBlock::ToolResult {
            content: c,
            is_error,
            tool_use_id: tu,
            ..
        } => {
            assert!(*is_error);
            assert_eq!(tu, &tool_use_id);
            // Deny reason reaches the model VERBATIM (no "Permission denied: " wrapper).
            assert!(
                c.contains("declined at prompt"),
                "tool_result carries the deny reason verbatim: {c:?}"
            );
        }
        other => panic!("expected tool_result first, got {other:?}"),
    }
    // Second block: the image, at the TOP LEVEL (not nested in the tool_result).
    assert!(
        matches!(
            &content[1],
            ContentBlock::Image { source: ImageSource::Base64 { media_type, data } }
                if media_type == "image/png" && data == "AQID"
        ),
        "the ask-rejection image rides at top level: {:?}",
        content.get(1)
    );
    assert_eq!(
        content.len(),
        2,
        "exactly [tool_result, image]: {content:?}"
    );
}

#[tokio::test]
async fn normal_deny_is_plain_text_tool_result_only() {
    // REGRESSION LOCK on the common path: a normal rule/mode deny (no
    // contentBlocks) produces ONLY the plain text tool_result — no top-level
    // image block, byte-identical to before this change.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "Echo", json!({ "x": 1 }));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    let hooks = Arc::new(HookExecutorImpl::new(
        registry,
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    ));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool));
    let gate = Arc::new(DenyGate {
        reason: "policy forbids it",
    });
    let orch = orch_with_gate(api, hooks, tools, gate);

    let outcome = orch.run_turn("run echo").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let session = orch.session();
    let s = session.lock().await;
    let content = deny_result_blocks(&s.history, &tool_use_id)
        .expect("a deny tool_result user message must be present");
    assert_eq!(
        content.len(),
        1,
        "a normal deny is the plain text tool_result ONLY (no image blocks): {content:?}"
    );
    assert!(
        matches!(&content[0], ContentBlock::ToolResult { is_error: true, .. }),
        "the sole block is the is_error tool_result: {content:?}"
    );
}

/// Build an executor with the classifier-deny gate's `PermissionDenied` hook
/// wired to the retrying builtin.
async fn exec_with_retry_hook() -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    {
        let mut r = registry.write().await;
        r.register(builtin_hook(
            "denied-retry",
            "perm-denied-retry",
            HookEventType::PermissionDenied,
        ));
    }
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(PermDeniedRetryHook));
    Arc::new(exec)
}

fn retry_meta_present(history: &[ConversationMessage]) -> bool {
    let verbatim = "The PermissionDenied hook indicated you may retry this tool call.";
    // (parity 2.1.207 P2-05) the retry message MUST be a META user message
    // (`createUserMessage({…, isMeta:!0})`), so require `is_meta: true` here.
    history.iter().any(|m| {
        matches!(m, ConversationMessage::User { content, is_meta: true, .. }
            if content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == verbatim)))
    })
}

#[tokio::test]
async fn permission_denied_retry_pushes_meta_when_classifier_gate_forced_on() {
    // Double-gated retry path (`toolExecution.ts:1075-1101`): with the
    // `TRANSCRIPT_CLASSIFIER` feature forced ON (config bit) AND a
    // classifier-source deny whose PermissionDenied hook returns {retry:true},
    // the verbatim isMeta retry message is pushed AFTER the deny result.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "Echo", json!({ "cmd": "rm -rf /" }));
    let hooks = exec_with_retry_hook().await;
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool));
    let gate = Arc::new(ClassifierDenyGate {
        reason: "classifier blocked it",
    });
    let config = OrchestratorConfig {
        transcript_classifier_enabled: true,
        ..Default::default()
    };
    let orch = orch_with_gate_config(api, hooks, tools, gate, config);

    let outcome = orch.run_turn("run echo").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let session = orch.session();
    let s = session.lock().await;
    assert!(
        retry_meta_present(&s.history),
        "the verbatim retry meta message must be pushed when the feature is forced on"
    );
    // Ordering: the retry meta sits AFTER the deny tool_result message.
    let deny_pos = s
        .history
        .iter()
        .position(|m| matches!(m, ConversationMessage::User { content, .. }
            if content.iter().any(|b| matches!(b, ContentBlock::ToolResult { tool_use_id: tu, is_error: true, .. } if *tu == tool_use_id))))
        .expect("deny tool_result present");
    let verbatim = "The PermissionDenied hook indicated you may retry this tool call.";
    let retry_pos = s
        .history
        .iter()
        .position(|m| {
            matches!(m, ConversationMessage::User { content, .. }
            if content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == verbatim)))
        })
        .expect("retry meta present");
    assert!(
        retry_pos > deny_pos,
        "retry meta must follow the deny result"
    );
}

#[tokio::test]
async fn permission_denied_retry_meta_when_classifier_enabled_by_default() {
    // With classifier permissions enabled, a classifier-deny whose
    // PermissionDenied hook returns {retry:true} pushes the retry message even
    // without forcing the test-only config bit.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "Echo", json!({ "cmd": "rm -rf /" }));
    let hooks = exec_with_retry_hook().await;
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool));
    let gate = Arc::new(ClassifierDenyGate {
        reason: "classifier blocked it",
    });
    // Default config → transcript_classifier_enabled = false, but the
    // permission classifier itself is now enabled.
    let orch = orch_with_gate(api, hooks, tools, gate);

    let outcome = orch.run_turn("run echo").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let session = orch.session();
    let s = session.lock().await;
    assert!(
        retry_meta_present(&s.history),
        "retry meta message should be present on the classifier-enabled default path"
    );
}
