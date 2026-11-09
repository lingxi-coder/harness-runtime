use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::host::prompt_policy::{
    PromptTarget, PromptTemplateOverride, ReminderPromptBinding,
};
use lingxi_core::host::session_context::ConversationMode;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use std::sync::atomic::{AtomicUsize, Ordering};

struct RoutingApi {
    inner: MockApiClient,
    resolutions: AtomicUsize,
}

impl RoutingApi {
    fn new() -> Self {
        Self {
            inner: MockApiClient::new(Vec::new()),
            resolutions: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl OrchestratorApiClient for RoutingApi {
    fn prompt_target(
        &self,
        model: &str,
        profile: Option<&str>,
        mode: ConversationMode,
    ) -> Result<PromptTarget, LlmError> {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        let (provider, underlying) = match profile {
            Some("claude-channel") => ("openrouter", Some("claude-sonnet-4-6")),
            Some("kimi-channel") => ("kimi", Some("kimi-k2.5")),
            Some("openai") => ("openai", None),
            _ => ("anthropic", None),
        };
        Ok(PromptTarget::from_route(
            mode, provider, model, underlying, None, profile,
        ))
    }

    async fn messages_create(
        &self,
        request: OrchestratorApiRequest,
    ) -> Result<llm_runtime::HistoryResponse, LlmError> {
        self.inner.messages_create(request).await
    }
}

fn orchestrator(
    api: Arc<dyn OrchestratorApiClient>,
    config: OrchestratorConfig,
) -> ConversationOrchestrator {
    let mut registry = tool_api::registry::ToolRegistry::new();
    let context = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        Vec::new(),
    );
    registry.register_builtin(Arc::new(tool_file::read::FileReadTool::new(context)));
    orchestrator_with_registry(api, config, registry)
}

fn orchestrator_with_registry(
    api: Arc<dyn OrchestratorApiClient>,
    config: OrchestratorConfig,
    registry: tool_api::registry::ToolRegistry,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        config,
        api,
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

fn task_registry() -> tool_api::registry::ToolRegistry {
    let context = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        Vec::new(),
    );
    let mut registry = tool_api::registry::ToolRegistry::new();
    registry.register_builtin(Arc::new(tool_file::read::FileReadTool::new(
        context.clone(),
    )));
    registry.register_builtin(Arc::new(tool_task::TaskCreateTool::new(context.clone())));
    registry.register_builtin(Arc::new(tool_task::TaskUpdateTool::new(context)));
    registry
}

struct OnceTaskNotifications {
    calls: AtomicUsize,
}

#[async_trait]
impl crate::prompt::task_notification::TaskNotificationProvider for OnceTaskNotifications {
    async fn take_pending_task_notifications(
        &self,
    ) -> Vec<lingxi_core::host::task_registry::TaskNotification> {
        if self.calls.fetch_add(1, Ordering::SeqCst) != 0 {
            return Vec::new();
        }
        vec![lingxi_core::host::task_registry::TaskNotification {
            task_id: "bprompt-fallback".into(),
            task_type: "local_bash".into(),
            status: "completed".into(),
            description: "ONE SHOT COMPLETION".into(),
            output_path: Some("/tmp/tasks/bprompt-fallback.output".into()),
            exit_code: Some(0),
            ..Default::default()
        }]
    }
}

#[tokio::test]
async fn prompt_policy_is_resolved_once_per_request_and_profile() {
    let api = Arc::new(RoutingApi::new());
    let orch = orchestrator(api.clone(), OrchestratorConfig::default());
    crate::request_context::scope_if_missing(&orch, async {
        let (first, retry) = tokio::join!(
            orch.prompt_policy_for_route("shared-alias", Some("claude-channel")),
            orch.prompt_policy_for_route("shared-alias", Some("claude-channel")),
        );
        let first = first.unwrap();
        assert!(Arc::ptr_eq(&first, &retry.unwrap()));
        assert_eq!(api.resolutions.load(Ordering::SeqCst), 1);
        assert!(first.tools.is_native());
        let other = orch
            .prompt_policy_for_route("shared-alias", Some("kimi-channel"))
            .await?;
        assert_eq!(other.target.provider_id, "kimi");
        assert!(!other.tools.is_native());
        assert_ne!(first.fingerprint(), other.fingerprint());
        assert_eq!(api.resolutions.load(Ordering::SeqCst), 2);
        Ok(())
    })
    .await
    .unwrap();
    crate::request_context::scope_if_missing(&orch, async {
        orch.prompt_policy_for_route("shared-alias", Some("claude-channel"))
            .await?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        api.resolutions.load(Ordering::SeqCst),
        3,
        "a new request re-evaluates the route"
    );
}

#[tokio::test]
async fn prompt_policy_concurrent_requests_do_not_share_routes_or_pending_sources() {
    let orch = orchestrator(Arc::new(RoutingApi::new()), OrchestratorConfig::default());
    let request = |profile: &'static str, marker: &'static str| {
        let orch = &orch;
        async move {
            crate::request_context::scope_if_missing(orch, async {
                let policy = orch
                    .prompt_policy_for_route("shared-alias", Some(profile))
                    .await?;
                crate::request_context::set_pending_prompt_source_vector(Some(vec![marker.into()]));
                tokio::task::yield_now().await;
                let vector = crate::request_context::take_pending_prompt_source_vector()
                    .unwrap()
                    .unwrap();
                assert_eq!(vector[0].display_text(), marker);
                Ok(policy)
            })
            .await
            .unwrap()
        }
    };
    let (claude, kimi) = tokio::join!(
        request("claude-channel", "native"),
        request("kimi-channel", "portable")
    );
    assert!(claude.tools.is_native());
    assert!(!kimi.tools.is_native());
}

#[tokio::test]
async fn prompt_policy_invalidates_legacy_and_changed_snapshots_without_changing_history() {
    let orch = orchestrator(Arc::new(RoutingApi::new()), OrchestratorConfig::default());
    let message = ConversationMessage::user(MessageId::new(), "preserve this request".into());
    orch.session.lock().await.history.push(message.clone());
    let native = orch
        .prompt_policy_for_route("alias", Some("claude-channel"))
        .await
        .unwrap();
    let mut snapshot = lingxi_core::host::PromptSnapshot {
        system_prompt: vec!["old source".into()],
        ..Default::default()
    };
    *orch.prompt_runtime.prompt_snapshot.lock().await = Some(snapshot.clone());
    orch.prompt_runtime
        .prompt_snapshot_resume
        .store(true, Ordering::Release);
    orch.reconcile_prompt_snapshot_policy(&native).await;
    assert!(orch.prompt_runtime.prompt_snapshot.lock().await.is_none());
    assert!(!orch.prompt_snapshot_resume());
    snapshot.prompt_policy = Some(native.metadata());
    *orch.prompt_runtime.prompt_snapshot.lock().await = Some(snapshot.clone());
    orch.reconcile_prompt_snapshot_policy(&native).await;
    assert_eq!(
        orch.prompt_runtime.prompt_snapshot.lock().await.as_ref(),
        Some(&snapshot)
    );
    let portable = orch
        .prompt_policy_for_route("alias", Some("kimi-channel"))
        .await
        .unwrap();
    orch.reconcile_prompt_snapshot_policy(&portable).await;
    assert!(orch.prompt_runtime.prompt_snapshot.lock().await.is_none());
    assert_eq!(orch.session.lock().await.history, vec![message]);
    let malformed: lingxi_core::host::PromptSnapshot = serde_json::from_value(serde_json::json!({
        "systemPrompt": ["retained for migration"], "promptPolicy": "old-or-invalid"
    }))
    .unwrap();
    assert!(malformed.prompt_policy.is_none());
}

struct SwitchingPreparer(std::sync::Mutex<(String, Option<String>)>);

#[async_trait]
impl ModelCallPreparer for SwitchingPreparer {
    async fn prepare(
        &self,
        _orch: &ConversationOrchestrator,
        _path: ModelCallPath,
        _system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        _cancel: Option<&CancellationToken>,
        mut draft: PreparedModelCall,
    ) -> Result<PreparedModelCall, OrchestratorError> {
        (draft.model, draft.model_profile) = self.0.lock().unwrap().clone();
        Ok(draft)
    }
}

#[tokio::test]
async fn prompt_policy_fallback_removes_unavailable_task_reminders_without_redraining() {
    let preparer = Arc::new(SwitchingPreparer(std::sync::Mutex::new((
        "gpt-test".into(),
        Some("openai".into()),
    ))));
    let notifications = Arc::new(OnceTaskNotifications {
        calls: AtomicUsize::new(0),
    });
    let orch = orchestrator_with_registry(
        Arc::new(RoutingApi::new()),
        OrchestratorConfig {
            model: "gpt-test".into(),
            ..Default::default()
        },
        task_registry(),
    )
    .with_model_call_preparer(preparer.clone())
    .with_task_notifications(notifications.clone());
    {
        let mut session = orch.session.lock().await;
        session.history.push(ConversationMessage::user(
            MessageId::new(),
            "continue the task".into(),
        ));
        session.turns_since_last_todo_write = tool_task::reminder::TURNS_SINCE_WRITE;
        session.turns_since_last_reminder = tool_task::reminder::TURNS_BETWEEN_REMINDERS;
    }
    crate::request_context::scope_if_missing(&orch, async {
        let baseline = orch.provider_system_prompt().await?;
        let step = orch
            .prepare_turn_step(ModelCallPath::Streaming, Some(&baseline), true, true, None)
            .await?;
        assert!(step.wire_tools.iter().any(|tool| tool.name == "TaskCreate"));
        assert!(step.wire_tools.iter().any(|tool| tool.name == "TaskUpdate"));
        let task_reminder = step
            .turn_reminders
            .iter()
            .find(|message| {
                message
                    .text_content()
                    .contains("The task tools haven't been used")
            })
            .expect("the generic model has an eligible task reminder");
        let completion = step
            .snapshot
            .iter()
            .find(|message| message.text_content().contains("bprompt-fallback"))
            .expect("the task completion was drained once")
            .clone();
        let retry = orch
            .reprepare_model_fallback(step.effective_system_prompt.as_ref(), None, &step)
            .await?;
        assert_eq!(retry.turn_reminders, step.turn_reminders);
        *preparer.0.lock().unwrap() = ("claude-opus-5".into(), None);
        let fallback = orch
            .reprepare_model_fallback(retry.effective_system_prompt.as_ref(), None, &retry)
            .await?;
        assert!(fallback.prompt_policy.tools.is_native());
        for tool in ["TaskCreate", "TaskUpdate"] {
            assert!(!fallback.wire_tools.iter().any(|entry| entry.name == tool));
        }
        assert!(!fallback
            .turn_reminders
            .iter()
            .any(|message| message.id() == task_reminder.id()));
        assert!(!fallback.snapshot.iter().any(|message| message
            .text_content()
            .contains("The task tools haven't been used")));
        assert_eq!(
            fallback
                .snapshot
                .iter()
                .filter(|message| message.id() == completion.id())
                .count(),
            1,
        );
        assert!(fallback.snapshot.contains(&completion));
        assert_eq!(notifications.calls.load(Ordering::SeqCst), 1);
        assert_eq!(orch.session.lock().await.turns_since_last_reminder, 0);
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn prompt_policy_fallback_rerenders_reminder_overrides_from_original_sources() {
    const OLD: &[PromptTemplateOverride] = &[PromptTemplateOverride {
        key: "todo_reminder",
        template: "OLD BUNDLE TASK GUIDANCE",
    }];
    const NEW: &[PromptTemplateOverride] = &[PromptTemplateOverride {
        key: "todo_reminder",
        template: "NEW BUNDLE TASK GUIDANCE",
    }];
    let preparer = Arc::new(SwitchingPreparer(std::sync::Mutex::new((
        "gpt-test".into(),
        Some("openai".into()),
    ))));
    let orch = orchestrator_with_registry(
        Arc::new(RoutingApi::new()),
        OrchestratorConfig {
            model: "gpt-test".into(),
            ..Default::default()
        },
        task_registry(),
    )
    .with_model_call_preparer(preparer.clone());
    {
        let mut session = orch.session.lock().await;
        session.history.push(ConversationMessage::user(
            MessageId::new(),
            "continue".into(),
        ));
        session.turns_since_last_todo_write = tool_task::reminder::TURNS_SINCE_WRITE;
        session.turns_since_last_reminder = tool_task::reminder::TURNS_BETWEEN_REMINDERS;
    }
    crate::request_context::scope_if_missing(&orch, async {
        let mut old_policy = (*orch
            .prompt_policy_for_route("gpt-test", Some("openai"))
            .await?)
            .clone();
        old_policy.reminders = ReminderPromptBinding::Portable {
            base: &[],
            overrides: OLD,
        };
        let route = crate::query_model::ModelRoute {
            model: "gpt-test".into(),
            profile: Some("openai".into()),
        };
        let step = super::prompt_selection::PROMPT_POLICY_OVERRIDE
            .scope((route, Arc::new(old_policy)), async {
                orch.prepare_turn_step(ModelCallPath::Streaming, None, true, true, None)
                    .await
            })
            .await?;
        let original = step
            .turn_reminders
            .iter()
            .find(|message| message.text_content().contains("OLD BUNDLE TASK GUIDANCE"))
            .expect("the first policy renders its own reminder");
        *preparer.0.lock().unwrap() = ("kimi-k2.5".into(), Some("kimi-channel".into()));
        let mut new_policy = (*orch
            .prompt_policy_for_route("kimi-k2.5", Some("kimi-channel"))
            .await?)
            .clone();
        new_policy.reminders = ReminderPromptBinding::Portable {
            base: &[],
            overrides: NEW,
        };
        let route = crate::query_model::ModelRoute {
            model: "kimi-k2.5".into(),
            profile: Some("kimi-channel".into()),
        };
        let fallback = super::prompt_selection::PROMPT_POLICY_OVERRIDE
            .scope((route, Arc::new(new_policy)), async {
                orch.reprepare_model_fallback(step.effective_system_prompt.as_ref(), None, &step)
                    .await
            })
            .await?;
        let replacement = fallback
            .turn_reminders
            .iter()
            .find(|message| message.id() == original.id())
            .expect("the eligible reminder retains its original identity");
        assert!(replacement
            .text_content()
            .contains("NEW BUNDLE TASK GUIDANCE"));
        assert!(!fallback
            .snapshot
            .iter()
            .any(|message| message.text_content().contains("OLD BUNDLE TASK GUIDANCE")));
        assert_eq!(orch.session.lock().await.turns_since_last_reminder, 0);
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn prompt_policy_model_preparation_and_fallback_rebuild_the_actual_system() {
    let preparer = Arc::new(SwitchingPreparer(std::sync::Mutex::new((
        "claude-sonnet-4-6".into(),
        None,
    ))));
    let orch = orchestrator(
        Arc::new(RoutingApi::new()),
        OrchestratorConfig {
            model: "claude-sonnet-4-6".into(),
            ..Default::default()
        },
    )
    .with_model_call_preparer(preparer.clone());
    crate::request_context::scope_if_missing(&orch, async {
        let announced = orch
            .model_prompt_ingress_attachment_for("claude-sonnet-4-6", None)
            .await
            .unwrap();
        let announced_id = announced.message.id();
        orch.persist_prompt_ingress_reminder(&announced).await;
        let baseline = orch.provider_system_prompt().await?;
        let step = orch
            .prepare_turn_step(ModelCallPath::Streaming, Some(&baseline), true, true, None)
            .await?;
        assert!(step.prompt_policy.tools.is_native());
        assert!(
            step.deferred_context_announcement_rows
                .iter()
                .all(|(_, payload)| payload.string_at("/type") != Some("model")),
            "same-route Claude preparation keeps the captured identity bytes"
        );
        assert_eq!(
            ConversationOrchestrator::system_prompt_source_text(
                step.effective_system_prompt.as_ref().unwrap()
            ),
            ConversationOrchestrator::system_prompt_source_text(&baseline),
            "same-route retries preserve the original source vector",
        );
        *preparer.0.lock().unwrap() = ("gpt-test".into(), Some("openai".into()));
        let mut fallback = orch
            .reprepare_model_fallback(step.effective_system_prompt.as_ref(), None, &step)
            .await?;
        assert_eq!(fallback.prompt_policy.bundle_id, "generic-code-lean-v1");
        assert_eq!(fallback.prompt_policy.target.provider_id, "openai");
        assert_eq!(fallback.model, "gpt-test");
        let actual_model = fallback
            .deferred_context_announcement_rows
            .iter()
            .rev()
            .find(|(_, payload)| payload.string_at("/type") == Some("model"))
            .unwrap();
        assert_eq!(
            actual_model.1.string_at("/identity/modelId"),
            Some("gpt-test")
        );
        assert_eq!(fallback.snapshot.last().unwrap().id(), actual_model.0.id());
        let actual_model_id = actual_model.0.id();
        assert!(
            fallback
                .snapshot
                .iter()
                .any(|message| message.id() == announced_id),
            "the earlier typed identity remains in history"
        );
        orch.persist_prepared_turn_attachments(&mut fallback).await;
        assert_eq!(
            orch.transcript
                .model_reminder_attachments
                .lock()
                .unwrap()
                .get(&actual_model_id)
                .unwrap()
                .string_at("/identity/modelId"),
            Some("gpt-test")
        );
        assert_eq!(step.wire_tools[0].name, "Read");
        assert_eq!(fallback.wire_tools[0].name, "Read");
        assert_ne!(
            step.wire_tools[0].description,
            fallback.wire_tools[0].description
        );
        assert!(!fallback.wire_tools[0]
            .description
            .string_at("")
            .unwrap()
            .contains("Claude"));
        let text = fallback.effective_system_prompt.unwrap().display_text();
        assert!(text.contains("# Tools and actions"));
        assert!(!text.contains("Claude"));
        assert_ne!(
            step.prompt_policy.fingerprint(),
            fallback.prompt_policy.fingerprint()
        );
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn prompt_policy_model_preparer_projects_actual_identity_before_the_first_request() {
    let orch = orchestrator(
        Arc::new(RoutingApi::new()),
        OrchestratorConfig {
            model: "claude-sonnet-4-6".into(),
            ..Default::default()
        },
    )
    .with_model_call_preparer(Arc::new(SwitchingPreparer(std::sync::Mutex::new((
        "gpt-test".into(),
        Some("openai".into()),
    )))));
    crate::request_context::scope_if_missing(&orch, async {
        let announced = orch
            .model_prompt_ingress_attachment_for("claude-sonnet-4-6", None)
            .await
            .unwrap();
        orch.persist_prompt_ingress_reminder(&announced).await;
        let baseline = orch.provider_system_prompt().await?;
        let step = orch
            .prepare_turn_step(ModelCallPath::Streaming, Some(&baseline), true, true, None)
            .await?;
        assert_eq!(step.model, "gpt-test");
        let actual = step
            .deferred_context_announcement_rows
            .iter()
            .rev()
            .find(|(_, payload)| payload.string_at("/type") == Some("model"))
            .unwrap();
        assert_eq!(actual.1.string_at("/identity/modelId"), Some("gpt-test"));
        assert_eq!(step.snapshot.last().unwrap().id(), actual.0.id());
        assert!(step
            .snapshot
            .iter()
            .any(|message| message.id() == announced.message.id()));
        assert!(step
            .snapshot
            .last()
            .unwrap()
            .text_content()
            .contains("gpt-test"));
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn prompt_policy_preserves_custom_system_utf16_and_append_instructions() {
    let orch = orchestrator(
        Arc::new(RoutingApi::new()),
        OrchestratorConfig {
            system_prompt_override: Some(Source::parse(r#""custom\ud800""#).unwrap()),
            append_system_prompt: Some("user and project additions".into()),
            ..Default::default()
        },
    );
    let source = ConversationOrchestrator::system_prompt_source_text(
        &orch.provider_system_prompt().await.unwrap(),
    );
    let mut expected: Vec<u16> = "custom".encode_utf16().collect();
    expected.push(0xd800);
    expected.extend("\n\nuser and project additions".encode_utf16());
    assert_eq!(source.string_units(""), Some(expected));
}

#[tokio::test]
async fn prompt_policy_reminder_override_reaches_the_real_attachment_pipeline() {
    const OVERRIDES: &[PromptTemplateOverride] = &[
        PromptTemplateOverride {
            key: "plan_mode",
            template: "Portable plan constraints",
        },
        PromptTemplateOverride {
            key: "unused",
            template: "",
        },
    ];
    let orch = orchestrator(Arc::new(RoutingApi::new()), OrchestratorConfig::default());
    let (model, profile) = orch.current_prompt_route().await;
    let mut policy = (*orch.current_prompt_policy().await.unwrap()).clone();
    policy.reminders = ReminderPromptBinding::Portable {
        base: &[],
        overrides: OVERRIDES,
    };
    let route = crate::query_model::ModelRoute { model, profile };
    super::prompt_selection::PROMPT_POLICY_OVERRIDE
        .scope((route, Arc::new(policy)), async {
            let original = ConversationMessage::user_meta(
                MessageId::new(),
                "<system-reminder>\nnative body\n</system-reminder>".into(),
            );
            let replacement = orch
                .mod_prompt_attachment("plan_mode", original.clone(), serde_json::json!({}))
                .await
                .unwrap();
            assert_eq!(replacement.id(), original.id());
            assert_eq!(
                replacement.text_content(),
                "<system-reminder>\nPortable plan constraints\n</system-reminder>"
            );
            assert_eq!(
                orch.mod_prompt_attachment("unrelated", original.clone(), serde_json::json!({}))
                    .await,
                Some(original.clone())
            );
            assert!(orch
                .mod_prompt_attachment("unused", original, serde_json::json!({}))
                .await
                .is_none());
        })
        .await;
}
