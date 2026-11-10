use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::host::OrchestratorHandle;
use std::sync::atomic::{AtomicUsize, Ordering};

struct HeldConfigChangeEvaluator {
    calls: AtomicUsize,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl hooks::HookPromptRunner for HeldConfigChangeEvaluator {
    async fn run(
        &self,
        _: hooks::PromptHookRequest,
    ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, hooks::PromptHookError> {
        let reason = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.notify_one();
            self.release.notified().await;
            // A retired run must not create a large-output file either.
            format!(
                "retired config marker {}",
                "x".repeat(hooks::attachment::HOOK_OUTPUT_INLINE_LIMIT + 1)
            )
        } else {
            "current config marker".to_owned()
        };
        Ok(serde_json::json!({"ok": true, "reason": reason})
            .to_string()
            .into())
    }
}

#[tokio::test]
async fn held_config_change_cannot_publish_attachments_after_actual_session_clear() {
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let writer = Arc::new(session::jsonl::JsonlWriter::new(
        cwd.join("initial.jsonl"),
        Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone())),
    ));
    let sink = Arc::new(crate::JsonlHookAttachmentSink::new());
    let evaluator = Arc::new(HeldConfigChangeEvaluator {
        calls: AtomicUsize::new(0),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let executor = Arc::new(
        Arc::try_unwrap(noop_hook_executor())
            .ok()
            .unwrap()
            .with_prompt_runner(evaluator.clone())
            .with_attachment_sink(sink.clone()),
    );
    let definition = hooks::HookDefinition {
        id: HookId::new(),
        name: "held-config-change".into(),
        events: vec![hooks::HookEventType::ConfigChange],
        if_condition: None,
        executor: hooks::HookExecutor::Prompt {
            prompt: "check configuration $ARGUMENTS".into(),
            model: None,
            continue_on_block: false,
        },
        source: hooks::HookSource::Settings(lingxi_core::types::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
        on_failure: None,
    };
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::ToolRegistry::new()),
            executor,
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_config_home(home.path().to_path_buf())
        .with_jsonl_writer(writer.clone()),
    );
    orch.attach_owned_session_switches();
    sink.attach(&orch);
    let old_session = orch.current_session_id().await;
    orch.hooks
        .upsert_session_named_hook(old_session, "held-config-change".into(), definition.clone())
        .await;
    let old_context = orch.lifecycle_hook_ctx(false).await;
    let running = orch.clone();
    let hook = tokio::spawn(async move {
        running
            .fire_config_change(hooks::events::ConfigChangeSource::UserSettings, None)
            .await;
    });
    tokio::time::timeout(Duration::from_secs(2), evaluator.started.notified())
        .await
        .unwrap();
    orch.clear_session().await.unwrap();
    assert_ne!(orch.current_session_id().await, old_session);
    assert!(!old_context.publication_guard.unwrap().is_current());
    let new_path = writer.active_path();
    let before_release = std::fs::read_to_string(&new_path).unwrap_or_default();
    evaluator.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), hook)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&new_path).unwrap_or_default(),
        before_release,
        "a retired lifecycle result must not append to the activated transcript"
    );
    let outputs = crate::tool_result_persistence::tool_results_dir(
        home.path(),
        &cwd.to_string_lossy(),
        &orch.current_session_id().await.as_uuid().to_string(),
    );
    assert!(
        !outputs.exists(),
        "retired output must not create {}",
        outputs.display()
    );

    // Session subscriptions are correctly removed by clear; install the same
    // current policy to verify a live generation still persists its output.
    orch.hooks
        .upsert_session_named_hook(
            orch.current_session_id().await,
            "held-config-change".into(),
            definition,
        )
        .await;
    orch.fire_config_change(hooks::events::ConfigChangeSource::UserSettings, None)
        .await;
    let current = std::fs::read_to_string(new_path).unwrap();
    assert!(current.contains("current config marker"), "{current}");
    assert!(!current.contains("retired config marker"));
    assert_eq!(evaluator.calls.load(Ordering::SeqCst), 2);
}
