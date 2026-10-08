use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use async_trait::async_trait;
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

/// The "no `tools:` frontmatter" policy (claude `s===undefined`) — keeps the
/// whole tool pool. Used as the default in the system-prompt-focused tests.
fn keep_all_tools() -> agent::AgentToolPolicy {
    agent::AgentToolPolicy::All {
        use_exact_tools: false,
    }
}

fn orch_with_config(config: OrchestratorConfig) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        config,
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    )
}

struct LiveModeGate(std::sync::RwLock<String>);

#[async_trait]
impl PermissionGate for LiveModeGate {
    async fn check(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
    ) -> lingxi_core::host::PermissionDecision {
        lingxi_core::host::PermissionDecision::Allow
    }

    async fn set_permission_mode(&self, mode: &str) -> Result<(), String> {
        *self.0.write().expect("live mode write lock") = mode.to_string();
        Ok(())
    }

    fn permission_mode(&self) -> Option<String> {
        Some(self.0.read().expect("live mode read lock").clone())
    }
}

/// A minimal builtin tool exposing a fixed `name()` — enough for the wire
/// serializer (`build_wire_tools`) to advertise it and for the main-thread
/// agent filter to inspect its name.
struct NamedTool(&'static str);

#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| json!({ "type": "object", "properties": {} }))
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
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        self.0.into()
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
            mcp_meta_projection: None,
            model_content_projection: None,
            data_projection: None,
            data: json!({ "content": "ok" }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// Build an orchestrator whose registry advertises the named builtin tools.
fn orch_with_tools(names: &[&'static str]) -> ConversationOrchestrator {
    let mut registry = ToolRegistry::new();
    for n in names {
        registry.register_builtin(Arc::new(NamedTool(n)) as Arc<dyn Tool>);
    }
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    )
}

/// The set of `name` fields the wire tool array advertises.
async fn wire_tool_names(orch: &ConversationOrchestrator) -> Vec<String> {
    orch.build_wire_tools()
        .await
        .0
        .into_iter()
        .filter_map(|t| {
            t.get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// A dead OAuth session must actually REACH the user as "Login expired",
/// not as the variant's bare `Display`. This is the wiring half of
/// `api_error_copy::oauth_refresh_dead_text` — the copy constants have their
/// own byte-exact tests, but a `match` arm that is never taken renders
/// nothing, and a unit test of the constant cannot detect that.
#[tokio::test]
async fn a_dead_oauth_session_renders_the_login_expired_copy() {
    let mut config = OrchestratorConfig::default();
    config.interactive_session = true;
    let orch = orch_with_config(config);
    assert_eq!(
        orch.model_error_text(&LlmError::OAuthRefreshDead).await,
        "Login expired \u{b7} Please run /connect"
    );

    // Same error, no TTY: the copy must stop naming a command the caller
    // cannot run.
    let mut headless = OrchestratorConfig::default();
    headless.interactive_session = false;
    let orch = orch_with_config(headless);
    assert_eq!(
        orch.model_error_text(&LlmError::OAuthRefreshDead).await,
        "Failed to authenticate: OAuth session expired and could not be refreshed"
    );
}

/// A real provider 403 now KEEPS its message, so the auth-copy family is
/// reachable. Before `Authentication`/`PermissionDenied` carried a message,
/// the decoder dropped it at the provider boundary and every branch below
/// gated on `Display` ("permission denied") — none could ever match.
#[tokio::test]
async fn a_real_403_keeps_the_message_the_auth_branches_gate_on() {
    // Exactly what `providers::map_error_status(403, …)` now produces.
    let revoked = LlmError::PermissionDenied {
        message: "403 OAuth token has been revoked".to_string(),
    };
    assert_eq!(revoked.http_status(), Some(403), "prefix survives");
    assert!(crate::api_error_copy::is_oauth_revoked(
        revoked.http_status(),
        revoked.provider_message().unwrap_or_default()
    ));

    let orch = orch_with_config(OrchestratorConfig::default());
    assert_eq!(
        orch.model_error_text(&revoked).await,
        "Your account does not have access to Claude. Please login again or \
         contact your administrator."
    );

    // The org-level OAuth block reaches its own copy too.
    let org_block = LlmError::PermissionDenied {
        message: "403 OAuth authentication is currently not allowed for this organization"
            .to_string(),
    };
    assert_eq!(
        orch.model_error_text(&org_block).await,
        crate::api_error_copy::OAUTH_ORG_NOT_ALLOWED
    );

    // A 403 with unrelated text reaches the oracle's TERMINAL arm, which
    // still carries the provider detail — NOT the bare `Display`.
    let plain = LlmError::PermissionDenied {
        message: "403 forbidden".to_string(),
    };
    assert_eq!(
        orch.model_error_text(&plain).await,
        "Failed to authenticate. API Error: 403 forbidden",
        "default config is non-interactive"
    );

    // The COMMON shape: the SDK stringifies the whole body into the
    // message, and the oracle unwraps it rather than showing raw JSON.
    let json_body = LlmError::PermissionDenied {
        message: r#"403 {"type":"error","error":{"message":"quota gone"}}"#.to_string(),
    };
    assert_eq!(
        orch.model_error_text(&json_body).await,
        "Failed to authenticate. API Error: 403 quota gone"
    );
}

/// `/status` warnings must reflect the CURRENT memory set, not a launch
/// snapshot: a LINGXI.md that grows past the limit mid-session is exactly
/// the case the panel exists to report.
#[tokio::test]
async fn large_memory_warnings_are_recomputed_from_the_live_memory_set() {
    use lingxi_core::host::OrchestratorHandle as _;

    // The default model is `claude-opus-4-8`, a 1M-context model, so the
    // threshold is 200_000 chars — NOT the 40_000 floor. Sizing the fixture
    // against the floor would have made this pass for the wrong reason.
    let big = "x".repeat(250_000);
    let file = crate::prompt::MemoryFile {
        parent: None,
        source_content: None,
        path: std::path::PathBuf::from("/work/repo/LINGXI.md"),
        body: big,
        is_local_override: false,
        tier: crate::prompt::LingxiMdTier::Project,
        globs: None,
        raw_content: String::new(),
        content_differs_from_disk: false,
    };
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![file])),
        PathBuf::from("/work/repo"),
    );
    let rows = orch
        .large_memory_warnings()
        .await
        .expect("production orchestrator can recompute memory warnings");
    assert_eq!(
        rows.len(),
        1,
        "the oversized file must be reported: {rows:?}"
    );
    assert!(
        rows[0].starts_with("Large ") && rows[0].contains("will impact performance"),
        "oracle row shape: {}",
        rows[0]
    );

    // An empty memory set reports nothing — the panel stays byte-identical.
    let empty = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    );
    assert_eq!(empty.large_memory_warnings().await, Some(Vec::new()));
}

/// The org-level OAuth block must reach the user as its own copy, and must
/// NOT be confused with the API-key disablement — they prescribe opposite
/// remedies.
#[tokio::test]
async fn an_org_oauth_block_tells_the_user_to_use_an_api_key() {
    let orch = orch_with_config(OrchestratorConfig::default());
    let err = LlmError::InvalidRequest {
        message: "403 OAuth authentication is currently not allowed for this organization"
            .to_string(),
    };
    // The gate keys on Authentication/PermissionDenied, so route it the way
    // a real decode would.
    let denied = LlmError::PermissionDenied {
        message: String::new(),
    };
    assert!(
        !crate::api_error_copy::is_oauth_org_not_allowed(denied.http_status(), &denied.to_string()),
        "a bare PermissionDenied carries no message and must not match"
    );
    assert!(crate::api_error_copy::is_oauth_org_not_allowed(
        Some(403),
        &err.to_string()
    ));
    let text = orch
        .model_error_text(&LlmError::PermissionDenied {
            message: String::new(),
        })
        .await;
    assert!(
        !text.contains("disabled Claude subscription access"),
        "a bare 403 with no message must not claim an org block: {text}"
    );
}

/// The sibling failures must NOT claim the login expired: a plain auth
/// failure keeps its own text, so the new arm cannot swallow them.
#[tokio::test]
async fn an_ordinary_auth_failure_is_not_reported_as_an_expired_login() {
    let mut config = OrchestratorConfig::default();
    config.interactive_session = true;
    let orch = orch_with_config(config);
    let text = orch
        .model_error_text(&LlmError::Authentication {
            message: String::new(),
        })
        .await;
    assert!(
        !text.contains("Login expired"),
        "a generic auth failure must not be rendered as an expired login: {text}"
    );
}

/// A resolved `--agent` with a prompt REPLACES the assembled default system
/// prompt on every query (claude-code `nre` uses `agentDef.getSystemPrompt()`
/// as the whole system prompt, exactly like `--system-prompt`).
#[tokio::test]
async fn main_thread_agent_prompt_replaces_default() {
    let orch = orch_with_config(OrchestratorConfig::default());
    let default = orch.assemble_system_prompt_preview().await;
    orch.set_main_thread_agent(
        "code-reviewer".to_string(),
        Some("You are a meticulous code reviewer.".to_string()),
        keep_all_tools(),
        Vec::new(),
        None,
    )
    .await;
    let after = orch.assemble_system_prompt_preview().await;
    assert_eq!(after, "You are a meticulous code reviewer.");
    assert_ne!(
        after, default,
        "the agent prompt must replace the assembled default"
    );
}

/// `--system-prompt` (`system_prompt_override` / claude `overrideSystemPrompt`)
/// beats the main-thread agent's prompt — `nre` returns `Zu([overrideSystemPrompt])`
/// before ever consulting the agent definition.
#[tokio::test]
async fn system_prompt_override_beats_main_thread_agent() {
    let mut config = OrchestratorConfig::default();
    config.system_prompt_override = Some("EXPLICIT --system-prompt wins".to_string());
    let orch = orch_with_config(config);
    orch.set_main_thread_agent(
        "code-reviewer".to_string(),
        Some("agent prompt should be ignored".to_string()),
        keep_all_tools(),
        Vec::new(),
        None,
    )
    .await;
    assert_eq!(
        orch.assemble_system_prompt_preview().await,
        "EXPLICIT --system-prompt wins"
    );
}

/// An adopted agent that declares NO prompt falls through to the assembled
/// default (claude `getSystemPrompt()` -> undefined -> default path).
#[tokio::test]
async fn main_thread_agent_without_prompt_uses_default() {
    let orch = orch_with_config(OrchestratorConfig::default());
    let default = orch.assemble_system_prompt_preview().await;
    orch.set_main_thread_agent(
        "promptless".to_string(),
        None,
        keep_all_tools(),
        Vec::new(),
        None,
    )
    .await;
    assert_eq!(orch.assemble_system_prompt_preview().await, default);
}

/// The adopted agent's `agentType` rides main-thread lifecycle hook payloads
/// (`expansion_hook_context` shares the `lifecycle_hook_ctx` builder that
/// `SessionStart` / `UserPromptSubmit` / `Stop` use). `None` before any
/// `--agent` is applied.
#[tokio::test]
async fn lifecycle_hook_ctx_carries_main_thread_agent_type() {
    let orch = orch_with_config(OrchestratorConfig::default());
    assert_eq!(orch.expansion_hook_context().await.agent_type, None);
    orch.set_main_thread_agent(
        "code-reviewer".to_string(),
        None,
        keep_all_tools(),
        Vec::new(),
        None,
    )
    .await;
    assert_eq!(
        orch.expansion_hook_context().await.agent_type,
        Some("code-reviewer".to_string())
    );
}

#[tokio::test]
async fn lifecycle_hook_ctx_reports_live_permission_mode() {
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(LiveModeGate(std::sync::RwLock::new("default".to_string()))),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    );

    orch.set_permission_mode("acceptEdits")
        .await
        .expect("set live permission mode");
    assert_eq!(
        orch.expansion_hook_context().await.permission_mode,
        Some("acceptEdits".to_string())
    );

    orch.session.lock().await.plan_mode = true;
    assert_eq!(
        orch.expansion_hook_context().await.permission_mode,
        Some("plan".to_string()),
        "explicit /plan state takes precedence over the gate's last mode"
    );
}

