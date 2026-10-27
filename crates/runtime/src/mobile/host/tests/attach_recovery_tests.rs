use super::*;
use crate::mobile::test_support::{new_engine_with_streaming, test_config};
use crate::mobile::turn_durability::DurableTurnStoreError;

#[derive(Default)]
struct AttachBarrierListener {
    events: tokio::sync::Mutex<Vec<ClientEvent>>,
    entered: Notify,
    release: Notify,
}

#[derive(Default)]
struct CancelBarrierListener {
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl ClientEventListener for CancelBarrierListener {
    async fn on_event(&self, event: ClientEvent) {
        if matches!(event, ClientEvent::TurnRecoveryState { snapshot } if snapshot.state == TurnRecoveryStateDto::Cancelled)
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
    async fn on_workflow_progress(
        &self,
        _: String,
        _: String,
        _: String,
        _: client::protocol::listings::WorkflowProgressDto,
    ) {
    }
}

#[derive(Default)]
struct CountBackgrounder(std::sync::atomic::AtomicUsize);

#[async_trait]
impl lingxi_core::host::task_registry::TaskBackgrounder for CountBackgrounder {
    async fn background(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    async fn background_with_outcome(
        &self,
        _: lingxi_core::host::task_registry::TaskBackgroundReason,
    ) -> lingxi_core::host::task_registry::TaskBackgroundOutcome {
        self.background().await;
        lingxi_core::host::task_registry::TaskBackgroundOutcome::Backgrounded
    }
}

#[derive(Default)]
struct PanicStartedListener;

struct GatedTwoTurnStream {
    calls: AtomicUsize,
    first_started: Arc<Notify>,
    release_first: Arc<Notify>,
    second_started: Arc<Notify>,
}

#[async_trait]
impl orchestrator::StreamingApiClient for GatedTwoTurnStream {
    async fn stream(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        _messages: Vec<lingxi_core::types::ConversationMessage>,
        _tools: Vec<llm_runtime::ToolDeclaration>,
        _query_source: &str,
        _skip_global_cache_for_system_prompt: bool,
        _request_dispatch_admission: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<
        futures_util::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        use futures_util::StreamExt as _;
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_started.notify_one();
            let release = self.release_first.clone();
            let stream = futures_util::stream::once(async move {
                release.notified().await;
                use orchestrator::test_support_stream::{
                    content_block_start_text, content_block_stop, message_delta_stop,
                    message_start, message_stop, text_delta,
                };
                vec![
                    message_start("first", "test"),
                    content_block_start_text(0),
                    text_delta(0, "answer"),
                    content_block_stop(0),
                    message_delta_stop("end_turn"),
                    message_stop(),
                ]
                .into_iter()
                .map(Ok::<_, llm_runtime::LlmError>)
                .collect::<Vec<_>>()
            })
            .flat_map(futures_util::stream::iter);
            Ok(stream.boxed())
        } else {
            self.second_started.notify_one();
            Ok(futures_util::stream::pending().boxed())
        }
    }
}

#[async_trait]
impl ClientEventListener for PanicStartedListener {
    async fn on_event(&self, event: ClientEvent) {
        if matches!(
            projected_turn_event(&event),
            ClientEvent::TurnStarted { .. }
        ) {
            panic!("controlled turn-start callback panic");
        }
    }
    async fn on_workflow_progress(
        &self,
        _: String,
        _: String,
        _: String,
        _: client::protocol::listings::WorkflowProgressDto,
    ) {
    }
}

async fn arm_background_probe(handle: &MobileEngineHandle) -> Arc<CountBackgrounder> {
    let registry = &handle.inner.task_registry;
    let (id, _) = registry.allocate_bash_output().await.unwrap();
    registry
        .register_foreground_bash(
            &id,
            lingxi_core::host::task_registry::BackgroundBashRegistration {
                command: "probe".into(),
                description: "probe".into(),
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
    let probe = Arc::new(CountBackgrounder::default());
    registry
        .bind_background_requester(&id, probe.clone())
        .await
        .unwrap();
    probe
}

#[async_trait]
impl ClientEventListener for AttachBarrierListener {
    async fn on_event(&self, event: ClientEvent) {
        let hold = matches!(&event, ClientEvent::TurnAttachStarted { request_id, .. } if request_id == "request-one");
        self.events.lock().await.push(event);
        if hold {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }

    async fn on_workflow_progress(
        &self,
        _: String,
        _: String,
        _: String,
        _: client::protocol::listings::WorkflowProgressDto,
    ) {
    }
}

fn engine(root: &Path, listener: Arc<AttachBarrierListener>) -> Arc<MobileEngineHandle> {
    new_engine_with_streaming(
        test_config(root),
        Arc::new(HostFakePlatform::new(root.to_path_buf())),
        listener,
        Arc::new(RecordingPermissionSink::default()),
        None,
    )
    .unwrap()
}

#[test]
fn dropped_attach_producer_finishes_its_batch_before_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = Arc::new(AttachBarrierListener::default());
    let handle = engine(tmp.path(), listener.clone());
    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        handle.durable_turns.begin(&session_id, 280, "prompt".into(), None, vec![]).unwrap();
        handle.durable_turns.append_event(
            &session_id, 280, r#"{"type":"text_delta","text":"retained"}"#.into(),
        ).unwrap();
        listener.events.lock().await.clear();
        let first_handle = handle.clone();
        let first_session = session_id.clone();
        let first = tokio::spawn(async move {
            first_handle.submit(ClientCommand::AttachTurn {
                session_id: first_session, turn_id: 280, after_sequence: None,
                request_id: "request-one".into(),
            }).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), listener.entered.notified())
            .await.unwrap();
        first.abort();
        let _ = first.await;
        let retry_handle = handle.clone();
        let retry_session = session_id.clone();
        let mut retry = tokio::spawn(async move {
            retry_handle.submit(ClientCommand::AttachTurn {
                session_id: retry_session, turn_id: 280, after_sequence: None,
                request_id: "request-two".into(),
            }).await
        });
        assert!(tokio::time::timeout(std::time::Duration::from_millis(100), &mut retry).await.is_err());
        listener.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), retry)
            .await.unwrap().unwrap().unwrap();
        let events = listener.events.lock().await;
        assert!(matches!(events.as_slice(), [
            ClientEvent::TurnAttachStarted { request_id: first, snapshot: first_snapshot },
            ClientEvent::TurnEventReplay { session_id: first_session, turn_id: 280, sequence: 1, .. },
            ClientEvent::TurnAttachFinished { request_id: first_done, session_id: first_done_session, turn_id: 280 },
            ClientEvent::TurnAttachStarted { request_id: second, snapshot: second_snapshot },
            ClientEvent::TurnEventReplay { session_id: second_session, turn_id: 280, sequence: 1, .. },
            ClientEvent::TurnAttachFinished { request_id: second_done, session_id: second_done_session, turn_id: 280 },
        ] if first == "request-one" && first_done == first
            && second == "request-two" && second_done == second
            && first_snapshot.session_id == session_id && second_snapshot.session_id == session_id
            && first_session == &session_id && first_done_session == &session_id
            && second_session == &session_id && second_done_session == &session_id),
            "attach batches were not correlated and serialized: {events:?}");
    });
}

