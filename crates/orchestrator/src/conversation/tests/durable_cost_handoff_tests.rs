use async_trait::async_trait;
use cost::{
    CostCalculator, CostDurabilityGate, CostHydration, CostMutationId, CostMutationSource,
    CostPersistAck, CostPersistError, CostPersistPermit, CostPersistRequest, CostPersistence,
    CostSessionScope, CostState, CostTracker, PricingCatalog, ProviderId,
    TokenUsage as CostTokenUsage, Usage as CostUsage,
};
use lingxi_core::host::live_sessions::{SessionWriterLease, SharedSessionWriterLease};
use lingxi_core::host::{CostSnapshot, OutputStream};
use lingxi_core::types::{
    ContentBlock, ConversationMessage, ImageSource, MediaAnalysis, MessageId, SessionId,
};
use llm_runtime::{
    Capabilities, ContentBlock as LlmContentBlock, ExecutionUsage as LlmUsage, LlmError,
    MediaRoute, ProviderId as LlmProvider, ResolvedRoute,
};
use sidequery::{
    CacheSafeParamsSlot, ForkedAgentRunner, SideQueryClient, SideQueryError, SideQueryRequest,
    SideQueryResponse, VisionDelegationResult, VisionPacket, PROMPT_VERSION,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;
use tool_api::registry::ToolRegistry;

use crate::conversation::{
    ModelCallPath, ModelCallPreparer, OrchestratorApiClient, PreparedModelCall,
};
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop_with_usage,
    message_start_with_usage, message_stop, text_delta, MockStreamingApiClient,
};
use crate::{ConversationOrchestrator, OrchestratorConfig, OrchestratorError};

const WAIT_BOUND: Duration = Duration::from_secs(2);

struct DurablePatchTool;

