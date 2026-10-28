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
        _input: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, hooks::mods::ModError> {
        let call = self
            .message_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.started.notify_one();
        if self.block_first && call == 0 {
            self.release_first.notified().await;
        }
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse("[]")
            .map_err(|error| hooks::mods::ModError::Protocol(error.to_string()))
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
    host.load(
        plugin,
        root,
        module,
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}").unwrap(),
    )
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

async fn build_owned_orchestrator(
    module: &std::path::Path,
    root: &std::path::Path,
    output: Arc<MockOutputStream>,
) -> Arc<ConversationOrchestrator> {
    build_owned_orchestrator_with_bun(module, root, output, None).await
}

async fn build_owned_orchestrator_with_bun(
    module: &std::path::Path,
    root: &std::path::Path,
    output: Arc<MockOutputStream>,
    bun: Option<&std::path::Path>,
) -> Arc<ConversationOrchestrator> {
    let host = hooks::mods::ModHost::start(bun).await.unwrap();
    host.load(
        "origin-mod",
        root,
        module,
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}").unwrap(),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let registry = Arc::new(tokio::sync::RwLock::new(registry));
    let orchestrator = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output,
            Arc::new(StaticMemoryProvider::empty()),
            root.to_path_buf(),
        )
        .with_hook_registry(registry.clone()),
    );
    let session: Arc<dyn hooks::mods::ModSessionContext> = orchestrator.clone();
    registry
        .write()
        .await
        .attach_mod_background_context(Arc::downgrade(&session));
    orchestrator
}

async fn event_session_view(
    orchestrator: &Arc<ConversationOrchestrator>,
    output: Arc<MockOutputStream>,
) -> Arc<dyn hooks::mods::ModSessionContext> {
    let guard = orchestrator
        .mod_session_work_gate
        .clone()
        .try_read_owned()
        .unwrap();
    let session_id = orchestrator.session.lock().await.session_id;
    let generation = orchestrator
        .mod_session_generation
        .load(std::sync::atomic::Ordering::Acquire);
    let session: Arc<dyn hooks::mods::ModSessionContext> = orchestrator.clone();
    let view = crate::turn_loop::tool_dispatch::origin_bound_mod_session_context(
        orchestrator,
        session,
        session_id,
        generation,
        output,
    )
    .unwrap();
    drop(guard);
    view
}

async fn settle_main_turn(
    orchestrator: &ConversationOrchestrator,
    turn_id: &str,
    answer: &str,
    is_aborted: bool,
) {
    let question: lingxi_core::types::utf16_json::Utf16JsonProjection = "question".into();
    orchestrator.fire_mod_turn_start(&question, turn_id).await;
    let response = ConversationMessage::Assistant {
        per_turn_effort: None,
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text: answer.to_owned(),
            citations: None,
        }],
        stop_reason: Some("end_turn".to_owned()),
        resumable: None,
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
            } if event_plugin == plugin => Some(text.string_at("").unwrap_or_default().to_owned()),

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
                cache_control: None,
                citations: None,
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
    let session_id = orchestrator
        .session
        .lock()
        .await
        .session_id
        .as_uuid()
        .to_string();
    *orchestrator
        .mod_turn
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(crate::conversation::ModTurnFacts {
            id: cancelled_turn_id.to_owned(),
            session_id,
            started_at: std::time::Instant::now(),
            answer: String::new().into(),

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
            if body.string_at("") == Some("queue-mod: queued summary")

    )));
    let history = format!("{:?}", orchestrator.session.lock().await.history);
    assert!(history.contains("first answer"));
    assert!(!history.contains("queued summary"));
}