#[test]
fn live_recovery_state_can_interleave_with_an_attach_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = Arc::new(AttachBarrierListener::default());
    let handle = engine(tmp.path(), listener.clone());
    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        handle.durable_turns.begin(&session_id, 284, "prompt".into(), None, vec![]).unwrap();
        handle.durable_turns.append_event(&session_id, 284, r#"{"type":"text_delta","text":"retained"}"#.into()).unwrap();
        listener.events.lock().await.clear();
        let attaching = handle.clone();
        let attach_session = session_id.clone();
        let attach = tokio::spawn(async move {
            attaching.submit(ClientCommand::AttachTurn {
                session_id: attach_session, turn_id: 284, after_sequence: None, request_id: "request-one".into(),
            }).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), listener.entered.notified()).await.unwrap();
        let newer = handle.durable_turns.transition(&session_id, 284, TurnRecoveryStateDto::WaitingForUser, false, Some("live".into())).unwrap();
        handle.event_sink.emit(ClientEvent::TurnRecoveryState { snapshot: newer.clone() }).await;
        listener.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), attach).await.unwrap().unwrap().unwrap();
        let events = listener.events.lock().await;
        assert!(matches!(events.as_slice(), [
            ClientEvent::TurnAttachStarted { snapshot: started, .. },
            ClientEvent::TurnRecoveryState { snapshot: live },
            ClientEvent::TurnEventReplay { session_id: replay_session, turn_id: 284, sequence: 1, .. },
            ClientEvent::TurnAttachFinished { session_id: done_session, turn_id: 284, .. },
        ] if started.session_id == session_id && live.session_id == session_id
            && live.revision > started.revision && live.revision == newer.revision
            && replay_session == &session_id && done_session == &session_id),
            "unexpected attach/live interleaving: {events:?}");
    });
}

#[test]
fn stale_attach_session_and_invalid_request_id_are_rejected_before_events() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = Arc::new(AttachBarrierListener::default());
    let handle = engine(tmp.path(), listener.clone());
    handle.runtime().block_on(async {
        let old_session = handle.active_session_id();
        handle
            .durable_turns
            .begin(&old_session, 281, "old".into(), None, vec![])
            .unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let old_handle = handle.clone();
        let old_request_session = old_session.clone();
        let delayed = tokio::spawn(async move {
            release_rx.await.unwrap();
            old_handle
                .submit(ClientCommand::AttachTurn {
                    session_id: old_request_session,
                    turn_id: 281,
                    after_sequence: None,
                    request_id: "old-request".into(),
                })
                .await
        });
        handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: None,
            })
            .await
            .unwrap();
        let new_session = handle.active_session_id();
        assert_ne!(old_session, new_session);
        handle
            .durable_turns
            .begin(&new_session, 281, "new".into(), None, vec![])
            .unwrap();
        listener.events.lock().await.clear();
        release_tx.send(()).unwrap();
        assert!(matches!(
            delayed.await.unwrap(),
            Err(ClientError::Rejected { .. })
        ));
        for request_id in [String::new(), "💥".repeat(33)] {
            assert!(matches!(
                handle
                    .submit(ClientCommand::AttachTurn {
                        session_id: new_session.clone(),
                        turn_id: 281,
                        after_sequence: None,
                        request_id,
                    })
                    .await,
                Err(ClientError::Rejected { .. })
            ));
        }
        assert_eq!(
            handle.durable_turns.load(&old_session, 281).unwrap().state,
            Some(TurnRecoveryStateDto::Running)
        );
        assert_eq!(
            handle.durable_turns.load(&new_session, 281).unwrap().state,
            Some(TurnRecoveryStateDto::Running)
        );
        assert!(!listener.events.lock().await.iter().any(|event| matches!(
            event,
            ClientEvent::TurnAttachStarted { .. } | ClientEvent::TurnAttachFinished { .. }
        )));
    });
}

#[test]
fn delayed_old_session_turn_commands_never_affect_new_session() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = Arc::new(AttachBarrierListener::default());
    let handle = engine(tmp.path(), listener);
    handle.runtime().block_on(async {
        let old_session = handle.active_session_id();
        handle
            .durable_turns
            .begin(&old_session, 285, "old".into(), None, vec![])
            .unwrap();
        handle
            .durable_turns
            .transition(
                &old_session,
                285,
                TurnRecoveryStateDto::PausedRecoverable,
                true,
                None,
            )
            .unwrap();
        // Keep the old turn's gate busy so cancellation observes A, then B is
        // selected before cancellation can acquire its mutation boundary.
        let gate = handle.lock_turn_delivery(&old_session, 285).await.unwrap();
        let cancelling = handle.clone();
        let cancel_session = old_session.clone();
        let mut cancel = tokio::spawn(async move {
            cancelling
                .submit(ClientCommand::Cancel {
                    session_id: Some(cancel_session),
                    turn_id: Some(285),
                })
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut cancel)
                .await
                .is_err()
        );
        handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: None,
            })
            .await
            .unwrap();
        let new_session = handle.active_session_id();
        assert_ne!(old_session, new_session);
        handle
            .durable_turns
            .begin(&new_session, 285, "new".into(), None, vec![])
            .unwrap();
        let new_owner = handle.reserve_turn(Some(285)).await.unwrap();
        drop(gate);
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(5), cancel)
                .await
                .unwrap()
                .unwrap(),
            Err(ClientError::Rejected { .. })
        ));
        for command in [
            ClientCommand::ResumeTurn {
                session_id: old_session.clone(),
                turn_id: 285,
            },
            ClientCommand::PauseTurn {
                session_id: old_session.clone(),
                turn_id: 285,
                reason: "background".into(),
            },
            ClientCommand::Cancel {
                session_id: Some(old_session.clone()),
                turn_id: None,
            },
            ClientCommand::Cancel {
                session_id: None,
                turn_id: None,
            },
        ] {
            assert!(matches!(
                handle.submit(command).await,
                Err(ClientError::Rejected { .. })
            ));
        }
        assert!(!new_owner.cancel.is_cancelled());
        assert_eq!(
            handle.durable_turns.load(&old_session, 285).unwrap().state,
            Some(TurnRecoveryStateDto::PausedRecoverable)
        );
        assert_eq!(
            handle.durable_turns.load(&new_session, 285).unwrap().state,
            Some(TurnRecoveryStateDto::Running)
        );
        handle.release_unspawned_turn(&new_owner).await;
    });
}