#[async_trait]
impl tool_api::Tool for DurablePatchTool {
    fn name(&self) -> &str {
        "DurablePatch"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({"type": "object", "properties": {}}))
    }
    fn is_enabled(&self, _: &tool_api::ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        false
    }
    async fn validate_input(
        &self,
        _: &serde_json::Value,
        _: &tool_api::ToolUseContext,
    ) -> Result<(), tool_api::ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &tool_api::ToolUseContext,
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
    async fn description(&self, _: &serde_json::Value, _: &tool_api::DescriptionOptions) -> String {
        "return an edit result".into()
    }
    async fn prompt(&self, _: &tool_api::PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _: serde_json::Value,
        _: tool_api::ToolUseContext,
        _: tool_api::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        Ok(tool_api::ToolCallResult {
            data: serde_json::json!({"structuredPatch": [{"lines": ["-old", "+new"]}]}),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

struct PatchBarrierStream {
    calls: AtomicUsize,
    edit_enqueued: Arc<AsyncLatch>,
    response_stopped: Arc<AsyncLatch>,
}

#[async_trait]
impl crate::conversation::StreamingApiClient for PatchBarrierStream {
    async fn stream(
        &self,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
        _: Vec<ConversationMessage>,
        _: Vec<serde_json::Value>,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<llm_runtime::HistoryEvent, LlmError>>,
        LlmError,
    > {
        use crate::test_support_stream::{content_block_start_tool_use, input_json_delta};
        use futures::StreamExt;
        let call = self.calls.fetch_add(1, Ordering::AcqRel);
        if call > 0 {
            return Ok(futures::stream::iter(
                vec![
                    message_start_with_usage(
                        "after-edit",
                        "claude-opus-4-8",
                        llm_usage(2, 0, 0, 0),
                    ),
                    content_block_start_text(0),
                    text_delta(0, "edit finished"),
                    content_block_stop(0),
                    message_delta_stop_with_usage("end_turn", llm_usage(0, 1, 0, 0)),
                    message_stop(),
                ]
                .into_iter()
                .map(Ok),
            )
            .boxed());
        }
        let edit_enqueued = self.edit_enqueued.clone();
        let response_stopped = self.response_stopped.clone();
        let first = futures::stream::iter(
            vec![
                message_start_with_usage("edit-stream", "claude-opus-4-8", llm_usage(3, 0, 0, 0)),
                content_block_start_tool_use(
                    0,
                    lingxi_core::types::ToolUseId::new(),
                    "DurablePatch",
                ),
                input_json_delta(0, "{}"),
                content_block_stop(0),
            ]
            .into_iter()
            .map(Ok),
        );
        // MessageStop cannot race ahead of the edit acquiring the ledger turn.
        let end = futures::stream::once(async move {
            edit_enqueued.wait().await;
            Ok(message_delta_stop_with_usage(
                "tool_use",
                llm_usage(0, 1, 0, 0),
            ))
        })
        .chain(futures::stream::once(async move {
            response_stopped.open();
            Ok(message_stop())
        }));
        Ok(first.chain(end).boxed())
    }
}

/// The edit's durability ack must be consumed while response settlement waits
/// behind that edit. A driver that stops polling tools at MessageStop deadlocks.
#[tokio::test]
async fn streaming_edit_ack_is_driven_while_model_cost_settlement_waits() {
    let edit_enqueued = Arc::new(AsyncLatch::default());
    let response_stopped = Arc::new(AsyncLatch::default());
    let streaming = Arc::new(PatchBarrierStream {
        calls: AtomicUsize::new(0),
        edit_enqueued: edit_enqueued.clone(),
        response_stopped: response_stopped.clone(),
    });
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(DurablePatchTool));
    let base = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let session_id = base.session().lock().await.session_id;
    let (sender, mut requests) = mpsc::channel::<CostPersistRequest>(8);
    let tracker = durable_tracker(
        session_id,
        Arc::new(BoundedPersistence::new(sender)),
        CostDurabilityGate::default(),
    );
    let orchestrator = Arc::new(base.with_cost_tracker(tracker.clone()));
    let turn = tokio::spawn({
        let orchestrator = orchestrator.clone();
        async move { orchestrator.run_turn_streaming("make an edit").await }
    });
    let edit = tokio::time::timeout(WAIT_BOUND, requests.recv())
        .await
        .expect("streaming executor begins edit persistence before MessageStop")
        .unwrap();
    assert_eq!(edit.source, CostMutationSource::Administrative);
    assert_eq!(edit.state.total_lines_added, 1);
    assert_eq!(edit.state.total_lines_removed, 1);
    edit_enqueued.open();
    tokio::time::timeout(WAIT_BOUND, response_stopped.wait())
        .await
        .expect("stream ends while edit ack is withheld");
    // This current-thread test releases the ack only after the turn has consumed
    // MessageStop and yielded. The tool future still owns the edit ledger turn.
    edit.ack
        .send(Ok(CostPersistAck {
            mutation_id: edit.mutation_id,
            cost_revision: edit.cost_revision,
            journal_revision: 1,
        }))
        .expect("edit future still awaits its ack");
    for revision in 2..=3 {
        let response = tokio::time::timeout(WAIT_BOUND, requests.recv())
            .await
            .expect(
                "model settlement must progress after the edit ack without starving the executor",
            )
            .unwrap();
        assert_eq!(response.source, CostMutationSource::ModelResponse);
        assert_eq!(response.cost_revision, revision);
        response
            .ack
            .send(Ok(CostPersistAck {
                mutation_id: response.mutation_id,
                cost_revision: revision,
                journal_revision: revision,
            }))
            .unwrap();
    }
    tokio::time::timeout(WAIT_BOUND, turn)
        .await
        .expect("turn completes")
        .unwrap()
        .unwrap();
    assert_eq!(streaming.calls.load(Ordering::Acquire), 2);
    assert_answer_reached_history(&orchestrator, "edit finished").await;
    assert_eq!(tracker.snapshot().await.cost_revision, 3);
}

#[derive(Default)]
struct AsyncLatch {
    open: AtomicBool,
    notify: Notify,
}

impl AsyncLatch {
    fn open(&self) {
        self.open.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            if self.open.load(Ordering::Acquire) {
                return;
            }
            let notified = self.notify.notified();
            if self.open.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

struct TestWriterLease(String);

impl SessionWriterLease for TestWriterLease {
    fn session_id(&self) -> &str {
        &self.0
    }
}

struct BoundedPersistence {
    sender: mpsc::Sender<CostPersistRequest>,
    acquire_count: AtomicUsize,
    acquire_changed: Notify,
}

impl BoundedPersistence {
    fn new(sender: mpsc::Sender<CostPersistRequest>) -> Self {
        Self {
            sender,
            acquire_count: AtomicUsize::new(0),
            acquire_changed: Notify::new(),
        }
    }

    async fn wait_for_acquires(&self, expected: usize) {
        loop {
            if self.acquire_count.load(Ordering::Acquire) >= expected {
                return;
            }
            let notified = self.acquire_changed.notified();
            if self.acquire_count.load(Ordering::Acquire) >= expected {
                return;
            }
            notified.await;
        }
    }
}

#[async_trait]
impl CostPersistence for BoundedPersistence {
    async fn acquire_permit(
        &self,
        _session_id: SessionId,
    ) -> Result<CostPersistPermit, CostPersistError> {
        self.acquire_count.fetch_add(1, Ordering::AcqRel);
        self.acquire_changed.notify_waiters();
        let permit = self
            .sender
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| CostPersistError::Rejected("test persistence queue closed".into()))?;
        Ok(CostPersistPermit::new(move |request| {
            permit.send(request);
            Ok(())
        }))
    }
}

struct BlockedDurability {
    session_id: SessionId,
    tracker: Arc<CostTracker>,
    scope: CostSessionScope,
    persistence: Arc<BoundedPersistence>,
    requests: mpsc::Receiver<CostPersistRequest>,
    queue_blocker: Option<tokio::sync::mpsc::OwnedPermit<CostPersistRequest>>,
}

impl BlockedDurability {
    async fn new(session_id: SessionId) -> Self {
        let (sender, requests) = mpsc::channel(1);
        let queue_blocker = sender
            .clone()
            .reserve_owned()
            .await
            .expect("test queue is open");
        let persistence = Arc::new(BoundedPersistence::new(sender));
        let gate = CostDurabilityGate::default();
        let tracker = durable_tracker(session_id, persistence.clone(), gate);
        let scope = tracker.session_scope(session_id);
        Self {
            session_id,
            tracker,
            scope,
            persistence,
            requests,
            queue_blocker: Some(queue_blocker),
        }
    }

    async fn wait_until_response_owner_is_blocked(&self) {
        tokio::time::timeout(WAIT_BOUND, self.persistence.wait_for_acquires(1))
            .await
            .expect("response owner reaches the full persistence queue");
    }

    fn release_queue(&mut self) {
        drop(self.queue_blocker.take());
    }

    async fn ack_exact_charge(
        &mut self,
        expected_model: &str,
        expected_usage: CostUsage,
    ) -> CostMutationId {
        let request = tokio::time::timeout(WAIT_BOUND, self.requests.recv())
            .await
            .expect("owned response enqueues after capacity is released")
            .expect("persistence request sender remains live");
        assert_eq!(request.session_id, self.session_id);
        assert_eq!(request.cost_revision, 1);
        assert_eq!(request.source, CostMutationSource::ModelResponse);
        assert_eq!(request.state.session_id, self.session_id);
        assert_eq!(request.state.cost_revision, 1);
        assert_eq!(request.state.per_model_usage.len(), 1);

        let row = &request.state.per_model_usage[0];
        assert_eq!(row.model_ref.provider, ProviderId::Anthropic);
        assert_eq!(row.model_ref.model, expected_model);
        assert_eq!(row.usage, expected_usage);
        assert_eq!(request.state.last_usage, Some(expected_usage));
        assert_eq!(request.state.total_nano_usd, row.cost_nano_usd);
        assert!(row.cost_nano_usd > 0, "known usage must produce a charge");
        let (pricing, _) = PricingCatalog::builtin_reference()
            .resolve(&row.model_ref)
            .expect("test model has exact builtin pricing");
        assert_eq!(
            row.cost_nano_usd,
            CostCalculator::calculate_nano_usd(&expected_usage, &pricing)
        );
        let row_cost_nano_usd = row.cost_nano_usd;

        let mutation_id = request.mutation_id.clone();
        let cost_revision = request.cost_revision;
        request
            .ack
            .send(Ok(CostPersistAck {
                mutation_id: mutation_id.clone(),
                journal_revision: 1,
                cost_revision,
            }))
            .expect("owned finalizer still awaits its exact ack");

        let retained = tokio::time::timeout(WAIT_BOUND, async {
            loop {
                let retained = self
                    .scope
                    .retained_response(&mutation_id)
                    .expect("provider observation is retained synchronously");
                if retained.settlement.is_some() {
                    break retained;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned finalizer records its ack");
        assert_eq!(retained.observation.usage, expected_usage);
        assert_eq!(retained.observation.observed_nano_usd, row_cost_nano_usd);
        assert!(matches!(
            retained.settlement,
            Some(Ok(Some(ref ack)))
                if ack.mutation_id == mutation_id
                    && ack.cost_revision == 1
                    && ack.journal_revision == 1
        ));
        assert_eq!(self.persistence.acquire_count.load(Ordering::Acquire), 1);
        let live = self.tracker.snapshot().await;
        assert_eq!(live.session_id, self.session_id);
        assert_eq!(live.total_nano_usd, row_cost_nano_usd);
        assert_eq!(live.cost_revision, 1);
        assert!(matches!(
            self.requests.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        mutation_id
    }
}

fn durable_tracker(
    session_id: SessionId,
    persistence: Arc<dyn CostPersistence>,
    gate: CostDurabilityGate,
) -> Arc<CostTracker> {
    let (legacy_tx, _legacy_rx) = mpsc::channel(1);
    let lease: SharedSessionWriterLease = Arc::new(TestWriterLease(session_id.to_string()));
    Arc::new(
        CostTracker::new(
            session_id,
            Arc::new(PricingCatalog::builtin_reference()),
            legacy_tx,
        )
        .try_with_durable_persistence(
            CostHydration {
                state: CostState {
                    session_id,
                    ..CostState::default()
                },
                journal_revision: 0,
                attempt_outputs: Vec::new(),
            },
            persistence,
            lease,
            gate,
        )
        .expect("test durable authority matches the orchestrator session"),
    )
}

fn llm_usage(input: u64, output: u64, cache_write: u64, cache_read: u64) -> LlmUsage {
    LlmUsage {
        report: llm_runtime::UsageReport::measured(
            llm_runtime::Usage {
                input_tokens: input,
                output_tokens: output,
                cache_write_tokens: cache_write,
                cache_read_tokens: cache_read,
                reasoning_tokens: 0,
                ..Default::default()
            },
            llm_runtime::services::sdk::protocol::UsageState::Complete,
        ),
        ..LlmUsage::default()
    }
}

fn cost_usage(input: u64, output: u64, cache_write: u64, cache_read: u64) -> CostUsage {
    CostUsage {
        tokens: CostTokenUsage {
            input,
            output,
            cache_write,
            cache_read,
            reasoning_output: 0,
            cache_write_1h: 0,
        },
        ..CostUsage::default()
    }
}

async fn abort_at_owned_cost_wait<T>(
    task: tokio::task::JoinHandle<T>,
    durability: &BlockedDurability,
) {
    durability.wait_until_response_owner_is_blocked().await;
    task.abort();
    let error = match task.await {
        Err(error) => error,
        Ok(_) => panic!("full queue must keep the caller pending"),
    };
    assert!(
        error.is_cancelled(),
        "the test must drop a live post-response caller"
    );
}

fn batched_orchestrator(response: llm_runtime::HistoryResponse) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![response])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

#[tokio::test]
async fn batched_turn_drop_retains_and_acks_the_originating_session_charge() {
    let expected = cost_usage(11, 7, 3, 5);
    let mut response = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "finished".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    );
    response.usage = llm_usage(11, 7, 3, 5);
    let base = batched_orchestrator(response);
    let session_id = base.session().lock().await.session_id;
    let mut durability = BlockedDurability::new(session_id).await;
    let orchestrator = Arc::new(base.with_cost_tracker(durability.tracker.clone()));

    let task = tokio::spawn({
        let orchestrator = orchestrator.clone();
        async move { orchestrator.run_turn("bill this response").await }
    });
    abort_at_owned_cost_wait(task, &durability).await;
    durability.release_queue();
    durability
        .ack_exact_charge("claude-opus-4-8", expected)
        .await;
}

#[tokio::test]
async fn streaming_turn_drop_retains_and_acks_the_originating_session_charge() {
    let expected = cost_usage(13, 9, 2, 4);
    let start_usage = llm_usage(13, 0, 2, 4);
    let delta_usage = llm_usage(0, 9, 0, 0);
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start_with_usage("stream-cost", "claude-opus-4-8", start_usage),
        content_block_start_text(0),
        text_delta(0, "finished"),
        content_block_stop(0),
        message_delta_stop_with_usage("end_turn", delta_usage),
        message_stop(),
    ]]));
    let base = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let session_id = base.session().lock().await.session_id;
    let mut durability = BlockedDurability::new(session_id).await;
    let orchestrator = Arc::new(base.with_cost_tracker(durability.tracker.clone()));

    let task = tokio::spawn({
        let orchestrator = orchestrator.clone();
        async move { orchestrator.run_turn_streaming("bill this stream").await }
    });
    abort_at_owned_cost_wait(task, &durability).await;
    durability.release_queue();
    durability
        .ack_exact_charge("claude-opus-4-8", expected)
        .await;
}

struct FreezeDuringPrepare {
    gate: CostDurabilityGate,
}

#[async_trait]
impl ModelCallPreparer for FreezeDuringPrepare {
    async fn prepare(
        &self,
        _orch: &ConversationOrchestrator,
        _path: ModelCallPath,
        _system_prompt: Option<&str>,
        _cancel: Option<&CancellationToken>,
        draft: PreparedModelCall,
    ) -> Result<PreparedModelCall, OrchestratorError> {
        self.gate
            .freeze("synthetic WAL failure after scope capture");
        Ok(draft)
    }
}

#[tokio::test]
async fn captured_durable_scope_frozen_during_prepare_prevents_stream_dispatch() {
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start_with_usage("must-not-open", "claude-opus-4-8", llm_usage(1, 0, 0, 0)),
        message_delta_stop_with_usage("end_turn", llm_usage(0, 1, 0, 0)),
        message_stop(),
    ]]));
    let base = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let session_id = base.session().lock().await.session_id;
    let (sender, mut requests) = mpsc::channel(1);
    let persistence = Arc::new(BoundedPersistence::new(sender));
    let gate = CostDurabilityGate::default();
    let tracker = durable_tracker(session_id, persistence.clone(), gate.clone());
    let orchestrator = base
        .with_cost_tracker(tracker)
        .with_model_call_preparer(Arc::new(FreezeDuringPrepare { gate: gate.clone() }));

    let result = orchestrator.run_turn_streaming("do not dispatch").await;

    assert!(result.is_err(), "durability failure must fail closed");
    assert!(streaming.captured_calls().await.is_empty());
    assert_eq!(persistence.acquire_count.load(Ordering::Acquire), 0);
    assert!(requests.try_recv().is_err());
    assert!(gate
        .frozen_reason()
        .is_some_and(|reason| reason.contains("synthetic WAL failure")));
}