#[tokio::test]
async fn lifecycle_hook_ctx_uses_trimmed_last_assistant_text() {
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    );

    {
        let session_handle = orch.session();
        let mut session = session_handle.lock().await;
        session
            .history
            .push(lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "earlier user".to_string(),
            ));
        session
            .history
            .push(lingxi_core::types::ConversationMessage::Assistant { per_turn_effort: None,
                id: lingxi_core::types::MessageId::new(),
                content: vec![
                    lingxi_core::types::ContentBlock::Text {
                        text: " first line ".to_string(),
                        citations: None,
                    },
                    lingxi_core::types::ContentBlock::Thinking {
                        thinking: "hidden".to_string(),
                        signature: None,
                    },
                    lingxi_core::types::ContentBlock::Text {
                        text: "second line ".to_string(),
                        citations: None,
                    },
                ],
                stop_reason: Some("end_turn".to_string()),
            });
    }

    let live = orch
        .expansion_hook_context()
        .await
        .prompt_transcript
        .unwrap();
    assert_eq!(live.messages, orch.session().lock().await.history);
    assert_eq!(
        live.messages.len(),
        2,
        "unpersisted assistant is visible to prompt hooks"
    );

    assert_eq!(
        orch.expansion_hook_context().await.last_assistant_message,
        Some("first line \nsecond line".to_string()),
        "hook context must join the last assistant's text blocks with newlines and trim outer whitespace"
    );

    orch.session()
        .lock()
        .await
        .history
        .push(lingxi_core::types::ConversationMessage::Assistant { per_turn_effort: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: "   ".to_string(),
                citations: None,
            }],
            stop_reason: Some("end_turn".to_string()),
        });
    assert_eq!(
        orch.expansion_hook_context().await.last_assistant_message,
        None,
        "an all-whitespace final assistant message must surface as None"
    );
}