#[test]
fn cancel_received_prompt_before_admission_is_durable_and_never_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let (entered_tx, _entered_rx) = tokio::sync::oneshot::channel();
    let provider = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let listener = Arc::new(FakeListener::default());
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(provider.clone()),
    )
    .unwrap();
    let session_id = handle.active_session_id();
    handle.runtime().block_on(async {
        let transition = handle.loop_transition.lock().await;
        let sender = handle.clone();
        let send_session = session_id.clone();
        let send = tokio::spawn(async move {
            sender
                .submit(ClientCommand::SendPrompt {
                    session_id: Some(send_session),
                    text: "never run".into(),
                    prompt_mode: None,
                    images: vec![],
                    turn_id: Some(287),
                    visualization_context: None,
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if handle
                    .pending_submissions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|p| p.turn_id == 287)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let cancelling = handle.clone();
        let cancel_session = session_id.clone();
        let cancel = tokio::spawn(async move {
            cancelling
                .submit(ClientCommand::Cancel {
                    session_id: Some(cancel_session),
                    turn_id: Some(287),
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if handle
                    .pending_submissions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|p| p.turn_id == 287 && p.cancel_requested.load(Ordering::Acquire))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(transition);
        tokio::time::timeout(std::time::Duration::from_secs(5), send)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), cancel)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let checkpoint = handle.durable_turns.load(&session_id, 287).unwrap();
        assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::Cancelled));
        assert!(!checkpoint.safe_to_resume);
        assert!(provider.captured_calls().is_empty());
        assert!(handle.active_cancel.lock().await.is_none());
    });
    drop(handle);
    let reopened = crate::mobile::turn_durability::DurableTurnStore::open_shared(
        tmp.path().join(branding::DOT_DIR).join("mobile-turns"),
    )
    .unwrap();
    assert_eq!(
        reopened.load(&session_id, 287).unwrap().state,
        Some(TurnRecoveryStateDto::Cancelled)
    );
}

#[test]
fn cancel_while_prompt_waits_for_cron_gate_wins_before_startup() {
    let tmp = tempfile::tempdir().unwrap();
    let (entered_tx, _entered_rx) = tokio::sync::oneshot::channel();
    let provider = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let listener = Arc::new(FakeListener::default());
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(provider.clone()),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        let cron_gate = super::super::mobile_cron_session_gate(&handle.session_cwd(), &session_id);
        let hold = cron_gate.lock().await;
        let sender = handle.clone();
        let send_session = session_id.clone();
        let send = tokio::spawn(async move {
            sender
                .submit(ClientCommand::SendPrompt {
                    session_id: Some(send_session),
                    text: "not admitted".into(),
                    prompt_mode: None,
                    images: vec![],
                    turn_id: Some(293),
                    visualization_context: None,
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if handle
                    .pending_submissions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|p| p.turn_id == 293 && p.test_stage.load(Ordering::Acquire) == 1)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let cancelling = handle.clone();
        let cancel_session = session_id.clone();
        let cancel = tokio::spawn(async move {
            cancelling
                .submit(ClientCommand::Cancel {
                    session_id: Some(cancel_session),
                    turn_id: Some(293),
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if handle
                    .pending_submissions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|p| p.turn_id == 293 && p.cancel_requested.load(Ordering::Acquire))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(hold);
        tokio::time::timeout(std::time::Duration::from_secs(5), send)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), cancel)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            handle.durable_turns.load(&session_id, 293).unwrap().state,
            Some(TurnRecoveryStateDto::Cancelled)
        );
        assert!(provider.captured_calls().is_empty());
        assert!(!listener.received.lock().await.iter().any(|event| {
            match projected_turn_event(event) {
                ClientEvent::TurnStarted { .. } => true,
                ClientEvent::TurnRecoveryState { snapshot } => {
                    snapshot.state == TurnRecoveryStateDto::Running
                }
                _ => false,
            }
        }));
    });
}

#[test]
fn pending_cancel_waits_for_its_terminal_callback() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = Arc::new(CancelBarrierListener::default());
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        None,
    )
    .unwrap();
    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        let transition = handle.loop_transition.lock().await;
        let sender = handle.clone();
        let send_session = session_id.clone();
        let send = tokio::spawn(async move {
            sender
                .submit(ClientCommand::SendPrompt {
                    session_id: Some(send_session),
                    text: "stop".into(),
                    prompt_mode: None,
                    images: vec![],
                    turn_id: Some(294),
                    visualization_context: None,
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if handle
                    .pending_submissions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|p| p.turn_id == 294)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let cancelling = handle.clone();
        let cancel_session = session_id.clone();
        let mut cancel = tokio::spawn(async move {
            cancelling
                .submit(ClientCommand::Cancel {
                    session_id: Some(cancel_session),
                    turn_id: Some(294),
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if handle
                    .pending_submissions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|p| p.turn_id == 294 && p.cancel_requested.load(Ordering::Acquire))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(transition);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            listener.entered.notified(),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut cancel)
                .await
                .is_err()
        );
        listener.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), send)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), cancel)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    });
}

#[test]
fn cancel_during_ambiguous_checkpoint_publish_delivers_terminal_before_ack() {
    let tmp = tempfile::tempdir().unwrap();
    let (provider_tx, _provider_rx) = tokio::sync::oneshot::channel();
    let provider = Arc::new(PendingPromptStreamClient::new(provider_tx));
    let listener = Arc::new(CancelBarrierListener::default());
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(provider.clone()),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let session = handle.active_session_id();
        let cron = super::super::mobile_cron_session_gate(&handle.session_cwd(), &session);
        let hold = cron.lock().await;
        let sender = handle.clone();
        let origin = session.clone();
        let send = tokio::spawn(async move {
            sender
                .submit(ClientCommand::SendPrompt {
                    session_id: Some(origin),
                    text: "ambiguous".into(),
                    prompt_mode: None,
                    images: vec![],
                    turn_id: Some(298),
                    visualization_context: None,
                })
                .await
        });
        let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let item = handle
                    .pending_submissions
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|p| p.turn_id == 298 && p.test_stage.load(Ordering::Acquire) == 1)
                    .cloned();
                if let Some(item) = item {
                    break item;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *pending.owner_barrier.lock().unwrap() = Some((entered.clone(), release.clone()));
        handle.durable_turns.fail_after_next_metadata_publish();
        drop(hold);
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        let owner = handle.active_cancel.lock().await.as_ref().unwrap().clone();
        let cancelling = handle.clone();
        let origin = session.clone();
        let mut cancel = tokio::spawn(async move {
            cancelling
                .submit(ClientCommand::Cancel {
                    session_id: Some(origin),
                    turn_id: Some(298),
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !owner.cancel.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            listener.entered.notified(),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut cancel)
                .await
                .is_err()
        );
        listener.release.notify_one();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), send)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), cancel)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            handle.durable_turns.load(&session, 298).unwrap().state,
            Some(TurnRecoveryStateDto::Cancelled)
        );
        assert!(provider.captured_calls().is_empty());
        assert!(handle.active_cancel.lock().await.is_none());
    });
}

