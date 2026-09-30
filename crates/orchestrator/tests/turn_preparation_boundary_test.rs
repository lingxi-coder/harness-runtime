//! Admission tests for PR 2 of the unified-driver plan (shared preparation and
//! retry material). These lock behaviour that exists TODAY, so the refactor has
//! something to be equivalent to.
//!
//! The plan (§9) names four things that must be under test before PR 2 starts —
//! preparation-phase cancellation, PTL retry, stream fallback, and the
//! two-entry prompt snapshot — and §3.1/§3.3 say what about them is load-bearing:
//!
//! * **Preparation is inside the cancel race.** `BatchedRound.run` must be
//!   `select(prepare + execute, cancel)`; the plan's words are "禁止外层先 await
//!   准备再进入策略". Hoisting preparation out of the `select!` would leave a
//!   cancel that arrives while a reminder source is blocked with nothing to
//!   race, and the turn would run to completion against a cancelled token.
//!
//! * **A model step collects its consume-once reminders exactly once.**
//!   Computing one ADVANCES session state — sent-sets, delta trackers, drains —
//!   so a retry can never recompute it: it comes back `None` and the reminder is
//!   gone for the rest of the session. Every rebuild-from-raw-history path
//!   therefore re-appends the SAME values (`reattach_outgoing_context`), and the
//!   drain must not run a second time.
//!
//! What these tests deliberately do NOT do is assert the new structure. They go
//! through the public entries, so they stay meaningful whether preparation lives
//! in `turn_loop.rs` (today) or in a `BatchedRound` strategy (after PR 2).

use lingxi_core::types::ConversationMessage;
use llm_runtime::{
    ContentBlock as LlmContentBlock, ExecutionUsage as Usage, HistoryResponse, LlmError,
};
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, MockStreamingApiClient,
    NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, OrchestratorApiClient, OrchestratorConfig, TurnOutcome,
};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tool_api::registry::ToolRegistry;

/// The orchestrator's turn machinery needs a deep stack in debug builds; the
/// other integration suites here spawn their own thread for the same reason.
fn run_with_large_stack<F, Fut>(build: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let handle = std::thread::Builder::new()
        .name("turn-preparation-boundary".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build test runtime")
                .block_on(build());
        })
        .expect("spawn large-stack test thread");
    handle.join().expect("large-stack test thread panicked");
}

