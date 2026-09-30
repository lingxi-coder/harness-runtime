use super::*;
use futures::StreamExt;
use llm_runtime::{ProtocolFamily, ProviderRequest};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

struct DurableLease(String);
impl lingxi_core::host::live_sessions::SessionWriterLease for DurableLease {
    fn session_id(&self) -> &str {
        &self.0
    }
}
struct DurableQueue(tokio::sync::mpsc::Sender<cost::AttemptPersistRequest>);
#[async_trait]
impl cost::CostPersistence for DurableQueue {
    async fn acquire_permit(
        &self,
        _: lingxi_core::types::SessionId,
    ) -> Result<cost::CostPersistPermit, cost::CostPersistError> {
        Err(cost::CostPersistError::Rejected(
            "ordinary write not expected".into(),
        ))
    }
    async fn acquire_attempt_permit(
        &self,
        _: lingxi_core::types::SessionId,
    ) -> Result<cost::AttemptPersistPermit, cost::CostPersistError> {
        let permit = self.0.clone().reserve_owned().await.unwrap();
        Ok(cost::AttemptPersistPermit::new(move |request| {
            permit.send(request);
            Ok(())
        }))
    }
}

struct Frames(std::collections::VecDeque<llm_runtime::RawStreamFrame>);
impl llm_runtime::test_support::FrameStream for Frames {
    fn next_frame(
        &mut self,
    ) -> llm_runtime::transport::BoxFuture<'_, Result<Option<llm_runtime::RawStreamFrame>, LlmError>>
    {
        Box::pin(async { Ok(self.0.pop_front()) })
    }
}
struct Wire {
    calls: Arc<AtomicUsize>,
    complete: bool,
}
impl llm_runtime::test_support::FixtureTransport for Wire {
    fn execute<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> llm_runtime::transport::BoxFuture<'a, Result<llm_runtime::ProviderResponse, LlmError>>
    {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut usage =
                json!({"completion_tokens":8,"completion_tokens_details":{"reasoning_tokens":3}});
            if self.complete {
                usage["prompt_tokens"] = json!(5);
                usage["total_tokens"] = json!(13);
            }
            Ok(llm_runtime::ProviderResponse {
                status: 200,
                headers: Default::default(),
                request_id: None,
                body_json: json!({"id":"fake","model":"wire","choices":[{"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],"usage":usage}),
            })
        })
    }
    fn open_stream<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> llm_runtime::transport::BoxFuture<
        'a,
        Result<llm_runtime::test_support::StreamingResponse, LlmError>,
    > {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut usage =
                json!({"completion_tokens":8,"completion_tokens_details":{"reasoning_tokens":3}});
            if self.complete {
                usage["prompt_tokens"] = json!(5);
                usage["total_tokens"] = json!(13);
            }
            let chunk = json!({"id":"fake","model":"wire","choices":[{"index":0,"delta":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],"usage":usage});
            Ok(llm_runtime::test_support::StreamingResponse {
                status: 200,
                headers: Default::default(),
                frames: Box::new(Frames(
                    [
                        llm_runtime::RawStreamFrame::new(serde_json::to_vec(&chunk).unwrap()),
                        llm_runtime::RawStreamFrame::new(b"[DONE]".to_vec()),
                    ]
                    .into(),
                )),
            })
        })
    }
}
llm_runtime::impl_fixture_transport!(Wire);

struct DurableHarness {
    service: Arc<llm_runtime::ApiService>,
    authority: Arc<RunAuthority>,
    request: llm_runtime::LlmRequest,
    queue: tokio::sync::mpsc::Receiver<cost::AttemptPersistRequest>,
    ledger: cost::AttemptLedger,
    state: cost::CostStateVector,
    journal: u64,
    calls: Arc<AtomicUsize>,
}
impl DurableHarness {
    async fn new(complete: bool) -> Self {
        Self::with_limits(complete, 10_000, 1_000).await
    }

    async fn with_limits(complete: bool, session_limit: u64, output_limit: u64) -> Self {
        Self::with_pricing(complete, session_limit, output_limit, false).await
    }