struct VisionApi {
    result: StdMutex<Option<VisionDelegationResult>>,
    calls: AtomicUsize,
}

impl VisionApi {
    fn new(result: VisionDelegationResult) -> Self {
        Self {
            result: StdMutex::new(Some(result)),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl OrchestratorApiClient for VisionApi {
    async fn messages_create(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _system: Option<&str>,
        _msgs: Vec<ConversationMessage>,
        _tools: Vec<serde_json::Value>,
    ) -> Result<llm_runtime::HistoryResponse, LlmError> {
        Err(LlmError::Transport {
            message: "main request must not run before vision cancellation".into(),
        })
    }

    fn resolve_media_route(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Result<MediaRoute, LlmError> {
        Ok(vision_route())
    }

    async fn analyze_vision_delegation(
        &self,
        _packet: VisionPacket,
    ) -> Result<VisionDelegationResult, LlmError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| LlmError::Transport {
                message: "vision result already consumed".into(),
            })
    }
}

fn vision_route() -> MediaRoute {
    let route = |model: &str, vision: bool| ResolvedRoute {
        provider_id: LlmProvider::AnthropicFirstParty,
        profile_name: "anthropic".into(),
        request_model: model.into(),
        display_model: model.into(),
        pricing_model: llm_runtime::PricingModelRef {
            pricing_provider_id: LlmProvider::AnthropicFirstParty,
            billing_model: model.into(),
            request_model: model.into(),
            display_model: model.into(),
        },
        capabilities: Capabilities {
            streaming: true,
            tools: !vision,
            vision,
            documents: false,
            reasoning: false,
            structured_output: false,
        },
        connection_chain: Vec::new(),
        failover: Default::default(),
    };
    MediaRoute {
        main: route("claude-opus-4-8", false),
        vision_delegate: Some(route("claude-opus-4-8", true)),
    }
}

struct VisionProgressOutput {
    finish_entered: AsyncLatch,
    finish_release: AsyncLatch,
}

impl VisionProgressOutput {
    fn new() -> Self {
        Self {
            finish_entered: AsyncLatch::default(),
            finish_release: AsyncLatch::default(),
        }
    }
}

#[async_trait]
impl OutputStream for VisionProgressOutput {
    async fn emit_text(&self, _text: &str) {}

    async fn emit_tool_call(
        &self,
        _id: &lingxi_core::types::ToolUseId,
        _tool: &str,
        _input: &serde_json::Value,
    ) {
    }

    async fn emit_tool_result(
        &self,
        _id: &lingxi_core::types::ToolUseId,
        _tool: &str,
        _model_text: &str,
        _result: &serde_json::Value,
    ) {
    }

    async fn emit_end_turn(&self, _stop_reason: &str, _cost: &CostSnapshot) {}

    async fn emit_hook_progress_finished(&self, _progress_id: &str) {
        self.finish_entered.open();
        self.finish_release.wait().await;
    }
}

#[tokio::test]
async fn vision_progress_cancellation_retains_known_delegate_usage() {
    let expected = cost_usage(17, 6, 2, 3);
    let message_id = MessageId::new();
    let image = ImageSource::Url {
        url: "https://example.com/durable-vision.png".into(),
    };
    let fingerprint =
        sidequery::collect_media_fingerprints(&[ConversationMessage::user_with_images(
            MessageId::new(),
            String::new(),
            vec![image.clone()],
        )])
        .expect("test URL has a stable fingerprint")
        .remove(0);
    let api = Arc::new(VisionApi::new(VisionDelegationResult {
        analysis: MediaAnalysis {
            question_key: message_id.to_string(),
            media_fingerprints: vec![fingerprint],
            model: "claude-opus-4-8".into(),
            prompt_version: PROMPT_VERSION,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            task_findings: vec!["visible object".into()],
            media: vec![],
            cross_media_findings: vec![],
            truncated: false,
        },
        usage: expected,
        elapsed: Duration::from_millis(7),
        retry_count: 0,
        api_calls: 1,
    }));
    let output = Arc::new(VisionProgressOutput::new());
    let base = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(MockStreamingApiClient::empty()),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_vision_delegation(true);
    let session_id = base.session().lock().await.session_id;
    let mut durability = BlockedDurability::new(session_id).await;
    let orchestrator = Arc::new(base.with_cost_tracker(durability.tracker.clone()));
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let orchestrator = orchestrator.clone();
        let cancel = cancel.clone();
        async move {
            orchestrator
                .run_turn_streaming_with_cancel_image_sources_and_message_id(
                    "inspect this",
                    vec![image],
                    cancel,
                    Some(message_id),
                )
                .await
        }
    });