fn text_response(text: &str) -> HistoryResponse {
    HistoryResponse {
        id: "msg_prep".into(),
        model: "claude-opus-4-7".into(),
        content: vec![LlmContentBlock::Text {
            text: text.into(),
            cache_control: None,
        }],
        stop_reason: Some("end_turn".into()),
        stop_details: None,
        usage: Usage::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

/// A provider whose per-call outcome is scripted and whose full request bodies
/// are kept.
///
/// `MockApiClient` cannot script a fail-then-succeed sequence, and
/// `ptl_recovery_test`'s `PtlMockApi` records only message COUNTS — these tests
/// have to look inside the messages for the reminder text.
struct ScriptedApi {
    script: tokio::sync::Mutex<std::collections::VecDeque<Result<HistoryResponse, LlmError>>>,
    captured: tokio::sync::Mutex<Vec<Vec<ConversationMessage>>>,
}

impl ScriptedApi {
    fn new(script: Vec<Result<HistoryResponse, LlmError>>) -> Self {
        Self {
            script: tokio::sync::Mutex::new(script.into()),
            captured: tokio::sync::Mutex::new(Vec::new()),
        }
    }
    async fn requests(&self) -> Vec<Vec<ConversationMessage>> {
        self.captured.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl OrchestratorApiClient for ScriptedApi {
    async fn messages_create(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _system: Option<&str>,
        msgs: Vec<ConversationMessage>,
        _tools: Vec<serde_json::Value>,
    ) -> Result<HistoryResponse, LlmError> {
        self.captured.lock().await.push(msgs);
        self.script.lock().await.pop_front().unwrap_or_else(|| {
            Err(LlmError::Transport {
                message: "scripted api exhausted".into(),
            })
        })
    }
}

/// A diagnostics source that parks inside turn preparation until released.
///
/// `new_diagnostics_reminder_message` is one of the producers the shared
/// collector awaits, so parking here parks the whole preparation phase — which
/// is the only way to ask "is preparation inside the cancel race?" from outside.
struct ParkingDiagnostics {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    entries: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lingxi_core::host::NewDiagnosticsSource for ParkingDiagnostics {
    async fn take_new_diagnostics_block(&self) -> Option<String> {
        self.entries.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        None
    }
}

/// A cancel that arrives while preparation is blocked must cancel the turn.
///
/// Today `try_run_turn_cancelable` races the whole
/// `execute_one_turn_with_recovery_tracked` — preparation included — against the
/// token. PR 2 moves preparation into the strategy, and §3.1 requires it to stay
/// inside that race.
///
/// The failure mode if it does not is NOT a wrong assertion, it is a turn that
/// never returns: preparation would await a source nothing releases while the
/// cancel branch no longer exists. Hence the timeout — the test reports a
/// blown boundary instead of hanging the suite.
#[test]
fn a_cancel_during_preparation_cancels_the_batched_turn() {
    run_with_large_stack(|| async {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let entries = Arc::new(AtomicUsize::new(0));
        let api = Arc::new(MockApiClient::new(vec![text_response("must not be sent")]));

        let orch = Arc::new(
            ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                api.clone(),
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/tmp"),
            )
            .with_new_diagnostics_source(Arc::new(ParkingDiagnostics {
                entered: entered.clone(),
                release: release.clone(),
                entries: entries.clone(),
            })),
        );

        let cancel = CancellationToken::new();
        let turn = {
            let orch = orch.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { orch.run_turn_with_cancel("ping", cancel).await })
        };

        // Only cancel once preparation is demonstrably parked. Cancelling before
        // that would be answered by the loop-top guard instead, which is a
        // different branch of §3.2 and proves nothing about preparation.
        tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
            .await
            .expect("turn preparation never reached the diagnostics source");
        cancel.cancel();

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
            .await
            .expect(
                "the turn did not finish within 10s of the cancel. Preparation is parked on a \
                 source nothing releases, so this means preparation is no longer inside the \
                 cancel race — see plan §3.1, `select(prepare + execute, cancel)`",
            )
            .expect("turn task panicked")
            .expect("cancelable turn returned an error");

        assert_eq!(
            outcome,
            TurnOutcome::Cancelled,
            "a cancel that arrives during preparation must return Cancelled"
        );
        assert_eq!(
            api.captured_msgs().await.len(),
            0,
            "the provider must never be reached: the cancel landed while preparation was still \
             parked, before any request could be built"
        );
        assert_eq!(
            entries.load(Ordering::SeqCst),
            1,
            "preparation must have been entered exactly once"
        );

        // Let the parked future unwind cleanly if it is still alive.
        release.notify_waiters();
    });
}

/// A summariser for the reactive-compaction recovery.
///
/// Required, not incidental: with no summariser wired, a `ContextOverflow` does
/// NOT retry at all — `ptl_recovery_test`'s
/// `unwired_ptl_does_not_truncate_or_retry_the_main_request` pins that. The
/// retry under test here exists only on the reactive-compaction path.
struct SummaryClient;

#[async_trait::async_trait]
impl sidequery::SideQueryClient for SummaryClient {
    async fn query(
        &self,
        _request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        Ok(sidequery::SideQueryResponse {
            text: Some("<summary>preserved work</summary>".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

/// Wire reactive compaction so a `ContextOverflow` summarises and retries.
fn with_reactive_recovery(orch: ConversationOrchestrator) -> ConversationOrchestrator {
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(Arc::new(SummaryClient), "test-model".into()),
    );
    orch.with_cache_safe_slot(slot.clone())
        .with_compaction(Arc::new(
            compaction::CompactionOrchestrator::with_autocompactor(
                compaction::Autocompactor::with_forked_runner(runner, slot),
                u64::MAX,
            ),
        ))
}

/// Give the reactive summariser enough history to be worth compacting.
async fn seed_rounds(orch: &ConversationOrchestrator, rounds: usize) {
    let session = orch.session();
    let mut s = session.lock().await;
    for i in 0..rounds {
        s.history.push(ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            format!("round-{i} user message with filler text to give the round a token estimate"),
        ));
        s.history.push(ConversationMessage::Assistant {
            id: lingxi_core::types::MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: format!("round-{i} assistant reply with filler text to give it weight"),
            }],
            stop_reason: Some("end_turn".to_string()),
        });
    }
}

/// A consume-once reminder source that reports how many times it was drained.
struct CountingDiagnostics {
    block: String,
    drains: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lingxi_core::host::NewDiagnosticsSource for CountingDiagnostics {
    async fn take_new_diagnostics_block(&self) -> Option<String> {
        // Consume-once: the first drain yields the block, later ones yield
        // nothing — the same shape as the real source.
        if self.drains.fetch_add(1, Ordering::SeqCst) == 0 {
            Some(self.block.clone())
        } else {
            None
        }
    }
}

/// A PTL retry must re-send this step's reminders without re-draining them.
///
/// The first provider call fails with `ContextOverflow`; recovery rebuilds the
/// request from raw `session.history` and calls again. Both requests must carry
/// the diagnostics reminder, and the source must have been drained exactly once
/// — a second drain would return `None` and silently lose the reminder for the
/// rest of the session, which is the whole reason `turn_reminders` is threaded
/// through `call_api_with_ptl_recovery` rather than recomputed.
#[test]
fn a_ptl_retry_reuses_this_steps_reminders_without_redraining_them() {
    run_with_large_stack(|| async {
        let drains = Arc::new(AtomicUsize::new(0));
        let block = "<new-diagnostics>The following new diagnostic issues were detected:\n\nx.rs:\n  \u{2718} [Line 1:1] boom</new-diagnostics>";

        // First call 413s, the rebuilt retry succeeds.
        let api = Arc::new(ScriptedApi::new(vec![
            Err(LlmError::ContextOverflow { token_gap: 100 }),
            Ok(text_response("recovered")),
        ]));

        let orch = with_reactive_recovery(
            ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                api.clone(),
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/tmp"),
            )
            .with_new_diagnostics_source(Arc::new(CountingDiagnostics {
                block: block.to_string(),
                drains: drains.clone(),
            })),
        );
        seed_rounds(&orch, 8).await;

        orch.run_turn("ping").await.expect("turn should recover");

        let calls = api.requests().await;
        assert!(
            calls.len() >= 2,
            "expected an original request and at least one rebuilt retry, got {}",
            calls.len()
        );

        let carries_reminder = |messages: &[ConversationMessage]| {
            messages
                .iter()
                .any(|m| m.text_content().contains("<new-diagnostics>"))
        };
        for (index, messages) in calls.iter().enumerate() {
            assert!(
                carries_reminder(messages),
                "request #{index} lost the diagnostics reminder. A rebuild from raw \
                 session.history must re-append this step's reminders (see \
                 `reattach_outgoing_context`); recomputing them would return None and drop \
                 the reminder for the rest of the session."
            );
        }

        assert_eq!(
            drains.load(Ordering::SeqCst),
            1,
            "the consume-once source must be drained exactly once per model step; a second \
             drain means the retry recomputed the reminders instead of reusing them"
        );
    });
}

/// The streaming path's non-streaming fallback must reuse the same way.
///
/// The stream open returns `ContextOverflow`, which drops the turn into the same
/// `call_api_with_ptl_recovery` the batched path uses. The reminder was already
/// drained during streaming preparation, so the recovered request has to carry
/// the value collected then.
#[test]
fn a_stream_fallback_reuses_this_steps_reminders_without_redraining_them() {
    run_with_large_stack(|| async {
        let drains = Arc::new(AtomicUsize::new(0));
        let block = "<new-diagnostics>The following new diagnostic issues were detected:\n\ny.rs:\n  \u{2718} [Line 2:2] bang</new-diagnostics>";

        let streaming = Arc::new(MockStreamingApiClient::with_open_error(
            LlmError::ContextOverflow { token_gap: 100 },
            Vec::new(),
        ));
        let batched = Arc::new(MockApiClient::new(vec![text_response("recovered")]));

        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            batched.clone(),
            streaming.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
        .with_new_diagnostics_source(Arc::new(CountingDiagnostics {
            block: block.to_string(),
            drains: drains.clone(),
        }));

        orch.run_turn_streaming("ping")
            .await
            .expect("streaming 413 must recover through the non-streaming fallback");

        let recovered = batched.captured_msgs().await;
        assert_eq!(
            recovered.len(),
            1,
            "the fallback should issue exactly one non-streaming request"
        );
        assert!(
            recovered[0]
                .iter()
                .any(|m| m.text_content().contains("<new-diagnostics>")),
            "the non-streaming fallback dropped the reminder collected during streaming \
             preparation; it rebuilds from raw session.history and must reattach this step's \
             reminders"
        );
        assert_eq!(
            drains.load(Ordering::SeqCst),
            1,
            "streaming preparation drains once; the fallback must reuse that value rather \
             than draining again"
        );
    });
}

/// A one-shot task-notification source.
struct OnceTaskNotifications(
    std::sync::Mutex<Vec<lingxi_core::host::task_registry::TaskNotification>>,
);

#[async_trait::async_trait]
impl orchestrator::prompt::task_notification::TaskNotificationProvider for OnceTaskNotifications {
    async fn take_pending_task_notifications(
        &self,
    ) -> Vec<lingxi_core::host::task_registry::TaskNotification> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// A retry must not send the durable task-notification twice.
///
/// The plan's PR-2 acceptance criteria include "无重复通知", and the collector
/// carries the reason in its own comment: a completion is a real conversation
/// event, so it goes into `session.history` and the JSONL — NOT into
/// `turn_reminders`. A retry rebuilds the request from raw history AND
/// re-appends the reminders, so a notification living in both would be sent
/// twice by exactly the mechanism that keeps the other reminders alive.
///
/// This is the asymmetry PR 2 is most likely to flatten while "unifying"
/// preparation, because it is the one producer whose result must NOT be
/// reattached.
#[test]
fn a_ptl_retry_does_not_duplicate_the_durable_task_notification() {
    run_with_large_stack(|| async {
        let api = Arc::new(ScriptedApi::new(vec![
            Err(LlmError::ContextOverflow { token_gap: 100 }),
            Ok(text_response("recovered")),
        ]));

        let notification = lingxi_core::host::task_registry::TaskNotification {
            task_id: "b12345678".into(),
            task_type: "local_bash".into(),
            status: "completed".into(),
            description: "run tests".into(),
            exit_code: Some(0),
            ..Default::default()
        };

        let orch = with_reactive_recovery(
            ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                api.clone(),
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/tmp"),
            )
            .with_task_notifications(Arc::new(OnceTaskNotifications(
                std::sync::Mutex::new(vec![notification]),
            ))),
        );
        seed_rounds(&orch, 8).await;

        orch.run_turn("ping").await.expect("turn should recover");

        let requests = api.requests().await;
        assert!(
            requests.len() >= 2,
            "expected an original request and a rebuilt retry, got {}",
            requests.len()
        );
        for (index, messages) in requests.iter().enumerate() {
            let hits = messages
                .iter()
                .filter(|m| m.text_content().contains("b12345678"))
                .count();
            assert!(
                hits <= 1,
                "request #{index} carries the completion {hits} times. A durable notification \
                 lives in session.history; putting it in turn_reminders as well means every \
                 rebuild re-appends it on top of the copy history already supplies."
            );
        }

        // And it survives the turn exactly once, rather than vanishing with it:
        // the drain marks each task notified, so a lost one can never reappear.
        let history = orch.session().lock().await.history.clone();
        assert_eq!(
            history
                .iter()
                .filter(|m| m.text_content().contains("b12345678"))
                .count(),
            1,
            "the completion must be appended to history exactly once"
        );
    });
}

/// The prompt snapshot is recorded from exactly one place per driver.
///
/// SOURCE-level, and labelled as such because the rest of this file is not.
/// `record_prompt_snapshot_if_needed` writes into a write-once slot — a second
/// call in the same step returns early — so "called twice" is invisible from
/// outside, and the recorded snapshot only reaches a test through the JSONL
/// attachment, which would test persistence rather than the wiring under
/// question here.
///
/// The plan calls for "唯一 `record_prompt_snapshot_if_needed`" inside shared
/// preparation, and since PR 2 that is literally true: ONE call site, in
/// `prepare_turn_step`, reached by both drivers. This test was written against
/// the previous shape (one call per driver, two in total) and tightened to 1
/// when the extraction landed — tightened, not renumbered: two call sites now
/// means a step prepared twice, which is the defect it was written for.
#[test]
fn the_prompt_snapshot_is_recorded_from_exactly_one_place() {
    const BATCHED: &str = include_str!("../src/turn_loop.rs");
    const STREAMING: &str = concat!(
        include_str!("../src/conversation/drivers/mod.rs"),
        include_str!("../src/conversation/drivers/streaming.rs"),
    );
    const COLLECTOR: &str = include_str!("../src/conversation/drivers/prepare.rs");

    let calls = |src: &str| src.matches(".record_prompt_snapshot_if_needed(").count();
    assert_eq!(
        calls(COLLECTOR),
        1,
        "the shared preparation must record the prompt snapshot exactly once per model step; \
         zero means the session lost carved-slate, two means it prepared twice"
    );
    assert_eq!(
        calls(BATCHED) + calls(STREAMING),
        0,
        "neither driver may record the snapshot itself any more. A driver that kept its own \
         call alongside prepare_turn_step's is the leftover-call mistake that produced two \
         preparations in the first place."
    );
}