#[tokio::test]
async fn delayed_turn_complete_uses_the_origin_captured_at_turn_start() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("turn-complete-origin.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.complete', async ($, e) => {
            await $.session.messages({});
            $.ui.log('old log', { to: 'transcript' });
            $.ui.toast('old toast');
            $.ui.status('old status');
            return { text: 'old notice' };
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
    let ordinary_output = Arc::new(MockOutputStream::new());
    let scoped_output = Arc::new(MockOutputStream::new());
    let orchestrator = build_orchestrator(
        &module,
        "origin-mod",
        &session,
        ordinary_output.clone(),
        Arc::new(MockApiClient::new(vec![])),
        root.path(),
    )
    .await;
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_factory = captured.clone();
    let scoped_factory = scoped_output.clone();
    orchestrator.set_mod_session_event_output_factory(Arc::new(move |origin| {
        captured_factory.lock().unwrap().push(origin);
        scoped_factory.clone()
    }));

    let session_a = orchestrator.session.lock().await.session_id;
    settle_main_turn(&orchestrator, "turn-a", "answer-a", false).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        session.started.notified(),
    )
    .await
    .expect("turn A completion should reach its held host call");

    // Deliberately replace the mutable session view after enqueue. The output
    // identity must come from the captured origin, even if a caller later
    // changes the shared view before the queued handler finishes.
    orchestrator.session.lock().await.session_id = lingxi_core::types::SessionId::new();
    session.release_first.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if scoped_output.snapshot().await.iter().any(|event| {
                matches!(
                    event,
                    lingxi_core::host::OutputEvent::SystemNotice { body, .. }
                        if body.string_at("") == Some("origin-mod: old notice")
                )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("queued notice should reach the origin-bound output");

    let origins = captured.lock().unwrap().clone();
    assert_eq!(origins.len(), 1);
    assert_eq!(origins[0].session_id, session_a.as_uuid().to_string());
    assert_eq!(origins[0].mod_turn_id.as_deref(), Some("turn-a"));
    let events = scoped_output.snapshot().await;
    assert!(events
        .iter()
        .any(|event| matches!(event, lingxi_core::host::OutputEvent::ModLog { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, lingxi_core::host::OutputEvent::ModToast { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, lingxi_core::host::OutputEvent::ModStatus { .. })));
    assert!(ordinary_output.snapshot().await.is_empty());
}

#[tokio::test]
async fn active_mod_session_work_blocks_switch_until_it_finishes() {
    use lingxi_core::host::OrchestratorHandle as _;

    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("session-work.js");
    std::fs::write(&module, "export function register(on) {}\n").unwrap();
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = build_owned_orchestrator(&module, root.path(), output.clone()).await;
    let session_a = orchestrator.session.lock().await.session_id;
    let view = event_session_view(&orchestrator, output).await;
    let lease = view.acquire_session_work_lease().await.unwrap().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let active_view = view.clone();
    let active = tokio::spawn(async move {
        let _lease = lease;
        let _ = started_tx.send(());
        finish_rx.await.expect("release active session work");
        active_view
            .messages(None)
            .await
            .expect("read originating session")
    });
    started_rx.await.unwrap();
    let error = orchestrator
        .clear_session()
        .await
        .expect_err("switch must refuse while real session work owns a lease");
    assert!(error.to_string().contains("background Mod operation"));
    assert_eq!(orchestrator.session.lock().await.session_id, session_a);

    finish_tx.send(()).unwrap();
    active.await.unwrap();
    orchestrator
        .clear_session()
        .await
        .expect("retry after work exits");
    assert_ne!(orchestrator.session.lock().await.session_id, session_a);
    assert!(view.acquire_session_work_lease().await.is_err());
}

#[tokio::test]
async fn dropped_mod_tool_waiter_cancels_but_keeps_session_owned_until_work_exits() {
    use lingxi_core::host::OrchestratorHandle as _;

    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("session-tool-work.js");
    std::fs::write(&module, "export function register(on) {}\n").unwrap();
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = build_owned_orchestrator(&module, root.path(), output.clone()).await;
    let session_a = orchestrator.session.lock().await.session_id;
    let view = event_session_view(&orchestrator, output).await;
    let lease = view.acquire_session_work_lease().await.unwrap().unwrap();
    let cancellation = lingxi_core::host::CancellationToken::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
    let worker_view = view.clone();
    let waiter = tokio::spawn(
        crate::turn_loop::tool_dispatch::run_owned_mod_session_tool_work(
            lease,
            cancellation.clone(),
            async move {
                let _ = started_tx.send(());
                finish_rx.await.expect("release physical tool work");
                let _ = observed_tx.send(worker_view.id().await);
                Ok(())
            },
        ),
    );
    started_rx.await.unwrap();
    waiter.abort();
    let _ = waiter.await;
    assert!(
        cancellation.is_cancelled(),
        "the physical tool is cancelled"
    );
    assert!(orchestrator.clear_session().await.is_err());
    finish_tx.send(()).unwrap();
    assert_eq!(observed_rx.await.unwrap(), session_a.to_string());
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match orchestrator.clear_session().await {
                Ok(()) => break,
                Err(error) if error.to_string().contains("background Mod operation") => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("unexpected session switch failure: {error}"),
            }
        }
    })
    .await
    .expect("session switch succeeds after physical work exits");
    assert_ne!(orchestrator.session.lock().await.session_id, session_a);
    assert!(view.acquire_session_work_lease().await.is_err());
}

