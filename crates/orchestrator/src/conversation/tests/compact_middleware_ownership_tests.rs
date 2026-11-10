use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::host::OutputEvent;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn summary_result(usage: Option<cost::Usage>) -> compaction::IterationCompactionResult {
    compaction::IterationCompactionResult {
        messages: vec![ConversationMessage::user(
            MessageId::new(),
            "summary".into(),
        )],
        raw_summary_text: "summary".into(),
        layers_applied: Vec::new(),
        total_tokens_freed: 0,
        cache_hit: false,
        consecutive_failures: 0,
        was_compacted: true,
        rapid_refill_breaker_tripped: false,
        consecutive_rapid_refills: 0,
        messages_to_preserve: Vec::new(),
        media_analysis_to_preserve: Vec::new(),
        compaction_usage: usage,
        compaction_model: Some("claude-opus-4-8".into()),
        compaction_profile: None,
    }
}

async fn mod_fixture(
    body: &str,
) -> (
    tempfile::TempDir,
    ConversationOrchestrator,
    Arc<MockOutputStream>,
    Arc<cost::CostTracker>,
    tokio::sync::mpsc::Receiver<cost::CostState>,
) {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("compact-ownership.js");
    std::fs::write(
        &module,
        format!("export function register(on) {{ on('session.compact', async ($, e, next) => {{ {body} }}); }}"),
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "compact-ownership",
        root.path(),
        &module,
        CompactSource::parse("{}").unwrap(),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let output = Arc::new(MockOutputStream::new());
    let id = SessionId::new();
    let (events, receiver) = tokio::sync::mpsc::channel(16);
    let tracker = Arc::new(cost::CostTracker::new(
        id,
        Arc::new(cost::PricingCatalog::builtin_reference()),
        events,
    ));
    let orch = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    )
    .with_session_id(id)
    .with_cost_tracker(tracker.clone())
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    (root, orch, output, tracker, receiver)
}

fn paid_core(calls: Arc<AtomicUsize>) -> SessionCompactCore {
    Arc::new(move |messages, _instructions| {
        calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let usage = cost::Usage {
                tokens: cost::TokenUsage {
                    input: 19,
                    output: 7,
                    ..Default::default()
                },
                ..Default::default()
            };
            Ok(SessionCompactCoreOutput::new(
                summary_result(Some(usage)),
                messages,
                Duration::from_millis(11),
            ))
        })
    })
}

#[tokio::test]
async fn post_next_failure_and_repeated_next_keep_each_paid_summary_once() {
    for (body, count, expect_error) in [
        // Mods keeps the last successful next result when its handler throws.
        // The paid core must still execute and be charged exactly once.
        (
            "await next(e); throw new Error('failed after paid summary');",
            1,
            false,
        ),
        ("await next(e); return next(e);", 2, false),
    ] {
        let (_root, orch, _output, tracker, _events) = mod_fixture(body).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let source = vec![ConversationMessage::user(
            MessageId::new(),
            "accepted source".into(),
        )];
        let result = orch
            .dispatch_session_compact_mods("manual", source, None, paid_core(calls.clone()))
            .await;
        assert_eq!(result.is_err(), expect_error);
        assert_eq!(calls.load(Ordering::SeqCst), count);
        let state = tracker.snapshot().await;
        assert_eq!(state.cost_revision, count as u64);
        assert_eq!(
            state
                .per_model_usage
                .values()
                .map(|model| model.usage.tokens.input)
                .sum::<u64>(),
            19 * count as u64
        );
        assert_eq!(
            state
                .per_model_usage
                .values()
                .map(|model| model.usage.tokens.output)
                .sum::<u64>(),
            7 * count as u64
        );
        assert!(state.total_nano_usd > 0);
        assert_eq!(
            orch.model_runtime.api_calls_recorded.load(Ordering::SeqCst),
            count as u32
        );
    }
}

