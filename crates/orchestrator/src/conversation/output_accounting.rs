//! Captured main-response participation in the host's shared output book.
use super::*;
use lingxi_core::host::{WorkflowOutputEventId, WorkflowOutputScope};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub(crate) struct OutputTurn {
    scope: WorkflowOutputScope,
    failed: Arc<AtomicBool>,
}

pub(crate) struct MainOutputObservation {
    turn: OutputTurn,
    event: MessageId,
    visible: u64,
    reasoning: u64,
    observed: bool,
    finished: bool,
}

impl MainOutputObservation {
    pub(crate) fn fork(&self) -> Self {
        Self {
            turn: self.turn.clone(),
            event: MessageId::new(),
            visible: 0,
            reasoning: 0,
            observed: false,
            finished: false,
        }
    }

    pub(crate) fn observe(&mut self, usage: &llm_runtime::ExecutionUsage) {
        // SDK partial reports are cumulative snapshots too: reclassification
        // of output into reasoning must not charge both old and new buckets.
        if let Some(counts) = usage.report.usage {
            self.visible = counts.output_tokens.saturating_sub(counts.reasoning_tokens);
            self.reasoning = counts.reasoning_tokens;
            self.observed = true;
        }
    }

    fn observe_completed(&mut self, usage: &llm_runtime::ExecutionUsage) {
        // A completed output report replaces provisional bucket splits;
        // max-per-bucket would double count reclassified reasoning tokens.
        // An absent/default Completed usage must not erase retained partials.
        // Speed/context/diagnostic metadata alone is not an output report.
        let has_output = usage.report.complete().is_some();
        if has_output {
            self.visible = usage
                .counts()
                .output_tokens
                .saturating_sub(usage.counts().reasoning_tokens);
            self.reasoning = usage.counts().reasoning_tokens;
            self.observed = true;
        }
    }

    pub(crate) fn finish(&mut self) -> Result<(), OrchestratorError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        if !self.observed {
            return Ok(());
        }
        let result = self
            .visible
            .checked_add(self.reasoning)
            .ok_or_else(|| "main response output overflow".to_string())
            .and_then(|tokens| {
                self.turn
                    .scope
                    .record_legacy(WorkflowOutputEventId::MainResponse(self.event), tokens)
                    .map_err(|error| error.to_string())
            });
        result.map_err(|error| {
            self.turn.failed.store(true, Ordering::Release);
            OrchestratorError::Internal(format!("output accounting failed: {error}"))
        })
    }
}

impl Drop for MainOutputObservation {
    fn drop(&mut self) {
        // Cancellation retains already observed real usage, never estimated
        // text lengths. A failed drop publication blocks subsequent dispatch.
        let _ = self.finish();
    }
}

pub(crate) fn account_stream(
    stream: futures::stream::BoxStream<
        'static,
        Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
    >,
    observation: Option<MainOutputObservation>,
) -> futures::stream::BoxStream<'static, Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>> {
    let Some(observation) = observation else {
        return stream;
    };
    Box::pin(OutputStream {
        stream,
        observation,
    })
}

struct OutputStream {
    stream: futures::stream::BoxStream<
        'static,
        Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
    >,
    observation: MainOutputObservation,
}

impl futures::Stream for OutputStream {
    type Item = Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use llm_runtime::HistoryEvent;
        use std::task::Poll;
        let this = self.get_mut();
        let polled = this.stream.as_mut().poll_next(cx);
        let terminal = match &polled {
            Poll::Ready(Some(Ok(event))) => {
                match event {
                    HistoryEvent::MessageStart { response } => {
                        this.observation.observe(&response.usage)
                    }
                    HistoryEvent::Completed { response } => {
                        this.observation.observe_completed(&response.usage)
                    }
                    HistoryEvent::MessageDelta {
                        usage: Some(usage), ..
                    } => this.observation.observe(usage),
                    _ => {}
                }
                matches!(event, HistoryEvent::Completed { .. })
            }
            Poll::Ready(None | Some(Err(_))) => true,
            Poll::Pending => false,
        };
        if terminal {
            // Preserve provider usage AND its original terminal error/EOF.
            // The driver observes the latch only after handing retained usage
            // to the existing cost owner; replacing this item loses evidence.
            let _ = this.observation.finish();
        }
        polled
    }
}

