use super::*;
use crate::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use hooks::definition::{HookDefinition, HookExecutor as DefinitionExecutor, HookSource};
use hooks::events::HookEventType;
use hooks::executor::BuiltinHookHandler;
use hooks::mods::ModHost;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{
    HookDecision, HookOutcome, HookResponse, HookResult, ModelSwitchPermissionBehavior,
};
use hooks::HookExecutorImpl;
use lingxi_core::host::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{HookId, HttpRequest, HttpResponse, SettingsScope};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;

struct UnusedHttp;

#[async_trait::async_trait]
impl HttpTransport for UnusedHttp {
    async fn request(&self, _request: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::InvalidRequest("unused test transport".into()))
    }

    async fn stream_sse(
        &self,
        _request: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        Err(HttpError::InvalidRequest("unused test transport".into()))
    }
}

struct UnusedRuntime;

#[async_trait::async_trait]
impl RuntimeSpawner for UnusedRuntime {
    async fn spawn(
        &self,
        _name: &str,
        _task: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, RuntimeError> {
        Err(RuntimeError::Internal("unused test runtime".into()))
    }

    async fn sleep(&self, _duration: Duration) {}

    async fn cancel(
        &self,
        _handle: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

struct RecordingHook {
    id: &'static str,
    response: HookResponse,
    stdout: Option<String>,
    seen: Option<Arc<Mutex<Vec<(Vec<u16>, Vec<u16>, Option<Vec<u16>>)>>>>,
}

#[async_trait::async_trait]
impl BuiltinHookHandler for RecordingHook {
    fn id(&self) -> &str {
        self.id
    }

    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        if let (
            Some(seen),
            HookEvent::PreModelSwitch {
                from_model,
                to_model,
                requested_model,
                ..
            },
        ) = (&self.seen, event)
        {
            seen.lock().unwrap().push((
                from_model.string_units("").unwrap_or_default(),
                to_model.string_units("").unwrap_or_default(),
                requested_model
                    .as_ref()
                    .and_then(|model| model.string_units("")),
            ));
        }
        HookResult {
            outcome: HookOutcome::Success,
            stdout: self.stdout.clone().unwrap_or_default().into(),
            stderr: String::new().into(),
            exit_code: None,
            response: Some(self.response.clone()),
        }
    }
}

struct ModelSwitchContextByFromHook {
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl BuiltinHookHandler for ModelSwitchContextByFromHook {
    fn id(&self) -> &str {
        "core-by-from"
    }

    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        let HookEvent::PreModelSwitch { from_model, .. } = event else {
            unreachable!("test hook is registered only for PreModelSwitch");
        };
        let from_model = from_model.string_at("").unwrap_or_default().to_owned();
        self.seen.lock().unwrap().push(from_model.clone());
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new().into(),
            stderr: String::new().into(),
            exit_code: None,
            response: Some(HookResponse {
                additional_context: Some(format!("core-{from_model}").into()),
                ..Default::default()
            }),
        }
    }
}

fn hook_definition(name: &str, id: &str, source: HookSource) -> HookDefinition {
    HookDefinition {
        id: HookId::new(),
        name: name.to_owned(),
        events: vec![HookEventType::PreModelSwitch],
        if_condition: None,
        executor: DefinitionExecutor::Builtin {
            handler_id: id.to_owned(),
        },
        source,
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
        on_failure: None,
    }
}

async fn make_orchestrator(
    module_source: &str,
    definitions: Vec<HookDefinition>,
    handlers: Vec<Arc<dyn BuiltinHookHandler>>,
) -> (
    ConversationOrchestrator,
    Arc<MockOutputStream>,
    tempfile::TempDir,
) {
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("model-switch-mod.js");
    std::fs::write(&module, module_source).unwrap();
    let host = ModHost::start(None).await.unwrap();
    host.load(
        "model-switch-mod",
        directory.path(),
        &module,
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}").unwrap(),
    )
    .await
    .unwrap();

    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    {
        let mut registry = registry.write().await;
        registry.set_mod_host(host);
        for definition in definitions {
            registry.register(definition);
        }
    }
    let mut executor = HookExecutorImpl::new(
        registry.clone(),
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    );
    for handler in handlers {
        executor.register_builtin(handler);
    }
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        Arc::new(executor),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        directory.path().to_path_buf(),
    )
    .with_hook_registry(registry);
    (orchestrator, output, directory)
}