    tokio::time::timeout(WAIT_BOUND, output.finish_entered.wait())
        .await
        .expect("vision response reaches the progress-finish boundary");
    durability.wait_until_response_owner_is_blocked().await;
    cancel.cancel();
    task.abort();
    assert!(task
        .await
        .expect_err("progress remains blocked")
        .is_cancelled());
    output.finish_release.open();
    durability.release_queue();
    durability
        .ack_exact_charge("claude-opus-4-8", expected)
        .await;
    assert_eq!(api.calls.load(Ordering::Acquire), 1);
}

struct CancelAfterCompactionResponse {
    cancel: CancellationToken,
    usage: CostUsage,
    calls: AtomicUsize,
}

#[async_trait]
impl SideQueryClient for CancelAfterCompactionResponse {
    async fn query(&self, _request: SideQueryRequest) -> Result<SideQueryResponse, SideQueryError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.cancel.cancel();
        Ok(SideQueryResponse {
            text: Some("<analysis>hidden</analysis><summary>durable summary</summary>".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: self.usage,
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

fn assistant_message(text: &str) -> ConversationMessage {
    ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![ContentBlock::Text { text: text.into() }],
        stop_reason: Some("end_turn".into()),
    }
}

#[tokio::test]
async fn manual_compaction_post_response_cancel_retains_known_usage() {
    let expected = cost_usage(19, 8, 1, 2);
    let cancel = CancellationToken::new();
    let client = Arc::new(CancelAfterCompactionResponse {
        cancel: cancel.clone(),
        usage: expected,
        calls: AtomicUsize::new(0),
    });
    let cache_slot = Arc::new(CacheSafeParamsSlot::new());
    let runner = Arc::new(
        ForkedAgentRunner::new().with_side_query_client(client.clone(), "claude-opus-4-8".into()),
    );
    let compactor = Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
        compaction::Autocompactor::with_forked_runner(runner, cache_slot.clone()),
        u64::MAX,
    ));
    let base = batched_orchestrator(mock_message_response(Vec::new(), Some("end_turn")))
        .with_compaction(compactor)
        .with_cache_safe_slot(cache_slot);
    {
        let session = base.session();
        let mut state = session.lock().await;
        state.history = vec![
            ConversationMessage::user(MessageId::new(), "first question".into()),
            assistant_message("first answer"),
            ConversationMessage::user(MessageId::new(), "second question".into()),
            assistant_message("second answer"),
        ];
    }
    let session_id = base.session().lock().await.session_id;
    let mut durability = BlockedDurability::new(session_id).await;
    let orchestrator = base.with_cost_tracker(durability.tracker.clone());

    let result = orchestrator.force_compact_with_cancel(cancel).await;
    assert!(
        matches!(&result, Err(lingxi_core::host::HandleError::ActionFailed(message)) if message == "Compaction canceled."),
        "post-response cancellation must leave compaction uncommitted: {result:?}"
    );
    durability.wait_until_response_owner_is_blocked().await;
    durability.release_queue();
    durability
        .ack_exact_charge("claude-opus-4-8", expected)
        .await;
    assert_eq!(client.calls.load(Ordering::Acquire), 1);
}

/// Persistence that always refuses. Every settlement through it fails after
/// the provider has already answered and billed.
struct RefusingPersistence;

#[async_trait]
impl CostPersistence for RefusingPersistence {
    async fn acquire_permit(
        &self,
        _session_id: SessionId,
    ) -> Result<CostPersistPermit, CostPersistError> {
        Err(CostPersistError::Storage("ledger volume went away".into()))
    }
}

fn refusing_durability(session_id: SessionId) -> Arc<CostTracker> {
    durable_tracker(
        session_id,
        Arc::new(RefusingPersistence),
        CostDurabilityGate::default(),
    )
}

fn answer_response(text: &str) -> llm_runtime::HistoryResponse {
    let mut response = mock_message_response(
        vec![LlmContentBlock::Text {
            text: text.into(),
            cache_control: None,
        }],
        Some("end_turn"),
    );
    response.usage = llm_usage(11, 7, 3, 5);
    response
}

/// The model's text has to be in the session, not merely returned: a turn that
/// errors out before translating the response leaves the history without it.
async fn assert_answer_reached_history(orchestrator: &ConversationOrchestrator, text: &str) {
    let handle = orchestrator.session();
    let session = handle.lock().await;
    let found = session.history.iter().any(|message| match message {
        ConversationMessage::Assistant { content, .. } => content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text: body } if body.contains(text))),
        _ => false,
    });
    assert!(
        found,
        "the paid answer never reached the session history: {:?}",
        session.history.len()
    );
}