impl ConversationOrchestrator {
    /// Snapshot only usage facts this orchestrator has actually observed.
    /// This is the host-side source for the main-loop `session.measure` event;
    /// breakdown-only context estimates are deliberately not computed here.
    pub(crate) async fn mod_session_measure_snapshot(&self) -> ModSessionMeasureSnapshot {
        let model = self.session.lock().await.model.clone();
        let window = llm_runtime::model::context_window::context_window_for_model(
            &model,
            &self.api.active_betas(),
        );
        let tokens = self
            .compaction_runtime
            .last_response_input_tokens
            .load(Ordering::Relaxed);
        let mut context = serde_json::json!({"window":window});
        if tokens > 0 && window > 0 {
            let percent = ((tokens as f64 / window as f64) * 100.0)
                .round()
                .clamp(0.0, 100.0);
            context["tokens"] = serde_json::json!(tokens);
            context["percent"] = serde_json::json!(percent);
        }

        let rate_limits = self
            .api
            .last_raw_utilization()
            .map_or_else(Vec::new, |raw| {
                let mut limits = Vec::with_capacity(2);
                if let Some(window) = raw.five_hour {
                    limits.push(session_measure_rate_limit("five_hour", window));
                }
                if let Some(window) = raw.seven_day {
                    limits.push(session_measure_rate_limit("seven_day", window));
                }
                limits
            });
        let cost_usd = if self.model_runtime.cost_tracker.is_some() {
            Some(self.snapshot_cost_real().await.total_usd)
        } else {
            None
        };
        let limit_status = self
            .api
            .last_rate_limit_full()
            .and_then(|snapshot| snapshot.status);

        let mut input = serde_json::json!({
            "context":context,
            "rateLimits":rate_limits,
        });
        if let Some(usd) = cost_usd {
            input["cost"] = serde_json::json!({"usd":usd});
        }
        ModSessionMeasureSnapshot {
            input,
            cost_usd,
            limit_status,
        }
    }

    /// Build the live `$.session.usage()` result from facts owned by the
    /// orchestrator. `startedAt` is reconstructed from the runtime's monotonic
    /// session clock because this host does not persist a wall-clock session
    /// start timestamp; it is therefore a best-effort ISO timestamp, not a
    /// byte-exact restoration of Claude's cost-ledger date.
    ///
    /// The native optional breakdown is computed by `contextData({ detail,
    /// terminalWidth })`. This host has no equivalent breakdown builder, so a
    /// valid request is surfaced as unavailable instead of returning invented
    /// data. `columns` is otherwise ignored when `breakdown` is absent, as in
    /// the native path.
    pub(crate) async fn mod_session_usage_snapshot(
        &self,
        breakdown: Option<&str>,
        columns: Option<serde_json::Number>,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        if breakdown.is_some() {
            return Err(hooks::mods::ModError::Unavailable(
                "session.usage context breakdown needs a contextData builder, which this host does not have".into(),
            ));
        }
        let _ = columns;

        let elapsed = self
            .model_runtime
            .session_started_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed();
        let started_at = SystemTime::now().checked_sub(elapsed).unwrap_or(UNIX_EPOCH);
        let started_at = chrono::DateTime::<chrono::Utc>::from(started_at)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        let model = self.session.lock().await.model.clone();
        let window = llm_runtime::model::context_window::context_window_for_model(
            &model,
            &self.api.active_betas(),
        );
        let tokens = self
            .compaction_runtime
            .last_response_input_tokens
            .load(Ordering::Relaxed);
        let mut context = serde_json::json!({"window":window});
        if tokens > 0 && window > 0 {
            #[allow(clippy::cast_precision_loss)]
            let percent = ((tokens as f64 / window as f64) * 100.0)
                .round()
                .clamp(0.0, 100.0);
            context["tokens"] = serde_json::json!(tokens);
            context["percent"] = serde_json::json!(percent);
        }

        let rate_limits = session_usage_rate_limits(self.api.last_raw_utilization());
        let cost_usd = self.snapshot_cost_real().await.total_usd;
        Ok(serde_json::json!({
            "startedAt":started_at,
            "context":context,
            "rateLimits":rate_limits,
            "cost":{"usd":cost_usd},
        }))
    }

