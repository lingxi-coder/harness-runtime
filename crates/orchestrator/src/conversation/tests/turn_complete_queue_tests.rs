use super::*;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use lingxi_core::types::{ContentBlock, MessageId};
use std::sync::Arc;

struct TurnCompleteSession {
    started: Arc<tokio::sync::Notify>,
    release_first: Arc<tokio::sync::Notify>,
    message_calls: std::sync::atomic::AtomicUsize,
    block_first: bool,
    dropped: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Drop for TurnCompleteSession {
    fn drop(&mut self) {
        if let Some(dropped) = &self.dropped {
            dropped.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl hooks::mods::ModSessionContext for TurnCompleteSession {
    fn cwd(&self) -> std::path::PathBuf {
        std::env::current_dir().unwrap()
    }

    fn root(&self) -> std::path::PathBuf {
        std::env::current_dir().unwrap()
    }

    async fn model(&self) -> String {
        "claude-opus-4-7".to_owned()
    }

    async fn id(&self) -> String {
        "turn-complete-queue-test".to_owned()
    }

    async fn turns(&self) -> u64 {
        0
    }

    async fn messages(
        &self,
        _input: serde_json::Value,
    ) -> Result<hooks::mods::ModUtf16ValueProjection, hooks::mods::ModError> {
        let call = self
            .message_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.started.notify_one();
        if self.block_first && call == 0 {
            self.release_first.notified().await;
        }
        Ok(hooks::mods::ModUtf16ValueProjection::plain(
            serde_json::json!([]),
        ))
    }
}

async fn build_orchestrator(
    module: &std::path::Path,
    plugin: &str,
    session: &Arc<TurnCompleteSession>,
    output: Arc<MockOutputStream>,
    api: Arc<dyn OrchestratorApiClient>,
    root: &std::path::Path,
) -> Arc<ConversationOrchestrator> {
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(plugin, root, module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let session_trait: Arc<dyn hooks::mods::ModSessionContext> = session.clone();
    registry.attach_mod_background_context(Arc::downgrade(&session_trait));
    Arc::new(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            api,
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output,
            Arc::new(StaticMemoryProvider::empty()),
            root.to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    )
}

async fn settle_main_turn(
    orchestrator: &ConversationOrchestrator,
    turn_id: &str,
    answer: &str,
    is_aborted: bool,
) {
    orchestrator.fire_mod_turn_start("question", turn_id).await;
    let response = ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text: answer.to_owned(), citations: None,
        }],
        stop_reason: Some("end_turn".to_owned()),
    };
    orchestrator.record_mod_turn_response(
        &response,
        None,
        "claude-opus-4-7",
        Some("end_turn"),
        None,
    );
    orchestrator.fire_mod_turn_complete(is_aborted, false).await;
}

async fn mod_logs(output: &MockOutputStream, plugin: &str) -> Vec<String> {
    output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ModLog {
                plugin: event_plugin,
                text,
            } if event_plugin == plugin => Some(text),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn turn_complete_queue_is_fifo_nonblocking_and_emits_once() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("turn-complete-queue.js");
    std::fs::write(
        &module,
        r#"let calls = 0;
        export function register(on) {
          on('turn.start', ($, e, next) => {
            $.ui.log(`start:${e.turnId}`, { to: 'transcript' });
            return next(e);
          });
          on('turn.complete', async ($, e, next) => {
            const call = ++calls;
            if (call === 1) await $.session.messages({});
            $.ui.log(`complete:${e.turnId}:${e.reason}:${e.isAborted}:${call}`, { to: 'transcript' });
            if (call === 1) return { text: 'queued summary' };
            return next(e);
          });
        }"#,
    )
    .unwrap();
    let session = Arc::new(TurnCompleteSession {
        started: Arc::new(tokio::sync::Notify::new()),
        release_first: Arc::new(tokio::sync::Notify::new()),
        message_calls: std::sync::atomic::AtomicUsize::new(0),
        block_first: true,
        dropped: None,
    });
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = build_orchestrator(
        &module,
        "queue-mod",
        &session,
        output.clone(),
        Arc::new(MockApiClient::new(vec![mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "first answer".to_owned(),
                cache_control: None, citations: None,
            }],
            Some("end_turn"),
        )])),
        root.path(),
    )
    .await;

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        orchestrator.run_turn("first question"),
    )
    .await
    .expect("main turn should return without awaiting turn.complete")
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        session.started.notified(),
    )
    .await
    .expect("first complete hook should be running and held");

    let first_turn_id = mod_logs(&output, "queue-mod")
        .await
        .iter()
        .find_map(|line| line.strip_prefix("start:"))
        .expect("the main turn should emit turn.start")
        .to_owned();

    // Queue the completion produced by a cancelled settled turn while the
    // preceding Mod hook is still pending. This follows the same production
    // `is_aborted=true` settle path without waiting for the worker's serialized
    // `turn.start` dispatch to finish first.
    let cancelled_turn_id = "cancelled-turn";
    *orchestrator
        .mod_turn
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(crate::conversation::ModTurnFacts {
            id: cancelled_turn_id.to_owned(),
            started_at: std::time::Instant::now(),
            answer: String::new(),
            usage: None,
            refusal: None,
            error: false,
        });
    orchestrator.fire_mod_turn_complete(true, false).await;

    // Taking lifecycle facts is one-shot: a duplicate settle callback cannot
    // enqueue a second completion for the same turn.
    orchestrator.fire_mod_turn_complete(true, false).await;
    assert!(mod_logs(&output, "queue-mod")
        .await
        .iter()
        .all(|line| !line.starts_with("complete:")));
    session.release_first.notify_one();

    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if mod_logs(&output, "queue-mod")
                .await
                .iter()
                .filter(|line| line.starts_with("complete:"))
                .count()
                == 2
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("both queued complete hooks should settle");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let logs = mod_logs(&output, "queue-mod").await;
    let complete_logs = logs
        .iter()
        .filter(|line| line.starts_with("complete:"))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        complete_logs,
        vec![
            format!("complete:{first_turn_id}:answer:false:1"),
            format!("complete:{cancelled_turn_id}:aborted:true:2"),
        ]
    );
    assert!(output.snapshot().await.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::SystemNotice { body, .. }
            if body == "queue-mod: queued summary"
    )));
    let history = format!("{:?}", orchestrator.session.lock().await.history);
    assert!(history.contains("first answer"));
    assert!(!history.contains("queued summary"));
}

