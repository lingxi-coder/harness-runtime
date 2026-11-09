use super::*;
use crate::conversation::{OrchestratorApiClient, OrchestratorApiRequest};
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};
use crate::{ConversationOrchestrator, OrchestratorConfig};
use lingxi_core::host::prompt_policy::PromptTarget;
use lingxi_core::host::session_context::ConversationMode;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use lingxi_llm_client::providers::anthropic::system_prompt::{PromptText, SystemPromptInput};
use llm_runtime::{HistoryEvent, HistoryResponse, LlmError};
use std::sync::atomic::{AtomicUsize, Ordering};

struct RoutingApi {
    inner: MockApiClient,
    resolutions: AtomicUsize,
}

#[async_trait::async_trait]
impl OrchestratorApiClient for RoutingApi {
    fn prompt_target(
        &self,
        model: &str,
        profile: Option<&str>,
        mode: ConversationMode,
    ) -> Result<PromptTarget, LlmError> {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        let claude_alias = profile == Some("claude-channel");
        Ok(PromptTarget::from_route(
            mode,
            if claude_alias || model.starts_with("claude-") {
                "anthropic"
            } else {
                "openai"
            },
            model,
            claude_alias.then_some("claude-sonnet-4-6"),
            None,
            profile,
        ))
    }

    async fn messages_create(
        &self,
        request: OrchestratorApiRequest,
    ) -> Result<HistoryResponse, LlmError> {
        self.inner.messages_create(request).await
    }
}

fn tool_registry() -> Arc<tool_api::registry::ToolRegistry> {
    let mut registry = tool_api::registry::ToolRegistry::new();
    let context = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        Vec::new(),
    );
    registry.register_builtin(Arc::new(tool_file::read::FileReadTool::new(context)));
    Arc::new(registry)
}

fn task_registry() -> Arc<tool_api::registry::ToolRegistry> {
    let context = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        Vec::new(),
    );
    let mut registry = tool_api::registry::ToolRegistry::new();
    registry.register_builtin(Arc::new(tool_task::TaskCreateTool::new(context.clone())));
    registry.register_builtin(Arc::new(tool_task::TaskUpdateTool::new(context)));
    Arc::new(registry)
}

#[derive(Default)]
struct OnceTaskNotifications(AtomicUsize);

#[async_trait::async_trait]
impl crate::prompt::task_notification::TaskNotificationProvider for OnceTaskNotifications {
    async fn take_pending_task_notifications(
        &self,
    ) -> Vec<lingxi_core::host::task_registry::TaskNotification> {
        if self.0.fetch_add(1, Ordering::SeqCst) != 0 {
            return Vec::new();
        }
        vec![lingxi_core::host::task_registry::TaskNotification {
            task_id: "mod-completion".into(),
            task_type: "local_bash".into(),
            status: "completed".into(),
            description: "ONE SHOT COMPLETION".into(),
            ..Default::default()
        }]
    }
}

fn assert_task_reminder_projection(
    model: &str,
    messages: &[lingxi_core::types::ConversationMessage],
    tools: &[llm_runtime::ToolDeclaration],
) {
    let generic = model == "gpt-5.5";
    for name in ["TaskCreate", "TaskUpdate"] {
        assert_eq!(
            tools.iter().any(|tool| tool.name == name),
            generic,
            "{model} {name}"
        );
    }
    let text = messages
        .iter()
        .map(|message| message.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        text.matches("The task tools haven't been used").count(),
        usize::from(generic),
        "{model}: {text}"
    );
    assert_eq!(
        text.matches("ONE SHOT COMPLETION").count(),
        1,
        "durable completion: {text}"
    );
}

