use super::*;
use client::protocol::commands::PromptModeDto;
use std::time::Duration;

const NO_ADMISSION: &str = "turn_cancelled_before_admission";
const WAIT: Duration = Duration::from_secs(10);

fn send(id: u64, mode: Option<PromptModeDto>) -> ClientCommand {
    ClientCommand::SendPrompt {
        text: "echo cancellation-fixture".into(),
        prompt_mode: mode,
        images: Vec::new(),
        turn_id: Some(id),
        visualization_context: None,
    }
}

fn poll_gate(id: u64) -> Arc<super::super::SubmitPollGate> {
    Arc::new(super::super::SubmitPollGate {
        turn_id: id,
        entered: Notify::new(),
        release: Notify::new(),
    })
}

async fn reached(gate: &super::super::SubmitPollGate) {
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .expect("worker reached the controlled boundary");
}

fn assert_no_admission(result: Result<(), ClientError>) {
    assert!(matches!(result, Err(ClientError::Rejected { message }) if message == NO_ADMISSION));
}

#[test]
fn cancel_admission_late_worker_normal_bash_and_queued() {
    for (id, mode, busy) in [
        (73101, None, false),
        (73102, Some(PromptModeDto::Bash), false),
        (73103, None, true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (handle, listener) = build_submit_handle(temp.path());
        handle.runtime().block_on(async {
            let session = handle.active_session_id();
            let active = if busy {
                Some(handle.reserve_turn(Some(73000)).await.unwrap())
            } else {
                None
            };
            let gate = poll_gate(id);
            *handle.submit_test_gate.lock().unwrap() = Some(gate.clone());
            let sender = handle.clone();
            let task = tokio::spawn(async move { sender.submit(send(id, mode)).await });
            reached(&gate).await;
            tokio::time::timeout(
                WAIT,
                handle.submit(ClientCommand::Cancel { turn_id: Some(id) }),
            )
            .await
            .unwrap()
            .unwrap();
            gate.release.notify_one();
            assert_no_admission(tokio::time::timeout(WAIT, task).await.unwrap().unwrap());
            assert!(matches!(
                handle.durable_turns.load(&session, id),
                Err(crate::mobile::turn_durability::DurableTurnStoreError::NotFound { .. })
            ));
            assert!(!listener.received.lock().await.iter().any(|event|
                matches!(event, Ev::TurnStarted { turn_id: Some(started), .. } if *started == id)));
            if let Some(active) = active {
                assert!(!active.cancel.is_cancelled());
                handle.release_unspawned_turn(&active).await;
            }
        });
    }
}

#[test]
fn cancel_admission_reserved_prompt_has_real_cancelled_terminal() {
    let temp = tempfile::tempdir().unwrap();
    let (handle, listener) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let id = 73104;
        let session = handle.active_session_id();
        let gate = poll_gate(id);
        *handle.reserve_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let send_task = tokio::spawn(async move { sender.submit(send(id, None)).await });
        reached(&gate).await;
        let owner = handle
            .active_cancel
            .lock()
            .await
            .clone()
            .expect("reserved owner");
        let canceller = handle.clone();
        let cancel_task = tokio::spawn(async move {
            canceller
                .submit(ClientCommand::Cancel { turn_id: Some(id) })
                .await
        });
        tokio::time::timeout(WAIT, async {
            while !owner.cancel.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancel reaches reserved owner");
        gate.release.notify_one();
        tokio::time::timeout(WAIT, send_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(WAIT, cancel_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let checkpoint = handle.durable_turns.load(&session, id).unwrap();
        assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::Cancelled));
        assert_eq!(
            checkpoint.prompt.string_at(""),
            Some("echo cancellation-fixture")
        );
        assert!(listener
            .received
            .lock()
            .await
            .iter()
            .any(|event| matches!(event,
            Ev::TurnRecoveryState { snapshot }
                if snapshot.session_id == session && snapshot.turn_id == id
                    && snapshot.state == TurnRecoveryStateDto::Cancelled)));
        assert!(!listener.received.lock().await.iter().any(|event|
            matches!(event, Ev::TurnStarted { turn_id: Some(started), .. } if *started == id)));
        assert!(handle.active_cancel.lock().await.is_none());
    });
}

#[test]
fn cancel_admission_terminal_precedes_delayed_initial_events() {
    let temp = tempfile::tempdir().unwrap();
    let (handle, listener) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let id = 73112;
        let session = handle.active_session_id();
        let gate = poll_gate(id);
        *handle.initial_event_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let send_task = tokio::spawn(async move { sender.submit(send(id, None)).await });
        reached(&gate).await;
        assert_eq!(
            handle.durable_turns.load(&session, id).unwrap().state,
            Some(TurnRecoveryStateDto::Running)
        );
        let owner = handle.active_cancel.lock().await.clone().unwrap();
        let canceller = handle.clone();
        let cancel_task = tokio::spawn(async move {
            canceller
                .submit(ClientCommand::Cancel { turn_id: Some(id) })
                .await
        });
        tokio::time::timeout(WAIT, async {
            loop {
                if listener.received.lock().await.iter().any(|event| {
                    matches!(event,
                    Ev::TurnRecoveryState { snapshot } if snapshot.session_id == session
                        && snapshot.turn_id == id
                        && snapshot.state == TurnRecoveryStateDto::Cancelled)
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancel publishes the terminal while send is held");
        assert!(owner.cancel.is_cancelled());
        gate.release.notify_one();
        tokio::time::timeout(WAIT, send_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(WAIT, cancel_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let events = listener.received.lock().await;
        assert!(!events.iter().any(|event| matches!(event,
            Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == id
                && snapshot.state == TurnRecoveryStateDto::Running)));
        assert!(!events.iter().any(|event| matches!(event,
            Ev::TurnStarted { turn_id: Some(started), .. } if *started == id)));
        assert_eq!(
            handle.durable_turns.load(&session, id).unwrap().state,
            Some(TurnRecoveryStateDto::Cancelled)
        );
        assert!(handle.active_cancel.lock().await.is_none());
    });
}

#[test]
fn cancel_admission_claimed_bash_never_starts_after_cancellation() {
    let temp = tempfile::tempdir().unwrap();
    let (handle, listener) = build_submit_handle(temp.path());
    let sentinel = temp.path().join("must-not-run");
    handle.runtime().block_on(async {
        let id = 73113;
        let session = handle.active_session_id();
        let gate = poll_gate(id);
        *handle.initial_event_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let command = format!("touch {}", sentinel.display());
        let send_task = tokio::spawn(async move {
            sender
                .submit(ClientCommand::SendPrompt {
                    text: command.into(),
                    prompt_mode: Some(PromptModeDto::Bash),
                    images: Vec::new(),
                    turn_id: Some(id),
                    visualization_context: None,
                })
                .await
        });
        reached(&gate).await;
        let owner = handle.active_cancel.lock().await.clone().unwrap();
        let canceller = handle.clone();
        let cancel_task = tokio::spawn(async move {
            canceller
                .submit(ClientCommand::Cancel { turn_id: Some(id) })
                .await
        });
        tokio::time::timeout(WAIT, async {
            while !owner.cancel.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.release.notify_one();
        tokio::time::timeout(WAIT, send_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(WAIT, cancel_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!sentinel.exists(), "cancelled Bash must not execute");
        let checkpoint = handle.durable_turns.load(&session, id).unwrap();
        assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::Cancelled));
        let events = listener.received.lock().await;
        let terminal = events
            .iter()
            .position(|event| {
                matches!(event,
            Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == id
                && snapshot.state == TurnRecoveryStateDto::Cancelled)
            })
            .unwrap();
        assert!(!events[terminal + 1..].iter().any(|event| matches!(event,
            Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == id
                && snapshot.state == TurnRecoveryStateDto::Running)));
        assert!(!events.iter().any(|event| matches!(event,
            Ev::TurnStarted { turn_id: Some(started), .. } if *started == id)));
    });
}

#[test]
fn cancel_admission_bash_already_queued_gets_terminal_not_no_admission() {
    let temp = tempfile::tempdir().unwrap();
    let (handle, listener) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let id = 73105;
        let session = handle.active_session_id();
        let gate = poll_gate(id);
        *handle.queue_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let send_task =
            tokio::spawn(async move { sender.submit(send(id, Some(PromptModeDto::Bash))).await });
        reached(&gate).await;
        assert!(handle.durable_turns.load(&session, id).is_ok());
        handle
            .submit(ClientCommand::Cancel { turn_id: Some(id) })
            .await
            .unwrap();
        gate.release.notify_one();
        tokio::time::timeout(WAIT, send_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let checkpoint = handle.durable_turns.load(&session, id).unwrap();
        assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::Cancelled));
        assert!(listener
            .received
            .lock()
            .await
            .iter()
            .any(|event| matches!(event,
            Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == id
                && snapshot.state == TurnRecoveryStateDto::Cancelled)));
        assert!(!listener.received.lock().await.iter().any(|event|
            matches!(event, Ev::TurnStarted { turn_id: Some(started), .. } if *started == id)));
    });
}

#[test]
fn cancel_admission_queued_marker_remains_withdrawn() {
    use orchestrator::prompt::mid_turn_input::MidTurnInputSource;
    let temp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let id = 73106;
        let session = handle.active_session_id();
        let owner = handle.reserve_turn(Some(73000)).await.unwrap();
        let gate = poll_gate(id);
        *handle.queue_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let send_task = tokio::spawn(async move { sender.submit(send(id, None)).await });
        reached(&gate).await;
        handle
            .submit(ClientCommand::Cancel { turn_id: Some(id) })
            .await
            .unwrap();
        gate.release.notify_one();
        tokio::time::timeout(WAIT, send_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let source = MobileMsgQueueInput {
            queue: handle.message_queue.clone(),
            loop_state: Arc::new(tool_cron::LoopRuntime::default()),
            durable_turns: handle.durable_turns.clone(),
            active_session_uuid: handle.inner.active_session_uuid.clone(),
        };
        assert!(source.take_mid_turn_batch().await.unwrap().is_none());
        assert_eq!(
            handle
                .durable_turns
                .load(&session, id)
                .unwrap()
                .queue_delivery,
            QueueDeliveryState::Cancelled
        );
        assert!(!owner.cancel.is_cancelled());
        handle.release_unspawned_turn(&owner).await;
    });
}

#[test]
fn cancel_admission_permission_wait_does_not_take_transition() {
    use permission::gate::PermissionGate;
    let temp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let id = 73107;
        let _transition = handle.loop_transition.lock().await;
        let owner = handle.reserve_turn(Some(id)).await.unwrap();
        let gate = handle.permission_gate();
        let task_gate = gate.clone();
        let engine = handle.clone();
        let task_owner = owner.clone();
        let task = tokio::spawn(async move {
            let _ = task_gate
                .check("Bash", &source_value(&serde_json::json!({"command":"ls"})))
                .await;
            engine.release_unspawned_turn(&task_owner).await;
        });
        owner.set_task_handle(task);
        tokio::time::timeout(WAIT, async {
            while gate.pending_count().await != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(
            WAIT,
            handle.submit(ClientCommand::Cancel { turn_id: Some(id) }),
        )
        .await
        .expect("cancel does not wait for the transition")
        .unwrap();
        assert_eq!(gate.pending_count().await, 0);
    });
}

#[test]
fn cancel_admission_same_id_is_isolated_by_actual_new_session() {
    let temp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let id = 73108;
        let before = handle.active_session_id();
        handle
            .submit(ClientCommand::Cancel { turn_id: Some(id) })
            .await
            .unwrap();
        assert_no_admission(handle.submit(send(id, None)).await);
        handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: None,
                mode: SessionModeDto::Code,
            })
            .await
            .unwrap();
        assert_ne!(handle.active_session_id(), before);
        let owner = handle.reserve_turn(Some(id)).await.unwrap();
        handle.release_unspawned_turn(&owner).await;
    });
}

