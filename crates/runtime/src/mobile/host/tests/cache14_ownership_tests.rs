//! Admission truth and startup ownership through the public mobile boundary.
//! Mobile SendPrompt always uses the streaming orchestrator entry; the shared
//! store/slot tests below intentionally make no second-driver claim.
use super::*;
use crate::mobile::test_support::new_engine_with_streaming;
use crate::mobile::turn_durability::DurableTurnStore;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(10);

fn prompt(id: u64, text: &str) -> ClientCommand {
    ClientCommand::SendPrompt {
        text: text.into(),
        prompt_mode: None,
        images: Vec::new(),
        turn_id: Some(id),
        visualization_context: None,
    }
}

fn gate(id: u64) -> Arc<super::super::SubmitPollGate> {
    Arc::new(super::super::SubmitPollGate {
        turn_id: id,
        entered: Notify::new(),
        release: Notify::new(),
    })
}

async fn wait_calls(client: &PendingPromptStreamClient, count: usize) {
    tokio::time::timeout(WAIT, async {
        while client.captured_calls().len() < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the public turn reached its physical streaming request");
    assert_eq!(client.captured_calls().len(), count);
}

fn pending_handle(
    root: &Path,
    listener: Arc<dyn ClientEventListener>,
) -> (Arc<MobileEngineHandle>, Arc<PendingPromptStreamClient>) {
    let (entered, _) = tokio::sync::oneshot::channel();
    let client = Arc::new(PendingPromptStreamClient::new(entered));
    let handle = new_engine_with_streaming(
        test_config(root),
        Arc::new(HostFakePlatform::new(root.to_path_buf())),
        listener,
        Arc::new(RecordingPermissionSink::default()),
        Some(client.clone()),
    )
    .unwrap();
    (handle, client)
}

async fn assert_admission_matrix(handle: &MobileEngineHandle, session: &str) {
    let facts = handle
        .query_turn_admissions(session.into(), vec![10, 11, 12, 13, 14, 15, 16, 17, 18, 19])
        .await
        .unwrap();
    assert_eq!(
        facts.iter().map(|fact| fact.turn_id).collect::<Vec<_>>(),
        [10, 11, 12, 13, 14, 15, 16, 17, 18, 19]
    );
    assert!(!facts[0].admitted && !facts[0].queued && !facts[0].settled);
    assert!(facts[0].recovery.is_none());
    assert!(facts[1].admitted && facts[1].queued && !facts[1].settled);
    assert!(facts[1].recovery.is_none());
    for fact in &facts[2..4] {
        assert!(fact.admitted && !fact.queued && !fact.settled);
        assert_eq!(
            fact.recovery.as_ref().unwrap().state,
            TurnRecoveryStateDto::Running
        );
    }
    for fact in &facts[4..7] {
        assert!(fact.admitted && !fact.queued && fact.settled);
        assert_eq!(
            fact.recovery.as_ref().unwrap().state,
            TurnRecoveryStateDto::Completed
        );
    }
    assert_eq!(
        facts[6].recovery.as_ref().unwrap().turn_id,
        15,
        "a settled coalesced member reports its actual terminal owner"
    );
    assert!(facts[7].admitted && facts[7].queued && !facts[7].settled);
    for fact in &facts[8..] {
        assert!(fact.admitted && fact.settled && !fact.queued);
        assert!(
            fact.recovery.is_none(),
            "append-ACKed input needs no independent executor snapshot"
        );
    }
    assert!(
        facts.iter().all(|fact| !fact.executor_active),
        "persisted Running is not proof of a live executor"
    );
    assert!(matches!(
        handle
            .query_turn_admissions("different-session".into(), vec![11])
            .await,
        Err(ClientError::Rejected { .. })
    ));
}

#[test]
fn public_admission_query_recovers_uncertain_writes_and_actual_coalesced_owner_warm_cold() {
    let temp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(temp.path());
    let session = handle.runtime().block_on(async {
        handle.ensure_mobile_session_anchor().await.unwrap();
        let session = handle.active_session_id();
        let store = &handle.durable_turns;
        store
            .admit_queued_prompt(
                &session,
                Some(11),
                Some(11),
                "pending".into(),
                None,
                Vec::new(),
            )
            .unwrap();
        store
            .begin(&session, 12, "stale running".into(), None, Vec::new())
            .unwrap();
        store
            .admit_queued_prompt(
                &session,
                Some(13),
                Some(13),
                "claimed".into(),
                None,
                Vec::new(),
            )
            .unwrap();
        store.claim_pending(&session, 13).unwrap();
        store
            .begin(&session, 14, "terminal".into(), None, Vec::new())
            .unwrap();
        store
            .transition(&session, 14, TurnRecoveryStateDto::Completed, false, None)
            .unwrap();
        for id in [15, 16] {
            store
                .admit_queued_prompt(
                    &session,
                    Some(id),
                    Some(id),
                    format!("member {id}").into(),
                    None,
                    Vec::new(),
                )
                .unwrap();
        }
        store.prepare_mid_turn_batch(&session, &[15, 16]).unwrap();
        store.claim_pending(&session, 15).unwrap();
        let owner = handle.reserve_turn(Some(15)).await.unwrap();
        let live = handle
            .query_turn_admissions(session.clone(), vec![12, 15, 16, 11, 10])
            .await
            .unwrap();
        assert!(!live[0].executor_active);
        assert!(live[1].executor_active && live[2].executor_active);
        assert_eq!(live[2].recovery.as_ref().unwrap().turn_id, 15);
        assert!(!live[3].executor_active && !live[4].executor_active);
        handle.release_unspawned_turn(&owner).await;
        store
            .transition(&session, 15, TurnRecoveryStateDto::Completed, false, None)
            .unwrap();
        for id in [18, 19] {
            store
                .admit_queued_prompt(
                    &session,
                    Some(id),
                    Some(id),
                    format!("delivered member {id}").into(),
                    None,
                    Vec::new(),
                )
                .unwrap();
        }
        store.prepare_mid_turn_batch(&session, &[18, 19]).unwrap();
        store
            .acknowledge_mid_turn_batch(&session, 18, &[18, 19])
            .unwrap();
        store.fail_next_metadata_after_publish_for_test();
        assert!(store
            .admit_queued_prompt(
                &session,
                Some(17),
                Some(17),
                "uncertain published admission".into(),
                None,
                Vec::new()
            )
            .is_err());
        assert_admission_matrix(&handle, &session).await;
        session
    });
    drop(handle);
    let (cold, _) = build_submit_handle(temp.path());
    cold.runtime().block_on(async {
        cold.resume_seeded_test_session(session.clone())
            .await
            .unwrap();
        assert_admission_matrix(&cold, &session).await;
    });
}

#[test]
fn public_pause_keeps_busy_prompt_queued_until_explicit_cancel_warm_cold() {
    for cold in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let listener = Arc::new(FakeListener::default());
        let (first, first_client) = pending_handle(temp.path(), listener.clone());
        let session = first.runtime().block_on(async {
            first.submit(prompt(81001, "CACHE14_ACTIVE_A")).await.unwrap();
            wait_calls(&first_client, 1).await;
            first.submit(prompt(81002, "CACHE14_QUEUED_B")).await.unwrap();
            let session = first.active_session_id();
            let facts = first.query_turn_admissions(session.clone(), vec![81001, 81002]).await.unwrap();
            assert!(facts[0].executor_active && facts[1].queued && !facts[1].executor_active);
            first.submit(ClientCommand::PauseTurn { turn_id: 81001, reason: "background_time_expired".into() }).await.unwrap();
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert_eq!(first_client.captured_calls().len(), 1, "paused A must block the queued watcher");
            assert!(first.active_cancel.lock().await.is_none());
            let facts = first.query_turn_admissions(session.clone(), vec![81001, 81002]).await.unwrap();
            assert_eq!(facts[0].recovery.as_ref().unwrap().state, TurnRecoveryStateDto::PausedRecoverable);
            assert!(!facts[0].executor_active && facts[1].queued && !facts[1].executor_active);
            assert!(!listener.received.lock().await.iter().any(|event| matches!(event,
                Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == 81002 && snapshot.state == TurnRecoveryStateDto::Running)));
            session
        });
        let (handle, client, previous_calls) = if cold {
            drop(first);
            let (resumed, client) = pending_handle(temp.path(), Arc::new(FakeListener::default()));
            resumed.runtime().block_on(async {
                resumed
                    .resume_seeded_test_session(session.clone())
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(250)).await;
                assert!(
                    client.captured_calls().is_empty(),
                    "cold paused recovery also blocks B"
                );
                let facts = resumed
                    .query_turn_admissions(session.clone(), vec![81001, 81002])
                    .await
                    .unwrap();
                assert!(!facts[0].executor_active && facts[1].queued && !facts[1].executor_active);
            });
            (resumed, client, 0)
        } else {
            (first, first_client, 1)
        };
        handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::Cancel {
                    turn_id: Some(81001),
                })
                .await
                .unwrap();
            wait_calls(&client, previous_calls + 1).await;
            let calls = client.captured_calls();
            assert_eq!(
                calls
                    .last()
                    .unwrap()
                    .messages
                    .iter()
                    .filter(|row| row.text_content() == "CACHE14_QUEUED_B")
                    .count(),
                1,
                "one admitted B becomes one prompt row"
            );
            let facts = handle
                .query_turn_admissions(session.clone(), vec![81001, 81002])
                .await
                .unwrap();
            assert!(facts[0].settled && !facts[0].executor_active);
            assert!(facts[1].executor_active && !facts[1].queued);
            handle
                .submit(ClientCommand::Cancel {
                    turn_id: Some(81002),
                })
                .await
                .unwrap();
            assert!(handle.active_cancel.lock().await.is_none());
        });
    }
}