#[tokio::test]
async fn prompt_mod_batched_rewrites_reminders_without_redraining_notifications() {
    let targets = ["claude-opus-5", "gpt-5.5", "gpt-5.5"];
    let (directory, hooks) = mod_host(&targets).await;
    let api = Arc::new(RoutingApi {
        inner: MockApiClient::new(targets.iter().map(|model| answer(model)).collect()),
        resolutions: AtomicUsize::new(0),
    });
    let notifications = Arc::new(OnceTaskNotifications::default());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            model: "gpt-5.5".into(),
            ..Default::default()
        },
        api.clone(),
        task_registry(),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        directory.path().to_path_buf(),
    )
    .with_hook_registry(hooks)
    .with_task_notifications(notifications.clone());
    {
        let mut session = orch.session.lock().await;
        session.turns_since_last_todo_write = tool_task::reminder::TURNS_SINCE_WRITE;
        session.turns_since_last_reminder = tool_task::reminder::TURNS_BETWEEN_REMINDERS;
    }
    orch.run_turn("keep the task assignment").await.unwrap();
    let calls = api.inner.captured_requests().await;
    assert_eq!(calls.len(), targets.len());
    for (call, target) in calls.iter().zip(targets) {
        let OrchestratorApiRequest::Main(call) = call else {
            panic!("main request")
        };
        assert_eq!(call.model, target);
        assert_task_reminder_projection(target, &call.messages, &call.tools);
    }
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        orch.session.lock().await.turns_since_last_reminder,
        targets.len() as u32
    );
}

#[tokio::test]
async fn prompt_mod_streaming_rewrites_reminders_for_every_next_request() {
    let targets = ["claude-opus-5", "gpt-5.5", "gpt-5.5"];
    let (directory, hooks) = mod_host(&targets).await;
    let api = Arc::new(RoutingApi {
        inner: MockApiClient::new(vec![]),
        resolutions: AtomicUsize::new(0),
    });
    let streaming = Arc::new(MockStreamingApiClient::with_turns(
        targets
            .iter()
            .enumerate()
            .map(|(index, model)| stream_answer(model, &format!("request-{index}")))
            .collect(),
    ));
    let notifications = Arc::new(OnceTaskNotifications::default());
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: "gpt-5.5".into(),
                ..Default::default()
            },
            api,
            streaming.clone(),
            task_registry(),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            directory.path().to_path_buf(),
        )
        .with_hook_registry(hooks)
        .with_task_notifications(notifications.clone()),
    );
    {
        let mut session = orch.session.lock().await;
        session.turns_since_last_todo_write = tool_task::reminder::TURNS_SINCE_WRITE;
        session.turns_since_last_reminder = tool_task::reminder::TURNS_BETWEEN_REMINDERS;
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        orch.run_turn_streaming("keep the task assignment"),
    )
    .await
    .expect("Mod response chain completes")
    .unwrap();
    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), targets.len());
    for (call, target) in calls.iter().zip(targets) {
        assert_eq!(call.model, target);
        assert_task_reminder_projection(target, &call.messages, &call.tools);
    }
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        orch.session.lock().await.turns_since_last_reminder,
        targets.len() as u32
    );
}

fn answer(model: &str) -> HistoryResponse {
    let mut response = mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "done".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    response.model = model.into();
    response
}

fn stream_answer(model: &str, id: &str) -> Vec<HistoryEvent> {
    crate::scripted![
        message_start(id, model),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]
}

