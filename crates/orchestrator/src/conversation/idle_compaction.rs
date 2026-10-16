//! Session-scoped idle compaction scheduling and its eligibility ticket.
//!
//! This controller is opt-in at the host boundary. It keeps the timer in
//! memory, fences one logical main request separately from the provider's
//! cache-expiry timestamp, and invokes the existing automatic compaction path.

use crate::ConversationOrchestrator;
use lingxi_core::types::SessionId;
use llm_runtime::prompt_cache::RequestSnapshot;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const ONE_HOUR_MS: u64 = 60 * 60 * 1_000;
const IDLE_FIRE_FRACTION_NUMERATOR: u64 = 9;
const IDLE_FIRE_FRACTION_DENOMINATOR: u64 = 10;
const MIN_CONTEXT_TOKENS: u64 = 200_000;
const MAX_LATE_MS: u64 = 60_000;

#[derive(Clone)]
struct IdleCompactionTicket {
    session_id: SessionId,
    model: String,
    model_profile: Option<String>,
    request: RequestSnapshot,
    fire_at_ms: u64,
}

struct IdleTaskState {
    generation: u64,
    closed: bool,
    cancel: Option<CancellationToken>,
    task: Option<JoinHandle<()>>,
}

/// One timer at a time. A replaced task receives cancellation before it is
/// joined, so it cannot race an in-flight summary with the newer ticket.
pub(crate) struct IdleCompactionState {
    inner: Mutex<IdleTaskState>,
}

impl Default for IdleCompactionState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(IdleTaskState {
                generation: 0,
                closed: false,
                cancel: None,
                task: None,
            }),
        }
    }
}

impl IdleCompactionState {
    pub(crate) fn cancel(&self) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation = state.generation.wrapping_add(1);
        if let Some(cancel) = state.cancel.as_ref() {
            cancel.cancel();
        }
    }

    pub(crate) async fn replace<F, Fut>(&self, task: F)
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (generation, old_task) = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed {
                return;
            }
            state.generation = state.generation.wrapping_add(1);
            if let Some(cancel) = state.cancel.take() {
                cancel.cancel();
            }
            (state.generation, state.task.take())
        };
        if let Some(old_task) = old_task {
            let _ = old_task.await;
        }

        let cancel = CancellationToken::new();
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || state.generation != generation {
            cancel.cancel();
            return;
        }
        let handle = tokio::spawn(task(cancel.clone()));
        state.cancel = Some(cancel);
        state.task = Some(handle);
    }

    pub(crate) async fn shutdown_and_drain(&self) {
        let task = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            state.generation = state.generation.wrapping_add(1);
            if let Some(cancel) = state.cancel.take() {
                cancel.cancel();
            }
            state.task.take()
        };
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for IdleCompactionState {
    fn drop(&mut self) {
        let state = self
            .inner
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.generation = state.generation.wrapping_add(1);
        if let Some(cancel) = state.cancel.take() {
            cancel.cancel();
        }
    }
}

fn request_context_tokens(summary: &cost::prompt_cache_ledger::CacheSummary) -> Option<u64> {
    let facts = &summary.last_request.as_ref()?.facts;
    Some(
        facts
            .input_tokens
            .saturating_add(facts.cache_read_tokens)
            .saturating_add(facts.cache_creation_tokens),
    )
}

fn fire_at_ms(summary: &cost::prompt_cache_ledger::CacheSummary) -> Option<u64> {
    let request = summary.last_request.as_ref()?;
    if request.facts.ttl != cost::prompt_cache_ledger::CacheTtl::OneHour {
        return None;
    }
    let activity_at = summary.last_activity_at?;
    let expires_at = summary.expires_at?;
    let ttl_window = expires_at.checked_sub(activity_at)?;
    if ttl_window != ONE_HOUR_MS {
        return None;
    }
    Some(activity_at.saturating_add(
        ttl_window.saturating_mul(IDLE_FIRE_FRACTION_NUMERATOR) / IDLE_FIRE_FRACTION_DENOMINATOR,
    ))
}