/// An adopted agent with a `tools:` allow-list narrows the advertised main-
/// loop tool pool to the named tools (claude `HJ(agentDef,to,!1,!0)` with an
/// explicit `s`: only listed tools survive). Tools not in the list are
/// dropped; a listed name that does not exist is simply absent.
#[tokio::test]
async fn main_thread_agent_explicit_tools_narrow_the_pool() {
    let orch = orch_with_tools(&["Read", "Write", "Bash", "Grep"]);
    // No agent yet ⇒ every registered tool is advertised.
    let before = wire_tool_names(&orch).await;
    assert_eq!(before, vec!["Bash", "Grep", "Read", "Write"]);

    orch.set_main_thread_agent(
        "reviewer".to_string(),
        None,
        agent::AgentToolPolicy::Explicit(vec!["Read".to_string(), "Grep".to_string()]),
        Vec::new(),
        None,
    )
    .await;
    let after = wire_tool_names(&orch).await;
    assert_eq!(after, vec!["Grep", "Read"]);
}

/// `AgentToolPolicy::All` (no `tools:` frontmatter, claude `s===undefined`)
/// keeps the WHOLE pool — including tools the SUBAGENT filter would strip as
/// "always-disallowed" (ExitPlanMode / AskUserQuestion). The main-thread
/// filter runs `HJ` with `n=true`, which bypasses that strip. Regression
/// guard against accidentally reusing the subagent resolver here.
#[tokio::test]
async fn main_thread_agent_all_policy_keeps_always_disallowed_tools() {
    let orch = orch_with_tools(&["Read", "ExitPlanMode", "AskUserQuestion"]);
    orch.set_main_thread_agent(
        "planner".to_string(),
        None,
        keep_all_tools(),
        Vec::new(),
        None,
    )
    .await;
    let after = wire_tool_names(&orch).await;
    assert_eq!(after, vec!["AskUserQuestion", "ExitPlanMode", "Read"]);
}