#[test]
fn cancel_after_slash_claim_publishes_correlated_uncertain_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = Arc::new(CancelBarrierListener::default());
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        None,
    )
    .unwrap();
    handle.runtime().block_on(async {
        let session = handle.active_session_id();
        let cron = super::super::mobile_cron_session_gate(&handle.session_cwd(), &session);
        let hold = cron.lock().await;
        let sender = handle.clone();
        let origin = session.clone();
        let submission = tokio::spawn(async move {
            sender
                .submit(ClientCommand::RunSlashCommand {
                    session_id: Some(origin),
                    raw: "/context".into(),
                    images: vec![],
                    turn_id: Some(299),
                })
                .await
        });
        let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let item = handle
                    .pending_submissions
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|p| p.turn_id == 299 && p.test_stage.load(Ordering::Acquire) == 1)
                    .cloned();
                if let Some(item) = item {
                    break item;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *pending.owner_barrier.lock().unwrap() = Some((entered.clone(), release.clone()));
        drop(hold);
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        let cancelling = handle.clone();
        let origin = session.clone();
        let mut cancel = tokio::spawn(async move {
            cancelling
                .submit(ClientCommand::Cancel {
                    session_id: Some(origin),
                    turn_id: Some(299),
                })
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            listener.entered.notified(),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut cancel)
                .await
                .is_err()
        );
        release.notify_one();
        listener.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), submission)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), cancel)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let checkpoint = handle.durable_turns.load(&session, 299).unwrap();
        assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::Cancelled));
        assert!(checkpoint.projection_uncertain);
        assert!(!checkpoint.cancelled_before_claim);
        assert_eq!(
            handle
                .durable_turns
                .queued_turn_status(&session, 299)
                .unwrap(),
            client::protocol::events::QueuedTurnStatusDto::Terminal {
                state: TurnRecoveryStateDto::Cancelled
            }
        );
    });
}

#[test]
fn metadata_publish_failure_releases_owner_and_attach_reports_waiting() {
    let tmp = tempfile::tempdir().unwrap();
    let (entered_tx, _entered_rx) = tokio::sync::oneshot::channel();
    let provider = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let listener = Arc::new(FakeListener::default());
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(provider.clone()),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        handle.durable_turns.fail_after_next_metadata_publish();
        assert!(matches!(handle.submit(ClientCommand::SendPrompt { session_id: Some(session_id.clone()), text: "uncertain".into(), prompt_mode: None, images: vec![], turn_id: Some(295), visualization_context: None }).await, Err(ClientError::Internal { .. })));
        assert!(handle.active_cancel.lock().await.is_none());
        assert!(provider.captured_calls().is_empty());
        let checkpoint = handle.durable_turns.load(&session_id, 295).unwrap();
        assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::WaitingForUser));
        assert!(!checkpoint.safe_to_resume);
        listener.received.lock().await.clear();
        handle.submit(ClientCommand::AttachTurn { session_id: session_id.clone(), turn_id: 295, after_sequence: None, request_id: "after-write-failure".into() }).await.unwrap();
        assert!(listener.received.lock().await.iter().any(|event| matches!(event,
            ClientEvent::TurnAttachStarted { snapshot, .. } if snapshot.session_id == session_id && snapshot.state == TurnRecoveryStateDto::WaitingForUser && !snapshot.safe_to_resume
        )));
    });
}

#[test]
fn failed_turn_started_callback_releases_unspawned_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let (entered_tx, _entered_rx) = tokio::sync::oneshot::channel();
    let provider = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        Arc::new(PanicStartedListener),
        Arc::new(RecordingPermissionSink::default()),
        Some(provider.clone()),
    )
    .unwrap();
    let session_id = handle.active_session_id();
    handle.runtime().block_on(async {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle.submit(ClientCommand::SendPrompt {
                session_id: Some(session_id.clone()),
                text: "not started".into(),
                prompt_mode: None,
                images: vec![],
                turn_id: Some(296),
                visualization_context: None,
            }),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert!(handle.active_cancel.lock().await.is_none());
        assert!(provider.captured_calls().is_empty());
        let checkpoint = handle.durable_turns.load(&session_id, 296).unwrap();
        assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::WaitingForUser));
        assert!(!checkpoint.safe_to_resume);
        assert!(tokio::time::timeout(
            std::time::Duration::from_secs(1),
            handle.submit(ClientCommand::Cancel {
                session_id: Some(session_id.clone()),
                turn_id: Some(296)
            })
        )
        .await
        .is_ok());
    });
}

#[test]
fn delayed_prompt_and_slash_keep_their_original_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (entered_tx, _entered_rx) = tokio::sync::oneshot::channel();
    let provider = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let listener = Arc::new(FakeListener::default());
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener,
        Arc::new(RecordingPermissionSink::default()),
        Some(provider.clone()),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let old_session = handle.active_session_id();
        let transition = handle.loop_transition.lock().await;
        let prompt_sender = handle.clone();
        let prompt_session = old_session.clone();
        let prompt = tokio::spawn(async move {
            prompt_sender
                .submit(ClientCommand::SendPrompt {
                    session_id: Some(prompt_session),
                    text: "old prompt".into(),
                    prompt_mode: None,
                    images: vec![],
                    turn_id: Some(288),
                    visualization_context: None,
                })
                .await
        });
        let slash_sender = handle.clone();
        let slash_session = old_session.clone();
        let slash = tokio::spawn(async move {
            slash_sender
                .submit(ClientCommand::RunSlashCommand {
                    session_id: Some(slash_session),
                    raw: "/context".into(),
                    images: vec![],
                    turn_id: Some(289),
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if handle.pending_submissions.lock().unwrap().len() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Exercise the same orchestrator/host retarget performed by NewSession
        // while its transition lock is owned and both old commands are queued.
        handle.inner.orchestrator.clear_session().await.unwrap();
        let selected = handle.inner.orchestrator.current_session_id().await;
        handle
            .retarget_session_writer(selected, &handle.session_cwd())
            .await
            .unwrap();
        let new_session = handle.active_session_id();
        assert_ne!(old_session, new_session);
        drop(transition);
        let prompt = prompt.await.unwrap();
        let slash = slash.await.unwrap();
        assert!(matches!(
            prompt,
            Err(ClientError::SubmissionNotAdmitted { .. })
        ));
        assert!(matches!(
            slash,
            Err(ClientError::SubmissionNotAdmitted { .. })
        ));
        assert!(provider.captured_calls().is_empty());
        assert!(handle
            .inner
            .orchestrator
            .snapshot_history()
            .await
            .is_empty());
        assert!(matches!(
            handle.durable_turns.load(&new_session, 288),
            Err(DurableTurnStoreError::NotFound { .. })
        ));
        assert!(matches!(
            handle.durable_turns.load(&new_session, 289),
            Err(DurableTurnStoreError::NotFound { .. })
        ));
    });
}

#[test]
fn old_cancel_does_not_background_new_session_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = Arc::new(CancelBarrierListener::default());
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        None,
    )
    .unwrap();
    handle.runtime().block_on(async {
        let old_session = handle.active_session_id();
        handle
            .durable_turns
            .begin(&old_session, 286, "old".into(), None, vec![])
            .unwrap();
        let old_owner = handle.reserve_turn(Some(286)).await.unwrap();
        let old_task = arm_background_probe(&handle).await;
        let cancelling = handle.clone();
        let mut cancel = tokio::spawn(async move {
            cancelling
                .submit(ClientCommand::Cancel {
                    session_id: Some(old_session),
                    turn_id: Some(286),
                })
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            listener.entered.notified(),
        )
        .await
        .unwrap();
        assert!(old_owner.cancel.is_cancelled());
        let mut active = handle.active_cancel.lock().await;
        super::super::release_active_turn(&mut active, &handle.message_queue, &old_owner);
        drop(active);
        old_owner.mark_completed();
        handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: None,
            })
            .await
            .unwrap();
        let new_session = handle.active_session_id();
        handle
            .durable_turns
            .begin(&new_session, 286, "new".into(), None, vec![])
            .unwrap();
        let new_owner = handle.reserve_turn(Some(286)).await.unwrap();
        let new_task = arm_background_probe(&handle).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut cancel)
                .await
                .is_err()
        );
        listener.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), cancel)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(old_task.0.load(Ordering::SeqCst), 1);
        assert_eq!(new_task.0.load(Ordering::SeqCst), 0);
        assert!(!new_owner.cancel.is_cancelled());
        handle.release_unspawned_turn(&new_owner).await;
    });
}