#[test]
fn cancel_admission_normal_slash_late_and_reserved() {
    let temp = tempfile::tempdir().unwrap();
    let (handle, listener) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let late_id = 73109;
        let gate = poll_gate(late_id);
        *handle.submit_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let task = tokio::spawn(async move {
            sender
                .submit(ClientCommand::RunSlashCommand {
                    raw: "/context".into(),
                    images: Vec::new(),
                    turn_id: Some(late_id),
                })
                .await
        });
        reached(&gate).await;
        handle
            .submit(ClientCommand::Cancel {
                turn_id: Some(late_id),
            })
            .await
            .unwrap();
        gate.release.notify_one();
        assert_no_admission(tokio::time::timeout(WAIT, task).await.unwrap().unwrap());
        *handle.submit_test_gate.lock().unwrap() = None;

        let reserved_id = 73110;
        let gate = poll_gate(reserved_id);
        *handle.reserve_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let task = tokio::spawn(async move {
            sender
                .submit(ClientCommand::RunSlashCommand {
                    raw: "/context".into(),
                    images: Vec::new(),
                    turn_id: Some(reserved_id),
                })
                .await
        });
        reached(&gate).await;
        let owner = handle.active_cancel.lock().await.clone().unwrap();
        let canceller = handle.clone();
        let cancel = tokio::spawn(async move {
            canceller
                .submit(ClientCommand::Cancel {
                    turn_id: Some(reserved_id),
                })
                .await
        });
        tokio::time::timeout(WAIT, async {
            while !owner.cancel.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.release.notify_one();
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(WAIT, cancel)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(listener
            .received
            .lock()
            .await
            .iter()
            .any(|event| matches!(event,
            Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == reserved_id
                && snapshot.state == TurnRecoveryStateDto::Cancelled)));
    });
}