#[tokio::test]
async fn same_session_directory_switch_invalidates_old_mod_session_view() {
    use lingxi_core::host::OrchestratorHandle as _;

    let root = tempfile::tempdir().unwrap();
    let next = tempfile::tempdir().unwrap();
    let module = root.path().join("session-generation.js");
    std::fs::write(&module, "export function register(on) {}\n").unwrap();
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = build_owned_orchestrator(&module, root.path(), output.clone()).await;
    let session_id = orchestrator.session.lock().await.session_id;
    let old = event_session_view(&orchestrator, output).await;
    let target = next.path().to_path_buf();
    orchestrator
        .change_working_directory(target.clone(), vec![target])
        .await
        .expect("change the directory without changing the session UUID");
    assert_eq!(orchestrator.session.lock().await.session_id, session_id);
    assert!(old.acquire_session_work_lease().await.is_err());
}

#[tokio::test]
async fn child_origin_capability_rejects_a_to_b_to_a_session_reactivation() {
    use hooks::mods::ModSessionContext as _;
    use lingxi_core::host::subagent_spawn::SessionOriginError;
    use lingxi_core::host::OrchestratorHandle as _;

    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("child-session-origin.js");
    std::fs::write(&module, "export function register(on) {}\n").unwrap();
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = build_owned_orchestrator(&module, root.path(), output).await;
    let original = orchestrator
        .capture_session_origin_capability()
        .await
        .unwrap()
        .unwrap();
    let session_a = original.session_id();
    let old_generation = original.generation();
    drop(original.admit().await.unwrap());

    orchestrator.clear_session().await.unwrap();
    assert_ne!(orchestrator.current_session_id().await, session_a);
    assert!(matches!(
        original.admit().await,
        Err(SessionOriginError::SessionChanged)
    ));
    orchestrator
        .resume_session(session_a, Vec::new(), None, None, Default::default())
        .await
        .unwrap();
    assert_eq!(orchestrator.current_session_id().await, session_a);
    let rebound = orchestrator
        .bind_session_origin(original.clone())
        .unwrap()
        .unwrap();
    assert!(rebound.acquire_session_work_lease().await.is_err());
    let current = orchestrator
        .capture_session_origin_capability()
        .await
        .unwrap()
        .unwrap();
    assert_ne!(current.generation(), old_generation);
    drop(current.admit().await.unwrap());
}

#[tokio::test]
async fn host_request_scoped_view_captures_generation_for_delayed_api() {
    use lingxi_core::host::OrchestratorHandle as _;
    use tool_api::context::{HostToolCancellation, HostToolRequest};

    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("host-scoped-origin.js");
    std::fs::write(&module, "export function register(on) {}\n").unwrap();
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = build_owned_orchestrator(&module, root.path(), output).await;
    let session_a = orchestrator.current_session_id().await;
    let request = Arc::new(HostToolRequest::default());
    let scoped = tool_api::context::with_host_tool_cancellation(
        HostToolCancellation::TerminateOwnedProcess(request),
        async {
            crate::turn_loop::tool_dispatch::scoped_mod_session_for_test(&orchestrator).unwrap()
        },
    )
    .await;
    let captured = scoped.capture_dispatch_context().await.unwrap().unwrap();
    assert_eq!(
        captured.session_origin_capability().unwrap().session_id(),
        session_a
    );
    orchestrator.clear_session().await.unwrap();
    assert!(captured.acquire_session_work_lease().await.is_err());
}