#[test]
fn attach_reports_cursor_ahead_and_never_replays_invalid_history() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = Arc::new(AttachBarrierListener::default());
    let handle = engine(tmp.path(), listener.clone());
    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        handle.durable_turns.begin(&session_id, 282, "prompt".into(), None, vec![]).unwrap();
        handle.durable_turns.append_event(
            &session_id, 282, r#"{"type":"text_delta","text":"one"}"#.into(),
        ).unwrap();
        listener.events.lock().await.clear();
        handle.submit(ClientCommand::AttachTurn {
            session_id: session_id.clone(), turn_id: 282, after_sequence: Some(2),
            request_id: "ahead".into(),
        }).await.unwrap();
        assert!(matches!(listener.events.lock().await.as_slice(), [
            ClientEvent::TurnAttachStarted { request_id, snapshot },
            ClientEvent::TurnAttachFinished { request_id: finished, session_id: origin, turn_id: 282 },
        ] if request_id == "ahead" && finished == request_id && origin == &session_id
            && snapshot.reason.as_deref() == Some("replay_cursor_ahead")
            && snapshot.state == TurnRecoveryStateDto::WaitingForUser));

        handle.durable_turns.begin(&session_id, 283, "other".into(), None, vec![]).unwrap();
        handle.durable_turns.append_event(
            &session_id, 283, r#"{"type":"unknown_future_event","value":"preserve"}"#.into(),
        ).unwrap();
        listener.events.lock().await.clear();
        assert!(matches!(handle.submit(ClientCommand::AttachTurn {
            session_id: session_id.clone(), turn_id: 283, after_sequence: None,
            request_id: "bad".into(),
        }).await, Err(ClientError::Internal { .. })));
        assert!(!listener.events.lock().await.iter().any(|event| matches!(
            event, ClientEvent::TurnAttachStarted { .. } | ClientEvent::TurnAttachFinished { .. }
        )));
        let checkpoint = handle.durable_turns.load(&session_id, 283).unwrap();
        assert_eq!(checkpoint.last_sequence, 1);
        assert!(checkpoint.events[0].event_json.contains("preserve"));
    });
}

#[test]
fn attach_distinguishes_pending_missing_corrupt_and_terminal_turns() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = Arc::new(AttachBarrierListener::default());
    let handle = engine(tmp.path(), listener.clone());
    handle.runtime().block_on(async {
        let session_id = handle.active_session_id();
        let attach = |turn_id| ClientCommand::AttachTurn {
            session_id: session_id.clone(), turn_id, after_sequence: None, request_id: format!("attach-{turn_id}"),
        };
        let shared = handle.durable_turns.register_pending_admission(&session_id, 290).unwrap();
        let pending = Arc::new(super::super::PendingSubmission::new(session_id.clone(), 290, shared));
        handle.pending_submissions.lock().unwrap().push(pending.clone());
        assert!(matches!(handle.submit(attach(290)).await, Err(ClientError::Rejected { .. })));
        handle.pending_submissions.lock().unwrap().retain(|entry| !Arc::ptr_eq(entry, &pending));
        drop(pending);
        assert!(matches!(handle.submit(attach(290)).await, Err(ClientError::NotFound { .. })));

        let dir = tmp.path().join(branding::DOT_DIR).join("mobile-turns").join(&session_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("291.json"), b"{invalid").unwrap();
        assert!(matches!(handle.submit(attach(291)).await, Err(ClientError::Internal { .. })));
        assert_eq!(std::fs::read(dir.join("291.json")).unwrap(), b"{invalid");
        std::fs::remove_file(dir.join("291.json")).unwrap();

        handle.durable_turns.begin(&session_id, 292, "done".into(), None, vec![]).unwrap();
        handle.durable_turns.cancel(&session_id, 292).unwrap();
        listener.events.lock().await.clear();
        handle.submit(attach(292)).await.unwrap();
        assert!(matches!(listener.events.lock().await.as_slice(), [
            ClientEvent::TurnAttachStarted { snapshot, .. }, ClientEvent::TurnAttachFinished { turn_id: 292, .. }
        ] if snapshot.state == TurnRecoveryStateDto::Cancelled));
    });
}