/// P0-6: an accounting write that fails AFTER the provider answered must not
/// cost the user the answer they already paid for. The pre-dispatch preflight
/// is what stops the next paid call, and it already does.
#[tokio::test]
async fn batched_settlement_failure_keeps_the_answer_and_stops_the_next_call() {
    let base = batched_orchestrator(answer_response("the paid answer"));
    let session_id = base.session().lock().await.session_id;
    let orchestrator = base.with_cost_tracker(refusing_durability(session_id));

    orchestrator
        .run_turn("ask once")
        .await
        .expect("the answer survives a failed accounting write");
    assert_answer_reached_history(&orchestrator, "the paid answer").await;

    let next = orchestrator.run_turn("ask again").await;
    let error = next.expect_err("a frozen ledger refuses the next paid call");
    assert!(
        error.to_string().contains("preflight"),
        "the next call must be stopped by the pre-dispatch gate, got {error}"
    );
}

/// The same contract on the streaming driver, which desktop and mobile run.
#[tokio::test]
async fn streaming_settlement_failure_keeps_the_answer_and_stops_the_next_call() {
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start_with_usage("stream-cost", "claude-opus-4-8", llm_usage(13, 0, 2, 4)),
        content_block_start_text(0),
        text_delta(0, "the paid answer"),
        content_block_stop(0),
        message_delta_stop_with_usage("end_turn", llm_usage(0, 9, 0, 0)),
        message_stop(),
    ]]));
    let base = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let session_id = base.session().lock().await.session_id;
    let orchestrator = base.with_cost_tracker(refusing_durability(session_id));

    orchestrator
        .run_turn_streaming("ask once")
        .await
        .expect("the streamed answer survives a failed accounting write");
    assert_answer_reached_history(&orchestrator, "the paid answer").await;

    let next = orchestrator.run_turn_streaming("ask again").await;
    let error = next.expect_err("a frozen ledger refuses the next paid call");
    assert!(
        error.to_string().contains("preflight"),
        "the next call must be stopped by the pre-dispatch gate, got {error}"
    );
}