#[tokio::test]
async fn detached_host_timer_uses_snapshot_only_while_request_scope_lives() {
    use lingxi_core::host::OrchestratorHandle as _;
    use tool_api::context::{HostToolCancellation, HostToolRequest};

    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("owned-host-timer.js");
    std::fs::write(&module, "export function register(on) {}\n").unwrap();
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = build_owned_orchestrator(&module, root.path(), output).await;
    let session_id = orchestrator.current_session_id().await;
    let snapshot = orchestrator
        .hooks
        .snapshot_for_owned_host_request(
            &[],
            &hooks::HookContext {
                session_id,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let request = Arc::new(HostToolRequest::default());
    let detached = tool_api::context::with_host_tool_cancellation(
        HostToolCancellation::TerminateOwnedProcess(request),
        hooks::executor::with_owned_host_hook_snapshot(snapshot, async {
            let shared = hooks::executor::current_owned_host_hook_snapshot_arc().unwrap();
            let detached = hooks::executor::with_owned_host_hook_snapshot_arc(shared, async {
                let scoped =
                    crate::turn_loop::tool_dispatch::scoped_mod_session_for_test(&orchestrator)
                        .unwrap();
                let captured = scoped.capture_dispatch_context().await.unwrap().unwrap();
                captured.ui_invalidation_context().unwrap()
            })
            .await;
            let lease = detached
                .acquire_session_work_lease()
                .await
                .expect("timer can use the still-owned request snapshot")
                .unwrap();
            drop(lease);
            detached
        }),
    )
    .await;
    let error = detached
        .acquire_session_work_lease()
        .await
        .err()
        .expect("timer must refuse after request scope ends");
    assert!(error.to_string().contains("snapshot has ended"));
}

#[test]
fn detached_host_publication_guard_does_not_keep_request_alive() {
    use tool_api::context::{HostToolCancellation, HostToolRequest};
    let request = Arc::new(HostToolRequest::default());
    let weak = Arc::downgrade(&request);
    let cancellation = HostToolCancellation::TerminateOwnedProcess(request.clone());
    let guard =
        crate::turn_loop::tool_dispatch::host_request_publication_guard(&cancellation, None)
            .unwrap();
    assert!(guard.is_current());
    drop(cancellation);
    drop(request);
    assert!(weak.upgrade().is_none());
    assert!(!guard.is_current());
    assert!(guard
        .generation_cancellation_token()
        .unwrap()
        .is_cancelled());
}

#[cfg(unix)]
#[tokio::test]
async fn recurring_mod_timer_does_not_retain_owner_or_worker() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("recurring-origin.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('turn.start', ($, e, next) => {
        $.clock.after(3600000, async () => { await $.ui.log('unexpected-long-timer', { to: 'transcript' }); });
        $.clock.every(20, async () => { await $.ui.log('timer-alive', { to: 'transcript' }); });
        return next(e);
      });
    }"#,
    )
    .unwrap();
    // The wrapper records the PID and execs the real worker, so kill -0
    // observes the actual process rather than the shell wrapper.
    let pid_file = root.path().join("worker.pid");
    let wrapper = root.path().join("bun-wrapper");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec bun \"$@\"\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Arc::new(MockOutputStream::new());
    let orchestrator =
        build_owned_orchestrator_with_bun(&module, root.path(), output.clone(), Some(&wrapper))
            .await;
    let host = orchestrator
        .lifecycle_runtime
        .hook_registry
        .as_ref()
        .unwrap()
        .read()
        .await
        .mod_host()
        .unwrap();
    let weak_host = Arc::downgrade(&host);
    let weak_owner = Arc::downgrade(&orchestrator);
    drop(host);
    orchestrator
        .fire_mod_turn_start(&"timer".into(), "turn-a")
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !mod_logs(&output, "origin-mod")
            .await
            .iter()
            .any(|text| text == "timer-alive")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the real recurring timer must have run");
    let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    assert!(pid > 1);
    let running = || {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(
        running(),
        "the recorded worker must be live before dropping its owner"
    );
    drop(orchestrator);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while weak_owner.upgrade().is_some() || weak_host.upgrade().is_some() || running() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the timer must not keep its owner or worker alive");
}