#[test]
fn another_handle_cannot_report_absent_or_cancel_a_received_submission() {
    let tmp = tempfile::tempdir().unwrap();
    let (entered_tx, _entered_rx) = tokio::sync::oneshot::channel();
    let provider = Arc::new(PendingPromptStreamClient::new(entered_tx));
    let first = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
        Some(provider.clone()),
    )
    .unwrap();
    let second = engine(tmp.path(), Arc::new(AttachBarrierListener::default()));
    first.runtime().block_on(async {
        let session_id = first.active_session_id();
        first
            .inner
            .session_writer
            .append_mobile_empty_session(&session_id, "pending submission", &first.session_cwd())
            .await
            .unwrap();
        first
            .inner
            .session_writer
            .append_session_mode(first.inner.session_mode.as_str())
            .await
            .unwrap();
        first
            .ensure_reserved_session_identity(&session_id)
            .await
            .unwrap();
        second
            .submit(ClientCommand::ResumeSession {
                session_id: session_id.clone(),
                cwd: None,
            })
            .await
            .unwrap();
        assert_eq!(second.active_session_id(), session_id);
        let transition = first.loop_transition.lock().await;
        let sender = first.clone();
        let origin = session_id.clone();
        let send = tokio::spawn(async move {
            sender
                .submit(ClientCommand::SendPrompt {
                    session_id: Some(origin),
                    text: "pending".into(),
                    prompt_mode: None,
                    images: vec![],
                    turn_id: Some(297),
                    visualization_context: None,
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if first
                    .pending_submissions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|p| p.turn_id == 297)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            second
                .submit(ClientCommand::AttachTurn {
                    session_id: session_id.clone(),
                    turn_id: 297,
                    after_sequence: None,
                    request_id: "second".into(),
                })
                .await,
            Err(ClientError::Rejected { .. })
        ));
        assert!(matches!(
            second
                .submit(ClientCommand::Cancel {
                    session_id: Some(session_id.clone()),
                    turn_id: Some(297),
                })
                .await,
            Err(ClientError::Rejected { .. })
        ));
        let cancelling = first.clone();
        let origin = session_id.clone();
        let cancel = tokio::spawn(async move {
            cancelling
                .submit(ClientCommand::Cancel {
                    session_id: Some(origin),
                    turn_id: Some(297),
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if first
                    .pending_submissions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|p| p.turn_id == 297 && p.cancel_requested.load(Ordering::Acquire))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(transition);
        tokio::time::timeout(std::time::Duration::from_secs(5), send)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), cancel)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(provider.captured_calls().is_empty());
        assert_eq!(
            first.durable_turns.load(&session_id, 297).unwrap().state,
            Some(TurnRecoveryStateDto::Cancelled)
        );
    });
}

#[test]
fn queued_prompt_and_slash_cancel_before_dispatch_after_recording_blocks() {
    struct BlockingEnqueue {
        target: String,
        entered: Notify,
        release: Notify,
    }
    #[async_trait]
    impl msgqueue::QueueOperationRecorder for BlockingEnqueue {
        async fn record(&self, operation: msgqueue::QueueOperation) {
            if matches!(operation, msgqueue::QueueOperation::Enqueue { uuid, .. } if uuid == self.target)
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
        }
    }
    for slash in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let provider = Arc::new(PendingPromptStreamClient::new(started_tx));
        let listener = Arc::new(FakeListener::default());
        let handle = new_engine_with_streaming(
            test_config(tmp.path()),
            Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
            listener.clone(),
            Arc::new(RecordingPermissionSink::default()),
            Some(provider.clone()),
        )
        .unwrap();
        handle.runtime().block_on(async {
            let session = handle.active_session_id();
            handle.submit(ClientCommand::SendPrompt { session_id: Some(session.clone()), text: "active".into(), prompt_mode: None, images: vec![], turn_id: Some(300), visualization_context: None }).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), started_rx).await.unwrap().unwrap();
            let target = if slash { format!("mobile-slash:{session}:301") } else { format!("mobile-prompt:{session}:301") };
            let recorder = Arc::new(BlockingEnqueue { target, entered: Notify::new(), release: Notify::new() });
            handle.message_queue.set_recorder(recorder.clone()).await;
            let sender = handle.clone(); let origin = session.clone();
            let submission = tokio::spawn(async move {
                if slash {
                    sender.submit(ClientCommand::RunSlashCommand { session_id: Some(origin), raw: "/context".into(), images: vec![], turn_id: Some(301) }).await
                } else {
                    sender.submit(ClientCommand::SendPrompt { session_id: Some(origin), text: "queued".into(), prompt_mode: None, images: vec![], turn_id: Some(301), visualization_context: None }).await
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), recorder.entered.notified()).await.unwrap();
            assert!(matches!(handle.durable_turns.with_no_pending_admission(&session, 301, || Ok(())), Err(DurableTurnStoreError::PendingAdmission { .. })));
            if !slash {
                use orchestrator::prompt::mid_turn_input::MidTurnInputSource;
                let source = super::super::MobileMsgQueueInput { queue: handle.message_queue.clone(), loop_state: Arc::new(tool_cron::LoopRuntime::default()), durable_turns: handle.durable_turns.clone(), active_session_uuid: handle.inner.active_session_uuid.clone(), active_turn: Some(handle.active_cancel.clone()) };
                assert!(source.take_mid_turn_batch().await.unwrap().is_none());
            }
            let cancelling = handle.clone(); let origin = session.clone();
            let cancel = tokio::spawn(async move { cancelling.submit(ClientCommand::Cancel { session_id: Some(origin), turn_id: Some(301) }).await });
            tokio::time::timeout(std::time::Duration::from_secs(5), async { loop {
                if handle.pending_submissions.lock().unwrap().iter().any(|p| p.turn_id == 301 && p.cancel_requested.load(Ordering::Acquire)) { break; }
                tokio::task::yield_now().await;
            }}).await.unwrap();
            recorder.release.notify_one();
            tokio::time::timeout(std::time::Duration::from_secs(5), submission).await.unwrap().unwrap().unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), cancel).await.unwrap().unwrap().unwrap();
            assert_eq!(handle.durable_turns.queued_turn_status(&session, 301).unwrap(), client::protocol::events::QueuedTurnStatusDto::Cancelled);
            assert!(!handle.active_cancel.lock().await.as_ref().unwrap().cancel.is_cancelled());
            assert_eq!(provider.captured_calls().len(), 1);
            assert!(listener.received.lock().await.iter().any(|event| matches!(event, ClientEvent::TurnRecoveryState { snapshot } if snapshot.session_id == session && snapshot.turn_id == 301 && snapshot.state == TurnRecoveryStateDto::Cancelled)));
            handle.submit(ClientCommand::Cancel { session_id: Some(session), turn_id: Some(300) }).await.unwrap();
        });
    }
}

#[test]
fn cold_claimed_batch_cancel_never_stops_a_different_live_turn() {
    for claimed in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let first = engine(tmp.path(), Arc::new(AttachBarrierListener::default()));
        let session = first.active_session_id();
        first.runtime().block_on(async {
            first
                .inner
                .session_writer
                .append_mobile_empty_session(&session, "claimed batch", &first.session_cwd())
                .await
                .unwrap();
            first
                .inner
                .session_writer
                .append_session_mode(first.inner.session_mode.as_str())
                .await
                .unwrap();
            first
                .ensure_reserved_session_identity(&session)
                .await
                .unwrap();
            for id in [311, 312] {
                first
                    .durable_turns
                    .admit_queued_prompt(
                        &session,
                        Some(id),
                        Some(id),
                        "queued".into(),
                        None,
                        vec![],
                    )
                    .unwrap();
            }
            first
                .durable_turns
                .prepare_mid_turn_batch(&session, &[311, 312])
                .unwrap();
            if claimed {
                first
                    .durable_turns
                    .claim_legacy_batch_for_test(&session, 311)
                    .unwrap();
            }
        });
        drop(first);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let provider = Arc::new(PendingPromptStreamClient::new(started_tx));
        let second = new_engine_with_streaming(
            test_config(tmp.path()),
            Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
            Arc::new(FakeListener::default()),
            Arc::new(RecordingPermissionSink::default()),
            Some(provider.clone()),
        )
        .unwrap();
        second.runtime().block_on(async {
            let mut transition = Some(second.loop_transition.lock().await);
            second
                .resume_session_impl(session.clone(), None, None, None)
                .await
                .unwrap();
            if claimed {
                drop(transition.take());
                second
                    .submit(ClientCommand::SendPrompt {
                        session_id: Some(session.clone()),
                        text: "different turn".into(),
                        prompt_mode: None,
                        images: vec![],
                        turn_id: Some(313),
                        visualization_context: None,
                    })
                    .await
                    .unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
                    .await
                    .unwrap()
                    .unwrap();
            }
            assert!(matches!(
                second
                    .submit(ClientCommand::Cancel {
                        session_id: Some(session.clone()),
                        turn_id: Some(312)
                    })
                    .await,
                Err(ClientError::Rejected { .. })
            ));
            let member = second.durable_turns.load(&session, 312).unwrap();
            assert!(!member.cancelled_before_claim);
            assert_eq!(
                second
                    .durable_turns
                    .queued_turn_status(&session, 312)
                    .unwrap(),
                client::protocol::events::QueuedTurnStatusDto::ClaimedUncertain
            );
            if claimed {
                let owner = second.active_cancel.lock().await.as_ref().unwrap().clone();
                assert_eq!(owner.turn_id, Some(313));
                assert!(!owner.cancel.is_cancelled());
                second
                    .submit(ClientCommand::Cancel {
                        session_id: Some(session.clone()),
                        turn_id: Some(313),
                    })
                    .await
                    .unwrap();
            }
            drop(transition);
        });
    }
}