/// The agent's per-definition `disallowedTools` subtracts from the pool
/// (base tool name; a trailing `(rule)` is stripped) BEFORE the `tools:`
/// projection — claude `HJ` `g=u.filter(P=>!isToolDisallowed(P))`.
#[tokio::test]
async fn main_thread_agent_disallowed_tools_subtract() {
    let orch = orch_with_tools(&["Read", "Write", "Bash"]);
    orch.set_main_thread_agent(
        "safe".to_string(),
        None,
        keep_all_tools(),
        vec!["Write".to_string(), "Bash(rm -rf)".to_string()],
        None,
    )
    .await;
    let after = wire_tool_names(&orch).await;
    assert_eq!(after, vec!["Read"]);
}

#[tokio::test]
async fn mobile_runtime_reminder_is_stable_across_agent_tool_filters() {
    let orch = orch_with_tools(&["Read", "Shell"]);
    let orch =
        orch.with_mobile_runtime_environment(lingxi_core::host::MobileRuntimeEnvironment::new(
            lingxi_core::host::MobileHostEnvironment::new(
                lingxi_core::host::MobileHostOs::Ios,
                Some("19.0".into()),
                lingxi_core::host::MobileDeviceClass::Phone,
                lingxi_core::host::MobileExecutionTarget::PhysicalDevice,
                lingxi_core::host::MobileLaunchMode::Interactive,
            ),
            lingxi_core::host::MobileToolRuntime::MobileLinuxGuest,
            Some("/workspace/a".into()),
            Some("/bin/sh".into()),
            Some("Mobile Linux sh".into()),
            lingxi_core::host::MobileNetworkPolicy::PermissionMediated,
            lingxi_core::host::MobileLifecyclePolicy::IosFiniteBackgroundAssertion,
        ));

    let before = orch
        .mobile_runtime_environment_preview()
        .await
        .expect("runtime reminder");
    orch.set_main_thread_agent(
        "reviewer".to_string(),
        None,
        agent::AgentToolPolicy::Explicit(vec!["Read".to_string()]),
        Vec::new(),
        None,
    )
    .await;

    let after = orch
        .mobile_runtime_environment_preview()
        .await
        .expect("runtime reminder");
    assert_eq!(after, before);
    assert!(after.contains("per-agent availability is defined by registered tool schemas"));
    assert!(!after.contains("available to this agent"));
}