#[tokio::test]
async fn idle_old_mod_timer_cannot_read_a_new_session() {
    use lingxi_core::host::OrchestratorHandle as _;

    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("session-origin-timer.js");
    std::fs::write(
        &module,
        r#"let release;
        export function register(on) {
          on('turn.start', ($, e, next) => {
            if (e.turnId === 'release') release();
            return next(e);
          });
          on('turn.complete', ($, e, next) => {
            const gate = new Promise(resolve => { release = resolve; });
            $.clock.after(0, async () => {
              await gate;
              try {
                const id = await $.session.id();
                $.ui.log(`old-read:${id}`, { to: 'transcript' });
              } catch (_) {
                await $.ui.notify('old notify');
                await $.ui.log('old-rejected', { to: 'transcript' });
              }
            });
            $.ui.log('scheduled', { to: 'transcript' });
            return next(e);
          });
        }"#,
    )
    .unwrap();
    let ordinary = Arc::new(MockOutputStream::new());
    let scoped = Arc::new(MockOutputStream::new());
    let orchestrator = build_owned_orchestrator(&module, root.path(), ordinary).await;
    let scoped_factory = scoped.clone();
    orchestrator.set_mod_session_event_output_factory(Arc::new(move |_| scoped_factory.clone()));
    let session_a = orchestrator.session.lock().await.session_id;
    settle_main_turn(&orchestrator, "turn-a", "answer-a", false).await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !mod_logs(&scoped, "origin-mod")
            .await
            .iter()
            .any(|text| text == "scheduled")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("old turn scheduled its real worker timer");

    orchestrator
        .clear_session()
        .await
        .expect("an idle timer must not hold the session read lease");
    assert_ne!(orchestrator.session.lock().await.session_id, session_a);
    orchestrator
        .fire_mod_turn_start(&"release timer".into(), "release")
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !mod_logs(&scoped, "origin-mod")
            .await
            .iter()
            .any(|text| text == "old-rejected")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the old timer must receive a stale-session error");
    assert!(!mod_logs(&scoped, "origin-mod")
        .await
        .iter()
        .any(|text| text.starts_with("old-read:")));
    assert!(scoped.snapshot().await.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::ModNotify { text, .. }
            if text.string_at("") == Some("old notify")
    )));
}

#[tokio::test]
async fn turn_start_timer_keeps_its_origin_after_session_switch() {
    use lingxi_core::host::OrchestratorHandle as _;

    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("turn-start-origin-timer.js");
    std::fs::write(
        &module,
        r#"let release;
        export function register(on) {
          on('turn.start', ($, e, next) => {
            $.ui.log(`immediate:${e.turnId}`, { to: 'transcript' });
            if (e.turnId === 'turn-a') {
              const gate = new Promise(resolve => { release = resolve; });
              $.clock.after(0, async () => {
                await gate;
                try {
                  const id = await $.session.id();
                  await $.ui.log(`old-read:${id}`, { to: 'transcript' });
                } catch (_) {
                  await $.ui.log('old-rejected', { to: 'transcript' });
                }
              });
            }
            if (e.turnId === 'release') release();
            return next(e);
          });
        }"#,
    )
    .unwrap();
    let ordinary = Arc::new(MockOutputStream::new());
    let scoped = Arc::new(MockOutputStream::new());
    let orchestrator = build_owned_orchestrator(&module, root.path(), ordinary.clone()).await;
    let origins = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = origins.clone();
    let scoped_factory = scoped.clone();
    orchestrator.set_mod_session_event_output_factory(Arc::new(move |origin| {
        captured.lock().unwrap().push(origin);
        scoped_factory.clone()
    }));
    let session_a = orchestrator.session.lock().await.session_id;
    orchestrator
        .fire_mod_turn_start(&"question".into(), "turn-a")
        .await;
    assert!(mod_logs(&ordinary, "origin-mod")
        .await
        .iter()
        .any(|text| text == "immediate:turn-a"));
    assert!(mod_logs(&scoped, "origin-mod").await.is_empty());

    orchestrator
        .clear_session()
        .await
        .expect("an idle turn.start timer must not block the switch");
    assert_ne!(orchestrator.session.lock().await.session_id, session_a);
    orchestrator
        .fire_mod_turn_start(&"release timer".into(), "release")
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !mod_logs(&scoped, "origin-mod")
            .await
            .iter()
            .any(|text| text == "old-rejected")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the old turn.start timer must receive a stale-session error");
    assert!(!mod_logs(&scoped, "origin-mod")
        .await
        .iter()
        .any(|text| text.starts_with("old-read:")));
    let origins = origins.lock().unwrap().clone();
    assert_eq!(origins[0].session_id, session_a.as_uuid().to_string());
    assert_eq!(origins[0].mod_turn_id.as_deref(), Some("turn-a"));
}