#[test]
fn paused_batch_owner_can_still_be_explicitly_cancelled() {
    let tmp = tempfile::tempdir().unwrap();
    let handle = engine(tmp.path(), Arc::new(AttachBarrierListener::default()));
    handle.runtime().block_on(async {
        let session = handle.active_session_id();
        for id in [314, 315] {
            handle
                .durable_turns
                .admit_queued_prompt(&session, Some(id), Some(id), "queued".into(), None, vec![])
                .unwrap();
        }
        handle
            .durable_turns
            .prepare_mid_turn_batch(&session, &[314, 315])
            .unwrap();
        handle
            .durable_turns
            .claim_legacy_batch_for_test(&session, 314)
            .unwrap();
        handle
            .durable_turns
            .transition(
                &session,
                314,
                TurnRecoveryStateDto::PausedRecoverable,
                false,
                None,
            )
            .unwrap();
        handle
            .submit(ClientCommand::Cancel {
                session_id: Some(session.clone()),
                turn_id: Some(314),
            })
            .await
            .unwrap();
        assert_eq!(
            handle.durable_turns.load(&session, 314).unwrap().state,
            Some(TurnRecoveryStateDto::Cancelled)
        );
        assert!(
            !handle
                .durable_turns
                .load(&session, 315)
                .unwrap()
                .cancelled_before_claim
        );
    });
}

#[test]
fn stopping_live_midturn_batch_preserves_uncertain_or_delivered_members() {
    use client::protocol::events::QueuedTurnStatusDto;
    use orchestrator::prompt::mid_turn_input::MidTurnInputSource;

    for (acknowledge_first, cancel_write_fails) in [(false, false), (true, false), (false, true)] {
        let tmp = tempfile::tempdir().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let provider = Arc::new(PendingPromptStreamClient::new(started_tx));
        let handle = new_engine_with_streaming(
            test_config(tmp.path()),
            Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
            Arc::new(FakeListener::default()),
            Arc::new(RecordingPermissionSink::default()),
            Some(provider),
        )
        .unwrap();
        let root = handle.lingxi_home.join("mobile-turns");
        let session = handle.runtime().block_on(async {
            let session = handle.active_session_id();
            handle
                .submit(ClientCommand::SendPrompt {
                    session_id: Some(session.clone()),
                    text: "active".into(),
                    prompt_mode: None,
                    images: vec![],
                    turn_id: Some(340),
                    visualization_context: None,
                })
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
                .await
                .unwrap()
                .unwrap();
            for id in [341, 342] {
                let checkpoint = handle
                    .durable_turns
                    .admit_queued_prompt(
                        &session,
                        Some(id),
                        Some(id),
                        format!("queued {id}").into(),
                        None,
                        vec![],
                    )
                    .unwrap();
                super::super::enqueue_mobile_prompt_dispatch(
                    &handle.message_queue,
                    &session,
                    &checkpoint,
                )
                .await;
            }
            let owner = handle.active_cancel.lock().await.as_ref().unwrap().clone();
            let source = super::super::MobileMsgQueueInput {
                queue: handle.message_queue.clone(),
                loop_state: Arc::new(tool_cron::LoopRuntime::default()),
                durable_turns: handle.durable_turns.clone(),
                active_session_uuid: handle.inner.active_session_uuid.clone(),
                active_turn: Some(handle.active_cancel.clone()),
            };
            // Poll Cancel through its initial "not yet a live member" route
            // while the member's delivery gate is held. The producer then
            // claims the member before the inactive route acquires that gate.
            let member_delivery = handle.lock_turn_delivery(&session, 342).await.unwrap();
            let mut cancellation = Box::pin(handle.cancel_active_turn(&session, Some(342)));
            let first_poll = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(cancellation.as_mut(), cx))
            })
            .await;
            assert!(matches!(first_poll, std::task::Poll::Pending));
            let inputs = super::super::MOBILE_TURN_ORIGIN
                .scope(Arc::downgrade(&owner), source.take_mid_turn_batch())
                .await
                .unwrap()
                .unwrap();
            if acknowledge_first {
                super::super::MOBILE_TURN_ORIGIN
                    .scope(Arc::downgrade(&owner), source.input_consumed(&inputs))
                    .await
                    .unwrap();
            }
            if cancel_write_fails {
                handle.durable_turns.fail_after_next_metadata_publish();
            }
            // A member's Stop cancels its actual live parent, not an unrelated
            // turn. An acknowledgement arriving after that decision cannot
            // make the interrupted batch eligible for replay.
            member_delivery.complete();
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), cancellation)
                .await
                .unwrap();
            if cancel_write_fails {
                assert!(result.is_err());
            } else {
                result.unwrap();
            }
            assert!(owner.cancel.is_cancelled());
            if !acknowledge_first {
                assert!(super::super::MOBILE_TURN_ORIGIN
                    .scope(Arc::downgrade(&owner), source.input_consumed(&inputs),)
                    .await
                    .is_err());
            }
            session
        });
        drop(handle);
        let reopened = crate::mobile::turn_durability::DurableTurnStore::new(root);
        assert!(reopened.scan_pending(&session).unwrap().is_empty());
        for id in [341, 342] {
            assert!(!reopened.load(&session, id).unwrap().cancelled_before_claim);
            match reopened.queued_turn_status(&session, id).unwrap() {
                QueuedTurnStatusDto::Delivered {
                    member_turn_ids, ..
                } if acknowledge_first => assert_eq!(member_turn_ids, vec![341, 342]),
                QueuedTurnStatusDto::ClaimedUncertain if !acknowledge_first => {}
                status => panic!("unexpected status after stop: {status:?}"),
            }
        }
    }
}