    pub(crate) async fn prepare_output_session(
        &self,
        session: SessionId,
    ) -> Result<Option<OutputTurn>, OrchestratorError> {
        let Some(provider) = &self.model_runtime.output_scopes else {
            return Ok(None);
        };
        self.check_output_accounting()?;
        let scope = provider
            .ensure_current(session, MessageId::new(), self.config.token_budget)
            .await
            .map_err(|error| {
                OrchestratorError::Internal(format!("output session preparation failed: {error}"))
            })?;
        if scope.session_id() != session {
            return Err(OrchestratorError::Internal(
                "prepared output session identity mismatch".into(),
            ));
        }
        Ok(Some(OutputTurn {
            scope,
            failed: Arc::new(AtomicBool::new(false)),
        }))
    }

    pub(crate) fn install_output_session(&self, prepared: Option<OutputTurn>) {
        *self
            .model_runtime
            .output_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = prepared;
    }

    /// Disclose an accounting write that failed AFTER the provider answered.
    ///
    /// The answer is kept: the user paid for it and the model produced it.
    /// What must not happen is another paid call on a ledger that cannot
    /// record it, and that is already enforced before dispatch --
    /// `cost_scope.preflight()` fails once the durability gate is frozen, at
    /// every call site in both turn loops. So this reports; it does not gate.
    pub(crate) async fn note_cost_settlement_failure(&self, error: &impl std::fmt::Display) {
        tracing::error!(%error, "cost settlement failed after the provider response");
        self.output
            .emit_system_notice(
                &format!(
                    "Spend for that response could not be recorded ({error}). \
The answer is unaffected, and /cost will under-report this session. New model \
calls are paused for this session only: start a new one with /clear, or \
restart, which rebuilds a damaged ledger."
                ),
                true,
            )
            .await;
    }

    /// Refuse further dispatch within a turn whose output write already
    /// failed. A fresh turn is fine: a storage fault is a fact about the write
    /// that failed, not a verdict on the session. What stops paid work on a
    /// broken ledger is the durability preflight, checked before every
    /// dispatch in both turn loops.
    pub(crate) fn check_output_accounting(&self) -> Result<(), OrchestratorError> {
        let failed = self
            .model_runtime
            .output_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|turn| turn.failed.load(Ordering::Acquire));
        if failed {
            Err(OrchestratorError::Internal(
                "output accounting for this turn is unavailable".into(),
            ))
        } else {
            Ok(())
        }
    }
    pub(crate) async fn begin_output_turn(
        &self,
        generation: MessageId,
    ) -> Result<(), OrchestratorError> {
        let Some(provider) = &self.model_runtime.output_scopes else {
            return Ok(());
        };
        let session = self.session.lock().await.session_id;
        let scope = provider
            .begin_turn(session, generation, self.config.token_budget)
            .await
            .map_err(|error| {
                OrchestratorError::Internal(format!("output turn unavailable: {error}"))
            })?;
        if scope.session_id() != session || scope.generation_id() != generation {
            return Err(OrchestratorError::Internal(
                "output turn identity mismatch".into(),
            ));
        }
        *self
            .model_runtime
            .output_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(OutputTurn {
            scope,
            failed: Arc::new(AtomicBool::new(false)),
        });
        Ok(())
    }

    pub(crate) async fn capture_main_output(
        &self,
    ) -> Result<Option<MainOutputObservation>, OrchestratorError> {
        let Some(provider) = &self.model_runtime.output_scopes else {
            return Ok(None);
        };
        let session = self.session.lock().await.session_id;
        let turn = self
            .model_runtime
            .output_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| OrchestratorError::Internal("output turn not initialized".into()))?;
        let current = provider.capture(session).map_err(|error| {
            OrchestratorError::Internal(format!("output scope unavailable: {error}"))
        })?;
        if turn.scope.session_id() != session
            || !turn.scope.shares_account(&current)
            || turn.failed.load(Ordering::Acquire)
        {
            return Err(OrchestratorError::Internal(
                "output scope changed or failed".into(),
            ));
        }
        Ok(Some(MainOutputObservation {
            turn,
            event: MessageId::new(),
            visible: 0,
            reasoning: 0,
            observed: false,
            finished: false,
        }))
    }
}