fn idle_timer_is_late(now_ms: u64, fire_at_ms: u64) -> bool {
    now_ms.saturating_sub(fire_at_ms) > MAX_LATE_MS
}

fn rate_limit_status_allows_idle_compaction(
    info: Option<llm_runtime::model::rate_limit::RateLimitInfo>,
) -> bool {
    info.is_none_or(|info| {
        info.status.as_deref().unwrap_or("allowed") == "allowed"
    })
}

impl ConversationOrchestrator {
    /// Cancel the timer before any new turn or session mutation waits for the
    /// shared turn gate. This makes user work win even if the timer woke first.
    pub fn cancel_idle_compaction(&self) {
        self.idle_compaction.cancel();
    }

    /// Cancel and join the session-owned idle compaction job during host shutdown.
    pub async fn shutdown_idle_compaction(&self) {
        self.idle_compaction.shutdown_and_drain().await;
    }

    /// Arm from a completed main-request usage record. The logical request
    /// anchor is carried separately from ledger `at_ms`: the latter measures
    /// cache activity; the former fences a newer request at timer fire.
    pub(crate) async fn arm_idle_compaction_after_request(&self, request: Option<RequestSnapshot>) {
        if !self.idle_compaction_enabled || self.side_chat {
            return;
        }
        let Some(request) = request.filter(|request| request.request_anchor_at_ms.is_some()) else {
            return;
        };

        let (session_id, model, model_profile) = {
            let session = self.session.lock().await;
            (
                session.session_id,
                session.model.clone(),
                session.model_profile.clone(),
            )
        };
        let now_ms = epoch_ms();
        let diagnostics = self.current_prompt_cache_ledger().await;
        let Some((latest, summary)) = diagnostics.idle_request_summary(session_id, now_ms) else {
            return;
        };
        if latest != request
            || summary.last_request.as_ref().map(|entry| entry.facts.at_ms) != Some(request.at_ms)
        {
            return;
        }
        let Some(fire_at_ms) = fire_at_ms(&summary) else {
            return;
        };

        let Some(owner) = self.upgrade_streaming_tool_dispatch_owner() else {
            return;
        };
        let owner = Arc::downgrade(&owner);
        let ticket = IdleCompactionTicket {
            session_id,
            model,
            model_profile,
            request,
            fire_at_ms,
        };
        let wait_ms = fire_at_ms.saturating_sub(now_ms);
        self.idle_compaction
            .replace(move |cancel| async move {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(Duration::from_millis(wait_ms)) => {}
                }
                let Some(owner) = owner.upgrade() else {
                    return;
                };
                owner.run_idle_compaction_if_current(ticket, cancel).await;
            })
            .await;
    }

    async fn run_idle_compaction_if_current(
        &self,
        ticket: IdleCompactionTicket,
        cancel: CancellationToken,
    ) {
        let _turn_guard = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            guard = self.turn_gate.lock() => guard,
        };
        if cancel.is_cancelled() || !self.idle_compaction_enabled || self.side_chat {
            return;
        }
        let now_ms = epoch_ms();
        let (session_id, model, model_profile) = {
            let session = self.session.lock().await;
            (
                session.session_id,
                session.model.clone(),
                session.model_profile.clone(),
            )
        };
        if session_id != ticket.session_id
            || model != ticket.model
            || model_profile != ticket.model_profile
        {
            return;
        }
        let diagnostics = self.current_prompt_cache_ledger().await;
        let Some((latest, summary)) = diagnostics.idle_request_summary(session_id, now_ms) else {
            return;
        };
        if latest != ticket.request
            || fire_at_ms(&summary) != Some(ticket.fire_at_ms)
            || idle_timer_is_late(now_ms, ticket.fire_at_ms)
            || !summary.warm
            || request_context_tokens(&summary).unwrap_or(0) < MIN_CONTEXT_TOKENS
            || !rate_limit_status_allows_idle_compaction(self.api.last_rate_limit_full())
        {
            return;
        }
        drop(diagnostics);
        if cancel.is_cancelled() {
            return;
        }
        self.maybe_compact_for_idle(&cancel).await;
    }
}

