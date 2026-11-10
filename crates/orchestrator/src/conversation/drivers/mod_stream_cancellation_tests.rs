use super::*;
use crate::conversation::TurnOutcome;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct NotifyOnDrop(Arc<tokio::sync::Notify>);
impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

struct PendingStream {
    first: Option<Vec<HistoryEvent>>,
    calls: AtomicUsize,
    started: Arc<tokio::sync::Notify>,
    dropped: Arc<tokio::sync::Notify>,
}
impl PendingStream {
    fn new(first: Option<Vec<HistoryEvent>>) -> Arc<Self> {
        Arc::new(Self {
            first,
            calls: AtomicUsize::new(0),
            started: Arc::new(tokio::sync::Notify::new()),
            dropped: Arc::new(tokio::sync::Notify::new()),
        })
    }
}
#[async_trait::async_trait]
impl crate::conversation::StreamingApiClient for PendingStream {
    async fn stream(
        &self,
        _: &str,
        _: Option<&str>,
        _: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        _: Vec<ConversationMessage>,
        _: Vec<llm_runtime::ToolDeclaration>,
        _: &str,
        _: bool,
        _: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            if let Some(first) = &self.first {
                return Ok(futures::stream::iter(first.clone().into_iter().map(Ok)).boxed());
            }
        }
        let _drop = NotifyOnDrop(self.dropped.clone());
        self.started.notify_one();
        std::future::pending().await
    }
}

async fn host(root: &std::path::Path) -> Arc<hooks::mods::ModHost> {
    let module = root.join("stream-cancel.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('turn.step', async function* ($, e, next) {
        for (let i=0;i<2;i++) {
          const below=next(e);
          for await (const chunk of below) yield chunk;
          await below.result;
        }
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "stream-cancel",
        root,
        &module,
        Utf16JsonProjection::parse("{}").unwrap(),
    )
    .await
    .unwrap();
    host
}

async fn raw_stream(
    root: &std::path::Path,
    host: Arc<hooks::mods::ModHost>,
    api: Arc<PendingStream>,
    cancel: Option<CancellationToken>,
) -> futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>> {
    let main_api = Arc::new(MockApiClient::new(vec![]));
    let output = Arc::new(MockOutputStream::new());
    let owner = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            crate::OrchestratorConfig {
                model: "model".into(),
                ..Default::default()
            },
            main_api.clone(),
            api.clone(),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            root.to_path_buf(),
        )
        .without_host_environment_probes(),
    );
    let route = crate::query_model::ModelRoute {
        model: "model".into(),
        profile: None,
    };
    let policy = owner
        .prompt_policy_for_route(&route.model, route.profile.as_deref())
        .await
        .expect("the controlled provider route has a prompt policy");
    let captured_components = ReselectedPromptComponents {
        route,
        policy,
        system: None,
        tools: vec![],
        available_tool_names: Some(vec![]),
        skip_global_cache: false,
        request_history: Some(vec![]),
        reminders: Default::default(),
    };
    mod_turn_step_stream(
        owner,
        captured_components,
        host,
        api,
        output,
        main_api,
        None,
        Default::default(),
        Default::default(),
        "model".into(),
        serde_json::json!({"model":"model","turnId":"cancel","index":0}),
        None,
        None,
        None,
        vec![],
        cancel,
        None,
        vec![],
        vec![],
        vec![],
        false,
        "sdk".into(),
        None,
        ModelCallRetryScope::default(),
        Default::default(),
        None,
        None,
        Default::default(),
        Default::default(),
    )
    .0
}

#[tokio::test]
async fn dropping_mod_stream_aborts_its_pending_provider_worker() {
    let root = tempfile::tempdir().unwrap();
    let api = PendingStream::new(None);
    let stream = raw_stream(root.path(), host(root.path()).await, api.clone(), None).await;
    tokio::time::timeout(Duration::from_secs(10), api.started.notified())
        .await
        .unwrap();
    drop(stream);
    tokio::time::timeout(Duration::from_secs(1), api.dropped.notified())
        .await
        .expect("dropping even an unpolled consumer owns and stops the provider worker");
    assert_eq!(api.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelling_mod_stream_open_joins_the_provider_worker() {
    let root = tempfile::tempdir().unwrap();
    let api = PendingStream::new(None);
    let cancel = CancellationToken::new();
    let mut stream = raw_stream(
        root.path(),
        host(root.path()).await,
        api.clone(),
        Some(cancel.clone()),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), api.started.notified())
        .await
        .unwrap();
    cancel.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .unwrap()
        .is_none());
    tokio::time::timeout(Duration::from_secs(1), api.dropped.notified())
        .await
        .expect("the pending physical request has been dropped before stream completion");
    assert_eq!(api.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn streaming_mod_cancel_after_completed_response_keeps_usage_and_history() {
    let root = tempfile::tempdir().unwrap();
    let mut response = mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "accepted answer".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    response.usage.counts_mut().input_tokens = 11;
    response.usage.counts_mut().output_tokens = 7;
    let api = PendingStream::new(Some(vec![HistoryEvent::Completed {
        response: Box::new(response),
    }]));
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host(root.path()).await);
    let base = ConversationOrchestrator::new_with_streaming(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        api.clone(),
        Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let session_id = base.session.lock().await.session_id;
    let (persist, _receiver) = tokio::sync::mpsc::channel(8);
    let tracker = Arc::new(cost::CostTracker::new(
        session_id,
        Arc::new(cost::PricingCatalog::builtin_reference()),
        persist,
    ));
    let orch = ConversationOrchestrator::into_shared(base.with_cost_tracker(tracker.clone()));
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let orch = orch.clone();
        let cancel = cancel.clone();
        async move {
            orch.run_turn_streaming_with_cancel("question", cancel)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(10), api.started.notified())
        .await
        .unwrap();
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("a second pending Mod request must not hold the turn")
            .unwrap()
            .unwrap(),
        TurnOutcome::Cancelled
    );
    tokio::time::timeout(Duration::from_secs(1), api.dropped.notified())
        .await
        .unwrap();
    assert_eq!(tracker.snapshot().await.cost_revision, 1);
    assert!(orch.snapshot_history().await.iter().any(|message| matches!(
        message,
        ConversationMessage::Assistant { .. }
    ) && message.text_content()
        == "accepted answer"));
    assert!(
        tokio::time::timeout(Duration::from_secs(1), orch.turn_gate.lock())
            .await
            .is_ok()
    );
}