#[test]
fn late_public_prompt_bash_and_slash_workers_obey_pause_barrier_and_explicit_resume_warm_cold() {
    use client::protocol::commands::PromptModeDto;

    for operation in ["prompt", "bash", "slash"] {
        for cold in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let listener = Arc::new(FakeListener::default());
            let (first, first_client) = pending_handle(temp.path(), listener.clone());
            let session = first.runtime().block_on(async {
                first.submit(prompt(86001, "CACHE14_LATE_ACTIVE_A")).await.unwrap();
                wait_calls(&first_client, 1).await;
                let delayed = gate(86002);
                *first.submit_test_gate.lock().unwrap() = Some(delayed.clone());
                let command = match operation {
                    "bash" => ClientCommand::SendPrompt {
                        text: "printf CACHE14_LATE_BASH_B".into(),
                        prompt_mode: Some(PromptModeDto::Bash),
                        images: Vec::new(),
                        turn_id: Some(86002),
                        visualization_context: None,
                    },
                    "slash" => ClientCommand::RunSlashCommand {
                        raw: "/context".into(),
                        images: Vec::new(),
                        turn_id: Some(86002),
                    },
                    _ => prompt(86002, "CACHE14_LATE_PROMPT_B"),
                };
                let sender = first.clone();
                let submit_b = tokio::spawn(async move { sender.submit(command).await });
                tokio::time::timeout(WAIT, delayed.entered.notified()).await.unwrap();
                first.submit(ClientCommand::PauseTurn {
                    turn_id: 86001,
                    reason: "background_time_expired".into(),
                }).await.unwrap();
                assert!(first.active_cancel.lock().await.is_none());
                delayed.release.notify_one();
                tokio::time::timeout(WAIT, submit_b).await.unwrap().unwrap().unwrap();
                *first.submit_test_gate.lock().unwrap() = None;
                tokio::time::sleep(Duration::from_millis(250)).await;
                assert_eq!(first_client.captured_calls().len(), 1, "late {operation} worker must respect paused A");
                let session = first.active_session_id();
                let facts = first.query_turn_admissions(session.clone(), vec![86001, 86002]).await.unwrap();
                assert_eq!(facts[0].recovery.as_ref().unwrap().state, TurnRecoveryStateDto::PausedRecoverable);
                assert!(facts[1].admitted && facts[1].queued && !facts[1].settled && !facts[1].executor_active, "late {operation} B must remain private queued input");
                assert!(!listener.received.lock().await.iter().any(|event| matches!(event,
                    Ev::TurnRecoveryState { snapshot } if snapshot.turn_id == 86002 && snapshot.state == TurnRecoveryStateDto::Running)));
                assert!(!listener.received.lock().await.iter().any(|event| matches!(event,
                    Ev::SlashCommandResult { turn_id: Some(86002), .. })));
                session
            });
            let (handle, client, previous_calls) = if cold {
                drop(first);
                let (resumed, client) =
                    pending_handle(temp.path(), Arc::new(FakeListener::default()));
                resumed.runtime().block_on(async {
                    resumed
                        .resume_seeded_test_session(session.clone())
                        .await
                        .unwrap();
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    assert!(
                        client.captured_calls().is_empty(),
                        "cold late {operation} B stays behind paused A"
                    );
                });
                (resumed, client, 0)
            } else {
                (first, first_client, 1)
            };
            handle.runtime().block_on(async {
                // Retire B first so the positive control measures A's explicit
                // resume, without prescribing how ordinary B could coalesce.
                handle
                    .submit(ClientCommand::Cancel {
                        turn_id: Some(86002),
                    })
                    .await
                    .unwrap();
                let facts = handle
                    .query_turn_admissions(session.clone(), vec![86001, 86002])
                    .await
                    .unwrap();
                assert_eq!(
                    facts[0].recovery.as_ref().unwrap().state,
                    TurnRecoveryStateDto::PausedRecoverable
                );
                assert!(facts[1].settled && !facts[1].executor_active);
                handle
                    .submit(ClientCommand::ResumeTurn { turn_id: 86001 })
                    .await
                    .unwrap();
                wait_calls(&client, previous_calls + 1).await;
                let calls = client.captured_calls();
                assert_eq!(
                    calls
                        .last()
                        .unwrap()
                        .messages
                        .iter()
                        .filter(|row| row.text_content() == "CACHE14_LATE_ACTIVE_A")
                        .count(),
                    1,
                    "explicit Resume A reuses its admitted prompt once"
                );
                assert_eq!(
                    handle.active_cancel.lock().await.as_ref().unwrap().turn_id,
                    Some(86001)
                );
                handle
                    .submit(ClientCommand::Cancel {
                        turn_id: Some(86001),
                    })
                    .await
                    .unwrap();
                assert!(handle.active_cancel.lock().await.is_none());
            });
        }
    }
}