#[test]
fn stop_between_real_midturn_append_and_ack_reports_cancelled_not_internal() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(GatedTwoTurnStream {
        calls: AtomicUsize::new(0),
        first_started: Arc::new(Notify::new()),
        release_first: Arc::new(Notify::new()),
        second_started: Arc::new(Notify::new()),
    });
    let listener = Arc::new(FakeListener::default());
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(provider.clone()),
    )
    .unwrap();
    let root = handle.lingxi_home.join("mobile-turns");
    let session = handle.runtime().block_on(async {
        let session = handle.active_session_id();
        handle.submit(ClientCommand::SendPrompt { session_id: Some(session.clone()), text: "active".into(), prompt_mode: None, images: vec![], turn_id: Some(350), visualization_context: None }).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), provider.first_started.notified()).await.unwrap();
        handle.submit(ClientCommand::SendPrompt { session_id: Some(session.clone()), text: "queued input".into(), prompt_mode: None, images: vec![], turn_id: Some(351), visualization_context: None }).await.unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        handle.durable_turns.set_midturn_ack_barrier(entered.clone(), release.clone());
        provider.release_first.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified()).await.unwrap();
        assert!(listener.received.lock().await.iter().any(|event| matches!(event, ClientEvent::UserTranscriptRowIdentity { row_token, .. } if row_token == "client-turn:351")));
        let owner = handle.active_cancel.lock().await.as_ref().unwrap().clone();
        let canceller = handle.clone();
        let cancel_session = session.clone();
        let cancelling = tokio::spawn(async move { canceller.submit(ClientCommand::Cancel { session_id: Some(cancel_session), turn_id: Some(351) }).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), owner.cancel.cancelled()).await.unwrap();
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(10), cancelling).await.unwrap().unwrap().unwrap();
        let events = listener.received.lock().await;
        assert!(events.iter().any(|event| matches!(event, ClientEvent::TurnEnded { outcome: client::protocol::events::TurnOutcomeDto::Cancelled, .. })));
        assert!(!events.iter().any(|event| matches!(event, ClientEvent::Error { .. })));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        session
    });
    drop(handle);
    let cold = crate::mobile::turn_durability::DurableTurnStore::new(root);
    assert!(cold.scan_pending(&session).unwrap().is_empty());
    assert_eq!(
        cold.queued_turn_status(&session, 351).unwrap(),
        client::protocol::events::QueuedTurnStatusDto::ClaimedUncertain
    );
    assert_eq!(
        cold.load(&session, 351).unwrap().prompt.string_at(""),
        Some("queued input")
    );
}

#[test]
fn ambiguous_midturn_claim_keeps_the_original_owner_for_stop() {
    use orchestrator::prompt::mid_turn_input::MidTurnInputSource;
    let tmp = tempfile::tempdir().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let provider = Arc::new(PendingPromptStreamClient::new(started_tx));
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
        Some(provider),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let session = handle.active_session_id();
        handle
            .submit(ClientCommand::SendPrompt {
                session_id: Some(session.clone()),
                text: "active".into(),
                prompt_mode: None,
                images: vec![],
                turn_id: Some(360),
                visualization_context: None,
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
            .await
            .unwrap()
            .unwrap();
        let queued = handle
            .durable_turns
            .admit_queued_prompt(
                &session,
                Some(361),
                Some(361),
                "queued".into(),
                None,
                vec![],
            )
            .unwrap();
        super::super::enqueue_mobile_prompt_dispatch(&handle.message_queue, &session, &queued)
            .await;
        let owner = handle.active_cancel.lock().await.as_ref().unwrap().clone();
        let source = super::super::MobileMsgQueueInput {
            queue: handle.message_queue.clone(),
            loop_state: Arc::new(tool_cron::LoopRuntime::default()),
            durable_turns: handle.durable_turns.clone(),
            active_session_uuid: handle.inner.active_session_uuid.clone(),
            active_turn: Some(handle.active_cancel.clone()),
        };
        handle.durable_turns.fail_after_next_metadata_publish();
        assert!(super::super::MOBILE_TURN_ORIGIN
            .scope(Arc::downgrade(&owner), source.take_mid_turn_batch())
            .await
            .is_err());
        assert_eq!(
            handle
                .durable_turns
                .queued_turn_status(&session, 361)
                .unwrap(),
            client::protocol::events::QueuedTurnStatusDto::ClaimedUncertain
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle.submit(ClientCommand::Cancel {
                session_id: Some(session.clone()),
                turn_id: Some(361),
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(owner.cancel.is_cancelled());
        assert!(handle
            .durable_turns
            .scan_pending(&session)
            .unwrap()
            .is_empty());
    });
}

#[test]
fn delivered_midturn_members_query_against_cold_resumed_transcript_uuid() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(GatedTwoTurnStream {
        calls: AtomicUsize::new(0),
        first_started: Arc::new(Notify::new()),
        release_first: Arc::new(Notify::new()),
        second_started: Arc::new(Notify::new()),
    });
    let first_listener = Arc::new(FakeListener::default());
    let first = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        first_listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(provider.clone()),
    )
    .unwrap();
    let (session, row_uuid) = first.runtime().block_on(async {
        let session = first.active_session_id();
        first.submit(ClientCommand::SendPrompt { session_id: Some(session.clone()), text: "active".into(), prompt_mode: None, images: vec![], turn_id: Some(320), visualization_context: None }).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), provider.first_started.notified()).await.unwrap();
        for id in [321, 322] {
            first.submit(ClientCommand::SendPrompt { session_id: Some(session.clone()), text: format!("queued {id}").into(), prompt_mode: None, images: vec![], turn_id: Some(id), visualization_context: None }).await.unwrap();
        }
        provider.release_first.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(10), provider.second_started.notified()).await.unwrap();
        let (batch_owner, members, uuid) = match first.durable_turns.queued_turn_status(&session, 322).unwrap() {
            client::protocol::events::QueuedTurnStatusDto::Delivered { batch_owner_turn_id, member_turn_ids, transcript_uuid } => (batch_owner_turn_id, member_turn_ids, transcript_uuid),
            other => panic!("expected delivered batch, got {other:?}"),
        };
        assert_eq!(batch_owner, 321);
        assert_eq!(members, vec![321, 322]);
        assert!(first_listener.received.lock().await.iter().any(|event| matches!(event, ClientEvent::UserTranscriptRowIdentity { row_token, uuid: emitted } if row_token == "client-turn:321,322" && emitted == &uuid)));
        // The foreign listener has only queued the callback. Discard it with
        // the source before a native reducer applies the identity.
        (session, uuid)
    });
    drop(first);
    drop(first_listener);
    let listener = Arc::new(FakeListener::default());
    let second = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        None,
    )
    .unwrap();
    second.runtime().block_on(async {
        second.submit(ClientCommand::ResumeSession { session_id: session.clone(), cwd: None }).await.unwrap();
        assert!(listener.received.lock().await.iter().any(|event| matches!(event, ClientEvent::SessionResumed { session_id, messages, .. } if session_id == &session && messages.iter().any(|message| message.role == "user" && message.transcript_uuid.as_deref() == Some(row_uuid.as_str()) && !message.blocks.is_empty()))));
        for id in [321, 322] {
            second.submit(ClientCommand::QueryQueuedTurn { session_id: session.clone(), turn_id: id, request_id: format!("query-{id}") }).await.unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let events = listener.received.lock().await;
                let count = events.iter().filter(|event| matches!(event, ClientEvent::QueuedTurnStatus { session_id, turn_id: 321 | 322, status: client::protocol::events::QueuedTurnStatusDto::Delivered { batch_owner_turn_id: 321, member_turn_ids, transcript_uuid }, .. } if session_id == &session && member_turn_ids == &[321, 322] && transcript_uuid == &row_uuid)).count();
                if count == 2 { break; }
                drop(events);
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        assert!(matches!(second.submit(ClientCommand::QueryQueuedTurn { session_id: "wrong-session".into(), turn_id: 321, request_id: "wrong".into() }).await, Err(ClientError::Rejected { .. })));
    });
}