/// A resolved `--agent` model and profile replace the session route
/// model (claude `jb(Zo(y.model))`); the caller has already gated it on
/// `!userSpecifiedModel` and resolved the alias against the selected profile.
#[tokio::test]
async fn main_thread_agent_model_override_replaces_session_model() {
    let mut config = OrchestratorConfig::default();
    config.model = "base-model".to_string();
    let orch = orch_with_config(config);
    orch.session().lock().await.model_profile = Some("previous-profile".into());
    assert_eq!(orch.session().lock().await.model, "base-model");

    orch.set_main_thread_agent(
        "fast".to_string(),
        None,
        keep_all_tools(),
        Vec::new(),
        Some((
            "claude-agent-model".to_string(),
            Some("selected-profile".into()),
        )),
    )
    .await;
    let session = orch.session();
    let s = session.lock().await;
    assert_eq!(s.model, "claude-agent-model");
    assert_eq!(s.model_profile.as_deref(), Some("selected-profile"));
}

#[tokio::test]
async fn main_thread_agent_model_override_distinguishes_empty_and_missing_profile() {
    for selected_profile in [Some(String::new()), None] {
        let mut config = OrchestratorConfig::default();
        config.model = "shared-wire-id".into();
        let orch = orch_with_config(config);
        orch.session().lock().await.model_profile = Some("previous-profile".into());
        orch.set_main_thread_agent(
            "selected-agent".into(),
            None,
            keep_all_tools(),
            Vec::new(),
            Some(("shared-wire-id".into(), selected_profile.clone())),
        )
        .await;
        let session = orch.session();
        let session = session.lock().await;
        assert_eq!(session.model, "shared-wire-id");
        assert_eq!(session.model_profile, selected_profile);
    }
}