struct ParkedInitialListener {
    inner: Arc<FakeListener>,
    id: u64,
    running: Notify,
    cancelled: Notify,
}

#[async_trait]
impl ClientEventListener for ParkedInitialListener {
    async fn on_event(&self, event: ClientEvent) {
        if let Ev::TurnRecoveryState { snapshot } = &event {
            if snapshot.turn_id == self.id {
                match &snapshot.state {
                    TurnRecoveryStateDto::Running => {
                        self.running.notify_one();
                        std::future::pending::<()>().await;
                    }
                    TurnRecoveryStateDto::Cancelled => {
                        self.cancelled.notify_one();
                        std::future::pending::<()>().await;
                    }
                    _ => {}
                }
            }
        }
        self.inner.on_event(event).await;
    }

    async fn on_workflow_progress(
        &self,
        _origin_session_id: String,
        _task_id: String,
        _run_id: String,
        _progress: client::protocol::listings::WorkflowProgressDto,
    ) {
    }
}

#[test]
fn dropped_public_submit_cleans_reserved_and_permanently_parked_running_callback() {
    for at_reservation in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let listener = Arc::new(ParkedInitialListener {
            inner: Arc::new(FakeListener::default()),
            id: 82001,
            running: Notify::new(),
            cancelled: Notify::new(),
        });
        let (handle, client) = pending_handle(temp.path(), listener.clone());
        handle.runtime().block_on(async {
            let session = handle.active_session_id();
            let reserved = gate(82001);
            if at_reservation {
                *handle.reserve_test_gate.lock().unwrap() = Some(reserved.clone());
            }
            let sender = handle.clone();
            let submit =
                tokio::spawn(async move { sender.submit(prompt(82001, "abandoned source")).await });
            tokio::time::timeout(WAIT, async {
                if at_reservation {
                    reserved.entered.notified().await
                } else {
                    listener.running.notified().await
                }
            })
            .await
            .expect("real startup boundary reached");
            let owner = handle.active_cancel.lock().await.clone().unwrap();
            assert!(owner.task.lock().unwrap().is_none());
            submit.abort();
            assert!(submit.await.unwrap_err().is_cancelled());
            tokio::time::timeout(WAIT, owner.wait_completed())
                .await
                .expect("startup guard completes despite a parked listener");
            assert!(handle.active_cancel.lock().await.is_none());
            assert!(!handle.message_queue.has_active_turn().await);
            assert!(client.captured_calls().is_empty());
            let fact = handle
                .query_turn_admissions(session.clone(), vec![82001])
                .await
                .unwrap()
                .remove(0);
            assert!(fact.admitted && fact.settled && !fact.executor_active);
            assert_eq!(
                fact.recovery.unwrap().state,
                TurnRecoveryStateDto::Cancelled
            );
            assert_eq!(
                handle
                    .durable_turns
                    .load(&session, 82001)
                    .unwrap()
                    .prompt
                    .string_at(""),
                Some("abandoned source")
            );
            tokio::time::timeout(WAIT, listener.cancelled.notified())
                .await
                .expect("terminal callback is also permanently parked");
            *handle.reserve_test_gate.lock().unwrap() = None;
            tokio::time::timeout(WAIT, handle.submit(prompt(82002, "successor B")))
                .await
                .unwrap()
                .unwrap();
            wait_calls(&client, 1).await;
            assert_eq!(
                handle.active_cancel.lock().await.as_ref().unwrap().turn_id,
                Some(82002)
            );
            handle
                .submit(ClientCommand::Cancel {
                    turn_id: Some(82002),
                })
                .await
                .unwrap();
        });
    }
}