fn model_source_with_surrogate() -> lingxi_core::types::utf16_json::Utf16JsonProjection {
    let units = "raw-model-"
        .encode_utf16()
        .chain([0xD800])
        .collect::<Vec<_>>();
    lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
        String::from_utf16_lossy(&units),
        units,
    )
    .unwrap()
}

#[tokio::test]
async fn classic_model_switch_mod_runs_without_hook_definition_and_direct_result_uses_default() {
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async () => ({ additionalContext: ['direct'] }));
        }"#,
        Vec::new(),
        Vec::new(),
    )
    .await;

    let requested = model_source_with_surrogate();
    let from = lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6");
    let to = lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7");
    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &from,
            &to,
            Some(&requested),
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(aggregate.all_results.len(), 0);
    assert_eq!(aggregate.additional_contexts.len(), 1);
    assert_eq!(aggregate.additional_contexts[0].display, "direct");
}

#[tokio::test]
async fn classic_post_model_switch_mod_is_best_effort_and_keeps_context() {
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PostModelSwitch', async () => ({ block: 'too late', additionalContext: ['post'] }));
        }"#,
        Vec::new(),
        Vec::new(),
    )
    .await;

    let from = lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6");
    let to = lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7");
    let aggregate = orchestrator
        .run_post_model_switch_hooks(&from, &to, None, "claude-opus-4-7", None, "command")
        .await;

    assert_eq!(aggregate.decision, None);
    assert_eq!(
        aggregate
            .reason
            .as_ref()
            .map(|reason| reason.display.as_str()),
        Some("too late")
    );
    assert_eq!(aggregate.additional_contexts.len(), 1);
    assert_eq!(aggregate.additional_contexts[0].display, "post");
}

#[tokio::test]
async fn managed_model_switch_block_prevents_classic_mod_dispatch() {
    let handler = Arc::new(RecordingHook {
        id: "managed-block",
        response: HookResponse {
            decision: Some(HookDecision::Block),
            reason: Some("managed policy".into()),
            ..Default::default()
        },
        stdout: None,
        seen: None,
    });
    let (orchestrator, output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async ($, event, next) => {
            $.ui.log('mod-ran');
            return next(event);
          });
        }"#,
        vec![hook_definition(
            "managed-block",
            "managed-block",
            HookSource::Settings(SettingsScope::Managed),
        )],
        vec![handler],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;
    let logs = output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ModLog { plugin, text }
                if plugin == "model-switch-mod" =>
            {
                Some(text.string_at("").unwrap_or_default().to_owned())
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(aggregate.decision, Some(HookDecision::Block));
    assert_eq!(aggregate.all_results.len(), 1);
    assert!(
        logs.is_empty(),
        "managed block must stop before Mod dispatch"
    );
}

#[tokio::test]
async fn managed_prevent_continuation_prevents_classic_mod_dispatch() {
    let handler = Arc::new(RecordingHook {
        id: "managed-stop",
        response: HookResponse {
            prevent_continuation: true,
            ..Default::default()
        },
        stdout: None,
        seen: None,
    });
    let (orchestrator, output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async ($, event, next) => {
            $.ui.log('mod-ran');
            return next(event);
          });
        }"#,
        vec![hook_definition(
            "managed-stop",
            "managed-stop",
            HookSource::Settings(SettingsScope::Managed),
        )],
        vec![handler],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;
    let logs = output
        .snapshot()
        .await
        .into_iter()
        .filter(|event| matches!(event, lingxi_core::host::OutputEvent::ModLog { plugin, .. } if plugin == "model-switch-mod"))
        .count();

    assert!(aggregate.prevent_continuation);
    assert_eq!(
        logs, 0,
        "managed preventContinuation must stop before Mod dispatch"
    );
}