fn session_measure_rate_limit(
    kind: &str,
    window: llm_runtime::model::rate_limit::RawWindow,
) -> serde_json::Value {
    let mut limit = serde_json::json!({
        "kind":kind,
        "percentUsed":(window.utilization * 1000.0).round() / 10.0,
    });
    if let Ok(seconds) = i64::try_from(window.resets_at) {
        if let Some(reset) = chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0) {
            limit["resetsAt"] =
                serde_json::json!(reset.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        }
    }
    limit
}

fn session_usage_rate_limits(
    raw: Option<llm_runtime::model::rate_limit::RawUtilization>,
) -> Vec<serde_json::Value> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let now_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let latest_reset = now_seconds.saturating_add(31_536_000);
    let mut limits = Vec::with_capacity(2);
    // Native `Pq()` drops raw windows whose reset is expired or more than a
    // year away. `RawUtilization` only stores the five-hour and seven-day
    // windows; this host has no raw gateway overage window for `spend_limit`.
    if let Some(window) = raw
        .five_hour
        .filter(|w| w.resets_at > now_seconds && w.resets_at < latest_reset)
    {
        limits.push(session_measure_rate_limit("five_hour", window));
    }
    if let Some(window) = raw
        .seven_day
        .filter(|w| w.resets_at > now_seconds && w.resets_at < latest_reset)
    {
        limits.push(session_measure_rate_limit("seven_day", window));
    }
    limits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream,
        NoOpPermissionGate, StaticMemoryProvider,
    };
    use futures::StreamExt;
    use lingxi_core::host::{BudgetError, WorkflowOutputAccount, WorkflowOutputScopes};
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct Account {
        session: SessionId,
        generation: MessageId,
        events: Mutex<HashMap<WorkflowOutputEventId, u64>>,
        /// Fail the next write, standing in for a real storage fault.
        fail_next: AtomicBool,
    }
    impl WorkflowOutputAccount for Account {
        fn session_id(&self) -> SessionId {
            self.session
        }
        fn generation_id(&self) -> MessageId {
            self.generation
        }
        fn spent(&self) -> u64 {
            self.events.lock().unwrap().values().sum()
        }
        fn record_legacy(
            &self,
            event: WorkflowOutputEventId,
            tokens: u64,
        ) -> Result<(), BudgetError> {
            if self.fail_next.swap(false, Ordering::AcqRel) {
                return Err(BudgetError::Internal("ledger volume went away".into()));
            }
            let mut events = self.events.lock().unwrap();
            if let Some(old) = events.get(&event) {
                if *old != tokens {
                    return Err(BudgetError::Internal("conflict".into()));
                }
            } else {
                events.insert(event, tokens);
            }
            Ok(())
        }
    }
    #[derive(Default)]
    /// `1` rejects scope preparation; `2` arms the next account to fail one write.
    struct Scopes(
        Mutex<HashMap<SessionId, WorkflowOutputScope>>,
        AtomicBool,
        AtomicBool,
    );
    #[async_trait]
    impl WorkflowOutputScopes for Scopes {
        async fn ensure_current(
            &self,
            session: SessionId,
            generation: MessageId,
            max: Option<u64>,
        ) -> Result<WorkflowOutputScope, BudgetError> {
            if self.1.load(Ordering::Acquire) {
                return Err(BudgetError::Internal("scope preparation rejected".into()));
            }
            if let Some(scope) = self.0.lock().unwrap().get(&session).cloned() {
                return Ok(scope);
            }
            self.begin_turn(session, generation, max).await
        }
        async fn begin_turn(
            &self,
            session: SessionId,
            generation: MessageId,
            _: Option<u64>,
        ) -> Result<WorkflowOutputScope, BudgetError> {
            let scope = WorkflowOutputScope::new(Arc::new(Account {
                session,
                generation,
                events: Mutex::new(HashMap::new()),
                fail_next: AtomicBool::new(self.2.swap(false, Ordering::AcqRel)),
            }));
            self.0.lock().unwrap().insert(session, scope.clone());
            Ok(scope)
        }
        fn capture(&self, session: SessionId) -> Result<WorkflowOutputScope, BudgetError> {
            self.0
                .lock()
                .unwrap()
                .get(&session)
                .cloned()
                .ok_or_else(|| BudgetError::Internal("missing scope".into()))
        }
    }
    fn orch(responses: Vec<llm_runtime::HistoryResponse>) -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(responses)),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
    }
    fn usage(visible: u64, reasoning: u64) -> llm_runtime::ExecutionUsage {
        llm_runtime::ExecutionUsage {
            report: llm_runtime::UsageReport::measured(
                llm_runtime::Usage {
                    input_tokens: 0,
                    output_tokens: visible.saturating_add(reasoning),
                    cache_write_tokens: 0,
                    cache_read_tokens: 0,
                    reasoning_tokens: reasoning,
                    ..Default::default()
                },
                llm_runtime::services::sdk::protocol::UsageState::Complete,
            ),
            ..Default::default()
        }
    }

    /// P0-4: a failed output write is a fact about that turn, not a verdict on
    /// the session. One storage fault must not make every later turn refuse.
    #[tokio::test]
    async fn a_failed_output_write_does_not_refuse_the_next_turn() {
        let scopes = Arc::new(Scopes::default());
        let orch = orch(vec![]).with_workflow_output_scopes(scopes.clone());
        let session = orch.session.lock().await.session_id;
        orch.begin_output_turn(MessageId::new()).await.unwrap();

        // Arm one write to fail, the shape a real storage fault takes.
        scopes
            .capture(session)
            .unwrap()
            .record_legacy(WorkflowOutputEventId::MainResponse(MessageId::new()), 1)
            .unwrap();
        scopes.2.store(true, Ordering::Release);
        orch.begin_output_turn(MessageId::new()).await.unwrap();
        let mut observation = orch.capture_main_output().await.unwrap().unwrap();
        observation.observe(&usage(9, 0));
        assert!(
            observation.finish().is_err(),
            "the conflicting write must fail"
        );

        // The next turn is a fresh one and must be allowed to proceed.
        assert!(
            orch.begin_output_turn(MessageId::new()).await.is_ok(),
            "a later turn was refused because an earlier one failed to record"
        );
        assert!(orch.check_output_accounting().is_ok());
        assert!(orch.capture_main_output().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn main_output_clear_resume_preserves_existing_scope_and_failed_prepare_is_inert() {
        let scopes = Arc::new(Scopes::default());
        let orch = orch(vec![]).with_workflow_output_scopes(scopes.clone());
        orch.begin_output_turn(MessageId::new()).await.unwrap();
        let old_session = orch.session.lock().await.session_id;
        let old = scopes.capture(old_session).unwrap();
        old.record_legacy(WorkflowOutputEventId::MainResponse(MessageId::new()), 13)
            .unwrap();
        lingxi_core::host::OrchestratorHandle::clear_session(&orch)
            .await
            .unwrap();
        let cleared = orch.session.lock().await.session_id;
        assert_ne!(old_session, cleared);
        assert!(orch.capture_main_output().await.unwrap().is_some());
        assert_eq!(scopes.capture(cleared).unwrap().spent(), 0);
        lingxi_core::host::OrchestratorHandle::resume_session(
            &orch,
            old_session,
            vec![],
            None,
            None,
            Default::default(),
        )
        .await
        .unwrap();
        let resumed = scopes.capture(old_session).unwrap();
        assert!(resumed.shares_account(&old));
        assert_eq!(resumed.spent(), 13);
        assert!(orch.capture_main_output().await.unwrap().is_some());

        let marker = ConversationMessage::user(MessageId::new(), "preserve history".to_string());
        orch.session.lock().await.history.push(marker.clone());
        scopes.1.store(true, Ordering::Release);
        assert!(lingxi_core::host::OrchestratorHandle::clear_session(&orch)
            .await
            .is_err());
        assert!(lingxi_core::host::OrchestratorHandle::resume_session(
            &orch,
            cleared,
            vec![],
            None,
            None,
            Default::default()
        )
        .await
        .is_err());
        let session = orch.session.lock().await;
        assert_eq!(session.session_id, old_session);
        assert_eq!(session.history.last().unwrap().id(), marker.id());
        drop(session);
        assert!(scopes.capture(old_session).unwrap().shares_account(&old));
        assert!(orch.capture_main_output().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn main_output_rejected_completed_preserves_final_provider_usage() {
        struct Reject;
        impl WorkflowOutputAccount for Reject {
            fn session_id(&self) -> SessionId {
                unreachable!()
            }
            fn generation_id(&self) -> MessageId {
                unreachable!()
            }
            fn spent(&self) -> u64 {
                0
            }
            fn record_legacy(&self, _: WorkflowOutputEventId, _: u64) -> Result<(), BudgetError> {
                Err(BudgetError::Internal("storage rejected output".into()))
            }
        }
        let failed = Arc::new(AtomicBool::new(false));
        let observation = MainOutputObservation {
            turn: OutputTurn {
                scope: WorkflowOutputScope::new(Arc::new(Reject)),
                failed: failed.clone(),
            },
            event: MessageId::new(),
            visible: 0,
            reasoning: 0,
            observed: false,
            finished: false,
        };
        let mut response = mock_message_response(vec![], Some("end_turn"));
        response.usage = usage(40, 60);
        let expected = response.usage.clone();
        let stream = futures::stream::iter(vec![
            Ok(llm_runtime::HistoryEvent::Completed {
                response: Box::new(response),
            }),
            Err(llm_runtime::LlmError::Overloaded { repeated: false }),
        ])
        .boxed();
        let mut wrapped = account_stream(stream, Some(observation));
        let Some(Ok(llm_runtime::HistoryEvent::Completed { response })) = wrapped.next().await
        else {
            panic!("output failure replaced final paid usage");
        };
        assert_eq!(response.usage, expected);
        assert!(failed.load(Ordering::Acquire));
        assert!(matches!(
            wrapped.next().await,
            Some(Err(llm_runtime::LlmError::Overloaded { repeated: false }))
        ));
        assert!(wrapped.next().await.is_none());
    }

    #[tokio::test]
    async fn main_output_nonstream_turn_records_disjoint_usage_once() {
        let scopes = Arc::new(Scopes::default());
        let mut response = mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "done".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        );
        response.usage = usage(40, 60);
        let orch = orch(vec![response]).with_workflow_output_scopes(scopes.clone());
        orch.run_turn("hello").await.unwrap();
        let session = orch.session.lock().await.session_id;
        assert_eq!(scopes.capture(session).unwrap().spent(), 100);
        assert_eq!(orch.output_token_pool().load(Ordering::Relaxed), 0);
        assert_eq!(
            orch.compaction_runtime
                .last_response_output_tokens
                .load(Ordering::Relaxed),
            40
        );
    }

    #[tokio::test]
    async fn main_output_streaming_turn_participates_without_legacy_double_count() {
        use crate::test_support::{
            content_block_start_text, content_block_stop, message_start, message_stop, text_delta,
            MockStreamingApiClient,
        };
        let scopes = Arc::new(Scopes::default());
        let stream = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
            message_start("m", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "done"),
            content_block_stop(0),
            llm_runtime::HistoryEvent::MessageDelta {
                delta: llm_runtime::HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: Some(usage(40, 60)),
            },
            message_stop(),
        ]]));
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                stream,
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            )
            .with_workflow_output_scopes(scopes.clone()),
        );
        orch.run_turn_streaming("hello").await.unwrap();
        let session = orch.session.lock().await.session_id;
        assert_eq!(scopes.capture(session).unwrap().spent(), 100);
        assert_eq!(orch.output_token_pool().load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn main_output_metadata_only_completion_cannot_erase_retained_output() {
        for explicit_zero in [false, true] {
            let scopes = Arc::new(Scopes::default());
            let orch = orch(vec![]).with_workflow_output_scopes(scopes.clone());
            orch.begin_output_turn(MessageId::new()).await.unwrap();
            let scope = scopes
                .capture(orch.session.lock().await.session_id)
                .unwrap();
            let mut observation = orch.capture_main_output().await.unwrap().unwrap();
            observation.observe(&usage(4, 6));
            let mut completed = llm_runtime::ExecutionUsage {
                inference: llm_runtime::services::sdk::protocol::InferenceReport {
                    service_tier: Some(llm_runtime::services::sdk::protocol::ServiceTier::Fast),
                    ..Default::default()
                },
                ..Default::default()
            };
            if explicit_zero {
                completed.report = llm_runtime::UsageReport::measured(
                    llm_runtime::Usage::default(),
                    llm_runtime::services::sdk::protocol::UsageState::Complete,
                );
            }
            observation.observe_completed(&completed);
            observation.finish().unwrap();
            assert_eq!(scope.spent(), if explicit_zero { 0 } else { 10 });
        }
    }

    #[tokio::test]
    async fn main_output_captured_a_survives_b_and_duplicate_finish() {
        let scopes = Arc::new(Scopes::default());
        let orch = orch(vec![]).with_workflow_output_scopes(scopes.clone());
        orch.begin_output_turn(MessageId::new()).await.unwrap();
        let a = scopes
            .capture(orch.session.lock().await.session_id)
            .unwrap();
        let mut observation = orch.capture_main_output().await.unwrap().unwrap();
        orch.session.lock().await.session_id = SessionId::new();
        orch.begin_output_turn(MessageId::new()).await.unwrap();
        let b = scopes
            .capture(orch.session.lock().await.session_id)
            .unwrap();
        observation.observe(&usage(4, 6));
        observation.finish().unwrap();
        observation.finish().unwrap();
        drop(observation);
        assert_eq!(a.spent(), 10);
        assert_eq!(b.spent(), 0);
    }

    #[tokio::test]
    async fn forked_main_observations_count_separate_mod_requests() {
        let scopes = Arc::new(Scopes::default());
        let orch = orch(vec![]).with_workflow_output_scopes(scopes.clone());
        orch.begin_output_turn(MessageId::new()).await.unwrap();
        let template = orch.capture_main_output().await.unwrap().unwrap();
        let mut first = template.fork();
        let mut second = template.fork();
        first.observe(&usage(3, 0));
        second.observe(&usage(5, 2));
        first.finish().unwrap();
        second.finish().unwrap();
        assert_eq!(
            scopes
                .capture(orch.session.lock().await.session_id)
                .unwrap()
                .spent(),
            10
        );
    }

    #[tokio::test]
    async fn main_output_missing_capture_and_maximum_canonical_counts() {
        let orch = orch(vec![]).with_workflow_output_scopes(Arc::new(Scopes::default()));
        assert!(orch.capture_main_output().await.is_err());
        orch.begin_output_turn(MessageId::new()).await.unwrap();
        let mut observation = orch.capture_main_output().await.unwrap().unwrap();
        // Reasoning is a subset of SDK output, so even the largest canonical
        // counter is representable without adding that subset a second time.
        observation.observe(&usage(u64::MAX - 1, 1));
        assert!(observation.finish().is_ok());
        assert!(orch.capture_main_output().await.is_ok());
    }

    #[tokio::test]
    async fn partial_reclassification_uses_latest_canonical_snapshot() {
        let scopes = Arc::new(Scopes::default());
        let orch = orch(vec![]).with_workflow_output_scopes(scopes.clone());
        orch.begin_output_turn(MessageId::new()).await.unwrap();
        let scope = scopes
            .capture(orch.session.lock().await.session_id)
            .unwrap();
        let mut observation = orch.capture_main_output().await.unwrap().unwrap();
        observation.observe(&usage(10, 0));
        let mut partial = usage(4, 6);
        partial.report.state = llm_runtime::services::sdk::protocol::UsageState::Partial;
        observation.observe(&partial);
        observation.observe(&llm_runtime::ExecutionUsage::default());
        observation.finish().unwrap();
        assert_eq!(scope.spent(), 10);
    }

    #[tokio::test]
    async fn main_output_stream_partial_drop_retains_only_known_usage() {
        let scopes = Arc::new(Scopes::default());
        let orch = orch(vec![]).with_workflow_output_scopes(scopes.clone());
        orch.begin_output_turn(MessageId::new()).await.unwrap();
        let scope = scopes
            .capture(orch.session.lock().await.session_id)
            .unwrap();
        let event = llm_runtime::HistoryEvent::MessageDelta {
            delta: llm_runtime::HistoryMessageDelta {
                stop_reason: None,
                stop_details: None,
            },
            usage: Some(usage(4, 6)),
        };
        let stream = futures::stream::iter(vec![Ok(event)])
            .chain(futures::stream::pending())
            .boxed();
        let mut wrapped = account_stream(stream, orch.capture_main_output().await.unwrap());
        wrapped.next().await.unwrap().unwrap();
        assert_eq!(scope.spent(), 0);
        drop(wrapped);
        assert_eq!(scope.spent(), 10);
    }

    #[tokio::test]
    async fn main_output_stream_completed_retains_final_cumulative_buckets() {
        let scopes = Arc::new(Scopes::default());
        let orch = orch(vec![]).with_workflow_output_scopes(scopes.clone());
        orch.begin_output_turn(MessageId::new()).await.unwrap();
        let scope = scopes
            .capture(orch.session.lock().await.session_id)
            .unwrap();
        let mut response = mock_message_response(vec![], Some("end_turn"));
        response.usage = usage(40, 60);
        let stream = futures::stream::iter(vec![
            Ok(llm_runtime::HistoryEvent::MessageDelta {
                delta: llm_runtime::HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                // The final provider snapshot reclassifies provisional visible output.
                usage: Some(usage(100, 0)),
            }),
            Ok(llm_runtime::HistoryEvent::MessageStop),
            Ok(llm_runtime::HistoryEvent::Completed {
                response: Box::new(response),
            }),
        ])
        .boxed();
        let mut wrapped = account_stream(stream, orch.capture_main_output().await.unwrap());
        while wrapped.next().await.is_some() {}
        drop(wrapped);
        assert_eq!(scope.spent(), 100);
    }
}