#[tokio::test]
async fn prompt_submit_timer_uses_captured_session_after_switch() {
    use lingxi_core::host::OrchestratorHandle as _;

    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("prompt-submit-origin-timer.js");
    std::fs::write(
        &module,
        r#"let release;
        export function register(on) {
          on('prompt.submit', ($, e, next) => {
            const gate = new Promise(resolve => { release = resolve; });
            $.clock.after(0, async () => {
              await gate;
              try {
                const id = await $.session.id();
                await $.ui.log(`old-read:${id}`, { to: 'transcript' });
              } catch (_) {
                await $.ui.log('old-rejected', { to: 'transcript' });
              }
            });
            $.ui.log('immediate-prompt', { to: 'transcript' });
            $.ui.notify('immediate-notify');
            return next(e);
          });
          on('turn.start', ($, e, next) => {
            if (e.turnId === 'release') release();
            return next(e);
          });
        }"#,
    )
    .unwrap();
    let ordinary = Arc::new(MockOutputStream::new());
    let scoped = Arc::new(MockOutputStream::new());
    let orchestrator = build_owned_orchestrator(&module, root.path(), ordinary.clone()).await;
    let origins = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = origins.clone();
    let scoped_factory = scoped.clone();
    orchestrator.set_mod_session_event_output_factory(Arc::new(move |origin| {
        captured.lock().unwrap().push(origin);
        scoped_factory.clone()
    }));
    let session_a = orchestrator.session.lock().await.session_id;
    let _ = orchestrator
        .screen_mod_prompt_submit(&"question".into(), &[], None)
        .await;
    assert!(mod_logs(&ordinary, "origin-mod")
        .await
        .iter()
        .any(|text| text == "immediate-prompt"));
    assert!(ordinary.snapshot().await.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::ModNotify { text, .. }
            if text.string_at("") == Some("immediate-notify")
    )));
    assert!(mod_logs(&scoped, "origin-mod").await.is_empty());

    orchestrator
        .clear_session()
        .await
        .expect("an idle prompt.submit timer must not block the switch");
    assert_ne!(orchestrator.session.lock().await.session_id, session_a);
    orchestrator
        .fire_mod_turn_start(&"release timer".into(), "release")
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !mod_logs(&scoped, "origin-mod")
            .await
            .iter()
            .any(|text| text == "old-rejected")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the old prompt.submit timer must receive a stale-session error");
    assert!(!mod_logs(&scoped, "origin-mod")
        .await
        .iter()
        .any(|text| text.starts_with("old-read:")));
    let origins = origins.lock().unwrap().clone();
    assert_eq!(origins[0].session_id, session_a.as_uuid().to_string());
    assert_eq!(origins[0].mod_turn_id, None);
}

#[tokio::test]
async fn session_start_timer_can_reenter_its_switch_writer() {
    use lingxi_core::host::OrchestratorHandle as _;

    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("session-start-writer.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('session.start', async ($, e, next) => {
            const id = await new Promise((resolve, reject) => {
              $.clock.after(0, async () => {
                try { resolve(await $.session.id()); }
                catch (error) { reject(error); }
              });
            });
            await $.ui.log(`started:${id}`, { to: 'transcript' });
            return next(e);
          });
        }"#,
    )
    .unwrap();
    let output = Arc::new(MockOutputStream::new());
    let orchestrator = build_owned_orchestrator(&module, root.path(), output.clone()).await;
    let session_a = orchestrator.session.lock().await.session_id;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        orchestrator.clear_session(),
    )
    .await
    .expect("session.start's timer must not deadlock its switch writer")
    .expect("session.start must succeed");
    let session_b = orchestrator.session.lock().await.session_id;
    assert_ne!(session_b, session_a);
    assert!(mod_logs(&output, "origin-mod")
        .await
        .iter()
        .any(|text| text == &format!("started:{session_b}")));
}

#[tokio::test]
async fn stale_session_measure_does_not_read_the_replacement_session_id() {
    struct StaleSession {
        id_reads: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl hooks::mods::ModSessionContext for StaleSession {
        async fn acquire_session_work_lease(
            &self,
        ) -> Result<Option<Arc<dyn Send + Sync>>, hooks::mods::ModError> {
            Err(hooks::mods::ModError::Unavailable(
                "originating session changed".into(),
            ))
        }

        fn cwd(&self) -> std::path::PathBuf {
            std::env::current_dir().unwrap()
        }

        fn root(&self) -> std::path::PathBuf {
            self.cwd()
        }

        async fn model(&self) -> String {
            "unused".into()
        }

        async fn id(&self) -> String {
            self.id_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            "replacement-session".into()
        }

        async fn turns(&self) -> u64 {
            0
        }
    }

    let id_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let session = StaleSession {
        id_reads: id_reads.clone(),
    };
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    let output = Arc::new(MockOutputStream::new());
    crate::conversation::hooks_impl::dispatch_mod_session_measure_once(
        host,
        &session,
        output.clone(),
        serde_json::json!({"changed": []}),
    )
    .await;
    assert_eq!(id_reads.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(output.snapshot().await.is_empty());
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