#[tokio::test]
async fn classic_model_switch_mod_next_rewrite_preserves_raw_source_and_core_effects() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = Arc::new(RecordingHook {
        id: "core-context",
        response: HookResponse {
            additional_context: Some("core-context".into()),
            ..Default::default()
        },
        stdout: None,
        seen: Some(seen.clone()),
    });
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async ($, event, next) => {
            const result = await next({
              ...event,
              from_model: 'mod-from-\ud800',
              to_model: 'mod-to-\udc00',
            });
            return { ...result, additionalContext: [...result.additionalContext, 'mod-context'] };
          });
        }"#,
        vec![hook_definition(
            "core-context",
            "core-context",
            HookSource::Settings(SettingsScope::User),
        )],
        vec![handler],
    )
    .await;

    let requested = model_source_with_surrogate();
    let expected_units = requested.string_units("").unwrap();
    let expected_from = "mod-from-"
        .encode_utf16()
        .chain([0xD800])
        .collect::<Vec<_>>();
    let expected_to = "mod-to-".encode_utf16().chain([0xDC00]).collect::<Vec<_>>();
    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            Some(&requested),
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[(expected_from, expected_to, Some(expected_units))]
    );
    assert_eq!(
        aggregate
            .additional_contexts
            .iter()
            .map(|context| context.display.as_str())
            .collect::<Vec<_>>(),
        ["core-context", "mod-context"]
    );
    assert_eq!(aggregate.all_results.len(), 1);
}

#[tokio::test]
async fn managed_ask_cannot_be_downgraded_by_core_allow() {
    let managed = Arc::new(RecordingHook {
        id: "managed-ask",
        response: HookResponse {
            decision: Some(HookDecision::Ask),
            reason: Some("managed ask".into()),
            ..Default::default()
        },
        stdout: Some(
            r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"managed ask"}}"#.into(),
        ),
        seen: None,
    });
    let core = Arc::new(RecordingHook {
        id: "core-allow",
        response: HookResponse {
            decision: Some(HookDecision::Approve),
            reason: Some("core allow".into()),
            ..Default::default()
        },
        stdout: Some(
            r#"{"hookSpecificOutput":{"permissionDecision":"allow","permissionDecisionReason":"core allow"}}"#.into(),
        ),
        seen: None,
    });
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async (_$, event, next) => next(event));
        }"#,
        vec![
            hook_definition(
                "managed-ask",
                "managed-ask",
                HookSource::Settings(SettingsScope::Managed),
            ),
            hook_definition(
                "core-allow",
                "core-allow",
                HookSource::Settings(SettingsScope::User),
            ),
        ],
        vec![managed, core],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(aggregate.decision, Some(HookDecision::Ask));
    assert_eq!(
        aggregate
            .reason
            .as_ref()
            .map(|reason| reason.display.as_str()),
        Some("managed ask")
    );
}

#[tokio::test]
async fn higher_permission_winner_does_not_borrow_lower_decision_reason() {
    let managed = Arc::new(RecordingHook {
        id: "managed-allow",
        response: HookResponse {
            decision: Some(HookDecision::Approve),
            reason: Some("allow reason".into()),
            ..Default::default()
        },
        stdout: Some(
            r#"{"hookSpecificOutput":{"permissionDecision":"allow","permissionDecisionReason":"allow reason"}}"#.into(),
        ),
        seen: None,
    });
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async () => ({ permissionDecision: 'deny' }));
        }"#,
        vec![hook_definition(
            "managed-allow",
            "managed-allow",
            HookSource::Settings(SettingsScope::Managed),
        )],
        vec![managed],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    let output = aggregate.model_switch_output.as_ref().unwrap();
    assert_eq!(
        output
            .permission_decision
            .as_ref()
            .map(|decision| decision.display.as_str()),
        Some("deny")
    );
    assert!(output.permission_decision_reason.is_none());
}

