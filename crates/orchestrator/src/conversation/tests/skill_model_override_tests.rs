use super::*;
use crate::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, mock_message_response, noop_hook_executor,
    text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use lingxi_core::types::ToolUseId;
use llm_runtime::ContentBlock as LlmContentBlock;
use std::sync::Arc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    ContextModifier, DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};

/// The model the [`ModelSwitchTool`] switches the session to.
const SWITCHED_MODEL: &str = "changed-model";

/// A tool that succeeds AND returns a `context_modifier` setting the turn's
/// `main_loop_model` to [`SWITCHED_MODEL`] — the orchestrator-side twin of a
/// Skill tool with a `model:` frontmatter. Sets the model directly (the
/// route and context-window resolution are tested in the Agent crate).
struct ModelSwitchTool;
#[async_trait]
impl Tool for ModelSwitchTool {
    fn name(&self) -> &str {
        "ModelSwitch"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> = once_cell::sync::Lazy::new(
            || serde_json::json!({ "type": "object", "properties": {} }),
        );
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
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        "model-switch".into()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let modifier = preference_modifier(
            input.get("model").and_then(serde_json::Value::as_str).unwrap_or(SWITCHED_MODEL),
            input.get("model_profile").and_then(serde_json::Value::as_str),
        );
        Ok(ToolCallResult {
            data: serde_json::json!({
                "content": "TOOL-RESULT",
                "model_content": "Launching skill: switcher",
            }),
            model_content: None,
            new_messages: vec![],
            context_modifier: Some(modifier),
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// A tool with NO `context_modifier` (the byte-identical baseline — like
/// every existing tool).
struct PlainTool;
#[async_trait]
impl Tool for PlainTool {
    fn name(&self) -> &str {
        "Plain"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> = once_cell::sync::Lazy::new(
            || serde_json::json!({ "type": "object", "properties": {} }),
        );
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
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        "plain".into()
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
            data: serde_json::json!({ "content": "PLAIN-RESULT" }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

fn registry_with(tool: Arc<dyn Tool>) -> Arc<ToolRegistry> {
    let mut reg = ToolRegistry::new();
    reg.register_builtin(tool);
    Arc::new(reg)
}

fn skill_test_route(
    model: &str,
    profile: Option<&str>,
) -> Result<agent::ModelResolutionContext, agent::ModelResolutionError> {
    let (qualifier, requested) = model
        .split_once('/')
        .map_or((None, model), |(profile, model)| (Some(profile), model));
    let route_error = || agent::ModelResolutionError::RouteUnavailable {
        model: model.into(),
        profile: profile.map(str::to_owned),
        reason: "unavailable test route".into(),
    };
    if profile
        .zip(qualifier)
        .is_some_and(|(profile, qualifier)| profile != qualifier)
    {
        return Err(route_error());
    }
    let profile = profile.or(qualifier).unwrap_or("a");
    if profile != "a" && profile != "b" {
        return Err(route_error());
    }
    let target = match requested {
        "balanced" => format!("balanced-{profile}"),
        "balanced-a" if profile == "a" => requested.into(),
        "balanced-b" if profile == "b" => requested.into(),
        crate::config::DEFAULT_MODEL | SWITCHED_MODEL | "shared" => requested.into(),
        _ => return Err(route_error()),
    };
    Ok(agent::ModelResolutionContext {
        route: agent::ModelRouteFacts {
            model: target,
            profile: Some(profile.into()),
            provider: Some(agent::ModelProviderKind::Other),
            ..Default::default()
        },
        family_defaults: agent::FamilyModelDefaults {
            sonnet: Some(format!("balanced-{profile}")),
            ..Default::default()
        },
        catalog_aliases: [("balanced".into(), vec![format!("balanced-{profile}")])].into(),
        ..Default::default()
    })
}

fn route_test_orchestrator() -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        registry_with(Arc::new(PlainTool)),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_model_resolution_context_provider(Arc::new(skill_test_route))
}

fn preference_modifier(model: &str, profile: Option<&str>) -> ContextModifier {
    let model = model.to_owned();
    let profile = profile.map(str::to_owned);
    Box::new(move |mut context| {
        context.options.main_loop_model = model;
        context.options.model_profile = profile;
        context
    })
}

#[tokio::test]
async fn skill_model_override_uses_live_profile_catalog_alias() {
    let orch = route_test_orchestrator();
    {
        let mut session = orch.session.lock().await;
        session.model = "shared".into();
        session.model_profile = Some("b".into());
    }
    crate::turn_loop::apply_model_context_modifiers(
        &orch,
        vec![preference_modifier("balanced", None)],
    )
    .await
    .unwrap();
    let session = orch.session.lock().await;
    assert_eq!(session.model, "balanced-b");
    assert_eq!(session.model_profile.as_deref(), Some("b"));
}

#[tokio::test]
async fn skill_model_override_same_model_different_profile_switches_route() {
    for (model, profile) in [("b/shared", None), ("shared", Some("b"))] {
        let orch = route_test_orchestrator();
        {
            let mut session = orch.session.lock().await;
            session.model = "shared".into();
            session.model_profile = Some("a".into());
        }
        crate::turn_loop::apply_model_context_modifiers(
            &orch,
            vec![preference_modifier(model, profile)],
        )
        .await
        .unwrap();
        let session = orch.session.lock().await;
        assert_eq!(session.model, "shared");
        assert_eq!(session.model_profile.as_deref(), Some("b"));
    }
}

#[tokio::test]
async fn skill_model_override_invalid_route_preserves_model_and_profile() {
    let orch = route_test_orchestrator();
    {
        let mut session = orch.session.lock().await;
        session.model = "shared".into();
        session.model_profile = Some("a".into());
    }
    let error = crate::turn_loop::apply_model_context_modifiers(
        &orch,
        vec![
            preference_modifier("b/shared", None),
            preference_modifier("missing/model", None),
        ],
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("could not be resolved"));
    let session = orch.session.lock().await;
    assert_eq!(session.model, "shared");
    assert_eq!(session.model_profile.as_deref(), Some("a"));
}

#[tokio::test]
async fn skill_model_override_resolves_later_alias_in_preceding_selected_profile() {
    let orch = route_test_orchestrator();
    {
        let mut session = orch.session.lock().await;
        session.model = "shared".into();
        session.model_profile = Some("a".into());
    }
    crate::turn_loop::apply_model_context_modifiers(
        &orch,
        vec![
            preference_modifier("b/shared", None),
            preference_modifier("balanced", None),
        ],
    )
    .await
    .unwrap();
    let session = orch.session.lock().await;
    assert_eq!(session.model, "balanced-b");
    assert_eq!(session.model_profile.as_deref(), Some("b"));
}

#[tokio::test]
async fn skill_model_override_streaming_context_state_applies_profile_only_change() {
    let orch = route_test_orchestrator();
    {
        let mut session = orch.session.lock().await;
        session.model = "shared".into();
        session.model_profile = Some("a".into());
    }
    let context = ToolUseContext::model_seed("shared".into(), Some("b".into()));
    let state = lingxi_core::host::tool_invoker::ToolInvocationContextState::new(Arc::new(context));
    crate::turn_loop::apply_model_context_state(&orch, state)
        .await
        .unwrap();
    let session = orch.session.lock().await;
    assert_eq!(session.model, "shared");
    assert_eq!(session.model_profile.as_deref(), Some("b"));
}

#[tokio::test]
async fn batched_skill_model_override_next_call_uses_selected_profile_with_same_model() {
    let resp1 = mock_message_response(
        vec![LlmContentBlock::ToolCall {
            id: ToolUseId::new().to_string(),
            name: "ModelSwitch".into(),
            input: serde_json::json!({"model": "b/shared"}),
        }],
        Some("tool_use"),
    );
    let resp2 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "done".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    let api = Arc::new(MockApiClient::new(vec![resp1, resp2]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        registry_with(Arc::new(ModelSwitchTool)),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_model_resolution_context_provider(Arc::new(skill_test_route));
    {
        let mut session = orch.session.lock().await;
        session.model = "shared".into();
        session.model_profile = Some("a".into());
    }
    orch.run_turn("switch route").await.unwrap();
    let requests = api.captured_requests().await;
    let [crate::OrchestratorApiRequest::Main(first), crate::OrchestratorApiRequest::Main(second)] =
        requests.as_slice()
    else {
        panic!("expected two main requests");
    };
    assert_eq!(first.profile.as_deref(), Some("a"));
    assert_eq!(second.profile.as_deref(), Some("b"));
    assert_eq!(first.model, "shared");
    assert_eq!(second.model, "shared");
}

#[tokio::test]
async fn streaming_skill_model_override_next_call_uses_selected_profile_with_same_model() {
    let tool_use_id = ToolUseId::new();
    let turn1 = vec![
        message_start("m1", "shared"),
        content_block_start_tool_use(0, tool_use_id, "ModelSwitch"),
        input_json_delta(0, "{\"model\":\"b/shared\"}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    let turn2 = vec![
        message_start("m2", "shared"),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![turn1, turn2]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            registry_with(Arc::new(ModelSwitchTool)),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_model_resolution_context_provider(Arc::new(skill_test_route)),
    );
    {
        let mut session = orch.session.lock().await;
        session.model = "shared".into();
        session.model_profile = Some("a".into());
    }
    orch.run_turn_streaming("switch route").await.unwrap();
    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].profile.as_deref(), Some("a"));
    assert_eq!(calls[1].profile.as_deref(), Some("b"));
    assert!(calls.iter().all(|call| call.model == "shared"));
}

// ----- batched driver (`run_turn`) -----

#[tokio::test]
async fn batched_skill_model_override_switches_session_model() {
    let tu = ToolUseId::new();
    let resp1 = mock_message_response(
        vec![LlmContentBlock::ToolCall {
            id: tu.to_string(),
            name: "ModelSwitch".into(),
            input: serde_json::json!({}),
        }],
        Some("tool_use"),
    );
    let resp2 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "done".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![resp1, resp2])),
        registry_with(Arc::new(ModelSwitchTool)),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_model_resolution_context_provider(Arc::new(skill_test_route));
    // Precondition: the session boots on the default model.
    assert_eq!(
        orch.session.lock().await.model,
        crate::config::DEFAULT_MODEL
    );

    orch.run_turn("switch please").await.expect("turn");

    // POST-BATCH the override took effect; the NEXT turn's API call reads it.
    assert_eq!(orch.session.lock().await.model, SWITCHED_MODEL);
}

#[tokio::test]
async fn batched_no_modifier_leaves_session_model_untouched() {
    // Byte-identical guard: a tool with NO context_modifier must not move
    // `session.model`.
    let tu = ToolUseId::new();
    let resp1 = mock_message_response(
        vec![LlmContentBlock::ToolCall {
            id: tu.to_string(),
            name: "Plain".into(),
            input: serde_json::json!({}),
        }],
        Some("tool_use"),
    );
    let resp2 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "done".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![resp1, resp2])),
        registry_with(Arc::new(PlainTool)),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.run_turn("no switch").await.expect("turn");
    assert_eq!(
        orch.session.lock().await.model,
        crate::config::DEFAULT_MODEL,
        "no context_modifier → session.model unchanged (byte-identical)"
    );
}

// ----- streaming driver (`run_turn_streaming`) -----

#[tokio::test]
async fn streaming_skill_model_override_switches_session_and_next_call() {
    let tu = ToolUseId::new();
    let turn1 = vec![
        message_start("m1", crate::config::DEFAULT_MODEL),
        content_block_start_tool_use(0, tu.clone(), "ModelSwitch"),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    let turn2 = vec![
        message_start("m2", SWITCHED_MODEL),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![turn1, turn2]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            registry_with(Arc::new(ModelSwitchTool)),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_model_resolution_context_provider(Arc::new(skill_test_route)),
    );

    orch.run_turn_streaming("switch please")
        .await
        .expect("streaming turn");

    // session.model switched POST-BATCH...
    assert_eq!(orch.session.lock().await.model, SWITCHED_MODEL);
    // ...and the NEXT (second) streaming call used the switched model, while
    // the first used the boot default.
    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 2, "two streaming calls (tool turn + end turn)");
    assert_eq!(calls[0].model, crate::config::DEFAULT_MODEL);
    assert_eq!(
        calls[1].model, SWITCHED_MODEL,
        "the NEXT API call must use the switched model"
    );
}

#[tokio::test]
async fn streaming_no_modifier_leaves_session_model_untouched() {
    // Byte-identical guard on the streaming path.
    let tu = ToolUseId::new();
    let turn1 = vec![
        message_start("m1", crate::config::DEFAULT_MODEL),
        content_block_start_tool_use(0, tu.clone(), "Plain"),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    let turn2 = vec![
        message_start("m2", crate::config::DEFAULT_MODEL),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![turn1, turn2]));
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        registry_with(Arc::new(PlainTool)),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));
    orch.run_turn_streaming("no switch")
        .await
        .expect("streaming turn");
    assert_eq!(
        orch.session.lock().await.model,
        crate::config::DEFAULT_MODEL,
        "no context_modifier → session.model unchanged on the streaming path"
    );
    let calls = streaming.captured_calls().await;
    assert!(
        calls
            .iter()
            .all(|c| c.model == crate::config::DEFAULT_MODEL),
        "every streaming call used the unchanged default model"
    );
}

/// Regression guard for the streaming-profile gap: when `session.model_profile`
/// is set (e.g. `"github-copilot"`) the INITIAL streaming `.stream()` call
/// must carry the profile, not `None`.  Mirrors the batched
/// `build_request_sets_profile_when_provided` test in `provider_adapter.rs`.
#[tokio::test]
async fn streaming_threads_model_profile_to_stream_call() {
    let turn = vec![
        message_start("m1", crate::config::DEFAULT_MODEL),
        content_block_start_text(0),
        text_delta(0, "hello"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![turn]));
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        registry_with(Arc::new(PlainTool)),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));
    // Set model_profile on the session directly (mirrors what switch_model does).
    {
        let mut s = orch.session.lock().await;
        s.model_profile = Some("github-copilot".to_string());
    }

    orch.run_turn_streaming("hello")
        .await
        .expect("streaming turn");

    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 1, "one streaming call");
    assert_eq!(
        calls[0].profile.as_deref(),
        Some("github-copilot"),
        "streaming path must thread session.model_profile through to the stream() call"
    );
}