    async fn with_pricing(
        complete: bool,
        session_limit: u64,
        output_limit: u64,
        dynamic: bool,
    ) -> Self {
        // Put the peak window twelve hours away so this call has a stable
        // off-peak quote while admission must retain the higher published rate.
        let hour = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / 3600
            % 24;
        let peak_hour = (hour + 12) % 24;
        let wire_profile = dynamic.then(|| serde_json::from_value(json!({
            "provider_id":"openai", "profile_name":"profile", "protocol":"open_ai_chat",
            "base_url":"https://unused.invalid/v1", "auth":"none",
            "pricing":{"billing_mode":"per_token","peak":{"utc_windows":[format!("{peak_hour:02}:00-{:02}:00",peak_hour+1)],"off_peak_multiplier":0.5}},
            "models":[{"display_model":"display","request_model":"wire","billing_model":"test",
                "pricing":{"input_per_million":0.002,"output_per_million":0.002,"cache_read_per_million":0.002}}]
        })).unwrap());
        let client = Arc::new(
            llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
                providers: vec![llm_runtime::ProviderProfile {
                    wire_profile,
                    regions: llm_runtime::Region::all(),
                    provider_id: llm_runtime::ProviderId::OpenAI,
                    profile_name: "profile".into(),
                    base_url: "https://unused.invalid/v1".into(),
                    protocol: ProtocolFamily::OpenAiChat,
                    auth: llm_runtime::AuthStrategy::None,
                    credential: llm_runtime::CredentialConfig::None,
                    models: vec![llm_runtime::ModelProfile {
                        display_model: "display".into(),
                        request_model: "wire".into(),
                        billing_model: "test".into(),
                        aliases: vec![],
                        description: None,
                        metadata: Default::default(),
                        capabilities: llm_runtime::Capabilities {
                            streaming: true,
                            ..Default::default()
                        },
                    }],
                    pricing: llm_runtime::PricingConfig {
                        billing_mode: if dynamic {
                            lingxi_core::host::ModelBillingMode::PerToken
                        } else {
                            Default::default()
                        },
                        ..Default::default()
                    },
                    signing: None,
                    azure: None,
                    supports_websockets: false,
                    supports_websocket_compression: false,
                    websocket_connect_timeout_ms: None,
                    vision_delegate: None,
                    connection: Default::default(),
                }],
            })
            .unwrap(),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let service = Arc::new(llm_runtime::ApiService::new(
            client,
            Arc::new(Wire {
                calls: calls.clone(),
                complete,
            }),
            Default::default(),
            Default::default(),
            "test",
            None,
            None,
        ));
        let session = lingxi_core::types::SessionId::new();
        let (legacy, _) = tokio::sync::mpsc::channel(1);
        let (tx, queue) = tokio::sync::mpsc::channel(4);
        let initial = cost::CostState {
            session_id: session,
            ..Default::default()
        };
        let tracker = Arc::new(
            cost::CostTracker::new(session, Arc::new(cost::PricingCatalog::empty()), legacy)
                .with_durable_persistence(
                    cost::CostHydration {
                        state: initial.clone(),
                        journal_revision: 0,
                        attempt_outputs: Vec::new(),
                    },
                    Arc::new(DurableQueue(tx)),
                    Arc::new(DurableLease(session.to_string())),
                    cost::CostDurabilityGate::default(),
                ),
        );
        let budget = Arc::new(cost::BudgetEnforcer::new(
            cost::BudgetConfig {
                max_session_nano_usd: Some(session_limit),
                max_turn_nano_usd: None,
                max_turn_tokens: None,
                warning_thresholds: vec![],
                on_exceed: cost::BudgetExceedPolicy::Halt,
            },
            tracker.clone(),
        ));
        let outputs = budget.workflow_output_scopes();
        let scope = outputs
            .begin_turn(
                session,
                lingxi_core::types::MessageId::new(),
                Some(output_limit),
            )
            .await
            .unwrap();
        let mut authority = authority().await;
        let inner = Arc::get_mut(&mut authority).unwrap();
        inner.output = scope;
        inner.tracker = tracker.scoped(session);
        inner.budget = budget.clone();
        inner.captured.control = lingxi_core::host::FusionRunControl::new_with_billing_mode(
            lingxi_core::host::FusionRunIdentity::new(
                lingxi_core::host::FusionRunId::generated(),
                Some(session),
                lingxi_core::host::FusionOrigin::Slash,
                None,
            ),
            60_000,
            tokio_util::sync::CancellationToken::new(),
            Default::default(),
            lingxi_core::host::ModelAttemptBillingMode::MeteredAttempts,
        );
        assert!(inner
            .captured
            .control
            .activate_at(tokio::time::Instant::now()));
        let mut pinned = route();
        pinned.resolved = service
            .resolve_media_route("wire", Some("profile"))
            .unwrap()
            .main;
        if dynamic {
            let bounds = service
                .attempt_price_bounds(&pinned.resolved)
                .unwrap()
                .unwrap();
            pinned.pricing = pricing::with_rate_bounds(&pinned.pricing, &bounds.standard).unwrap();
            pinned.token_pricing_requires_quote = true;
        }
        inner
            .routes
            .insert((ModelAttemptStage::Panel, Some(0)), pinned);
        let run = Arc::new(ModelAttemptRun::new(authority.clone()));
        let host = DesktopFusionAttempts::new(
            service.clone(),
            budget,
            tracker,
            Arc::new(cost::PricingCatalog::empty()),
            outputs,
        );
        host.registry
            .lock()
            .unwrap()
            .insert(run.registration_id(), Arc::downgrade(&authority));
        service.set_model_attempt_hooks(host);
        let mut request = llm_runtime::LlmRequest::new("wire").with_user_text("hello");
        request.profile = Some("profile".into());
        request.input.max_tokens = Some(50);
        request.execution.model_attempt =
            Some(run.context(ModelAttemptStage::Panel, Some(0)).unwrap());
        Self {
            service,
            authority,
            request,
            queue,
            ledger: cost::AttemptLedger::new(session),
            state: cost::CostStateVector::from(&initial),
            journal: 0,
            calls,
        }
    }
    fn acknowledge(&mut self, request: cost::AttemptPersistRequest) {
        self.journal += 1;
        let (id, receipt, applied) = match request.mutation {
            cost::AttemptPersistMutation::Intent(intent) => {
                let id = format!("attempt-intent:{}", intent.attempt_id);
                (id, None, self.ledger.record_intent(intent).unwrap())
            }
            cost::AttemptPersistMutation::Receipt(receipt) => {
                let id = format!(
                    "attempt-receipt:{}:{}",
                    receipt.attempt_id, receipt.revision
                );
                // A later attempt shares the session's existing completion
                // marker; only the first receipt starts from None.
                let last_usage_revision = self.state.last_usage_revision;
                let ack = self
                    .ledger
                    .fold_receipt(&mut self.state, receipt, last_usage_revision)
                    .unwrap();
                (id, Some(ack), true)
            }
        };
        request
            .ack
            .send(Ok(cost::AttemptPersistAck {
                persistence: cost::CostPersistAck {
                    mutation_id: cost::CostMutationId::new(id),
                    journal_revision: self.journal,
                    cost_revision: self.state.cost_revision,
                },
                state: self.state.clone(),
                receipt,
                applied,
            }))
            .unwrap();
    }
    fn permits(&self) -> usize {
        self.authority.profiles.lock().unwrap()["profile"].available_permits()
    }
}

