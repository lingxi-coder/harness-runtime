use ::fusion as fusion_engine;
use async_trait::async_trait;
use platform_api::panel_pool::PanelPoolDrain;
use platform_api::subagent_spawn::SubagentInheritance;
use platform_api::tool_invoker::{SubagentInvocationContext, ToolInvoker, ToolInvokerError};
use platform_api::*;
use serde_json::{json, Value};
use sidequery::{
    SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse,
    StrictStructuredQueryRequest, StrictStructuredQueryResponse,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

struct Invoker;
#[async_trait]
impl ToolInvoker for Invoker {
    async fn invoke(
        &self,
        _: &str,
        _: Value,
        _: SubagentInvocationContext,
    ) -> Result<Value, ToolInvokerError> {
        Ok(Value::Null)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
#[derive(Default)]
struct Budget(AtomicUsize);
#[async_trait]
impl BudgetEnforcerHandle for Budget {
    async fn check_and_charge(&self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    async fn snapshot_total_nano_usd(&self) -> u64 {
        0
    }
    async fn reserve_nano_usd(&self, _: u64) -> Result<BudgetReservationId, BudgetError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(BudgetReservationId::NOOP)
    }
}
struct Prices;
impl fusion_engine::FusionPriceBook for Prices {
    fn rates_for(&self, _: &str, _: &str) -> Option<fusion_engine::ModelRates> {
        Some(fusion_engine::ModelRates {
            input_nano_usd_per_token: 1,
            output_nano_usd_per_token: 1,
            per_request_nano_usd: 1,
            cache_read_nano_usd_per_token: 1,
            cache_write_nano_usd_per_token: 1,
            reasoning_nano_usd_per_token: 1,
            cache_write_rate_is_ttl_approximated: false,
        })
    }
}
struct Api {
    calls: AtomicUsize,
    both: tokio::sync::Barrier,
    registered: AtomicUsize,
}
#[async_trait]
impl agent::SubagentApiClient for Api {
    async fn messages_create_stream_in_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<Value>,
        effort: Option<Value>,
        opts: agent::api::SubagentApiCallOpts,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        self.messages_create_stream_forced_in_opts(
            model, profile, system, messages, tools, None, effort, opts,
        )
        .await
    }

    async fn messages_create_stream_forced_in_opts(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<protocol::ConversationMessage>,
        tools: Vec<Value>,
        forced_tool: Option<&str>,
        effort: Option<Value>,
        opts: agent::api::SubagentApiCallOpts,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        // Explicit mock host: retain and inspect the actual Rust capability,
        // rather than relying on the legacy default's fail-closed opts path.
        let context = opts.model_attempt;
        if let Some(context) = &context {
            assert_eq!(context.stage(), ModelAttemptStage::Panel);
            assert!(context.panel_slot().is_some());
            self.registered.fetch_add(1, Ordering::SeqCst);
        }
        let result = self
            .messages_create_stream_forced_in(
                model,
                profile,
                system,
                messages,
                tools,
                forced_tool,
                effort,
            )
            .await;
        drop(context);
        result
    }
    async fn messages_create(
        &self,
        _: &str,
        _: Option<&str>,
        _: Vec<protocol::ConversationMessage>,
        _: Vec<Value>,
    ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.both.wait().await;
        Ok(llm_runtime::HistoryResponse {
            id: "fake-panel".into(),
            model: "mock".into(),
            content: vec![llm_runtime::ContentBlock::ToolCall {
                id: "report".into(),
                name: "StructuredOutput".into(),
                input: json!({"schema_version":1,"summary":"summary","candidate_answer":"answer","claims":[],"evidence":[],"assumptions":[],"risks":[],"unresolved_questions":[]}),
            }],
            stop_reason: Some("tool_use".into()),
            stop_details: None,
            usage: Default::default(),
            cost: None,
            provider_metadata: Value::Null,
        })
    }
}
#[derive(Default)]
struct Judge(AtomicUsize);
#[async_trait]
impl SideQueryClient for Judge {
    async fn query(&self, _: SideQueryRequest) -> Result<SideQueryResponse, SideQueryError> {
        Err(SideQueryError::InvalidResponse("offline judge".into()))
    }
    async fn query_json_schema(
        &self,
        _: StrictStructuredQueryRequest,
    ) -> Result<StrictStructuredQueryResponse, SideQueryError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(SideQueryError::InvalidResponse("offline judge".into()))
    }
}

#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    released: std::sync::atomic::AtomicBool,
    changed: tokio::sync::Notify,
}
impl Gate {
    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
    async fn wait(&self) {
        self.entered.notify_one();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.released.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
}
struct GatedDrain {
    real: Arc<dyn PanelPoolDrain>,
    gate: Arc<Gate>,
}
#[async_trait]
impl PanelPoolDrain for GatedDrain {
    async fn wait(&self) {
        self.real.wait().await;
        self.gate.wait().await;
    }
}
#[derive(Default)]
struct ReceiptFence {
    closed: std::sync::atomic::AtomicBool,
    gate: Gate,
}
#[async_trait]
impl fusion_engine::FusionPanelAttemptFence for ReceiptFence {
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
    async fn wait(&self) -> Result<(), FusionError> {
        assert!(self.closed.load(Ordering::Acquire));
        self.gate.wait().await;
        Ok(())
    }
}
struct Registrar(Arc<ReceiptFence>);
impl fusion_engine::FusionAttemptRegistrar for Registrar {
    fn register(
        &self,
        _: fusion_engine::FusionAttemptRegistration,
    ) -> Result<fusion_engine::RegisteredFusionAttempts, FusionError> {
        Ok(fusion_engine::RegisteredFusionAttempts {
            run: Arc::new(ModelAttemptRun::new(self.0.clone())),
            panel_fence: Some(self.0.clone()),
            finalizer: Box::new(Finalizer),
        })
    }
}
struct Finalizer;
impl fusion_engine::FusionAttemptFinalizer for Finalizer {
    fn finish(self: Box<Self>) -> Box<dyn fusion_engine::FusionAttemptSettlement> {
        self
    }
}
#[async_trait]
impl fusion_engine::FusionAttemptSettlement for Finalizer {
    async fn wait(
        self: Box<Self>,
    ) -> Result<fusion_engine::FusionAttemptSummary, fusion_engine::FusionAttemptSettlementError>
    {
        // Scheduling-only mock: these fakes do not incur provider charges.
        Ok(fusion_engine::FusionAttemptSummary {
            usage: Default::default(),
            confirmed_egress: vec![],
            possible_egress: vec![],
        })
    }
}

struct AdmissionProbe {
    inner: Arc<agent::PoolSubagentSpawner>,
    queued: Arc<tokio::sync::Notify>,
    producer_gate: Option<Arc<Gate>>,
}
#[async_trait]
impl SubagentSpawner for AdmissionProbe {
    async fn spawn(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.inner.spawn(request, inherit).await
    }
    async fn reserve_fusion_panel_group(
        &self,
        count: usize,
        deadline: tokio::time::Instant,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<PanelPoolLease, SubagentSpawnError> {
        let future = self
            .inner
            .reserve_fusion_panel_group(count, deadline, cancel);
        tokio::pin!(future);
        let lease = std::future::poll_fn(|cx| {
            let result = std::future::Future::poll(future.as_mut(), cx);
            if result.is_pending() {
                self.queued.notify_one();
            }
            result
        })
        .await?;
        if let Some(gate) = &self.producer_gate {
            let (permits, drain) = lease.into_parts();
            Ok(PanelPoolLease::with_drain(
                permits,
                Arc::new(GatedDrain {
                    real: drain.expect("real pool must provide producer drain"),
                    gate: gate.clone(),
                }),
            ))
        } else {
            Ok(lease)
        }
    }
    async fn spawn_workflow_with_observer_admitted(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
        progress: Option<tokio::sync::mpsc::Sender<String>>,
        observer: Option<Arc<dyn platform_api::subagent_spawn::SubagentSpawnObserver>>,
        watchdog: WorkflowQueryWatchdog,
        permit: PanelPoolPermit,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.inner
            .spawn_workflow_with_observer_admitted(
                request, inherit, progress, observer, watchdog, permit,
            )
            .await
    }
}

struct Fixture {
    spawner: Arc<agent::PoolSubagentSpawner>,
    orchestrator: Arc<fusion_engine::FusionOrchestrator>,
    api: Arc<Api>,
    judge: Arc<Judge>,
    budget: Arc<Budget>,
    queued: Arc<tokio::sync::Notify>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_gates(None)
    }
    fn with_gates(gates: Option<(Arc<Gate>, Arc<ReceiptFence>)>) -> Self {
        let pool = Arc::new(agent::StateMachinePool::new(
            Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
            2,
        ));
        let api = Arc::new(Api {
            calls: AtomicUsize::new(0),
            both: tokio::sync::Barrier::new(2),
            registered: AtomicUsize::new(0),
        });
        let spawner = Arc::new(agent::PoolSubagentSpawner::new(pool).with_api_client(api.clone()));
        let judge = Arc::new(Judge::default());
        let catalog: Vec<_> = ["claude-sonnet-5", "claude-opus-4-7"]
            .into_iter()
            .map(|model| fusion_engine::CatalogModel {
                profile: "anthropic".into(),
                model: model.into(),
                hints: FusionModelHints {
                    eligible: true,
                    judge_eligible: true,
                    quality_rank: 90,
                    ..Default::default()
                },
                structured_output: true,
                limits: fusion_engine::ModelLimits {
                    context_window_tokens: Some(200_000),
                    max_input_tokens: Some(180_000),
                    max_output_tokens: Some(32_000),
                },
            })
            .collect();
        let queued = Arc::new(tokio::sync::Notify::new());
        let probe = Arc::new(AdmissionProbe {
            inner: spawner.clone(),
            queued: queued.clone(),
            producer_gate: gates.as_ref().map(|(producer, _)| producer.clone()),
        });
        let mut orchestrator = fusion_engine::FusionOrchestrator::new(
            probe,
            judge.clone(),
            // Fusion has no automatic model selection: every role must be
            // named or preflight refuses the run with `NotConfigured` before
            // the admission behaviour this file tests can be reached. The
            // request supplies the panels explicitly; the analyst and
            // synthesizer are configuration.
            Arc::new(fusion_engine::FusionRuntimeConfig {
                analysis_protocol_retries: 0,
                panel_models: vec![
                    platform_api::FusionModelChoice::new("anthropic", "claude-sonnet-5"),
                    platform_api::FusionModelChoice::new("anthropic", "claude-opus-4-7"),
                ],
                analyst_model: Some(platform_api::FusionModelChoice::new(
                    "anthropic",
                    "claude-sonnet-5",
                )),
                ..fusion_engine::FusionRuntimeConfig::defaults()
            }),
            Arc::new(catalog),
        )
        .with_price_book(Arc::new(Prices))
        .with_panel_admission();
        if let Some((_, fence)) = gates {
            orchestrator = orchestrator.with_attempt_registrar(Arc::new(Registrar(fence)));
        }
        let orchestrator = Arc::new(orchestrator);
        Self {
            spawner,
            orchestrator,
            api,
            judge,
            budget: Arc::new(Budget::default()),
            queued,
        }
    }
    fn prepare(&self, cancel: tokio_util::sync::CancellationToken) -> PreparedFusionRun {
        let request = FusionRequest {
            schema_version: 1,
            origin: FusionOrigin::Slash,
            prompt: "review safely".into(),
            preset: FusionPreset::Quality,
            models: Some(
                ["claude-sonnet-5", "claude-opus-4-7"]
                    .into_iter()
                    .map(|model| FusionModelRef {
                        profile: Some("anthropic".into()),
                        model: model.into(),
                    })
                    .collect(),
            ),
            dimensions: vec!["correctness".into()],
            partial_ok: true,
            max_panel: None,
            cross_provider: false,
            parent_profile: "anthropic".into(),
            parent_model: "claude-sonnet-5".into(),
        };
        let inherit = FusionInheritance::new(
            SubagentInheritance {
                tool_invoker: Arc::new(Invoker),
                budget: self.budget.clone(),
            },
            cancel,
        );
        self.orchestrator
            .clone()
            .prepare(
                FusionSubmission::new(
                    request,
                    inherit,
                    FusionRunIdentity::new(
                        FusionRunId::generated(),
                        None,
                        FusionOrigin::Slash,
                        None,
                    ),
                )
                .unwrap(),
            )
            .unwrap()
    }
}

#[tokio::test]
async fn fusion_pool_admission_judge_waits_for_producer_then_receipt_contract_gates() {
    let producer = Arc::new(Gate::default());
    let receipt = Arc::new(ReceiptFence::default());
    let fixture = Fixture::with_gates(Some((producer.clone(), receipt.clone())));
    let run = fixture.prepare(tokio_util::sync::CancellationToken::new());
    let task = tokio::spawn(async move { run.activate(FusionActivation::now(), None).await });
    tokio::time::timeout(Duration::from_secs(5), producer.entered.notified())
        .await
        .unwrap();
    assert_eq!(fixture.api.calls.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.api.registered.load(Ordering::SeqCst), 2);
    assert!(receipt.closed.load(Ordering::Acquire));
    assert_eq!(fixture.judge.0.load(Ordering::SeqCst), 0);
    producer.release();
    tokio::time::timeout(Duration::from_secs(5), receipt.gate.entered.notified())
        .await
        .unwrap();
    assert_eq!(fixture.judge.0.load(Ordering::SeqCst), 0);
    receipt.gate.release();
    let _outcome = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.judge.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fusion_pool_admission_real_spawner_consumes_whole_group_without_reacquiring() {
    let fixture = Fixture::new();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        fixture
            .prepare(tokio_util::sync::CancellationToken::new())
            .activate(FusionActivation::now(), None),
    )
    .await
    .unwrap();
    assert_eq!(fixture.api.calls.load(Ordering::SeqCst), 2);
    assert!(
        fixture.judge.0.load(Ordering::SeqCst) > 0,
        "two valid reports must reach analysis"
    );
    assert_eq!(outcome.facts.allocated_panels, Some(2));
    assert!(fixture.budget.0.load(Ordering::SeqCst) > 0);
}

#[tokio::test(start_paused = true)]
async fn fusion_pool_admission_queue_cancel_and_timeout_do_not_reserve_or_call_api() {
    for cancel_queue in [true, false] {
        let fixture = Fixture::new();
        // Occupy every Fusion panel slot so the run below has to queue.
        let held = fixture
            .spawner
            .reserve_fusion_panel_group(
                platform_api::FUSION_PANEL_POOL_CAP,
                tokio::time::Instant::now() + Duration::from_secs(60),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut run = Box::pin(
            fixture
                .prepare(cancel.clone())
                .activate(FusionActivation::now(), None),
        );
        tokio::select! {
            outcome = &mut run => panic!("occupied pool returned before queue: {:?}", outcome.result),
            () = fixture.queued.notified() => {}
        }
        if cancel_queue {
            cancel.cancel();
        } else {
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        let outcome = run.await;
        assert!(outcome.result.is_err());
        assert_eq!(fixture.api.calls.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.judge.0.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.budget.0.load(Ordering::SeqCst), 0);
        drop(held);
        drop(
            fixture
                .spawner
                .reserve_fusion_panel_group(
                    2,
                    tokio::time::Instant::now() + Duration::from_secs(1),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap(),
        );
    }
}