fn resumed_agent_with_model(model: agent::AgentModel) -> agent::AgentDefinition {
    agent::AgentDefinition {
        agent_type: "resumed-agent".into(),
        when_to_use: "resume fixture".into(),
        tools: keep_all_tools(),
        max_turns: 4,
        model,
        permission_mode: agent::AgentPermissionMode::Default,
        source: agent::AgentSource::Flag,
        offer_provider: None,
        base_dir: PathBuf::from("/work/repo"),
        system_prompt: Some("resumed prompt".into()),
        mcp_servers: Vec::new(),
        frontmatter_hooks: Vec::new(),
        icon: None,
        allowed_tools: Vec::new(),
        worktree_requirement: None,
        disallowed_tools: Vec::new(),
        skills: Vec::new(),
        required_mcp_servers: Vec::new(),
        background: false,
        omit_instructions: false,
        isolation: None,
        memory: None,
        effort: None,
        initial_prompt: None,
        color: None,
        observer: None,
        cache_ttl: None,
    }
}

#[tokio::test]
async fn resumed_concrete_agent_model_recovers_a_retired_saved_model() {
    use agent::model_resolution::{ModelResolutionContext, ModelResolutionError, ModelRouteFacts};

    for (spec, expected_profile) in [
        ("current-model", "profile-a"),
        ("profile-b/current-model", "profile-b"),
    ] {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = calls.clone();
        let orch = orch_with_config(OrchestratorConfig {
            model: "live-model".into(),
            apply_resumed_agent_model: true,
            ..Default::default()
        })
        .with_model_resolution_context_provider(Arc::new(
            move |model: &str, profile: Option<&str>| {
                captured
                    .lock()
                    .unwrap()
                    .push((model.to_owned(), profile.map(str::to_owned)));
                let unavailable = || ModelResolutionError::RouteUnavailable {
                    model: model.to_owned(),
                    profile: profile.map(str::to_owned),
                    reason: "model is unavailable on this profile".into(),
                };
                let (model, profile) = match (model, profile) {
                    ("live-model", _) => ("live-model", "live-profile"),
                    ("current-model", Some(profile @ ("profile-a" | "profile-b"))) => {
                        ("current-model", profile)
                    }
                    ("profile-b/current-model", None | Some("profile-b")) => {
                        ("current-model", "profile-b")
                    }
                    _ => return Err(unavailable()),
                };
                Ok(ModelResolutionContext {
                    route: ModelRouteFacts {
                        model: model.to_owned(),
                        profile: Some(profile.to_owned()),
                        ..Default::default()
                    },
                    ..Default::default()
                })
            },
        ));
        orch.session.lock().await.model_profile = Some("live-profile".into());
        calls.lock().unwrap().clear();
        let definition = resumed_agent_with_model(agent::AgentModel::Explicit(spec.into()));
        let resumed_session = lingxi_core::types::SessionId::new();

        lingxi_core::host::OrchestratorHandle::resume_session(
            &orch,
            resumed_session,
            Vec::new(),
            None,
            None,
            lingxi_core::host::ResumeRuntimeSnapshot {
                model: "retired-model".into(),
                model_profile: Some("profile-a".into()),
                main_thread_agent_type: Some(definition.agent_type.clone()),
                main_thread_agent_definition: Some(serde_json::to_value(definition).unwrap()),
                ..Default::default()
            },
        )
        .await
        .expect("the agent's configured model can replace the retired saved model");

        let session = orch.session.lock().await;
        assert_eq!(session.session_id, resumed_session);
        assert_eq!(session.model, "current-model");
        assert_eq!(session.model_profile.as_deref(), Some(expected_profile));
        assert_eq!(
            orch.tools.main_loop_model().as_deref(),
            Some("current-model")
        );
        assert!(calls
            .lock()
            .unwrap()
            .iter()
            .all(|(model, _)| model != "retired-model"));
    }
}

