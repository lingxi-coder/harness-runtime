//! Queued turn entries keep the model catalog stable until the turn ends.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lingxi_core::types::ConversationMessage;
use llm_runtime::{ClientConfig, HistoryEvent, HistoryResponse, LlmError, ModelRuntime};
use orchestrator::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};
use orchestrator::{
    ConversationOrchestrator, OrchestratorApiClient, OrchestratorApiRequest, OrchestratorConfig,
    QueuedPromptInput, StreamingApiClient,
};
use tokio_util::sync::CancellationToken;
use tool_api::registry::ToolRegistry;

#[derive(Debug)]
struct ChangingCatalog;

impl llm_runtime::client::ModelRuntimeSource for ChangingCatalog {
    fn current(&self, template: &ModelRuntime) -> Arc<ModelRuntime> {
        // A fresh Arc on every source read models a newly published catalog.
        Arc::new(template.clone())
    }
}

struct CatalogProbe {
    template: ModelRuntime,
    observed: Mutex<Vec<Arc<ModelRuntime>>>,
}

impl CatalogProbe {
    fn new() -> Self {
        Self {
            template: ModelRuntime::from_config(ClientConfig::default())
                .unwrap()
                .with_catalog_source(Arc::new(ChangingCatalog)),
            observed: Mutex::new(Vec::new()),
        }
    }

    fn observe(&self) {
        let first = self.template.current();
        let second = self.template.current();
        assert!(
            Arc::ptr_eq(&first, &second),
            "a turn must retain the catalog selected on its first lookup"
        );
        self.observed.lock().unwrap().push(first);
    }

    fn assert_refreshed_between_turns(&self) {
        let observed = self.observed.lock().unwrap();
        assert_eq!(observed.len(), 2);
        assert!(
            !Arc::ptr_eq(&observed[0], &observed[1]),
            "the next turn must resolve the newly published catalog"
        );
    }
}

struct ProbeApi(Arc<CatalogProbe>);

#[async_trait]
impl OrchestratorApiClient for ProbeApi {
    async fn messages_create(
        &self,
        _request: OrchestratorApiRequest,
    ) -> Result<HistoryResponse, LlmError> {
        self.0.observe();
        Ok(mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "done".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        ))
    }
}

struct ProbeStream {
    probe: Arc<CatalogProbe>,
    inner: MockStreamingApiClient,
}

#[async_trait]
impl StreamingApiClient for ProbeStream {
    async fn stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<llm_runtime::ToolDeclaration>,
        query_source: &str,
        skip_global_cache_for_system_prompt: bool,
        admission: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.probe.observe();
        self.inner
            .stream(
                model,
                profile,
                system,
                messages,
                tools,
                query_source,
                skip_global_cache_for_system_prompt,
                admission,
            )
            .await
    }
}

fn queued(text: &str) -> Vec<QueuedPromptInput> {
    vec![QueuedPromptInput {
        text: text.into(),
        ..Default::default()
    }]
}

#[tokio::test]
async fn queued_batched_turns_freeze_then_refresh_catalog() {
    let probe = Arc::new(CatalogProbe::new());
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(ProbeApi(probe.clone())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));
    for text in ["first", "second"] {
        orch.run_queued_prompt_batch_batched(queued(text), CancellationToken::new())
            .await
            .unwrap();
    }
    probe.assert_refreshed_between_turns();
}

#[tokio::test]
async fn queued_streaming_turns_freeze_then_refresh_catalog() {
    let probe = Arc::new(CatalogProbe::new());
    let turn = |id| {
        vec![
            message_start(id, "claude-sonnet-4-6"),
            content_block_start_text(0),
            text_delta(0, "done"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]
    };
    let streaming = Arc::new(ProbeStream {
        probe: probe.clone(),
        inner: MockStreamingApiClient::with_turns(vec![turn("one"), turn("two")]),
    });
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        streaming,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));
    for text in ["first", "second"] {
        orch.run_queued_prompt_batch(queued(text), CancellationToken::new())
            .await
            .unwrap();
    }
    probe.assert_refreshed_between_turns();
}
