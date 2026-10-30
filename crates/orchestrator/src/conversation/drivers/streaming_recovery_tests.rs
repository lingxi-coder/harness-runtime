use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::conversation::ConversationOrchestrator;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::test_support_stream::MockStreamingApiClient;
use crate::{OrchestratorConfig, OrchestratorError};
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use serde_json::json;
use tool_api::registry::ToolRegistry;

struct CountingSideClient(AtomicUsize);

struct TestPublicationGuard {
    current: AtomicBool,
    attempts: AtomicUsize,
    invalidate_after_publish: AtomicBool,
    hold_next_publish: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl hooks::attachment::HookPublicationGuard for TestPublicationGuard {
    fn is_current(&self) -> bool {
        self.current.load(Ordering::SeqCst)
    }

    fn generation_cancellation_token(&self) -> Option<lingxi_core::host::CancellationToken> {
        None
    }

    fn publish_if_current<'a>(
        &'a self,
        publication: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.hold_next_publish.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            if !self.current.load(Ordering::SeqCst) {
                return false;
            }
            publication.await;
            if self.invalidate_after_publish.swap(false, Ordering::SeqCst) {
                self.current.store(false, Ordering::SeqCst);
            }
            true
        })
    }

    fn commit_if_current<'a>(
        &'a self,
        mutation: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        hooks::attachment::HookPublicationGuard::publish_if_current(self, mutation)
    }
}

#[async_trait::async_trait]
impl sidequery::SideQueryClient for CountingSideClient {
    async fn query(
        &self,
        _request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(sidequery::SideQueryResponse {
            text: Some("<summary>test summary</summary>".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

async fn fixture(
    root: &std::path::Path,
    event: &str,
    admission_error: Option<hooks::RecoveryAdmissionError>,
) -> (
    ConversationOrchestrator,
    Arc<MockStreamingApiClient>,
    Arc<MockApiClient>,
    Arc<CountingSideClient>,
    Arc<AtomicUsize>,
    Arc<MockOutputStream>,
) {
    let module = root.join("admission.js");
    std::fs::write(
        &module,
        format!("export function register(on) {{ on({event:?}, ($, e, next) => next(e)); }}"),
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "recovery-test",
        root,
        &module,
        lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(&json!({})).unwrap(),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);

    let streaming = Arc::new(MockStreamingApiClient::empty());
    let api = Arc::new(MockApiClient::new(Vec::new()));
    let side = Arc::new(CountingSideClient(AtomicUsize::new(0)));
    let output = Arc::new(MockOutputStream::new());
    let cache = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(side.clone(), "test-compact-model".into()),
    );
    let compactor = Arc::new(compaction::CompactionOrchestrator::with_autocompactor(
        compaction::Autocompactor::with_forked_runner(runner, cache.clone()),
        1,
    ));
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        api.clone(),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        root.to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)))
    .with_compaction(compactor)
    .with_cache_safe_slot(cache);

    {
        let session = orch.session();
        let mut session = session.lock().await;
        for index in 0..20 {
            if index % 2 == 0 {
                session.history.push(ConversationMessage::user(
                    MessageId::new(),
                    format!("turn {index}: enough text to trigger automatic compaction"),
                ));
            } else {
                session.history.push(ConversationMessage::Assistant {
                    id: MessageId::new(),
                    content: vec![ContentBlock::Text {
                        text: format!("reply {index}"),
                        citations: None,
                    }],
                    stop_reason: Some("end_turn".into()),
                    resumable: None,
                    per_turn_effort: None,
                });
            }
        }
    }
    let admissions = Arc::new(AtomicUsize::new(0));
    let observed = admissions.clone();
    orch.set_tool_dispatch_recovery_guard(Arc::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
        match &admission_error {
            Some(error) => Err(error.clone()),
            None => Ok(None),
        }
    }));
    (orch, streaming, api, side, admissions, output)
}