async fn mod_host(
    models: &[&str],
) -> (
    tempfile::TempDir,
    Arc<tokio::sync::RwLock<hooks::HookRegistry>>,
) {
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("prompt-model-rewrite.js");
    let models = serde_json::to_string(models).unwrap();
    std::fs::write(
        &module,
        format!(
            r#"
      export function register(on) {{
        on('turn.step', async function* ($, e, next) {{
          let result;
          for (const model of {models}) {{
            const below = next({{ ...e, model }});
            for await (const chunk of below) yield chunk;
            result = await below.result;
          }}
          return result;
        }});
      }}
    "#
        ),
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "prompt-model-rewrite",
        directory.path(),
        &module,
        Source::parse("{}").unwrap(),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    (directory, Arc::new(tokio::sync::RwLock::new(registry)))
}

fn assert_components(
    model: &str,
    system: &SystemPromptInput,
    tools: &[llm_runtime::ToolDeclaration],
) {
    let body = system.display_text();
    let read = tools.iter().find(|tool| tool.name == "Read").unwrap();
    let read = read.description.string_at("").unwrap();
    if model.starts_with("claude-") {
        assert!(body.contains("# Doing tasks"), "{body}");
        assert!(
            read.contains("You can access any file directly by using this tool."),
            "{read}"
        );
    } else {
        assert!(body.contains("# Working with the user"), "{body}");
        assert!(!body.contains("The most recent Claude models"));
        assert!(
            !read.contains("You can access any file directly by using this tool."),
            "{read}"
        );
    }
    assert!(body.contains("retained append instructions"));
}

fn latest_model_identity(
    orch: &ConversationOrchestrator,
    messages: &[lingxi_core::types::ConversationMessage],
) -> Option<String> {
    let rows = orch.transcript.model_reminder_attachments.lock().unwrap();
    messages.iter().rev().find_map(|message| {
        let row = rows.get(&message.id())?;
        (row.string_at("/type") == Some("model"))
            .then(|| row.string_at("/identity/modelId").map(str::to_owned))
            .flatten()
    })
}

#[tokio::test]
async fn mod_batched_model_rewrite_reselects_both_system_and_tool_prompts() {
    for (initial, target) in [
        ("claude-sonnet-4-6", "gpt-5.5"),
        ("gpt-5.5", "claude-sonnet-4-6"),
    ] {
        let (directory, hooks) = mod_host(&[target]).await;
        let api = Arc::new(RoutingApi {
            inner: MockApiClient::new(vec![answer(target)]),
            resolutions: AtomicUsize::new(0),
        });
        let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let orch = ConversationOrchestrator::new(
            OrchestratorConfig {
                model: initial.into(),
                append_system_prompt: Some("retained append instructions".into()),
                ..Default::default()
            },
            api.clone(),
            tool_registry(),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            directory.path().to_path_buf(),
        )
        .with_hook_registry(hooks)
        .with_cache_safe_slot(slot.clone());
        orch.run_turn("retain the user's request").await.unwrap();
        let calls = api.inner.captured_requests().await;
        let OrchestratorApiRequest::Main(call) = &calls[0] else {
            panic!("main request")
        };
        assert_eq!(call.model, target);
        assert_components(target, call.system.as_ref().unwrap(), &call.tools);
        assert_eq!(
            latest_model_identity(&orch, &call.messages).as_deref(),
            Some(target)
        );
        assert!(call
            .messages
            .iter()
            .any(|message| format!("{message:?}").contains("retain the user's request")));
        let cached = slot.get_last().await.unwrap();
        assert_eq!(cached.system_prompt_input.as_ref(), call.system.as_ref());
        let policy = cached.tool_use_options.prompt_policy.as_ref().unwrap();
        assert_eq!(policy.target.request_model, target);
        let (prefix, metadata) = orch.current_turn_prompt_state().await.unwrap();
        assert_eq!(
            prefix,
            ConversationOrchestrator::system_prompt_source_text(call.system.as_ref().unwrap())
        );
        assert_eq!(metadata, Some(policy.metadata()));
    }
}

#[tokio::test]
async fn mod_streaming_model_rewrite_and_second_next_keep_their_actual_components() {
    let targets = ["gpt-5.5", "claude-sonnet-4-6"];
    let (directory, hooks) = mod_host(&targets).await;
    let api = Arc::new(RoutingApi {
        inner: MockApiClient::new(vec![]),
        resolutions: AtomicUsize::new(0),
    });
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![
        stream_answer(targets[0], "generic-request"),
        stream_answer(targets[1], "native-request"),
    ]));
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: "claude-sonnet-4-6".into(),
                append_system_prompt: Some("retained append instructions".into()),
                ..Default::default()
            },
            api,
            streaming.clone(),
            tool_registry(),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            directory.path().to_path_buf(),
        )
        .with_hook_registry(hooks)
        .with_cache_safe_slot(slot.clone()),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        orch.run_turn_streaming("keep this history"),
    )
    .await
    .expect("Mod response chain makes progress")
    .unwrap();
    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), targets.len());
    for (call, target) in calls.iter().zip(targets) {
        assert_eq!(call.model, target);
        assert_components(target, call.system.as_ref().unwrap(), &call.tools);
        assert_eq!(
            latest_model_identity(&orch, &call.messages).as_deref(),
            Some(target)
        );
    }
    let cached = slot.get_last().await.unwrap();
    assert_eq!(cached.system_prompt_input, calls[1].system);
    assert!(cached
        .tool_use_options
        .prompt_policy
        .as_ref()
        .unwrap()
        .tools
        .is_native());
    let (prefix, metadata) = orch.current_turn_prompt_state().await.unwrap();
    assert_eq!(
        prefix,
        ConversationOrchestrator::system_prompt_source_text(calls[1].system.as_ref().unwrap())
    );
    assert_eq!(
        metadata,
        Some(
            cached
                .tool_use_options
                .prompt_policy
                .as_ref()
                .unwrap()
                .metadata()
        )
    );
}