#[tokio::test]
async fn same_permission_winner_falls_back_to_other_partition_reason() {
    let managed = Arc::new(RecordingHook {
        id: "managed-ask",
        response: HookResponse {
            decision: Some(HookDecision::Ask),
            ..Default::default()
        },
        stdout: Some(r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#.into()),
        seen: None,
    });
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async () => ({
            permissionDecision: 'ask',
            permissionDecisionReason: 'mod ask reason',
          }));
        }"#,
        vec![hook_definition(
            "managed-ask",
            "managed-ask",
            HookSource::Settings(SettingsScope::Managed),
        )],
        vec![managed],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(
        aggregate
            .model_switch_output
            .as_ref()
            .and_then(|output| output.permission_decision_reason.as_ref())
            .map(|reason| reason.display.as_str()),
        Some("mod ask reason")
    );
}

#[tokio::test]
async fn classic_mod_decision_allow_overrides_explicit_permission_decision() {
    let managed = Arc::new(RecordingHook {
        id: "managed-ask",
        response: HookResponse {
            decision: Some(HookDecision::Ask),
            ..Default::default()
        },
        stdout: Some(
            r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"managed ask"}}"#.into(),
        ),
        seen: None,
    });
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async () => ({
            decision: { behavior: 'allow', updatedInput: { ignored: true } },
            permissionDecisionReason: 'not a permission reason',
          }));
        }"#,
        vec![hook_definition(
            "managed-ask",
            "managed-ask",
            HookSource::Settings(SettingsScope::Managed),
        )],
        vec![managed],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(aggregate.decision, Some(HookDecision::Approve));
    let output = aggregate.model_switch_output.as_ref().unwrap();
    assert_eq!(
        output
            .permission_decision
            .as_ref()
            .map(|decision| decision.display.as_str()),
        Some("ask")
    );
    assert_eq!(
        output.mod_permission_behavior_override,
        Some(ModelSwitchPermissionBehavior::Allow)
    );
    assert_eq!(
        output
            .permission_decision_reason
            .as_ref()
            .map(|reason| reason.display.as_str()),
        Some("managed ask")
    );
}

#[tokio::test]
async fn classic_mod_generic_deny_proceeds_without_ask_or_skip_confirm() {
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async () => ({
            decision: { behavior: 'deny', message: 'not a permission reason' },
          }));
        }"#,
        vec![],
        vec![],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(aggregate.decision, None);
    assert!(aggregate.reason.is_none());
    let output = aggregate.model_switch_output.as_ref().unwrap();
    assert!(output.permission_decision.is_none());
    assert_eq!(
        output.mod_permission_behavior_override,
        Some(ModelSwitchPermissionBehavior::Deny)
    );
}

#[tokio::test]
async fn mod_prevent_continuation_does_not_mask_generic_allow_behavior() {
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async () => ({
            preventContinuation: true,
            decision: { behavior: 'allow' },
          }));
        }"#,
        vec![],
        vec![],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert!(aggregate.prevent_continuation);
    assert_eq!(aggregate.decision, Some(HookDecision::Approve));
}

#[tokio::test]
async fn mod_prevent_continuation_does_not_mask_generic_deny_behavior() {
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async () => ({
            preventContinuation: true,
            permissionDecision: 'allow',
            decision: { behavior: 'deny' },
          }));
        }"#,
        vec![],
        vec![],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert!(aggregate.prevent_continuation);
    assert_eq!(aggregate.decision, None);
}

