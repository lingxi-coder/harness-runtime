use async_trait::async_trait;
use futures::StreamExt;
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ConversationMessage, ToolUseId};
use orchestrator::conversation::StreamingApiClient;
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::test_support_stream::{
    content_block_start_tool_use, content_block_stop, message_start, message_stop,
    MockStreamingApiClient,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig, OrchestratorError};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
};

struct CleanupGate {
    started: Arc<tokio::sync::Notify>,
    cancelled: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    finished: Arc<tokio::sync::Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CleanupGate {
    fn name(&self) -> &str {
        "CleanupGate"
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: std::sync::LazyLock<Utf16JsonProjection> = std::sync::LazyLock::new(|| {
            tool_api::tool_trait::generated_builtin_schema(&json!({"type":"object"}))
        });
        &SCHEMA
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Utf16JsonProjection) -> InterruptBehavior {
        InterruptBehavior::Cancel
    }
    fn await_cleanup_on_interrupt(&self) -> bool {
        true
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "test".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "test".into()
    }
    async fn call(
        &self,
        _: Utf16JsonProjection,
        ctx: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        ctx.cancel
            .expect("tool receives its generation token")
            .cancelled()
            .await;
        self.cancelled.notify_one();
        self.release.notified().await;
        self.finished.notify_one();
        Err(ToolError::CleanupUnverified("test cleanup failed".into()))
    }
}

struct GatedFallbackClient {
    inner: MockStreamingApiClient,
    started: Arc<tokio::sync::Notify>,
    after_fallback: Arc<AtomicUsize>,
}

#[async_trait]
impl StreamingApiClient for GatedFallbackClient {
    async fn stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<llm_runtime::ToolDeclaration>,
        query_source: &str,
        skip_global_cache_for_system_prompt: bool,
        request_dispatch_admission: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let stream = self
            .inner
            .stream(
                model,
                profile,
                system,
                messages,
                tools,
                query_source,
                skip_global_cache_for_system_prompt,
                request_dispatch_admission,
            )
            .await?;
        let started = self.started.clone();
        let after_fallback = self.after_fallback.clone();
        let seen = Arc::new(AtomicBool::new(false));
        Ok(stream
            .then(move |event| {
                let started = started.clone();
                let after_fallback = after_fallback.clone();
                let seen = seen.clone();
                async move {
                    if matches!(&event, Ok(llm_runtime::HistoryEvent::ServerFallback { .. })) {
                        started.notified().await;
                        seen.store(true, Ordering::SeqCst);
                    } else if seen.load(Ordering::SeqCst) {
                        after_fallback.fetch_add(1, Ordering::SeqCst);
                    }
                    event
                }
            })
            .boxed())
    }
}

#[tokio::test]
async fn fallback_cannot_advance_while_old_tool_cleanup_is_unverified() {
    for allow_fallback in [false, true] {
        let started = Arc::new(tokio::sync::Notify::new());
        let cancelled = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let after_fallback = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CleanupGate {
            started: started.clone(),
            cancelled: cancelled.clone(),
            release: release.clone(),
            finished: finished.clone(),
            calls: calls.clone(),
        }));
        let fallback = llm_runtime::HistoryEvent::ServerFallback {
            event: Box::new(
                lingxi_llm_client::providers::anthropic::fallback_response::ServerFallbackEvent {
                    from_model: "requested-model".into(),
                    to_model: "fallback-model".into(),
                    reason: "sticky".into(),
                    api_refusal_category: None,
                    mid_stream: true,
                    request_id: Some("cleanup-fallback".into()),
                    discarded_blocks: vec![0],
                    retained_blocks: vec![],
                    retained_text: String::new(),
                    final_stop_reason: None,
                },
            ),
            profile: "anthropic-profile".into(),
            lane: lingxi_llm_client::providers::anthropic::fallback_request::ServerLane {
                for_model: "requested-model".into(),
                model: "fallback-model".into(),
                mode: lingxi_llm_client::providers::anthropic::fallback_request::LaneMode::Explicit,
            },
        };
        let stream = Arc::new(GatedFallbackClient {
            inner: MockStreamingApiClient::with_turns(vec![vec![
                message_start("m1", "requested-model"),
                content_block_start_tool_use(0, ToolUseId::from("old-tool"), "CleanupGate"),
                content_block_stop(0),
                fallback,
                content_block_start_tool_use(1, ToolUseId::from("new-tool"), "CleanupGate"),
                content_block_stop(1),
                message_stop(),
            ]]),
            started,
            after_fallback: after_fallback.clone(),
        });
        let mut allowed = vec!["requested-model".into()];
        if allow_fallback {
            allowed.push("fallback-model".into());
        }
        let config = OrchestratorConfig {
            model: "requested-model".into(),
            server_fallback_regular_available_models: Some(
                Utf16JsonProjection::array(allowed).unwrap(),
            ),
            ..OrchestratorConfig::default()
        };
        let output = Arc::new(MockOutputStream::new());
        let orch =
            ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
                config,
                Arc::new(MockApiClient::new(vec![])),
                stream.clone(),
                Arc::new(registry),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/tmp"),
            ));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            orch.run_turn_streaming("test"),
        )
        .await
        .expect("fallback terminates with unverified cleanup");
        tokio::time::timeout(std::time::Duration::from_secs(5), cancelled.notified())
            .await
            .expect("the old generation receives cancellation");
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), finished.notified())
            .await
            .expect("the old tool exits its cleanup gate");
        assert!(
            matches!(result, Err(OrchestratorError::CleanupUnverified(_))),
            "allow_fallback={allow_fallback}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "new-generation tool must not start"
        );
        assert_eq!(stream.inner.captured_calls().await.len(), 1);
        assert_eq!(
            after_fallback.load(Ordering::SeqCst),
            0,
            "fallback stream must not advance"
        );
        assert!(
            !output
                .snapshot()
                .await
                .iter()
                .any(|event| matches!(event, lingxi_core::host::OutputEvent::EndTurn { .. })),
            "unverified cleanup must reach the caller as an error, not a model terminal"
        );
    }
}