#[tokio::test]
async fn resumed_relative_agent_alias_uses_the_saved_profiles_catalog() {
    use agent::model_resolution::{
        FamilyModelDefaults, ModelProviderKind, ModelResolutionContext, ModelResolutionError,
        ModelRouteFacts,
    };

    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = calls.clone();
    let orch = orch_with_config(OrchestratorConfig {
        model: "live-model".into(),
        apply_resumed_agent_model: true,
        ..Default::default()
    })
    .with_model_resolution_context_provider(Arc::new(
        move |model: &str, profile: Option<&str>| {
            captured
                .lock()
                .unwrap()
                .push((model.to_owned(), profile.map(str::to_owned)));
            let (model, profile) = match (model, profile) {
                ("live-model", _) => ("live-model", "profile-b"),
                ("sonnet", Some("profile-a")) => ("sonnet", "profile-a"),
                ("profile-a-sonnet", Some("profile-a")) => ("profile-a-sonnet", "profile-a"),
                _ => {
                    return Err(ModelResolutionError::RouteUnavailable {
                        model: model.to_owned(),
                        profile: profile.map(str::to_owned),
                        reason: "model is unavailable on this profile".into(),
                    });
                }
            };
            Ok(ModelResolutionContext {
                route: ModelRouteFacts {
                    model: model.to_owned(),
                    profile: Some(profile.to_owned()),
                    provider: Some(ModelProviderKind::Other),
                    ..Default::default()
                },
                family_defaults: FamilyModelDefaults {
                    sonnet: Some(format!("{profile}-sonnet")),
                    ..Default::default()
                },
                ..Default::default()
            })
        },
    ));
    orch.session.lock().await.model_profile = Some("profile-b".into());
    calls.lock().unwrap().clear();
    let definition = resumed_agent_with_model(agent::AgentModel::Alias("sonnet".into()));

    lingxi_core::host::OrchestratorHandle::resume_session(
        &orch,
        lingxi_core::types::SessionId::new(),
        Vec::new(),
        None,
        None,
        lingxi_core::host::ResumeRuntimeSnapshot {
            model: "retired-model".into(),
            model_profile: Some("profile-a".into()),
            main_thread_agent_type: Some(definition.agent_type.clone()),
            main_thread_agent_definition: Some(serde_json::to_value(definition).unwrap()),
            ..Default::default()
        },
    )
    .await
    .expect("a relative alias resolves from the resumed profile without the retired model");

    let session = orch.session.lock().await;
    assert_eq!(session.model, "profile-a-sonnet");
    assert_eq!(session.model_profile.as_deref(), Some("profile-a"));
    assert_eq!(
        orch.tools.main_loop_model().as_deref(),
        Some("profile-a-sonnet")
    );
    let calls = calls.lock().unwrap();
    assert!(calls.contains(&("sonnet".into(), Some("profile-a".into()))));
    assert!(calls
        .iter()
        .all(|(model, profile)| model != "sonnet" || profile.as_deref() == Some("profile-a")));
    assert!(calls.iter().all(|(model, _)| model != "retired-model"));
}