#[tokio::test]
async fn desktop_attempt_panel_fence_waits_for_durable_receipt_and_keeps_analyst_live() {
    use fusion::FusionPanelAttemptFence;
    let mut harness = DurableHarness::new(true).await;
    let service = harness.service.clone();
    let request = harness.request.clone();
    let begin = tokio::spawn(async move { service.stream_request(request).await });
    let intent = harness.queue.recv().await.unwrap();
    assert_eq!(harness.authority.state.lock().unwrap().panel_pending, 1);
    harness.authority.close();
    assert!(harness
        .service
        .stream_request(harness.request.clone())
        .await
        .is_err());
    assert_eq!(harness.calls.load(Ordering::SeqCst), 0);
    let authority = harness.authority.clone();
    let mut fence = Box::pin(authority.wait());
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(fence.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    // Closing after admission but before its intent ack must also deny final mark.
    harness.acknowledge(intent);
    let receipt = harness.queue.recv().await.unwrap();
    let cost::AttemptPersistMutation::Receipt(observed) = &receipt.mutation else {
        panic!("receipt expected")
    };
    assert_eq!(
        observed.disposition,
        cost::AttemptDisposition::ProvenNotSent
    );
    assert_eq!(harness.calls.load(Ordering::SeqCst), 0);
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(fence.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    // Dropping a fence waiter does not cancel the owned receipt worker.
    drop(fence);
    harness.acknowledge(receipt);
    assert!(begin.await.unwrap().is_err());
    authority.wait().await.unwrap();
    assert_eq!(harness.permits(), 4);
    assert!(!authority.state.lock().unwrap().closed);
    authority.live((ModelAttemptStage::Analyst, None)).unwrap();
    assert!(authority.live((ModelAttemptStage::Panel, Some(0))).is_err());
    assert!(authority
        .tracker
        .durability_gate()
        .frozen_reason()
        .is_none());
}

#[tokio::test]
async fn desktop_attempt_durable_dropped_begin_waits_for_not_sent_receipt_ack() {
    use fusion::FusionAttemptFinalizer;
    let mut harness = DurableHarness::new(true).await;
    let service = harness.service.clone();
    let request = harness.request.clone();
    let begin = tokio::spawn(async move { service.stream_request(request).await });
    let intent = harness.queue.recv().await.unwrap();
    assert_eq!(harness.permits(), 3);
    begin.abort();
    let _ = begin.await;
    let finalizer = Box::new(RunFinalizer {
        authority: Some(harness.authority.clone()),
    })
    .finish();
    let final_wait = tokio::spawn(async move { finalizer.wait().await });
    harness.acknowledge(intent);
    let receipt = harness.queue.recv().await.unwrap();
    let cost::AttemptPersistMutation::Receipt(observed) = &receipt.mutation else {
        panic!("receipt expected");
    };
    assert_eq!(
        observed.disposition,
        cost::AttemptDisposition::ProvenNotSent
    );
    assert_eq!(harness.calls.load(Ordering::SeqCst), 0);
    assert_eq!(harness.permits(), 3);
    assert!(harness.authority.budget.active_reservation_nano_usd().await > 0);
    assert!(!final_wait.is_finished());
    harness.acknowledge(receipt);
    let summary = final_wait.await.unwrap().unwrap();
    assert_eq!(summary.usage.realized_nano_usd, 0);
    assert_eq!(summary.usage.provider_requests, 0);
    assert_eq!(harness.authority.output.spent(), 0);
    assert_eq!(
        harness.authority.budget.active_reservation_nano_usd().await,
        0
    );
    assert_eq!(harness.permits(), 4);
}

#[tokio::test]
async fn desktop_attempt_durable_stream_complete_and_partial_publish_once() {
    use fusion::FusionAttemptFinalizer;
    for complete in [true, false] {
        let mut harness = DurableHarness::new(complete).await;
        let service = harness.service.clone();
        let request = harness.request.clone();
        let call = tokio::spawn(async move {
            let mut stream = service.stream_request(request).await.unwrap();
            while let Some(event) = stream.next().await {
                event.unwrap();
            }
        });
        let intent = harness.queue.recv().await.unwrap();
        harness.acknowledge(intent);
        let receipt = harness.queue.recv().await.unwrap();
        let cost::AttemptPersistMutation::Receipt(observed) = &receipt.mutation else {
            panic!("receipt expected");
        };
        assert_eq!(
            observed.disposition,
            if complete {
                cost::AttemptDisposition::Exact
            } else {
                cost::AttemptDisposition::Unknown
            }
        );
        let finalizer = Box::new(RunFinalizer {
            authority: Some(harness.authority.clone()),
        })
        .finish();
        let final_wait = tokio::spawn(async move { finalizer.wait().await });
        assert!(!final_wait.is_finished());
        assert_eq!(harness.permits(), 3);
        harness.acknowledge(receipt);
        call.await.unwrap();
        let summary = final_wait.await.unwrap().unwrap();
        assert_eq!(summary.usage.output_tokens, 5);
        assert_eq!(summary.usage.reasoning_tokens, 3);
        assert_eq!(summary.usage.provider_requests, 1);
        assert_eq!(summary.usage.estimated, !complete);
        assert_eq!(
            harness.authority.output.spent(),
            if complete { 8 } else { 50 }
        );
        assert_eq!(
            harness.authority.budget.active_reservation_nano_usd().await,
            0
        );
        assert_eq!(harness.permits(), 4);
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert!(harness.queue.try_recv().is_err());
    }
}

#[tokio::test]
async fn desktop_dynamic_price_quote_is_persisted_for_stream_and_nonstream() {
    for stream in [true, false] {
        for complete in [true, false] {
            let mut harness = DurableHarness::with_pricing(complete, 100_000, 1_000, true).await;
            let service = harness.service.clone();
            let request = harness.request.clone();
            let call = tokio::spawn(async move {
                if stream {
                    let mut events = service.stream_request(request).await.unwrap();
                    while let Some(event) = events.next().await {
                        event.unwrap();
                    }
                } else {
                    service.execute_side_query_request(request).await.unwrap();
                }
            });
            let intent = harness.queue.recv().await.unwrap();
            let cost::AttemptPersistMutation::Intent(saved) = &intent.mutation else {
                panic!("intent")
            };
            assert!(saved.token_pricing_requires_quote);
            assert_eq!(
                saved.pricing.token_rates[&cost::TokenClass::Input].nano_usd_per_token,
                2
            );
            let saved_intent = saved.clone();
            harness.acknowledge(intent);
            let receipt = harness.queue.recv().await.unwrap();
            let cost::AttemptPersistMutation::Receipt(saved) = &receipt.mutation else {
                panic!("receipt")
            };
            assert_eq!(saved.token_quote_nano_usd, complete.then_some(13));
            assert_eq!(
                saved.disposition,
                if complete {
                    cost::AttemptDisposition::Exact
                } else {
                    cost::AttemptDisposition::Unknown
                }
            );
            let saved_receipt = saved.clone();
            harness.acknowledge(receipt);
            call.await.unwrap();
            assert_eq!(harness.state.total_nano_usd, if complete { 13 } else { 0 });
            assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
            // Restart replays captured money, independent of the next day's peak.
            let mut replay = cost::AttemptLedger::new(saved_intent.session_id);
            replay
                .record_intent(
                    serde_json::from_value(serde_json::to_value(saved_intent).unwrap()).unwrap(),
                )
                .unwrap();
            let mut state = cost::CostStateVector::from(&cost::CostState {
                session_id: saved_receipt.session_id,
                ..Default::default()
            });
            replay
                .fold_receipt(
                    &mut state,
                    serde_json::from_value(serde_json::to_value(saved_receipt).unwrap()).unwrap(),
                    None,
                )
                .unwrap();
            assert_eq!(state.total_nano_usd, harness.state.total_nano_usd);
        }
    }
}

#[tokio::test]
async fn desktop_attempt_registration_allows_pick_when_optional_synthesis_is_unavailable() {
    use fusion::FusionAttemptRegistrar;
    let harness = DurableHarness::new(true).await;
    let origin = &harness.authority.captured;
    assert!(origin.snapshot.config.max_reserved_nano_usd.is_none());
    let mut request = origin.request.clone();
    request.parent_model = "unavailable-parent".into();
    let panel = fusion::ResolvedPanel {
        profile: "profile".into(),
        model: "wire".into(),
    };
    let row = fusion::CatalogModel {
        profile: panel.profile.clone(),
        model: panel.model.clone(),
        hints: Default::default(),
        structured_output: true,
        limits: route().limits,
    };
    let snapshot = fusion::FusionRuntimeSnapshot::new(
        origin.snapshot.config.clone(),
        fusion::CatalogSnapshot::capture(&vec![row]).unwrap(),
        origin.snapshot.prices.clone(),
    );
    let captured = fusion::FusionAttemptRegistration {
        control: origin.control.clone(),
        inherit: origin.inherit.clone(),
        request,
        resolved: fusion::ResolvedSet {
            panels: vec![panel.clone()],
            analyst: panel,
        },
        snapshot: Arc::new(snapshot),
        live_policy: Arc::new(Live),
    };
    let host = DesktopFusionAttempts::new(
        harness.service.clone(),
        harness.authority.budget.clone(),
        harness.authority.tracker.clone(),
        Arc::new(cost::PricingCatalog::empty().with_entry(route().pricing)),
        harness.authority.budget.workflow_output_scopes(),
    );
    let registered = host
        .register(captured)
        .expect("unused synthesis must not reject a pick-capable run");
    let authority = host
        .registry
        .lock()
        .unwrap()
        .get(&registered.run.registration_id())
        .unwrap()
        .upgrade()
        .unwrap();
    assert!(authority
        .routes
        .contains_key(&(ModelAttemptStage::Panel, Some(0))));
    assert!(authority
        .routes
        .contains_key(&(ModelAttemptStage::Analyst, None)));
    assert_eq!(harness.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn desktop_attempt_default_run_cap_keeps_session_and_output_limits() {
    use fusion::FusionAttemptFinalizer;
    for (money, output) in [(1, 1_000), (10_000, 1)] {
        let mut harness = DurableHarness::with_limits(true, money, output).await;
        assert!(harness
            .authority
            .captured
            .snapshot
            .config
            .max_reserved_nano_usd
            .is_none());
        assert!(harness
            .service
            .stream_request(harness.request.clone())
            .await
            .is_err());
        let summary = Box::new(RunFinalizer {
            authority: Some(harness.authority.clone()),
        })
        .finish()
        .wait()
        .await
        .unwrap();
        assert_eq!(summary.usage.provider_requests, 0);
        assert_eq!(summary.usage.realized_nano_usd, 0);
        assert_eq!(harness.calls.load(Ordering::SeqCst), 0);
        assert_eq!(harness.authority.output.spent(), 0);
        assert_eq!(
            harness.authority.budget.active_reservation_nano_usd().await,
            0
        );
        assert!(harness
            .authority
            .tracker
            .durability_gate()
            .frozen_reason()
            .is_none());
        assert_eq!(harness.permits(), 4);
        assert!(
            harness.queue.try_recv().is_err(),
            "denied quote cannot enqueue intent"
        );
    }
}

struct NoTransport;
impl llm_runtime::test_support::FixtureTransport for NoTransport {
    fn execute<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> llm_runtime::transport::BoxFuture<'a, Result<llm_runtime::ProviderResponse, LlmError>>
    {
        Box::pin(async { panic!("offline host test must not send") })
    }
    fn open_stream<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> llm_runtime::transport::BoxFuture<
        'a,
        Result<llm_runtime::test_support::StreamingResponse, LlmError>,
    > {
        Box::pin(async { panic!("offline host test must not stream") })
    }
}
llm_runtime::impl_fixture_transport!(NoTransport);

fn tracker_and_budget() -> (Arc<cost::CostTracker>, Arc<cost::BudgetEnforcer>) {
    let (tx, _) = tokio::sync::mpsc::channel(1);
    let tracker = Arc::new(cost::CostTracker::new(
        lingxi_core::types::SessionId::new(),
        Arc::new(cost::PricingCatalog::empty()),
        tx,
    ));
    let budget = Arc::new(cost::BudgetEnforcer::new(
        cost::BudgetConfig {
            max_session_nano_usd: Some(1_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: cost::BudgetExceedPolicy::Halt,
        },
        tracker.clone(),
    ));
    (tracker, budget)
}

#[test]
fn desktop_attempt_service_hook_backedge_is_weak() {
    let service = Arc::new(llm_runtime::ApiService::new(
        Arc::new(llm_runtime::ModelRuntime::from_config(Default::default()).unwrap()),
        Arc::new(NoTransport),
        Default::default(),
        Default::default(),
        "test",
        None,
        None,
    ));
    let weak_service = Arc::downgrade(&service);
    let (tracker, budget) = tracker_and_budget();
    let outputs = budget.workflow_output_scopes();
    let host = DesktopFusionAttempts::new(
        service.clone(),
        budget,
        tracker,
        Arc::new(cost::PricingCatalog::empty()),
        outputs,
    );
    service.set_model_attempt_hooks(host.clone());
    let weak_host = Arc::downgrade(&host);
    drop(service);
    assert!(weak_service.upgrade().is_none());
    assert!(host.service.upgrade().is_none());
    drop(host);
    assert!(weak_host.upgrade().is_none());
}

fn route() -> PinnedRoute {
    let model = cost::ModelRef {
        provider: cost::ProviderId::OpenAI,
        model: "test".into(),
    };
    PinnedRoute {
        token_pricing_requires_quote: false,
        default_fast: false,
        resolved: llm_runtime::ResolvedRoute {
            provider_id: llm_runtime::ProviderId::OpenAI,
            profile_name: "profile".into(),
            request_model: "wire".into(),
            display_model: "display".into(),
            pricing_model: llm_runtime::PricingModelRef {
                pricing_provider_id: llm_runtime::ProviderId::OpenAI,
                billing_model: "test".into(),
                request_model: "wire".into(),
                display_model: "display".into(),
            },
            capabilities: Default::default(),
            connection_chain: Vec::new(),
            failover: Default::default(),
        },
        limits: fusion::ModelLimits {
            context_window_tokens: Some(20_000),
            max_input_tokens: Some(10_000),
            max_output_tokens: Some(100),
        },
        output_cap: 100,
        input_cap: None,
        pricing: cost::ModelPricing {
            model_ref: model,
            token_rates: [
                cost::TokenClass::Input,
                cost::TokenClass::Output,
                cost::TokenClass::CacheRead,
                cost::TokenClass::ReasoningOutput,
            ]
            .into_iter()
            .map(|class| {
                (
                    class,
                    cost::MoneyPerToken {
                        nano_usd_per_token: 2,
                    },
                )
            })
            .collect(),
            non_token_rates_nano_usd: HashMap::new(),
            effective_from: None,
            source: cost::PricingSource::BuiltInReference {
                provider: cost::ProviderId::OpenAI,
            },
        },
        fast: None,
    }
}

#[test]
fn desktop_attempt_quote_is_strict_and_counts_wire_utf16_overrides() {
    let mut route = route();
    let mut request = ProviderRequest::post_json(
        "https://unused.invalid",
        json!({"max_completion_tokens": 50, "messages":[{"role":"user","content":"x"}]}),
    );
    let plain = pricing::quote_body(&route, ProtocolFamily::OpenAiChat, &request).unwrap();
    assert_eq!(
        plain.1,
        cost::AttemptUsageContract::StandardDisjointTokensV1
    );
    assert_eq!(plain.3, 50);
    request
        .json_string_overrides
        .insert("/messages/0/content".into(), vec![0xD800; 64]);
    let overridden = pricing::quote_body(&route, ProtocolFamily::OpenAiChat, &request).unwrap();
    assert!(overridden.2 > plain.2);
    assert!(overridden.4 > plain.4);
    assert!(
        pricing::quote_body(&route, ProtocolFamily::AnthropicMessages, &request).is_err(),
        "missing TTL rates cannot become zero"
    );
    route
        .pricing
        .token_rates
        .remove(&cost::TokenClass::ReasoningOutput);
    assert!(pricing::quote_body(&route, ProtocolFamily::OpenAiChat, &request).is_err());
}

#[test]
fn desktop_attempt_quote_rejects_native_tools_ambiguous_caps_and_unpriced_fast() {
    let route = route();
    for body in [
        json!({"max_tokens":50,"tools":[{"type":"web_search_20250305","name":"web_search"}]}),
        json!({"max_tokens":50,"max_completion_tokens":100}),
        json!({"max_tokens":101}),
        json!({"max_tokens":50,"speed":"fast"}),
        json!({"max_tokens":50,"speed":42}),
        json!({"max_tokens":50,"web_search_options":{}}),
    ] {
        assert!(pricing::quote_body(
            &route,
            ProtocolFamily::OpenAiChat,
            &ProviderRequest::post_json("https://unused.invalid", body)
        )
        .is_err());
    }
}

#[test]
fn desktop_attempt_quote_uses_the_captured_default_tier() {
    let mut route = route();
    let mut fast = route.pricing.clone();
    for rate in fast.token_rates.values_mut() {
        rate.nano_usd_per_token *= 2;
    }
    route.fast = Some(fast.clone());
    route.default_fast = true;
    let mut request = ProviderRequest::post_json("https://example.test", json!({"max_tokens":50}));
    assert_eq!(
        pricing::quote_body(&route, ProtocolFamily::OpenAiChat, &request)
            .unwrap()
            .0,
        fast
    );
    request.body_json["service_tier"] = json!("default");
    assert_eq!(
        pricing::quote_body(&route, ProtocolFamily::OpenAiChat, &request)
            .unwrap()
            .0,
        route.pricing
    );
    request.body_json["service_tier"] = json!("priority");
    assert_eq!(
        pricing::quote_body(&route, ProtocolFamily::OpenAiChat, &request)
            .unwrap()
            .0,
        fast
    );
}

#[test]
fn desktop_attempt_explicit_reasoning_zero_and_fast_override_stay_pinned() {
    let mut route = route();
    route.pricing.token_rates.insert(
        cost::TokenClass::ReasoningOutput,
        cost::MoneyPerToken {
            nano_usd_per_token: 0,
        },
    );
    let catalog = cost::PricingCatalog::empty().with_entry(route.pricing.clone());
    let config = llm_runtime::PricingConfig {
        overrides: vec![(
            "test".into(),
            llm_runtime::PricingOverride {
                input_per_million: 0.002,
                output_per_million: 0.002,
                cache_read_per_million: 0.002,
                cache_write_per_million: 0.0,
                reasoning_per_million: 0.0,
            },
        )],
        ..Default::default()
    };
    let (normal, fast) =
        pricing::captured_prices(&catalog, &route.pricing.model_ref, &route.resolved, &config)
            .unwrap();
    assert_eq!(
        normal.token_rates[&cost::TokenClass::ReasoningOutput].nano_usd_per_token,
        0
    );
    assert_eq!(fast.unwrap(), normal);
    let stale = llm_runtime::PricingConfig {
        overrides: vec![(
            "test".into(),
            llm_runtime::PricingOverride::input_output(1.0, 1.0),
        )],
        ..Default::default()
    };
    assert!(
        pricing::captured_prices(&catalog, &route.pricing.model_ref, &route.resolved, &stale)
            .is_err(),
        "a different live override must not silently reuse stale catalog rates"
    );
    let subscription = llm_runtime::PricingConfig {
        billing_mode: lingxi_core::host::ModelBillingMode::Subscription,
        ..Default::default()
    };
    assert!(pricing::captured_prices(
        &catalog,
        &route.pricing.model_ref,
        &route.resolved,
        &subscription
    )
    .is_err());
}

#[tokio::test]
async fn desktop_attempt_wait_slot_survives_dropped_waiter() {
    let slot = Arc::new(WaitSlot::default());
    let waiter = HostWaiter(slot.clone());
    drop(waiter);
    let (started, receive) = tokio::sync::oneshot::channel();
    let owner = slot.clone();
    let task = tokio::spawn(async move {
        started.send(()).unwrap();
        owner.complete(Ok(()));
    });
    receive.await.unwrap();
    slot.wait().await.unwrap();
    task.await.unwrap();
}

struct InertTools;
#[async_trait]
impl lingxi_core::host::ToolInvoker for InertTools {
    async fn invoke(
        &self,
        _: &str,
        _: serde_json::Value,
        _: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        Ok(serde_json::Value::Null)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct Live;
impl fusion::FusionAttemptLivePolicy for Live {
    fn validate(
        &self,
        _: ModelAttemptStage,
        _: Option<u32>,
    ) -> Result<(), lingxi_core::host::FusionError> {
        Ok(())
    }
}
struct Output(lingxi_core::types::SessionId, lingxi_core::types::MessageId);
impl lingxi_core::host::WorkflowOutputAccount for Output {
    fn session_id(&self) -> lingxi_core::types::SessionId {
        self.0
    }
    fn generation_id(&self) -> lingxi_core::types::MessageId {
        self.1
    }
    fn spent(&self) -> u64 {
        0
    }
    fn record_legacy(
        &self,
        _: lingxi_core::host::WorkflowOutputEventId,
        _: u64,
    ) -> Result<(), lingxi_core::host::BudgetError> {
        Ok(())
    }
}

async fn authority() -> Arc<RunAuthority> {
    let (tracker, budget) = tracker_and_budget();
    let session = tracker.session_id().await;
    let output = WorkflowOutputScope::new(Arc::new(Output(
        session,
        lingxi_core::types::MessageId::new(),
    )));
    let control = lingxi_core::host::FusionRunControl::new_with_billing_mode(
        lingxi_core::host::FusionRunIdentity::new(
            lingxi_core::host::FusionRunId::generated(),
            Some(session),
            lingxi_core::host::FusionOrigin::Slash,
            None,
        ),
        1_000,
        tokio_util::sync::CancellationToken::new(),
        Default::default(),
        lingxi_core::host::ModelAttemptBillingMode::MeteredAttempts,
    );
    let config = fusion::FusionRuntimeConfig::defaults();
    let catalog = fusion::CatalogSnapshot::capture(&Vec::<fusion::CatalogModel>::new()).unwrap();
    let prices = fusion::CapturedPriceBook::capture(&(), Vec::<(String, String)>::new());
    let captured = fusion::FusionAttemptRegistration {
        control,
        inherit: lingxi_core::host::FusionInheritance::new(
            lingxi_core::host::SubagentInheritance {
                tool_invoker: Arc::new(InertTools),
                budget: budget.clone(),
            },
            tokio_util::sync::CancellationToken::new(),
        ),
        request: lingxi_core::host::FusionRequest {
            verify_claims: false,
            schema_version: lingxi_core::host::FUSION_SCHEMA_VERSION,
            origin: lingxi_core::host::FusionOrigin::Slash,
            prompt: "test".into(),
            preset: lingxi_core::host::FusionPreset::Quality,
            models: None,
            dimensions: vec!["correctness".into()],
            partial_ok: true,
            max_panel: None,
            cross_provider: false,
            parent_profile: "profile".into(),
            parent_model: "test".into(),
            mode: Default::default(),
            verify_commands: Vec::new(),
        },
        resolved: fusion::ResolvedSet {
            panels: vec![],
            analyst: fusion::ResolvedPanel {
                profile: "profile".into(),
                model: "test".into(),
            },
        },
        snapshot: Arc::new(fusion::FusionRuntimeSnapshot::new(config, catalog, prices)),
        live_policy: Arc::new(Live),
    };
    Arc::new(RunAuthority {
        captured,
        output,
        tracker,
        budget,
        profiles: Arc::new(Mutex::new(HashMap::new())),
        routes: HashMap::new(),
        state: Mutex::new(RunState::default()),
        changed: tokio::sync::Notify::new(),
        runtime: Mutex::new(Some(tokio::runtime::Handle::current())),
    })
}

#[tokio::test]
async fn desktop_attempt_run_finalizer_drains_after_waiter_drop_without_registry_cycle() {
    use fusion::FusionAttemptFinalizer;
    let authority = authority().await;
    authority.state.lock().unwrap().pending = 1;
    let run = ModelAttemptRun::new(authority.clone());
    let mut registry = HashMap::new();
    registry.insert(run.registration_id(), Arc::downgrade(&authority));
    let spoof = ModelAttemptRun::new(Arc::new(()));
    assert!(!registry.contains_key(&spoof.registration_id()));
    let weak = Arc::downgrade(&authority);
    let waiter = Box::new(RunFinalizer {
        authority: Some(authority.clone()),
    })
    .finish();
    assert!(authority.state.lock().unwrap().closed);
    drop(waiter);
    let ack = cost::CostAttemptSettlement {
        persistence: cost::CostPersistAck {
            mutation_id: cost::CostMutationId::new("test"),
            journal_revision: 1,
            cost_revision: 1,
        },
        receipt: Some(cost::AttemptFoldAck {
            cost_revision: 1,
            last_usage_revision: None,
            contribution: Default::default(),
        }),
        applied: true,
    };
    authority.complete("test", Ok(ack)).unwrap();
    drop(run);
    drop(authority);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while weak.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(registry.values().all(|entry| entry.upgrade().is_none()));
}