#[tokio::test]
async fn spawned_same_route_reuses_exact_captured_utf16_without_reresolving() {
    let api = Arc::new(RoutingApi {
        inner: MockApiClient::new(vec![]),
        resolutions: AtomicUsize::new(0),
    });
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        tool_registry(),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));
    let policy = orch
        .prompt_policy_for_mode_route("gpt-5.5", None, ConversationMode::Chat)
        .await
        .unwrap();
    let route = crate::query_model::ModelRoute {
        model: "gpt-5.5".into(),
        profile: None,
    };
    let system = Some(SystemPromptInput::native_custom_prompt(
        PromptText::from_utf16(vec![0xd800, 0x78]),
        vec![PromptText::from_utf16(vec![0xdc00, 0x79])],
    ));
    let previous = ReselectedPromptComponents {
        route: route.clone(),
        policy: Arc::clone(&policy),
        system: system.clone(),
        tools: vec![lingxi_core::host::SourceToolWireDefinition {
            name: "Read".into(),
            description: Source::parse(r#""description\ud800""#).unwrap(),
            input_schema: Source::parse(r#"{"type":"object"}"#).unwrap(),
            strict: false,
            defer_loading: false,
        }],
        available_tool_names: Some(vec!["Read".into()]),
        skip_global_cache: true,
        request_history: None,
        reminders: Default::default(),
    };
    let tools = previous.tools.clone();
    let before = api.resolutions.load(Ordering::SeqCst);
    let worker = Arc::clone(&orch);
    let selected =
        tokio::spawn(async move { worker.reselect_prompt_components(previous, route).await })
            .await
            .unwrap()
            .unwrap();
    assert_eq!(api.resolutions.load(Ordering::SeqCst), before);
    assert!(Arc::ptr_eq(&selected.policy, &policy));
    assert_eq!(selected.policy.target.mode, ConversationMode::Chat);
    assert_eq!(selected.system, system);
    assert_eq!(selected.tools, tools);
    assert!(selected.skip_global_cache);
}

#[tokio::test]
async fn changed_profile_rebuilds_tools_with_frozen_mode_and_preserves_custom_source() {
    let api = Arc::new(RoutingApi {
        inner: MockApiClient::new(vec![]),
        resolutions: AtomicUsize::new(0),
    });
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        tool_registry(),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let policy = orch
        .prompt_policy_for_mode_route(
            "shared-alias",
            Some("openai-channel"),
            ConversationMode::Chat,
        )
        .await
        .unwrap();
    let system = Some(SystemPromptInput::custom_prompt(PromptText::from_utf16(
        vec![0x78, 0xd800],
    )));
    let history = vec![lingxi_core::types::ConversationMessage::user(
        lingxi_core::types::MessageId::new(),
        "preserve this history".into(),
    )];
    orch.session.lock().await.history = history.clone();
    let selected = orch
        .reselect_prompt_components(
            ReselectedPromptComponents {
                route: crate::query_model::ModelRoute {
                    model: "shared-alias".into(),
                    profile: Some("openai-channel".into()),
                },
                policy,
                system: system.clone(),
                tools: vec![],
                available_tool_names: None,
                skip_global_cache: false,
                request_history: Some(history.clone()),
                reminders: Default::default(),
            },
            crate::query_model::ModelRoute {
                model: "shared-alias".into(),
                profile: Some("claude-channel".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(selected.policy.bundle_id, "claude-chat");
    assert_eq!(selected.policy.target.mode, ConversationMode::Chat);
    assert_eq!(selected.system, system);
    assert!(selected
        .tools
        .iter()
        .find(|tool| tool.name == "Read")
        .unwrap()
        .description
        .string_at("")
        .unwrap()
        .contains("You can access any file directly by using this tool."));
    assert_eq!(orch.session.lock().await.history, history);
}