#[tokio::test]
async fn initial_model_gate_refreshes_with_route_authority_before_wire_tool_assembly() {
    let mut config = OrchestratorConfig::default();
    config.model = "opus".into();
    let calls = Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
    let captured = calls.clone();
    let orch = orch_with_config(config).with_model_resolution_context_provider(Arc::new(
        move |model: &str, profile: Option<&str>| {
            captured.lock().unwrap().push(profile.map(str::to_owned));
            Ok(agent::model_resolution::ModelResolutionContext {
                route: agent::model_resolution::ModelRouteFacts {
                    model: model.to_owned(),
                    profile: profile.map(str::to_owned),
                    ..Default::default()
                },
                family_defaults: agent::model_resolution::FamilyModelDefaults {
                    opus: Some(format!("wire-opus-{}", profile.unwrap_or("unqualified"))),
                    ..Default::default()
                },
                ..Default::default()
            })
        },
    ));
    assert_eq!(
        orch.tools.main_loop_model().as_deref(),
        Some("wire-opus-unqualified")
    );
    for profile in ["provider-a", "provider-b", ""] {
        orch.seed_initial_model_profile("opus", profile).await;
        assert_eq!(
            orch.tools.main_loop_model(),
            Some(format!("wire-opus-{profile}")),
            "the reminder gate must see the seeded route before tools are assembled"
        );
        assert_eq!(
            orch.session.lock().await.model_profile.as_deref(),
            Some(profile)
        );
    }
    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            None,
            Some("provider-a".into()),
            Some("provider-b".into()),
            Some(String::new())
        ]
    );
}

/// `model_override == None` (agent `model: inherit`, or the user passed
/// `--model` so the caller gated it out) leaves the session model untouched.
#[tokio::test]
async fn main_thread_agent_no_model_override_leaves_session_model() {
    let mut config = OrchestratorConfig::default();
    config.model = "base-model".to_string();
    let orch = orch_with_config(config);
    orch.set_main_thread_agent(
        "inheritor".to_string(),
        None,
        keep_all_tools(),
        Vec::new(),
        None,
    )
    .await;
    assert_eq!(orch.session().lock().await.model, "base-model");
}

/// Native 2.1.293 surfaces a provider APIError without the taxonomy's
/// `invalid request:` prefix. Local validator failures have no provider status.
#[tokio::test]
async fn provider_invalid_request_uses_native_api_error_copy() {
    let orch = orch_with_config(OrchestratorConfig::default());
    for (message, expected) in [
        (
            "400 HEADLESS_LOCAL_PROVIDER_ERROR",
            "API Error: 400 HEADLESS_LOCAL_PROVIDER_ERROR",
        ),
        (
            r#"400 {"type":"error","error":{"message":"bad request"}}"#,
            "API Error: 400 bad request",
        ),
        (
            r#"422 {"error":{"message":"unsupported field"}}"#,
            "API Error: 422 unsupported field",
        ),
        (
            "invalid model configuration",
            "invalid request: invalid model configuration",
        ),
    ] {
        assert_eq!(
            orch.model_error_text(&LlmError::InvalidRequest {
                message: message.into()
            })
            .await,
            expected,
        );
    }
}

/// The result status comes from the typed provider error, not the legacy
/// envelope fallback which categorizes local InvalidRequest errors as 400.
#[tokio::test]
async fn graceful_provider_error_metrics_preserve_actual_status_without_guessing() {
    use crate::test_support::MockStreamingApiClient;
    for (message, expected_status) in [
        ("400 HEADLESS_LOCAL_PROVIDER_ERROR", Some(400)),
        ("422 unsupported field", Some(422)),
        ("invalid model configuration", None),
    ] {
        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(MockStreamingApiClient::with_open_error(
                LlmError::InvalidRequest {
                    message: message.into(),
                },
                vec![],
            )),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/work/repo"),
        );
        orch.run_turn_streaming_with_cancel("hi", tokio_util::sync::CancellationToken::new())
            .await
            .expect("APIError remains a graceful assistant result");
        let metrics = orch.completed_turn_metrics().unwrap();
        assert_eq!(metrics.stop_reason.as_deref(), Some("model_error"));
        assert_eq!(metrics.api_error_status, expected_status);
        assert_eq!(
            metrics.api_error_stop_reason.as_deref(),
            Some("stop_sequence")
        );
    }
}