#[tokio::test]
async fn dropping_compact_while_mod_waits_after_next_retains_paid_usage() {
    let (_root, orch, output, tracker, _events) = mod_fixture(
        "const r = await next(e); $.ui.log('paid-next-returned'); await new Promise(resolve => next.signal.addEventListener('abort', resolve, {once:true})); return r;",
    ).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut compact = Box::pin(orch.dispatch_session_compact_mods(
        "manual",
        vec![ConversationMessage::user(MessageId::new(), "source".into())],
        None,
        paid_core(calls.clone()),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                result = &mut compact => panic!("post-next Mod unexpectedly completed: {}", result.is_ok()),
                () = tokio::task::yield_now() => {}
            }
            if output.snapshot().await.iter().any(|event| matches!(event,
                OutputEvent::ModLog { text, .. } if text.string_at("") == Some("paid-next-returned"))) {
                break;
            }
        }
    }).await.expect("Mod receives the completed paid summary before cancellation");
    drop(compact);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = tracker.snapshot().await;
            if state.cost_revision == 1 {
                assert_eq!(
                    state
                        .per_model_usage
                        .values()
                        .map(|model| model.usage.tokens.input)
                        .sum::<u64>(),
                    19
                );
                assert!(state.total_nano_usd > 0);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropped middleware cannot cancel the session-owned cost finalizer");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn no_op_compact_mod_preserves_hidden_context_at_every_boundary() {
    let (_root, orch, _output, _tracker, _events) = mod_fixture("return next(e);").await;
    let visible = || ConversationMessage::user(MessageId::new(), "ordinary request".into());
    let hidden =
        || ConversationMessage::user_meta(MessageId::new(), "supplemental instructions".into());
    let system = || ConversationMessage::System {
        id: MessageId::new(),
        content: "system context".into(),
        subtype: None,
        api_system: None,
        compact_metadata: None,
        model_fallback: None,
        refusal_fallback: None,
    };
    for source in [
        vec![visible(), hidden()],
        vec![system(), visible(), hidden(), system()],
        vec![hidden(), visible(), hidden(), visible(), hidden()],
        vec![hidden(), hidden()],
    ] {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_for_core = captured.clone();
        let core: SessionCompactCore = Arc::new(move |messages, _| {
            *captured_for_core.lock().unwrap() = messages.clone();
            Box::pin(async move {
                Ok(SessionCompactCoreOutput::new(
                    summary_result(None),
                    messages,
                    Duration::ZERO,
                ))
            })
        });
        let result = orch
            .dispatch_session_compact_mods("manual", source.clone(), None, core)
            .await
            .unwrap();
        assert_eq!(*captured.lock().unwrap(), source);
        assert!(
            matches!(result, SessionCompactDecision::Continue { source_messages, .. } if source_messages == source)
        );
    }
}

#[test]
fn hidden_tail_is_restored_once_when_the_last_visible_handle_is_repeated() {
    let visible = ConversationMessage::user(MessageId::new(), "request".into());
    let hidden = ConversationMessage::user_meta(MessageId::new(), "tail".into());
    let mut codec = SessionCompactRowCodec::default();
    let rows = codec.rows_for_messages(&[visible, hidden.clone()]);
    let restored = codec
        .messages_of_rows(&[rows[0].clone(), rows[0].clone()])
        .unwrap();
    assert_eq!(
        restored
            .iter()
            .filter(|message| message.id() == hidden.id())
            .count(),
        1
    );
}

struct PaidReaskClient {
    calls: AtomicUsize,
    owned: Arc<std::sync::atomic::AtomicU32>,
    retry: u8,
}