#[test]
fn explicit_cancel_releases_owner_and_finishes_while_terminal_callback_is_parked() {
    let temp = tempfile::tempdir().unwrap();
    let listener = Arc::new(ParkedInitialListener {
        inner: Arc::new(FakeListener::default()),
        id: 82501,
        running: Notify::new(),
        cancelled: Notify::new(),
    });
    let (handle, client) = pending_handle(temp.path(), listener.clone());
    handle.runtime().block_on(async {
        let sender = handle.clone();
        let submit =
            tokio::spawn(async move { sender.submit(prompt(82501, "cancelled startup")).await });
        tokio::time::timeout(WAIT, listener.running.notified())
            .await
            .unwrap();
        let owner = handle.active_cancel.lock().await.clone().unwrap();
        assert!(owner.task.lock().unwrap().is_none());
        let canceller = handle.clone();
        let cancellation = tokio::spawn(async move {
            canceller
                .submit(ClientCommand::Cancel {
                    turn_id: Some(82501),
                })
                .await
        });
        tokio::time::timeout(WAIT, listener.cancelled.notified())
            .await
            .unwrap();
        tokio::time::timeout(WAIT, owner.wait_completed())
            .await
            .unwrap();
        assert!(handle.active_cancel.lock().await.is_none());
        assert!(!handle.message_queue.has_active_turn().await);
        assert!(client.captured_calls().is_empty());
        tokio::time::timeout(WAIT, submit)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        // The terminal callback remains parked. Its bounded acknowledgement
        // cannot retain the old reservation or prevent a physical successor.
        tokio::time::timeout(WAIT, handle.submit(prompt(82502, "successor after cancel")))
            .await
            .unwrap()
            .unwrap();
        wait_calls(&client, 1).await;
        assert_eq!(
            handle.active_cancel.lock().await.as_ref().unwrap().turn_id,
            Some(82502)
        );
        tokio::time::timeout(WAIT, cancellation)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(owner.completed.load(Ordering::Acquire));
        handle
            .submit(ClientCommand::Cancel {
                turn_id: Some(82502),
            })
            .await
            .unwrap();
    });
}