// ── §3.2: the vision-delegation cancel maps to Cancelled, under a guard ──────
//
// `prepare_iteration` propagates `OrchestratorError::VisionDelegationCancelled`
// straight out — it does NOT go through the `aborted_*` end-event branch — and
// the main streaming wrapper turns it into `TurnOutcome::Cancelled`, but only
// when the token is actually cancelled:
//
//     Err(OrchestratorError::VisionDelegationCancelled) if cancel.is_cancelled()
//
// The guard is the part PR 5 can lose. A unified error mapping that folds this
// arm into "any cancel-shaped error becomes Cancelled" drops it, and the row
// above (`vision_progress_cancellation_retains_known_delegate_usage`) would not
// notice: it aborts its task and never reads an outcome.

/// A provider whose vision delegation never answers, so the cancel branch of
/// `vision_model_call`'s `select!` is the one that resolves.
struct BlockingVisionApi;

#[async_trait::async_trait]
impl OrchestratorApiClient for BlockingVisionApi {
    async fn messages_create(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _system: Option<&str>,
        _msgs: Vec<ConversationMessage>,
        _tools: Vec<serde_json::Value>,
    ) -> Result<llm_runtime::HistoryResponse, LlmError> {
        Err(LlmError::Transport {
            message: "the main request must not run: vision is cancelled first".into(),
        })
    }