fn attempt_usage(input: u64, output: u64) -> cost::Usage {
    cost::Usage {
        tokens: cost::TokenUsage {
            input,
            output,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl sidequery::SideQueryClient for PaidReaskClient {
    async fn query(
        &self,
        _: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            self.owned.load(Ordering::SeqCst),
            call as u32,
            "the preceding physical response is owned before a reask dispatches"
        );
        if call == 1 && self.retry != 0 {
            let source = sidequery::SideQueryError::Api(llm_runtime::LlmError::Transport {
                message: "retry disconnected".into(),
            });
            return Err(if self.retry == 2 {
                sidequery::SideQueryError::Partial {
                    source: Box::new(source),
                    usage: attempt_usage(17, 5),
                    elapsed: Duration::from_millis(4),
                    retry_count: 0,
                    api_calls: 1,
                }
            } else {
                source
            });
        }
        assert!(call < 2, "one native reask only");
        Ok(sidequery::SideQueryResponse {
            text: Some(
                if call == 0 {
                    "paid but untagged"
                } else {
                    "<summary>accepted retry</summary>"
                }
                .into(),
            ),
            structured: None,
            tool_calls: Vec::new(),
            usage: if call == 0 {
                attempt_usage(13, 3)
            } else {
                attempt_usage(17, 5)
            },
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

fn reask_history() -> Vec<ConversationMessage> {
    let assistant = || ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: "accepted answer".into(),
            citations: None,
        }],
        stop_reason: Some("end_turn".into()),
        resumable: None,
        per_turn_effort: None,
    };
    vec![
        ConversationMessage::user(MessageId::new(), "first question".into()),
        assistant(),
        ConversationMessage::user(MessageId::new(), "second question".into()),
        assistant(),
    ]
}

fn real_summary_core(compactor: Arc<compaction::CompactionOrchestrator>) -> SessionCompactCore {
    Arc::new(move |source_messages, instructions| {
        let compactor = compactor.clone();
        Box::pin(async move {
            let started = std::time::Instant::now();
            let result = compactor
                .process_forced(
                    source_messages.clone(),
                    instructions.as_ref(),
                    Some(llm_runtime::ApiAttemptsLeft::new(5)),
                )
                .await
                .map_err(|error| hooks::mods::ModError::Native(error.to_string()))?;
            Ok(SessionCompactCoreOutput::new(
                result,
                source_messages,
                started.elapsed(),
            ))
        })
    })
}

#[tokio::test]
async fn real_reask_owns_the_untagged_response_and_each_successful_or_partial_retry() {
    for retry in [0, 1, 2] {
        let (_root, orch, _output, tracker, _events) = mod_fixture("return next(e);").await;
        let client = Arc::new(PaidReaskClient {
            calls: AtomicUsize::new(0),
            owned: orch.model_runtime.api_calls_recorded.clone(),
            retry,
        });
        let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let runner = Arc::new(
            sidequery::ForkedAgentRunner::new()
                .with_side_query_client(client.clone(), "claude-opus-4-8".into()),
        );
        let compactor = Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
            u64::MAX,
        ));
        let orch = orch
            .with_compaction(compactor.clone())
            .with_cache_safe_slot(slot);
        let history = reask_history();
        orch.session.lock().await.history = history.clone();
        orch.seed_compact_cache_safe_params(None).await.unwrap();
        let result = orch
            .dispatch_session_compact_mods("manual", history, None, real_summary_core(compactor))
            .await;
        assert_eq!(result.is_ok(), retry == 0);
        assert_eq!(client.calls.load(Ordering::SeqCst), 2);
        let state = tracker.snapshot().await;
        let paid = if retry == 1 { 1 } else { 2 };
        assert_eq!(state.cost_revision, paid);
        assert_eq!(
            state
                .per_model_usage
                .values()
                .map(|model| model.usage.tokens.input)
                .sum::<u64>(),
            if paid == 1 { 13 } else { 30 }
        );
        assert_eq!(
            state
                .per_model_usage
                .values()
                .map(|model| model.usage.tokens.output)
                .sum::<u64>(),
            if paid == 1 { 3 } else { 8 }
        );
        assert!(state.total_nano_usd > 0);
    }
}

#[derive(Default)]
struct PendingOrdinarySummaryClient {
    started: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl sidequery::SideQueryClient for PendingOrdinarySummaryClient {
    async fn query(
        &self,
        _: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        unreachable!("the physical fork must carry its usage observer")
    }

    async fn query_fork(
        &self,
        _: sidequery::SideQueryRequest,
        options: sidequery::ForkedSideQueryOptions,
    ) -> Result<sidequery::ForkedSideQueryResponse, sidequery::SideQueryError> {
        options
            .usage_observer
            .expect("ordinary summary has a request-local observer")
            .observe(attempt_usage(29, 2));
        self.started.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn dropping_an_ordinary_summary_with_partial_usage_keeps_its_physical_charge() {
    let (_root, orch, _output, tracker, _events) = mod_fixture("return next(e);").await;
    let client = Arc::new(PendingOrdinarySummaryClient::default());
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(client.clone(), "claude-opus-4-8".into()),
    );
    let compactor = Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
        compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
        u64::MAX,
    ));
    let orch = orch
        .with_compaction(compactor.clone())
        .with_cache_safe_slot(slot);
    let history = reask_history();
    orch.session.lock().await.history = history.clone();
    orch.seed_compact_cache_safe_params(None).await.unwrap();
    let mut compact = Box::pin(orch.dispatch_session_compact_mods(
        "manual",
        history,
        None,
        real_summary_core(compactor),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = &mut compact => panic!("ordinary summary unexpectedly completed: {}", result.is_ok()),
            () = client.started.notified() => {}
        }
    }).await.expect("ordinary summary observes partial usage");
    drop(compact);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = tracker.snapshot().await;
            if state.cost_revision == 1 {
                assert_eq!(
                    state
                        .per_model_usage
                        .values()
                        .map(|model| model.usage.tokens.input)
                        .sum::<u64>(),
                    29
                );
                assert!(state.total_nano_usd > 0);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("request drop transfers its partial observation to the original session");
}