fn epoch_ms() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use cost::prompt_cache_ledger::{PromptCacheLedger, RequestFacts};
    use std::sync::Arc;

    #[derive(Default)]
    struct RecordingSummaryClient {
        budgets: Mutex<Vec<llm_runtime::ApiAttemptsLeft>>,
    }

    #[async_trait]
    impl sidequery::SideQueryClient for RecordingSummaryClient {
        async fn query(
            &self,
            request: sidequery::SideQueryRequest,
        ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
            if let Some(budget) = request.api_attempts_left {
                self.budgets.lock().unwrap().push(budget);
            }
            Ok(sidequery::SideQueryResponse {
                text: Some("<summary>Keep the latest project state.</summary>".into()),
                structured: None,
                tool_calls: Vec::new(),
                usage: cost::Usage::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }

    struct BlockingSummaryClient {
        started: tokio::sync::mpsc::UnboundedSender<()>,
    }

    #[async_trait]
    impl sidequery::SideQueryClient for BlockingSummaryClient {
        async fn query(
            &self,
            _request: sidequery::SideQueryRequest,
        ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
            let _ = self.started.send(());
            std::future::pending::<Result<sidequery::SideQueryResponse, sidequery::SideQueryError>>(
            )
            .await
        }
    }

    struct UnusedIdleHookHttp;

    #[async_trait]
    impl lingxi_core::host::HttpTransport for UnusedIdleHookHttp {
        async fn request(
            &self,
            _request: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "idle PreCompact fixture has no HTTP hooks".into(),
            ))
        }

        async fn stream_sse(
            &self,
            _request: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "idle PreCompact fixture has no SSE hooks".into(),
            ))
        }
    }

    struct UnusedIdleHookRuntime;

    #[async_trait]
    impl lingxi_core::host::RuntimeSpawner for UnusedIdleHookRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            Err(lingxi_core::host::RuntimeError::Internal(
                "idle PreCompact fixture does not spawn background hooks".into(),
            ))
        }

        async fn sleep(&self, _duration: Duration) {}

        async fn cancel(
            &self,
            _handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }

    struct BlockingPreCompactHandler {
        started: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl hooks::BuiltinHookHandler for BlockingPreCompactHandler {
        async fn handle(
            &self,
            _event: &hooks::HookEvent,
            ctx: &hooks::HookContext,
        ) -> hooks::HookResult {
            self.started.notify_one();
            if let Some(cancellation) = &ctx.idle_compaction_cancellation {
                cancellation.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
            hooks::HookResult {
                outcome: hooks::HookOutcome::Cancelled,
                stdout: hooks::response::ExactHookText::default(),
                stderr: hooks::response::ExactHookText::default(),
                exit_code: None,
                response: None,
            }
        }

        fn id(&self) -> &str {
            "idle-blocking-pre-compact"
        }
    }

    #[test]
    fn one_hour_fire_target_is_based_on_cache_activity_not_request_anchor() {
        let mut ledger = PromptCacheLedger::new();
        ledger.record(RequestFacts {
            at_ms: 10_000,
            input_tokens: 200_001,
            cache_read_tokens: 0,
            cache_creation_tokens: 1,
            ttl: cost::prompt_cache_ledger::CacheTtl::OneHour,
        });
        let summary = ledger.summary(10_000);
        assert_eq!(fire_at_ms(&summary), Some(10_000 + 3_600_000 * 9 / 10));
        assert!(summary.warm);
        assert_eq!(request_context_tokens(&summary), Some(200_002));
    }

    #[test]
    fn one_hour_fire_target_uses_ledger_activity_and_late_window_is_inclusive() {
        let mut ledger = PromptCacheLedger::new();
        ledger.record(RequestFacts {
            at_ms: 42_000,
            input_tokens: 1,
            cache_read_tokens: 0,
            cache_creation_tokens: 1,
            ttl: cost::prompt_cache_ledger::CacheTtl::OneHour,
        });
        let summary = ledger.summary(42_000);
        let target = fire_at_ms(&summary).expect("one-hour ledger has an expiry target");
        assert_eq!(target, 42_000 + ONE_HOUR_MS * 9 / 10);
        assert!(!idle_timer_is_late(target + MAX_LATE_MS, target));
        assert!(idle_timer_is_late(target + MAX_LATE_MS + 1, target));
        assert!(!idle_timer_is_late(target - 1, target));
    }

    #[test]
    fn absent_provider_status_uses_native_allowed_default_but_explicit_warnings_block() {
        assert!(rate_limit_status_allows_idle_compaction(None));
        let mut info = llm_runtime::model::rate_limit::RateLimitInfo::default();
        assert!(rate_limit_status_allows_idle_compaction(Some(info.clone())));
        info.status = Some("allowed_warning".into());
        assert!(!rate_limit_status_allows_idle_compaction(Some(
            info.clone()
        )));
        info.status = Some("rejected".into());
        assert!(!rate_limit_status_allows_idle_compaction(Some(info)));
    }

    #[tokio::test(start_paused = true)]
    async fn replacement_and_shutdown_cancel_and_join_the_previous_timer() {
        let state = IdleCompactionState::default();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stopped_tx, mut stopped_rx) = tokio::sync::mpsc::unbounded_channel();
        let first_started = started_tx.clone();
        let first_stopped = stopped_tx.clone();
        state
            .replace(move |cancel| async move {
                first_started.send(1).unwrap();
                cancel.cancelled().await;
                first_stopped.send(1).unwrap();
            })
            .await;
        assert_eq!(started_rx.recv().await, Some(1));
        state
            .replace(|cancel| async move {
                started_tx.send(2).unwrap();
                cancel.cancelled().await;
                stopped_tx.send(2).unwrap();
            })
            .await;
        assert_eq!(stopped_rx.recv().await, Some(1));
        assert_eq!(started_rx.recv().await, Some(2));
        state.shutdown_and_drain().await;
        assert_eq!(stopped_rx.recv().await, Some(2));
    }

    #[tokio::test(start_paused = true)]
    async fn fake_clock_waits_for_the_ledger_deadline_and_cancellation_suppresses_fire() {
        let state = IdleCompactionState::default();
        let (fired_tx, mut fired_rx) = tokio::sync::oneshot::channel();
        let mut ledger = PromptCacheLedger::new();
        ledger.record(RequestFacts {
            at_ms: 0,
            input_tokens: 1,
            cache_read_tokens: 0,
            cache_creation_tokens: 1,
            ttl: cost::prompt_cache_ledger::CacheTtl::OneHour,
        });
        let target = fire_at_ms(&ledger.summary(0)).expect("eligible ledger target");
        let wait = Duration::from_millis(target);
        state
            .replace(move |cancel| async move {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(wait) => {}
                }
                let _ = fired_tx.send(());
            })
            .await;
        tokio::task::yield_now().await;
        tokio::time::advance(wait - Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(fired_rx.try_recv().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(fired_rx.await, Ok(()));

        let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
        state
            .replace(move |cancel| async move {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => { let _ = cancelled_tx.send(()); }
                    () = tokio::time::sleep(wait) => panic!("cancelled idle compaction fired"),
                }
            })
            .await;
        tokio::task::yield_now().await;
        state.cancel();
        assert_eq!(cancelled_rx.await, Ok(()));
        state.shutdown_and_drain().await;
    }

    #[tokio::test]
    async fn idle_compaction_runs_auto_transition_with_one_operation_budget() {
        let summary_client = Arc::new(RecordingSummaryClient::default());
        let cache_safe_slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let runner = Arc::new(
            sidequery::ForkedAgentRunner::new()
                .with_side_query_client(summary_client.clone(), "test-compact-model".into()),
        );
        let compactor = Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(runner, cache_safe_slot.clone()),
            1_000_000,
        ));
        let orchestrator = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(crate::test_support::MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            crate::test_support::noop_hook_executor(),
            Arc::new(crate::test_support::NoOpPermissionGate),
            Arc::new(crate::test_support::MockOutputStream::new()),
            Arc::new(crate::test_support::StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_compaction(compactor)
        .with_cache_safe_slot(cache_safe_slot)
        .with_idle_compaction_enabled(true);
        let original_history = vec![
            lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "We need to preserve the repository decision.".into(),
            ),
            assistant_message("I will keep the decision in context."),
            lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "The source data lives in the local project folder.".into(),
            ),
            assistant_message("The local project folder is the source of truth."),
        ];
        orchestrator.session.lock().await.history = original_history.clone();
        // Main-turn preparation publishes the cache-safe request prefix before
        // compaction. The forked summarizer requires that same snapshot.
        orchestrator
            .seed_compact_cache_safe_params(None)
            .await
            .expect("seed the main-request cache-safe snapshot");
        assert!(orchestrator.idle_compaction_enabled);

        orchestrator
            .maybe_compact_for_idle(&CancellationToken::new())
            .await;

        let budgets = summary_client.budgets.lock().unwrap();
        assert_eq!(
            budgets.len(),
            1,
            "idle compaction must send one summary request"
        );
        assert_eq!(budgets[0].remaining(), 1);
        drop(budgets);
        let history = orchestrator.session.lock().await.history.clone();
        assert_ne!(
            history, original_history,
            "automatic compact transition applies"
        );
        assert!(history.iter().any(|message| matches!(
            message,
            lingxi_core::types::ConversationMessage::System { content, .. }
                if content.string_at("").as_deref() == Some(compaction::BOUNDARY_CONTENT)
        )));
        let trigger = history.iter().find_map(|message| match message {
            lingxi_core::types::ConversationMessage::System {
                compact_metadata: Some(metadata),
                ..
            } => Some(metadata.trigger),
            _ => None,
        });
        assert_eq!(trigger, Some(lingxi_core::types::CompactTrigger::Auto));
    }

    #[tokio::test]
    async fn cancellation_during_idle_summary_leaves_history_unapplied() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let cache_safe_slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let runner = Arc::new(sidequery::ForkedAgentRunner::new().with_side_query_client(
            Arc::new(BlockingSummaryClient {
                started: started_tx,
            }),
            "test-compact-model".into(),
        ));
        let compactor = Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(runner, cache_safe_slot.clone()),
            1_000_000,
        ));
        let orchestrator = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(crate::test_support::MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            crate::test_support::noop_hook_executor(),
            Arc::new(crate::test_support::NoOpPermissionGate),
            Arc::new(crate::test_support::MockOutputStream::new()),
            Arc::new(crate::test_support::StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_compaction(compactor)
        .with_cache_safe_slot(cache_safe_slot)
        .with_idle_compaction_enabled(true);
        let original_history = vec![
            lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "Preserve the first user request.".into(),
            ),
            assistant_message("The first reply is settled."),
            lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "Preserve the second user request.".into(),
            ),
            assistant_message("The second reply is settled."),
        ];
        orchestrator.session.lock().await.history = original_history.clone();
        // Exercise the real forked summary path instead of returning early on
        // the empty slot used only while assembling this fixture.
        orchestrator
            .seed_compact_cache_safe_params(None)
            .await
            .expect("seed the main-request cache-safe snapshot");
        let cancel = CancellationToken::new();
        let compact = orchestrator.maybe_compact_for_idle(&cancel);
        tokio::pin!(compact);
        tokio::select! {
            biased;
            result = &mut compact => panic!("idle summary completed before cancellation: {result:?}"),
            started = started_rx.recv() => assert_eq!(started, Some(())),
        }
        cancel.cancel();
        compact.await;
        assert_eq!(orchestrator.session.lock().await.history, original_history);
    }

    #[tokio::test]
    async fn cancelling_blocked_idle_pre_compact_hook_releases_gate_without_applying_history() {
        let started = Arc::new(tokio::sync::Notify::new());
        let mut registry = hooks::HookRegistry::new();
        registry.register(hooks::HookDefinition {
            id: lingxi_core::types::HookId::new(),
            name: "idle-pre-compact-blocker".into(),
            events: vec![hooks::events::HookEventType::PreCompact],
            if_condition: None,
            executor: hooks::definition::HookExecutor::Builtin {
                handler_id: "idle-blocking-pre-compact".into(),
            },
            source: hooks::definition::HookSource::Settings(
                lingxi_core::types::SettingsScope::User,
            ),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
            on_failure: None,
        });
        let mut hook_executor = hooks::HookExecutorImpl::new(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(UnusedIdleHookHttp),
            Arc::new(UnusedIdleHookRuntime),
        );
        hook_executor.register_builtin(Arc::new(BlockingPreCompactHandler {
            started: started.clone(),
        }));

        let summary_client = Arc::new(RecordingSummaryClient::default());
        let cache_safe_slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let runner = Arc::new(
            sidequery::ForkedAgentRunner::new()
                .with_side_query_client(summary_client.clone(), "test-compact-model".into()),
        );
        let compactor = Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(runner, cache_safe_slot.clone()),
            1_000_000,
        ));
        let orchestrator = Arc::new(
            ConversationOrchestrator::new(
                crate::OrchestratorConfig::default(),
                Arc::new(crate::test_support::MockApiClient::new(Vec::new())),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                Arc::new(hook_executor),
                Arc::new(crate::test_support::NoOpPermissionGate),
                Arc::new(crate::test_support::MockOutputStream::new()),
                Arc::new(crate::test_support::StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            )
            .with_compaction(compactor)
            .with_cache_safe_slot(cache_safe_slot)
            .with_idle_compaction_enabled(true),
        );
        let original_history = vec![
            lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "Keep the first accepted message.".into(),
            ),
            assistant_message("The first settled reply."),
            lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "Keep the second accepted message.".into(),
            ),
            assistant_message("The second settled reply."),
        ];
        orchestrator.session.lock().await.history = original_history.clone();

        orchestrator
            .idle_compaction
            .replace({
                let orchestrator = orchestrator.clone();
                move |cancel| async move {
                    let _turn_guard = orchestrator.turn_gate.lock().await;
                    orchestrator.maybe_compact_for_idle(&cancel).await;
                }
            })
            .await;
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("idle PreCompact hook starts and blocks");

        let (admitted_tx, admitted_rx) = tokio::sync::oneshot::channel();
        let new_turn = tokio::spawn({
            let orchestrator = orchestrator.clone();
            async move {
                orchestrator.cancel_idle_compaction();
                let _turn_guard = orchestrator.turn_gate.lock().await;
                let _ = admitted_tx.send(());
            }
        });
        tokio::time::timeout(Duration::from_secs(1), admitted_rx)
            .await
            .expect("new turn acquires the gate after cancelling idle PreCompact")
            .expect("new turn admission signal is sent");
        new_turn.await.unwrap();
        orchestrator.shutdown_idle_compaction().await;

        assert_eq!(orchestrator.session.lock().await.history, original_history);
        assert!(
            summary_client.budgets.lock().unwrap().is_empty(),
            "cancelled PreCompact hook must not start the compaction callback"
        );
    }

    #[tokio::test]
    async fn cancelling_blocked_idle_session_compact_mod_releases_gate_without_applying_history() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("idle-session-compact.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('session.compact', async ($, event, next) => {
                if (event.instructions === 'probe') return { skip: 'worker-responsive' };
                $.ui.log('idle-session-compact-started');
                await new Promise(resolve => {
                  next.signal.addEventListener('abort', resolve, { once: true });
                });
                $.ui.log('idle-session-compact-aborted');
                return { skip: 'cancelled idle compact' };
              });
            }"#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            "idle-session-compact",
            dir.path(),
            &module,
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}")
                .expect("empty options JSON"),
        )
        .await
        .unwrap();
        let mod_host = host.clone();
        let mut registry = hooks::HookRegistry::new();
        registry.set_mod_host(host);

        let summary_client = Arc::new(RecordingSummaryClient::default());
        let cache_safe_slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let runner = Arc::new(
            sidequery::ForkedAgentRunner::new()
                .with_side_query_client(summary_client.clone(), "test-compact-model".into()),
        );
        let compactor = Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(runner, cache_safe_slot.clone()),
            1_000_000,
        ));
        let output = Arc::new(crate::test_support::MockOutputStream::new());
        let orchestrator = Arc::new(
            ConversationOrchestrator::new(
                crate::OrchestratorConfig::default(),
                Arc::new(crate::test_support::MockApiClient::new(Vec::new())),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                crate::test_support::noop_hook_executor(),
                Arc::new(crate::test_support::NoOpPermissionGate),
                output.clone(),
                Arc::new(crate::test_support::StaticMemoryProvider::empty()),
                dir.path().to_path_buf(),
            )
            .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)))
            .with_compaction(compactor)
            .with_cache_safe_slot(cache_safe_slot)
            .with_idle_compaction_enabled(true),
        );
        let original_history = vec![
            lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "Keep the first accepted message.".into(),
            ),
            assistant_message("The first settled reply."),
            lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "Keep the second accepted message.".into(),
            ),
            assistant_message("The second settled reply."),
        ];
        orchestrator.session.lock().await.history = original_history.clone();

        orchestrator
            .idle_compaction
            .replace({
                let orchestrator = orchestrator.clone();
                move |cancel| async move {
                    let _turn_guard = orchestrator.turn_gate.lock().await;
                    orchestrator.maybe_compact_for_idle(&cancel).await;
                }
            })
            .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if output.snapshot().await.iter().any(|event| {
                    matches!(
                        event,
                        lingxi_core::host::OutputEvent::ModLog { plugin, text }
                            if plugin == "idle-session-compact"
                                && text.string_at("") == Some("idle-session-compact-started")
                    )
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("real session.compact Mod reaches its blocked handler");

        let (admitted_tx, admitted_rx) = tokio::sync::oneshot::channel();
        let new_turn = tokio::spawn({
            let orchestrator = orchestrator.clone();
            async move {
                orchestrator.cancel_idle_compaction();
                let _turn_guard = orchestrator.turn_gate.lock().await;
                let _ = admitted_tx.send(());
            }
        });
        tokio::time::timeout(Duration::from_secs(1), admitted_rx)
            .await
            .expect("new turn must acquire the gate promptly after cancellation")
            .expect("new turn admission signal is sent");
        new_turn.await.unwrap();
        orchestrator.shutdown_idle_compaction().await;

        assert_eq!(orchestrator.session.lock().await.history, original_history);
        assert!(
            summary_client.budgets.lock().unwrap().is_empty(),
            "a cancelled Mod must not enter the summary callback"
        );
        let probe = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
            r#"{"trigger":"probe","instructions":"probe","messages":[{"role":"user","text":"probe"}]}"#,
        )
        .expect("probe event JSON");
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            mod_host.dispatch("session.compact", probe, |_| async {
                panic!("responsive probe must skip before next")
            }),
        )
        .await
        .expect("Mod worker must remain responsive after request cancellation")
        .expect("probe dispatch succeeds");
        assert_eq!(response.string_at("/skip"), Some("worker-responsive"));
    }

    fn assistant_message(text: &str) -> lingxi_core::types::ConversationMessage {
        lingxi_core::types::ConversationMessage::Assistant {
            id: lingxi_core::types::MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: text.into(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            resumable: None,
            per_turn_effort: None,
        }
    }
}