#[tokio::test]
async fn explicit_mod_permission_deny_still_blocks_alongside_generic_allow() {
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async () => ({
            permissionDecision: 'deny',
            permissionDecisionReason: 'explicit reason',
            decision: { behavior: 'allow', message: 'not a permission reason' },
          }));
        }"#,
        vec![],
        vec![],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(aggregate.decision, Some(HookDecision::Block));
    assert_eq!(
        aggregate
            .model_switch_output
            .as_ref()
            .and_then(|output| output.permission_decision.as_ref())
            .map(|decision| decision.display.as_str()),
        Some("deny")
    );
    assert_eq!(
        aggregate
            .reason
            .as_ref()
            .map(|reason| reason.display.as_str()),
        Some("explicit reason")
    );
}

#[tokio::test]
async fn classic_mod_generic_deny_overrides_explicit_permission_allow_without_blocking() {
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async () => ({
            permissionDecision: 'allow',
            decision: { behavior: 'deny', message: 'not a permission reason' },
          }));
        }"#,
        vec![],
        vec![],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(aggregate.decision, None);
    let output = aggregate.model_switch_output.as_ref().unwrap();
    assert_eq!(
        output
            .permission_decision
            .as_ref()
            .map(|decision| decision.display.as_str()),
        Some("allow")
    );
    assert!(output.permission_decision_reason.is_none());
    assert_eq!(
        output.mod_permission_behavior_override,
        Some(ModelSwitchPermissionBehavior::Deny)
    );
}

#[tokio::test]
async fn classic_post_mod_decision_does_not_emit_pre_permission_fields() {
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PostModelSwitch', async () => ({
            decision: { behavior: 'allow' },
          }));
        }"#,
        vec![],
        vec![],
    )
    .await;

    let aggregate = orchestrator
        .run_post_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert!(aggregate
        .model_switch_output
        .as_ref()
        .is_some_and(|output| output.permission_decision.is_none()));
}

#[tokio::test]
async fn next_returns_core_only_then_final_result_concatenates_managed_context_once() {
    let managed = Arc::new(RecordingHook {
        id: "managed-context",
        response: HookResponse {
            additional_context: Some("managed-context".into()),
            ..Default::default()
        },
        stdout: None,
        seen: None,
    });
    let core = Arc::new(RecordingHook {
        id: "core-context",
        response: HookResponse {
            additional_context: Some("core-context".into()),
            ..Default::default()
        },
        stdout: None,
        seen: None,
    });
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async (_$, event, next) => {
            const result = await next(event);
            const leaked = result.additionalContext.includes('managed-context');
            return {
              ...result,
              additionalContext: leaked
                ? ['managed-context-leaked-through-next']
                : [...result.additionalContext, 'mod-context'],
            };
          });
        }"#,
        vec![
            hook_definition(
                "managed-context",
                "managed-context",
                HookSource::Settings(SettingsScope::Managed),
            ),
            hook_definition(
                "core-context",
                "core-context",
                HookSource::Settings(SettingsScope::User),
            ),
        ],
        vec![managed, core],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(
        aggregate
            .additional_contexts
            .iter()
            .map(|context| context.display.as_str())
            .collect::<Vec<_>>(),
        ["managed-context", "core-context", "mod-context"]
    );
}

#[tokio::test]
async fn next_result_omits_empty_additional_context_property() {
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async (_$, event, next) => {
            const result = await next(event);
            const present = Object.prototype.hasOwnProperty.call(result, 'additionalContext');
            return { additionalContext: [present ? 'present' : 'absent'] };
          });
        }"#,
        vec![],
        vec![],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(
        aggregate
            .additional_contexts
            .iter()
            .map(|context| context.display.as_str())
            .collect::<Vec<_>>(),
        ["absent"]
    );
}