    fn resolve_media_route(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Result<MediaRoute, LlmError> {
        Ok(vision_route())
    }

    async fn analyze_vision_delegation(
        &self,
        _packet: VisionPacket,
    ) -> Result<VisionDelegationResult, LlmError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn a_cancelled_vision_delegation_surfaces_as_cancelled_not_an_error() {
    let image = ImageSource::Url {
        url: "https://example.com/vision-cancel.png".into(),
    };
    let orchestrator = Arc::new(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(BlockingVisionApi),
            Arc::new(MockStreamingApiClient::empty()),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_vision_delegation(true),
    );

    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let orchestrator = orchestrator.clone();
        let cancel = cancel.clone();
        async move {
            orchestrator
                .run_turn_streaming_with_cancel_image_sources_and_message_id(
                    "inspect this",
                    vec![image],
                    cancel,
                    Some(MessageId::new()),
                )
                .await
        }
    });

    // Yield until the delegation is parked, then cancel. The turn must come back
    // on its own — not be aborted — so the outcome can be read.
    tokio::task::yield_now().await;
    cancel.cancel();

    let outcome = tokio::time::timeout(WAIT_BOUND, task)
        .await
        .expect("the cancelled vision delegation must let the turn return")
        .expect("turn task panicked")
        .expect("a cancelled vision delegation is an outcome, not an error");

    assert_eq!(
        outcome,
        crate::conversation::TurnOutcome::Cancelled,
        "§3.2: VisionDelegationCancelled propagates out of prepare_iteration and the main \
         streaming wrapper maps it to Cancelled WHEN the token is cancelled. Losing the arm \
         surfaces a raw error to the caller; losing its `if cancel.is_cancelled()` guard makes \
         every vision failure look like a user interrupt."
    );
}