#[tokio::test]
async fn queued_turn_complete_keeps_session_alive_after_orchestrator_drop() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("turn-complete-close.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.complete', async ($, e, next) => {
            await $.session.messages({});
            $.ui.log(`${e.turnId}:${e.reason}:${e.isAborted}`, { to: 'transcript' });
            return next(e);
          });
        }"#,
    )
    .unwrap();
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let session = Arc::new(TurnCompleteSession {
        started: Arc::new(tokio::sync::Notify::new()),
        release_first: Arc::new(tokio::sync::Notify::new()),
        message_calls: std::sync::atomic::AtomicUsize::new(0),
        block_first: true,
        dropped: Some(dropped.clone()),
    });
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = build_orchestrator(
        &module,
        "closing-mod",
        &session,
        output.clone(),
        Arc::new(MockApiClient::new(vec![])),
        root.path(),
    )
    .await;

    let started = session.started.clone();
    let release_first = session.release_first.clone();
    settle_main_turn(&orchestrator, "closed-turn", "answer", true).await;
    drop(orchestrator);
    drop(session);

    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .expect("the queued event should retain its session after orchestrator drop");
    release_first.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if !mod_logs(&output, "closing-mod").await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("queued event should still reach the Mod host after owner drop");
    assert_eq!(
        mod_logs(&output, "closing-mod").await,
        vec!["closed-turn:aborted:true"]
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the queue should release its strong session after dispatch");
}