#[test]
fn stale_cleanup_keeps_successor_slot_and_queue_token() {
    let temp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let old = handle.reserve_turn(Some(83001)).await.unwrap();
        handle.release_unspawned_turn(&old).await;
        let current = handle.reserve_turn(Some(83002)).await.unwrap();
        handle
            .message_queue
            .register_active_turn(current.cancel.clone())
            .await;
        super::super::clear_mobile_turn_slot(&handle.active_cancel, &handle.message_queue, &old)
            .await;
        assert!(Arc::ptr_eq(
            handle.active_cancel.lock().await.as_ref().unwrap(),
            &current
        ));
        assert!(handle.message_queue.has_active_turn().await);
        assert!(!current.cancel.is_cancelled());
        handle.release_unspawned_turn(&current).await;
        assert!(!handle.message_queue.has_active_turn().await);
    });
}

#[test]
fn release_waits_for_final_driver_after_old_slash_wrapper_join_finishes() {
    let temp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(temp.path());
    handle.runtime().block_on(async {
        let owner = handle.reserve_turn(Some(84001)).await.unwrap();
        let (old_done, old_wait) = tokio::sync::oneshot::channel();
        owner.set_task_handle(tokio::spawn(async move {
            let _ = old_wait.await;
        }));
        let waiter = {
            let handle = handle.clone();
            let owner = owner.clone();
            tokio::spawn(async move { handle.wait_for_turn_release(&owner, false).await })
        };
        tokio::time::timeout(WAIT, async {
            while owner.task.lock().unwrap().as_ref().unwrap().join.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (new_done, new_wait) = tokio::sync::oneshot::channel();
        let final_owner = owner.clone();
        let active = handle.active_cancel.clone();
        let queue = handle.message_queue.clone();
        owner.set_task_handle(tokio::spawn(async move {
            let _ = new_wait.await;
            super::super::clear_mobile_turn_slot(&active, &queue, &final_owner).await;
            final_owner.mark_completed();
        }));
        old_done.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), owner.wait_completed())
                .await
                .is_err()
        );
        assert!(
            !waiter.is_finished(),
            "joining the old wrapper cannot prove final-driver completion"
        );
        new_done.send(()).unwrap();
        tokio::time::timeout(WAIT, waiter).await.unwrap().unwrap();
        assert!(owner.completed.load(Ordering::Acquire));
        assert!(handle.active_cancel.lock().await.is_none());
    });
}

