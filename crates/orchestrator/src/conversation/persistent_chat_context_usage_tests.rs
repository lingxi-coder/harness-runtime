// Included by persistent_chat_context_tests.rs for the physically owned Chat fixture.

#[derive(Clone, Copy)]
enum ObservedSummaryEnd {
    Complete,
    Error,
    Pending,
}

struct ObservedSummaryClient {
    estimator: Arc<SummaryClient>,
    observed: Arc<tokio::sync::Notify>,
    end: ObservedSummaryEnd,
}

fn summary_usage() -> cost::Usage {
    cost::Usage {
        tokens: cost::TokenUsage {
            input: 42,
            output: 7,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[async_trait]
impl sidequery::SideQueryClient for ObservedSummaryClient {
    async fn query(
        &self,
        _: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        panic!("Chat summary must carry admission and its usage observer")
    }

    fn estimate_request(
        &self,
        request: sidequery::CanonicalSideQueryRequest,
    ) -> Result<sidequery::SideQueryEstimate, sidequery::SideQueryError> {
        sidequery::SideQueryClient::estimate_request(self.estimator.as_ref(), request)
    }

    async fn query_with_dispatch_admission_and_usage_observer(
        &self,
        _: sidequery::SideQueryRequest,
        admission: llm_runtime::RequestDispatchAdmission,
        observer: sidequery::ForkedUsageObserver,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        assert!(admission.is_admitted());
        let mut first = summary_usage();
        first.tokens.output = 0;
        observer.observe(first);
        observer.observe(summary_usage());
        self.observed.notify_one();
        match self.end {
            ObservedSummaryEnd::Pending => std::future::pending().await,
            ObservedSummaryEnd::Error => Err(sidequery::SideQueryError::Api(
                llm_runtime::LlmError::StreamInterrupted {
                    message: "summary disconnected".into(),
                },
            )),
            ObservedSummaryEnd::Complete => Ok(sidequery::SideQueryResponse {
                text: Some("Keep the agreed constraint.".into()),
                structured: None,
                tool_calls: Vec::new(),
                usage: summary_usage(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            }),
        }
    }
}

async fn observed_summary_fixture(
    end: ObservedSummaryEnd,
) -> (
    ChatFixture,
    Arc<cost::CostTracker>,
    Arc<tokio::sync::Notify>,
) {
    let mut fixture = fixture(false, Some(8000)).await;
    let session = fixture.orch.session.lock().await.session_id;
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let tracker = Arc::new(cost::CostTracker::new(
        session,
        Arc::new(cost::PricingCatalog::builtin_reference()),
        tx,
    ));
    let observed = Arc::new(tokio::sync::Notify::new());
    let orch = Arc::get_mut(&mut fixture.orch).expect("fixture owns the unshared orchestrator");
    orch.model_runtime.cost_tracker = Some(tracker.clone());
    let context = Arc::get_mut(orch.persistent_chat_context.as_mut().unwrap()).unwrap();
    context.config.client = Arc::new(ObservedSummaryClient {
        estimator: fixture.summary.clone(),
        observed: observed.clone(),
        end,
    });
    (fixture, tracker, observed)
}

#[tokio::test]
async fn summary_complete_and_error_each_charge_observed_cumulative_usage_once() {
    for end in [ObservedSummaryEnd::Complete, ObservedSummaryEnd::Error] {
        let (fixture, tracker, _) = observed_summary_fixture(end).await;
        let result = prepare_current(&fixture).await;
        assert_eq!(result.is_ok(), matches!(end, ObservedSummaryEnd::Complete));
        tracker.drain_owned_settlements().await.unwrap();
        let state = tracker.snapshot().await;
        assert_eq!(state.cost_revision, 1);
        assert_eq!(state.last_usage, Some(summary_usage()));
        assert_eq!(fixture.orch.snapshot_cost_real().await.api_calls, 1);
    }
}

#[tokio::test]
async fn cancelled_and_dropped_summary_preserve_originating_session_usage() {
    for drop_future in [false, true] {
        let (fixture, tracker, observed) =
            observed_summary_fixture(ObservedSummaryEnd::Pending).await;
        let original_session = fixture.orch.session.lock().await.session_id;
        fixture
            .orch
            .append_external_history_message(ConversationMessage::user(
                MessageId::new(),
                format!("current-only {}", "c".repeat(6000)),
            ))
            .await;
        let cancel = CancellationToken::new();
        let history = fixture.orch.snapshot_history().await;
        let mut query = Box::pin(fixture.orch.prepare_persistent_chat_request(
            history,
            None,
            "fixture-model",
            None,
            &[],
            Some(&cancel),
        ));
        tokio::select! {
            result = &mut query => panic!("summary unexpectedly finished: {result:?}"),
            () = observed.notified() => {}
        }
        let replacement_session = SessionId::new();
        tracker.switch_session(replacement_session).await.unwrap();
        if drop_future {
            drop(query);
        } else {
            cancel.cancel();
            assert!(query.await.is_err());
        }
        tracker.drain_owned_settlements().await.unwrap();
        let origin = tracker.scoped(original_session).snapshot().await;
        assert_eq!(origin.cost_revision, 1);
        assert_eq!(origin.last_usage, Some(summary_usage()));
        assert_eq!(fixture.orch.snapshot_cost_real().await.api_calls, 1);
        assert!(
            tracker.snapshot().await.per_model_usage.is_empty(),
            "replacement session owns no part of this summary"
        );
        assert!(!std::fs::read_to_string(&fixture.path)
            .unwrap()
            .contains(RECEIPT_KIND));
    }
}