#[test]
fn cancel_admission_initial_slash_admit_and_claim_settle_before_dispatch() {
    for (id, pause_after_claim) in [(73114, false), (73115, true)] {
        let temp = tempfile::tempdir().unwrap();
        let (handle, listener) = build_submit_handle(temp.path());
        handle.runtime().block_on(async {
            let session = handle.active_session_id();
            let gate = poll_gate(id);
            if pause_after_claim {
                *handle.initial_event_test_gate.lock().unwrap() = Some(gate.clone());
            } else {
                *handle.queue_test_gate.lock().unwrap() = Some(gate.clone());
            }
            let sender = handle.clone();
            let task = tokio::spawn(async move {
                sender.submit(ClientCommand::RunSlashCommand {
                    raw: "/context".into(), images: Vec::new(), turn_id: Some(id),
                }).await
            });
            reached(&gate).await;
            let checkpoint = handle.durable_turns.load(&session, id).unwrap();
            assert_eq!(checkpoint.state, None);
            assert_eq!(checkpoint.queue_delivery, if pause_after_claim {
                QueueDeliveryState::SlashClaimed
            } else {
                QueueDeliveryState::Pending
            });
            let owner = handle.active_cancel.lock().await.clone().unwrap();
            let canceller = handle.clone();
            let cancel = tokio::spawn(async move {
                canceller.submit(ClientCommand::Cancel { turn_id: Some(id) }).await
            });
            tokio::time::timeout(WAIT, async {
                while !owner.cancel.is_cancelled() { tokio::task::yield_now().await; }
            }).await.unwrap();
            gate.release.notify_one();
            tokio::time::timeout(WAIT, task).await.unwrap().unwrap().unwrap();
            tokio::time::timeout(WAIT, cancel).await.unwrap().unwrap().unwrap();
            assert_eq!(
                handle.durable_turns.load(&session, id).unwrap().state,
                Some(TurnRecoveryStateDto::Cancelled)
            );
            let events = listener.received.lock().await;
            assert!(events.iter().any(|event| matches!(event,
                Ev::TurnRecoveryState { snapshot } if snapshot.session_id == session
                    && snapshot.turn_id == id && snapshot.state == TurnRecoveryStateDto::Cancelled)));
            assert!(!events.iter().any(|event| matches!(event,
                Ev::SlashCommandResult { turn_id: Some(result_id), .. } if *result_id == id)));
        });
    }
}