#[test]
fn abandoned_startup_cleanup_is_tracked_after_last_engine_arc_is_released() {
    use std::sync::mpsc;
    struct Sentinel(mpsc::Sender<()>);
    impl Drop for Sentinel {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle(temp.path());
    let (release_gate, gate_wait) = tokio::sync::oneshot::channel();
    let (sentinel_drop, sentinel_wait) = mpsc::channel();
    let (owner, session) = handle.runtime().block_on(async {
        let reserved = gate(85001);
        *handle.reserve_test_gate.lock().unwrap() = Some(reserved.clone());
        let sender = handle.clone();
        let submit =
            tokio::spawn(async move { sender.submit(prompt(85001, "last owner source")).await });
        tokio::time::timeout(WAIT, reserved.entered.notified())
            .await
            .unwrap();
        let owner = handle.active_cancel.lock().await.clone().unwrap();
        let blocker = owner.clone();
        let (blocked, block_wait) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _initial_event_guard = blocker.initial_event_gate.lock().await;
            let _ = blocked.send(());
            let _ = gate_wait.await;
        });
        block_wait.await.unwrap();
        submit.abort();
        assert!(submit.await.unwrap_err().is_cancelled());
        tokio::time::timeout(WAIT, async {
            while owner.task.lock().unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!owner.completed.load(Ordering::Acquire));
        let (started, started_wait) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _sentinel = Sentinel(sentinel_drop);
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        started_wait.await.unwrap();
        (owner, handle.active_session_id())
    });
    let durable = handle.durable_turns.clone();
    let active = handle.active_cancel.clone();
    drop(handle);
    assert!(
        matches!(
            sentinel_wait.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "tracked cleanup must keep the owned runtime alive"
    );
    release_gate.send(()).unwrap();
    sentinel_wait
        .recv_timeout(WAIT)
        .expect("runtime drains only after startup cleanup completes");
    assert!(owner.completed.load(Ordering::Acquire));
    assert!(active.try_lock().unwrap().is_none());
    assert_eq!(
        durable.load(&session, 85001).unwrap().state,
        Some(TurnRecoveryStateDto::Cancelled)
    );
    let cold = DurableTurnStore::new(test_config(temp.path()).lingxi_home.join("mobile-turns"));
    assert_eq!(
        cold.load(&session, 85001).unwrap().state,
        Some(TurnRecoveryStateDto::Cancelled)
    );
}