#[tokio::test]
async fn direct_mod_appends_to_managed_context_and_cannot_replace_managed_stop_reason() {
    let managed = Arc::new(RecordingHook {
        id: "managed-context",
        response: HookResponse {
            additional_context: Some("managed-context".into()),
            ..Default::default()
        },
        stdout: Some(r#"{"stopReason":"managed-stop"}"#.into()),
        seen: None,
    });
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async () => ({
            additionalContext: ['direct-context'],
            stopReason: 'direct-stop',
          }));
        }"#,
        vec![hook_definition(
            "managed-context",
            "managed-context",
            HookSource::Settings(SettingsScope::Managed),
        )],
        vec![managed],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(
        aggregate
            .additional_contexts
            .iter()
            .map(|context| context.display.as_str())
            .collect::<Vec<_>>(),
        ["managed-context", "direct-context"]
    );
    assert_eq!(
        aggregate
            .model_switch_output
            .as_ref()
            .and_then(|output| output.stop_reason.as_ref())
            .map(|reason| reason.display.as_str()),
        Some("managed-stop")
    );
    assert!(!aggregate.prevent_continuation);
}

#[tokio::test]
async fn multiple_next_calls_keep_last_core_yields_and_append_returned_branch_context() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let core = Arc::new(ModelSwitchContextByFromHook { seen: seen.clone() });
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async (_$, event, next) => {
            const first = await next({ ...event, from_model: 'first' });
            await next({ ...event, from_model: 'second' });
            return first;
          });
        }"#,
        vec![hook_definition(
            "core-by-from",
            "core-by-from",
            HookSource::Settings(SettingsScope::User),
        )],
        vec![core],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(seen.lock().unwrap().as_slice(), ["first", "second"]);
    // Native b9n overwrites its single `core` slot for each `next()` call;
    // k9n keeps only the last core yields, then c9n appends the returned
    // first-branch context as a Mod result.
    assert_eq!(aggregate.all_results.len(), 1);
    assert_eq!(
        aggregate
            .additional_contexts
            .iter()
            .map(|context| context.display.as_str())
            .collect::<Vec<_>>(),
        ["core-second", "core-first"]
    );
}

#[tokio::test]
async fn mod_can_rewrite_unmanaged_core_stop_reason() {
    let core = Arc::new(RecordingHook {
        id: "core-stop-reason",
        response: HookResponse::default(),
        stdout: Some(r#"{"stopReason":"core-stop"}"#.into()),
        seen: None,
    });
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async (_$, event, next) => {
            const result = await next(event);
            return { ...result, stopReason: 'mod-stop' };
          });
        }"#,
        vec![hook_definition(
            "core-stop-reason",
            "core-stop-reason",
            HookSource::Settings(SettingsScope::User),
        )],
        vec![core],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert_eq!(
        aggregate
            .model_switch_output
            .as_ref()
            .and_then(|output| output.stop_reason.as_ref())
            .map(|reason| reason.display.as_str()),
        Some("mod-stop")
    );
}

#[tokio::test]
async fn mod_can_remove_unmanaged_core_stop_reason() {
    let core = Arc::new(RecordingHook {
        id: "core-stop-reason",
        response: HookResponse::default(),
        stdout: Some(r#"{"stopReason":"core-stop"}"#.into()),
        seen: None,
    });
    let (orchestrator, _output, _directory) = make_orchestrator(
        r#"export function register(on) {
          on('classic.PreModelSwitch', async (_$, event, next) => {
            const { stopReason, ...result } = await next(event);
            return result;
          });
        }"#,
        vec![hook_definition(
            "core-stop-reason",
            "core-stop-reason",
            HookSource::Settings(SettingsScope::User),
        )],
        vec![core],
    )
    .await;

    let aggregate = orchestrator
        .run_pre_model_switch_hooks(
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-sonnet-4-6"),
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from("claude-opus-4-7"),
            None,
            "claude-opus-4-7",
            None,
            "command",
        )
        .await;

    assert!(aggregate
        .model_switch_output
        .as_ref()
        .is_some_and(|output| output.stop_reason.is_none()));
}