#[test]
fn cancel_admission_skill_slash_cancel_before_running_publication_skips_model() {
    use crate::mobile::test_support::{new_engine_with_streaming, test_config};
    let temp = tempfile::tempdir().unwrap();
    write_skill(temp.path(), "cancel-fixture", "test skill", "body");
    let (entered, _receiver) = tokio::sync::oneshot::channel();
    let streaming = Arc::new(PendingPromptStreamClient::new(entered));
    let listener = Arc::new(FakeListener::default());
    let handle = new_engine_with_streaming(
        test_config(temp.path()),
        Arc::new(HostFakePlatform::new(temp.path().into())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(streaming.clone()),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let id = 73116;
        let session = handle.active_session_id();
        let gate = poll_gate(id);
        *handle.slash_running_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let task = tokio::spawn(async move {
            sender
                .submit(ClientCommand::RunSlashCommand {
                    raw: "/cancel-fixture".into(),
                    images: Vec::new(),
                    turn_id: Some(id),
                })
                .await
        });
        reached(&gate).await;
        let owner = handle.active_cancel.lock().await.clone().unwrap();
        let canceller = handle.clone();
        let cancel = tokio::spawn(async move {
            canceller
                .submit(ClientCommand::Cancel { turn_id: Some(id) })
                .await
        });
        tokio::time::timeout(WAIT, async {
            while !owner.cancel.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.release.notify_one();
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(WAIT, cancel)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(streaming.captured_calls().is_empty());
        assert_eq!(
            handle.durable_turns.load(&session, id).unwrap().state,
            Some(TurnRecoveryStateDto::Cancelled)
        );
        let events = listener.received.lock().await;
        assert!(events.iter().any(|event| matches!(event,
            Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == id
                && snapshot.state == TurnRecoveryStateDto::Cancelled)));
        assert!(!events.iter().any(|event| matches!(event,
            Ev::TurnStarted { turn_id: Some(started), .. } if *started == id)));
    });
}

#[test]
fn cancel_admission_skill_running_pause_stays_recoverable() {
    use crate::mobile::test_support::{new_engine_with_streaming, test_config};
    let temp = tempfile::tempdir().unwrap();
    write_skill(temp.path(), "pause-fixture", "test skill", "body");
    let (entered, _receiver) = tokio::sync::oneshot::channel();
    let streaming = Arc::new(PendingPromptStreamClient::new(entered));
    let listener = Arc::new(FakeListener::default());
    let handle = new_engine_with_streaming(
        test_config(temp.path()),
        Arc::new(HostFakePlatform::new(temp.path().into())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(streaming.clone()),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let id = 73117;
        let session = handle.active_session_id();
        let gate = poll_gate(id);
        *handle.slash_running_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let task = tokio::spawn(async move {
            sender
                .submit(ClientCommand::RunSlashCommand {
                    raw: "/pause-fixture".into(),
                    images: Vec::new(),
                    turn_id: Some(id),
                })
                .await
        });
        reached(&gate).await;
        let owner = handle.active_cancel.lock().await.clone().unwrap();
        let pauser = handle.clone();
        let pause = tokio::spawn(async move {
            pauser
                .submit(ClientCommand::PauseTurn {
                    turn_id: id,
                    reason: "background".into(),
                })
                .await
        });
        tokio::time::timeout(WAIT, async {
            while !owner.is_quiescing() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.release.notify_one();
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(WAIT, pause)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(streaming.captured_calls().is_empty());
        assert_eq!(
            handle.durable_turns.load(&session, id).unwrap().state,
            Some(TurnRecoveryStateDto::PausedRecoverable)
        );
        let events = listener.received.lock().await;
        assert!(events.iter().any(|event| matches!(event,
            Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == id
                && snapshot.state == TurnRecoveryStateDto::PausedRecoverable)));
        assert!(!events.iter().any(|event| matches!(event,
            Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == id
                && snapshot.state == TurnRecoveryStateDto::Cancelled)));
    });
}

#[test]
fn cancel_admission_claimed_slash_pause_does_not_forge_stop() {
    let temp = tempfile::tempdir().unwrap();
    let (handle, listener) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let id = 73118;
        let session = handle.active_session_id();
        let gate = poll_gate(id);
        *handle.initial_event_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let task = tokio::spawn(async move {
            sender
                .submit(ClientCommand::RunSlashCommand {
                    raw: "/context".into(),
                    images: Vec::new(),
                    turn_id: Some(id),
                })
                .await
        });
        reached(&gate).await;
        let owner = handle.active_cancel.lock().await.clone().unwrap();
        let pauser = handle.clone();
        let pause = tokio::spawn(async move {
            pauser
                .submit(ClientCommand::PauseTurn {
                    turn_id: id,
                    reason: "background".into(),
                })
                .await
        });
        tokio::time::timeout(WAIT, async {
            while !owner.is_quiescing() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.release.notify_one();
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(tokio::time::timeout(WAIT, pause)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        let checkpoint = handle.durable_turns.load(&session, id).unwrap();
        assert_eq!(checkpoint.state, None);
        assert_eq!(checkpoint.queue_delivery, QueueDeliveryState::SlashClaimed);
        assert!(!listener
            .received
            .lock()
            .await
            .iter()
            .any(|event| matches!(event,
            Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == id
                && snapshot.state == TurnRecoveryStateDto::Cancelled)));
    });
}

#[test]
fn cancel_admission_queued_slash_pause_does_not_forge_stop() {
    let temp = tempfile::tempdir().unwrap();
    let (handle, listener) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let id = 73121;
        let session = handle.active_session_id();
        handle
            .durable_turns
            .admit_queued_slash(
                &session,
                Some(id),
                Some(id),
                source_text("/context"),
                Vec::new(),
            )
            .unwrap();
        let gate = poll_gate(id);
        *handle.initial_event_test_gate.lock().unwrap() = Some(gate.clone());
        let engine = handle.clone();
        let source_session = session.clone();
        let task =
            tokio::spawn(async move { engine.run_queued_mobile_slash(&source_session, id).await });
        reached(&gate).await;
        let owner = handle.active_cancel.lock().await.clone().unwrap();
        let pauser = handle.clone();
        let pause = tokio::spawn(async move {
            pauser
                .submit(ClientCommand::PauseTurn {
                    turn_id: id,
                    reason: "background".into(),
                })
                .await
        });
        tokio::time::timeout(WAIT, async {
            while !owner.is_quiescing() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.release.notify_one();
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(tokio::time::timeout(WAIT, pause)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        let checkpoint = handle.durable_turns.load(&session, id).unwrap();
        assert_eq!(checkpoint.state, None);
        assert_eq!(checkpoint.queue_delivery, QueueDeliveryState::SlashClaimed);
        assert!(!listener
            .received
            .lock()
            .await
            .iter()
            .any(|event| matches!(event,
            Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == id
                && snapshot.state == TurnRecoveryStateDto::Cancelled)));
    });
}

#[test]
fn cancel_admission_claimed_slash_keeps_same_owner_cron_busy() {
    let temp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let id = 73120;
        let session = handle.active_session_id();
        handle.inner.session_writer
            .append_mobile_empty_session(&session, "Selected", &handle.session_cwd())
            .await.unwrap();
        let path = handle.inner.session_writer.active_path();
        let task: cron::CronTask = serde_json::from_value(serde_json::json!({
            "id":"d12345678", "cron":"0 9 * * *", "prompt":"brief", "createdAt":1, "recurring":false,
            "automation":{"version":2,"status":"active","model":"anthropic/claude-sonnet-4-6",
                "reasoning":{"type":"invalid-test-reasoning"},"runMode":"selected_session",
                "targetSessionId":session,"targetSessionTranscriptPath":path,
                "targetSessionMode":"code","targetSessionScopeKey":"project.test",
                "notificationPolicy":"all"}
        })).unwrap();
        let root = std::path::PathBuf::from(handle.session_cwd());
        let mut document = cron::ScheduledTasks::default();
        document.tasks.push(task);
        cron::tasks_file::write_automation_tasks_body(
            handle.fs.as_ref(), &root, &cron::serialize_tasks(&document),
        ).await.unwrap();
        let request = cron::claim_automation_run_now(
            handle.fs.as_ref(), &root, "d12345678", std::time::SystemTime::now(),
        ).await.unwrap();
        let gate = poll_gate(id);
        *handle.initial_event_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let slash = tokio::spawn(async move {
            sender.submit(ClientCommand::RunSlashCommand {
                raw: "/context".into(), images: Vec::new(), turn_id: Some(id),
            }).await
        });
        reached(&gate).await;
        let firer = super::super::MobileTurnFirer {
            cfg: handle.firer_cfg.clone(), platform: handle.firer_platform.clone(),
        };
        let error = firer.fire_session_with_admission(
            "scheduled".into(), request.task.automation.as_ref(), Some(&request),
            Some(super::super::automation::allow_cron_owner()),
        ).await.unwrap_err();
        assert_eq!(error, "busy: Conversation has an active turn");
        gate.release.notify_one();
        tokio::time::timeout(WAIT, slash).await.unwrap().unwrap().unwrap();
    });
}

#[test]
fn cancel_admission_explicit_stop_wins_over_quiescing_pause() {
    use crate::mobile::test_support::{new_engine_with_streaming, test_config};
    let temp = tempfile::tempdir().unwrap();
    write_skill(temp.path(), "pause-stop-fixture", "test skill", "body");
    let (entered, _receiver) = tokio::sync::oneshot::channel();
    let streaming = Arc::new(PendingPromptStreamClient::new(entered));
    let listener = Arc::new(FakeListener::default());
    let handle = new_engine_with_streaming(
        test_config(temp.path()),
        Arc::new(HostFakePlatform::new(temp.path().into())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(streaming),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let id = 73119;
        let session = handle.active_session_id();
        let gate = poll_gate(id);
        *handle.slash_running_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let task = tokio::spawn(async move {
            sender
                .submit(ClientCommand::RunSlashCommand {
                    raw: "/pause-stop-fixture".into(),
                    images: Vec::new(),
                    turn_id: Some(id),
                })
                .await
        });
        reached(&gate).await;
        let owner = handle.active_cancel.lock().await.clone().unwrap();
        let pauser = handle.clone();
        let pause = tokio::spawn(async move {
            pauser
                .submit(ClientCommand::PauseTurn {
                    turn_id: id,
                    reason: "background".into(),
                })
                .await
        });
        tokio::time::timeout(WAIT, async {
            while !owner.is_quiescing() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let canceller = handle.clone();
        let cancel = tokio::spawn(async move {
            canceller
                .submit(ClientCommand::Cancel { turn_id: Some(id) })
                .await
        });
        tokio::time::timeout(WAIT, async {
            while !owner.explicit_cancel.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.release.notify_one();
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(WAIT, cancel)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let _ = tokio::time::timeout(WAIT, pause).await.unwrap().unwrap();
        assert_eq!(
            handle.durable_turns.load(&session, id).unwrap().state,
            Some(TurnRecoveryStateDto::Cancelled)
        );
        assert!(listener
            .received
            .lock()
            .await
            .iter()
            .any(|event| matches!(event,
            Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == id
                && snapshot.state == TurnRecoveryStateDto::Cancelled)));
    });
}

#[test]
fn cancel_admission_side_chat_is_owner_and_id_scoped() {
    use crate::mobile::test_support::CollectingPermissionSink;
    let temp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let listener = Arc::new(FakeListener::default());
        let permissions = Arc::new(CollectingPermissionSink::default());
        let side_a = handle
            .create_side_chat("side-a".into(), None, listener.clone(), permissions.clone())
            .await
            .unwrap()
            .side_id;
        let side_b = handle
            .create_side_chat("side-b".into(), None, listener, permissions)
            .await
            .unwrap()
            .side_id;
        let id = 73111;
        let gate = poll_gate(id);
        *handle.submit_test_gate.lock().unwrap() = Some(gate.clone());
        let sender = handle.clone();
        let task_side = side_a.clone();
        let task =
            tokio::spawn(async move { sender.submit_side_chat(task_side, send(id, None)).await });
        reached(&gate).await;
        handle
            .submit_side_chat(side_a.clone(), ClientCommand::Cancel { turn_id: Some(id) })
            .await
            .unwrap();
        gate.release.notify_one();
        assert_no_admission(tokio::time::timeout(WAIT, task).await.unwrap().unwrap());
        *handle.submit_test_gate.lock().unwrap() = None;
        handle
            .submit_side_chat(side_a.clone(), send(id + 1, None))
            .await
            .unwrap();
        handle
            .submit_side_chat(
                side_a.clone(),
                ClientCommand::Cancel {
                    turn_id: Some(id + 1),
                },
            )
            .await
            .unwrap();
        handle
            .submit_side_chat(side_b.clone(), send(id, None))
            .await
            .unwrap();
        handle
            .submit_side_chat(side_b.clone(), ClientCommand::Cancel { turn_id: Some(id) })
            .await
            .unwrap();
        handle.end_side_chat(side_a).await.unwrap();
        handle.end_side_chat(side_b).await.unwrap();
    });
}