#[tokio::test]
async fn prompt_section_admission_failure_never_opens_a_provider() {
    let root = tempfile::tempdir().unwrap();
    let (orch, streaming, api, side, admissions, _) = fixture(
        root.path(),
        "prompt.section",
        Some(hooks::RecoveryAdmissionError::Failure(
            "admission failed".into(),
        )),
    )
    .await;
    let result = orch.run_turn_streaming("hello").await;
    assert!(
        matches!(result, Err(OrchestratorError::Internal(message)) if message == "admission failed")
    );
    assert!(admissions.load(Ordering::SeqCst) > 0);
    assert!(streaming.captured_calls().await.is_empty());
    assert!(api.captured_requests().await.is_empty());
    assert_eq!(side.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn prompt_context_admission_failure_stops_compaction_provider() {
    let root = tempfile::tempdir().unwrap();
    let (orch, streaming, api, side, admissions, _) = fixture(
        root.path(),
        "prompt.context",
        Some(hooks::RecoveryAdmissionError::Failure(
            "admission failed".into(),
        )),
    )
    .await;
    let result = orch.run_turn_streaming("hello").await;
    assert!(
        matches!(result, Err(OrchestratorError::Internal(message)) if message == "admission failed")
    );
    assert!(admissions.load(Ordering::SeqCst) > 0);
    assert!(streaming.captured_calls().await.is_empty());
    assert!(api.captured_requests().await.is_empty());
    assert_eq!(side.0.load(Ordering::SeqCst), 0);

    // The same low-threshold fixture must really reach the side provider when
    // admission succeeds; otherwise the zero-call assertion would be vacuous.
    let control_root = tempfile::tempdir().unwrap();
    let (control, _, _, control_side, control_admissions, _) =
        fixture(control_root.path(), "prompt.context", None).await;
    let _ = control.run_turn_streaming("hello").await;
    assert!(control_admissions.load(Ordering::SeqCst) > 0);
    assert!(control_side.0.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn interrupted_admission_propagates_without_provider_error_or_rejection() {
    for event in ["prompt.section", "prompt.context"] {
        let root = tempfile::tempdir().unwrap();
        let (orch, streaming, api, side, admissions, output) = fixture(
            root.path(),
            event,
            Some(hooks::RecoveryAdmissionError::Interrupted),
        )
        .await;
        let initial_assistant_count = orch
            .session()
            .lock()
            .await
            .history
            .iter()
            .filter(|message| matches!(message, ConversationMessage::Assistant { .. }))
            .count();

        let result = orch.run_turn_streaming("hello").await;
        assert!(
            matches!(&result, Err(OrchestratorError::RecoveryInterrupted)),
            "{event} must propagate the typed interruption: {result:?}"
        );
        assert!(admissions.load(Ordering::SeqCst) > 0);
        assert!(streaming.captured_calls().await.is_empty());
        assert!(api.captured_requests().await.is_empty());
        assert_eq!(side.0.load(Ordering::SeqCst), 0);

        let session = orch.session();
        let session = session.lock().await;
        let assistant_count = session
            .history
            .iter()
            .filter(|message| matches!(message, ConversationMessage::Assistant { .. }))
            .count();
        assert_eq!(
            assistant_count, initial_assistant_count,
            "no provider error row"
        );
        let history = serde_json::to_string(&session.history).unwrap();
        assert!(!history.contains("User rejected tool use"));
        assert!(!history.contains("[Request interrupted by user]"));
        assert!(!history.contains("[Request interrupted by user for tool use]"));
        drop(session);

        for event in output.snapshot().await {
            assert!(
                !matches!(
                    event,
                    lingxi_core::host::OutputEvent::TerminalApiError { .. }
                        | lingxi_core::host::OutputEvent::SystemNotice { is_error: true, .. }
                ),
                "interrupted admission must not emit a provider error"
            );
        }
    }
}

#[tokio::test]
async fn tool_requested_terminal_retains_publication_guard_until_final_emit() {
    for remains_current in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (orch, _, _, _, _, output) = fixture(root.path(), "prompt.section", None).await;
        let guard = Arc::new(TestPublicationGuard {
            current: AtomicBool::new(true),
            attempts: AtomicUsize::new(0),
            invalidate_after_publish: AtomicBool::new(false),
            hold_next_publish: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let mut state = super::TurnLoopState::new(&orch, MessageId::new(), None);
        let disposition = orch
            .finish_tool_requested_streaming_end(&mut state, MessageId::new(), Some(guard.clone()))
            .await
            .unwrap();
        let super::StreamingIterationDisposition::ForcedComplete(_, terminal) = disposition else {
            panic!("tool-requested end must remain a forced completion");
        };
        assert_eq!(guard.attempts.load(Ordering::SeqCst), 1);
        assert!(!output
            .snapshot()
            .await
            .iter()
            .any(|event| matches!(event, lingxi_core::host::OutputEvent::EndTurn { .. })));
        guard.current.store(remains_current, Ordering::SeqCst);
        orch.publish_deferred_terminal(terminal, &None).await;
        assert_eq!(guard.attempts.load(Ordering::SeqCst), 2);
        let reasons = output
            .snapshot()
            .await
            .into_iter()
            .filter_map(|event| match event {
                lingxi_core::host::OutputEvent::EndTurn { stop_reason, .. } => Some(stop_reason),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            if remains_current {
                vec!["end_turn"]
            } else {
                vec![]
            }
        );
    }
}

#[tokio::test]
async fn cancelled_tool_terminal_does_not_persist_interruption_after_guard_reset() {
    let root = tempfile::tempdir().unwrap();
    let (orch, _, _, _, _, output) = fixture(root.path(), "prompt.section", None).await;
    let guard = Arc::new(TestPublicationGuard {
        current: AtomicBool::new(true),
        attempts: AtomicUsize::new(0),
        invalidate_after_publish: AtomicBool::new(false),
        hold_next_publish: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut state = super::TurnLoopState::new(&orch, MessageId::new(), None);
    let disposition = orch
        .finish_tool_requested_streaming_end(&mut state, MessageId::new(), Some(guard.clone()))
        .await
        .unwrap();
    let super::StreamingIterationDisposition::ForcedComplete(_, terminal) = disposition else {
        panic!("tool-requested end must remain a forced completion");
    };
    guard.invalidate_after_publish.store(true, Ordering::SeqCst);
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    orch.publish_deferred_terminal(terminal, &Some(cancel))
        .await;
    assert_eq!(guard.attempts.load(Ordering::SeqCst), 3);
    assert!(output.snapshot().await.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "aborted_tools"
    )));
    let session = orch.session();
    assert!(!serde_json::to_string(&session.lock().await.history)
        .unwrap()
        .contains("[Request interrupted by user for tool use]"));
}

#[tokio::test]
async fn cancellation_while_waiting_for_tool_terminal_guard_changes_terminal_reason() {
    let root = tempfile::tempdir().unwrap();
    let (orch, _, _, _, _, output) = fixture(root.path(), "prompt.section", None).await;
    let orch = Arc::new(orch);
    let guard = Arc::new(TestPublicationGuard {
        current: AtomicBool::new(true),
        attempts: AtomicUsize::new(0),
        invalidate_after_publish: AtomicBool::new(false),
        hold_next_publish: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut state = super::TurnLoopState::new(&orch, MessageId::new(), None);
    let disposition = orch
        .finish_tool_requested_streaming_end(&mut state, MessageId::new(), Some(guard.clone()))
        .await
        .unwrap();
    let super::StreamingIterationDisposition::ForcedComplete(_, terminal) = disposition else {
        panic!("tool-requested end must remain a forced completion");
    };
    guard.hold_next_publish.store(true, Ordering::SeqCst);
    let cancel = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn({
        let orch = orch.clone();
        let cancel = cancel.clone();
        async move {
            orch.publish_deferred_terminal(terminal, &Some(cancel))
                .await;
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), guard.entered.notified())
        .await
        .expect("terminal waits for its publication guard");
    cancel.cancel();
    guard.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("terminal publication completes")
        .unwrap();
    let reasons = output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::EndTurn { stop_reason, .. } => Some(stop_reason),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(reasons, ["aborted_tools"]);
}
