use super::*;
use lingxi_core::host::OrchestratorHandle;
use lingxi_core::types::SessionId;
use std::time::{Duration, Instant};

struct RejectBeforeCronClaim {
    begins: Arc<AtomicUsize>,
}

struct PausedRejectBeforeCronClaim {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl super::super::MobileCronOwnerAdmission for PausedRejectBeforeCronClaim {
    async fn begin_owner(&self, _: super::super::CronOwnerCandidateDto) -> bool {
        self.started.notify_one();
        self.release.notified().await;
        false
    }

    async fn confirm_owner(&self, _: super::super::CronOwnerReservationDto) -> bool {
        panic!("a rejected candidate must never be confirmed")
    }
}

#[async_trait::async_trait]
impl super::super::MobileCronOwnerAdmission for RejectBeforeCronClaim {
    async fn begin_owner(&self, candidate: super::super::CronOwnerCandidateDto) -> bool {
        assert_eq!(candidate.session_mode, "code");
        assert_eq!(candidate.session_scope_key, "project.test");
        assert!(candidate.session_transcript_path.is_none());
        self.begins.fetch_add(1, Ordering::SeqCst);
        false
    }

    async fn confirm_owner(&self, _: super::super::CronOwnerReservationDto) -> bool {
        panic!("a rejected candidate must never be confirmed")
    }
}

#[test]
fn cron_owner_rejection_is_retryable_before_all_three_entrypoint_claims() {
    fn snapshot_jsonl(root: &Path, files: &mut Vec<(std::path::PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                snapshot_jsonl(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push((path.clone(), std::fs::read(path).unwrap()));
            }
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle_with_config(test_config(tmp.path()), tmp.path());
    handle.runtime().block_on(async {
        let cwd = std::path::PathBuf::from(handle.session_cwd());
        let task = serde_json::json!({
            "id":"d12345678", "cron":"0 9 * * *", "prompt":"brief", "createdAt":1,
            "recurring":true, "automation":{
                "version":2, "status":"active", "model":"provider/model",
                "reasoning":{"type":"automatic"}, "runMode":"new_session",
                "sessionMode":"code", "sessionScopeKey":"project.test", "notificationPolicy":"all"
            }
        });
        let body = serde_json::json!({"tasks":[task]}).to_string();
        cron::tasks_file::write_automation_tasks_body(handle.fs.as_ref(), &cwd, &body)
            .await
            .unwrap();
        let stored = cron::tasks_file::parse_automation_tasks_strict(&body).unwrap();
        let now = handle.firer_platform.clock().now();
        let due_at = cron::next_fire_epoch_ms_for_persisted_task(&stored.tasks[0], now).unwrap();
        assert!(
            due_at
                <= now
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
        );
        let task_file = cron::scheduled_tasks_path(&cwd);
        let before_tasks = std::fs::read(&task_file).unwrap();
        let mut before_jsonl = Vec::new();
        snapshot_jsonl(tmp.path(), &mut before_jsonl);
        before_jsonl.sort_by(|a, b| a.0.cmp(&b.0));
        let begins = Arc::new(AtomicUsize::new(0));

        for entrypoint in 0..3 {
            let admission =
                super::super::MobileCronAdmission::new(Box::new(RejectBeforeCronClaim {
                    begins: begins.clone(),
                }));
            let result = match entrypoint {
                0 => {
                    handle
                        .run_cron_task_if_due("d12345678".into(), due_at, admission.clone())
                        .await
                }
                1 => {
                    handle
                        .run_cron_task_now_at("d12345678".into(), 1, admission.clone())
                        .await
                }
                _ => {
                    handle
                        .run_cron_task_now("d12345678".into(), admission.clone())
                        .await
                }
            }
            .expect("rejected owner must produce a result");
            admission.cancel_and_wait().await;
            assert!(result.retryable, "entrypoint {entrypoint}");
            assert!(
                matches!(result.status, super::super::CronFireStatusDto::Failed { ref message }
                if message == "busy: Scheduled session owner could not be reserved")
            );
            assert_eq!(begins.load(Ordering::SeqCst), entrypoint + 1);
            assert_eq!(std::fs::read(&task_file).unwrap(), before_tasks);
            assert!(
                cron::list_owner_reservations_strict(handle.fs.as_ref(), &cwd)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let mut after_jsonl = Vec::new();
            snapshot_jsonl(tmp.path(), &mut after_jsonl);
            after_jsonl.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(after_jsonl, before_jsonl);
        }

        // Re-delivery of the same terminal, interrupted or still-running
        // occurrence is not a fresh pre-claim rejection. The callback can
        // still refuse, but the public API must preserve claim's None result.
        let mut calls = 3;
        for entrypoint in 0..2 {
            for status in [
                cron::AutomationRunStatus::Succeeded,
                cron::AutomationRunStatus::Interrupted,
                cron::AutomationRunStatus::Running,
                cron::AutomationRunStatus::Queued,
            ] {
                let at = if entrypoint == 0 { due_at } else { 123 };
                let mut duplicate = stored.tasks[0].clone();
                duplicate
                    .automation
                    .as_mut()
                    .unwrap()
                    .runs
                    .push(cron::AutomationRun {
                        id: if entrypoint == 0 {
                            format!("d12345678-{at}")
                        } else {
                            format!("d12345678-manual-at-{at}")
                        },
                        task_id: "d12345678".into(),
                        owner_pid: None,
                        claim_generation: Some(1),
                        manual_occurrence_at: (entrypoint == 1).then_some(at),
                        scheduled_at: at,
                        started_at: None,
                        finished_at: None,
                        status,
                        model: "provider/model".into(),
                        reasoning: serde_json::json!({"type":"automatic"}),
                        session_id: None,
                        session_transcript_path: None,
                        session_mode: None,
                        session_scope_key: None,
                        summary: None,
                        error: None,
                    });
                let document = cron::ScheduledTasks {
                    tasks: vec![duplicate],
                    ..Default::default()
                };
                cron::tasks_file::write_automation_tasks_body(
                    handle.fs.as_ref(),
                    &cwd,
                    &cron::serialize_tasks(&document),
                )
                .await
                .unwrap();
                let unchanged = std::fs::read(&task_file).unwrap();
                let admission =
                    super::super::MobileCronAdmission::new(Box::new(RejectBeforeCronClaim {
                        begins: begins.clone(),
                    }));
                let result = if entrypoint == 0 {
                    handle
                        .run_cron_task_if_due("d12345678".into(), at, admission.clone())
                        .await
                } else {
                    handle
                        .run_cron_task_now_at("d12345678".into(), at, admission.clone())
                        .await
                };
                admission.cancel_and_wait().await;
                calls += 1;
                if status == cron::AutomationRunStatus::Queued {
                    assert!(
                        result.is_some_and(|result| result.retryable),
                        "queued {entrypoint}"
                    );
                } else {
                    assert!(result.is_none(), "duplicate {entrypoint} {status:?}");
                }
                assert_eq!(begins.load(Ordering::SeqCst), calls);
                assert_eq!(std::fs::read(&task_file).unwrap(), unchanged);

                // The pre-existing transition-busy path must use the same
                // occurrence check and must not call owner admission.
                let transition = handle.loop_transition.lock().await;
                let admission =
                    super::super::MobileCronAdmission::new(Box::new(RejectBeforeCronClaim {
                        begins: begins.clone(),
                    }));
                let result = if entrypoint == 0 {
                    handle
                        .run_cron_task_if_due("d12345678".into(), at, admission.clone())
                        .await
                } else {
                    handle
                        .run_cron_task_now_at("d12345678".into(), at, admission.clone())
                        .await
                };
                admission.cancel_and_wait().await;
                drop(transition);
                if status == cron::AutomationRunStatus::Queued {
                    assert!(
                        result.is_some_and(|result| result.retryable),
                        "transition queued {entrypoint}"
                    );
                } else {
                    assert!(
                        result.is_none(),
                        "transition duplicate {entrypoint} {status:?}"
                    );
                }
                assert_eq!(begins.load(Ordering::SeqCst), calls);
                assert_eq!(std::fs::read(&task_file).unwrap(), unchanged);
            }
        }
        cron::tasks_file::write_automation_tasks_body(handle.fs.as_ref(), &cwd, &body)
            .await
            .unwrap();
        let admission = super::super::MobileCronAdmission::new(Box::new(RejectBeforeCronClaim {
            begins: begins.clone(),
        }));
        assert!(handle
            .run_cron_task_if_due("d12345678".into(), due_at + 1, admission.clone())
            .await
            .is_none());
        admission.cancel_and_wait().await;
        calls += 1;

        // Invalid source remains a permanent pre-admission failure, and must
        // not call the callback or consume an occurrence either.
        let mut invalid: serde_json::Value = serde_json::from_slice(&before_tasks).unwrap();
        invalid["tasks"][0]["automation"]
            .as_object_mut()
            .unwrap()
            .remove("sessionMode");
        cron::tasks_file::write_automation_tasks_body(
            handle.fs.as_ref(),
            &cwd,
            &invalid.to_string(),
        )
        .await
        .unwrap();
        let invalid_tasks = std::fs::read(&task_file).unwrap();
        let admission = super::super::MobileCronAdmission::new(Box::new(RejectBeforeCronClaim {
            begins: begins.clone(),
        }));
        let result = handle
            .run_cron_task_now("d12345678".into(), admission.clone())
            .await
            .unwrap();
        admission.cancel_and_wait().await;
        assert!(!result.retryable);
        assert!(
            matches!(result.status, super::super::CronFireStatusDto::Failed { ref message }
            if message.starts_with("paused:"))
        );
        assert_eq!(begins.load(Ordering::SeqCst), calls);
        assert_eq!(std::fs::read(&task_file).unwrap(), invalid_tasks);
    });
}

#[test]
fn cron_owner_rejection_after_cancellation_does_not_retry_or_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle_with_config(test_config(tmp.path()), tmp.path());
    handle.runtime().block_on(async {
        let cwd = std::path::PathBuf::from(handle.session_cwd());
        let body = serde_json::json!({"tasks":[{
            "id":"d12345678", "cron":"0 9 * * *", "prompt":"brief", "createdAt":1,
            "recurring":true, "automation":{
                "version":2, "status":"active", "model":"provider/model",
                "reasoning":{"type":"automatic"}, "runMode":"new_session",
                "sessionMode":"code", "sessionScopeKey":"project.test", "notificationPolicy":"all"
            }
        }]})
        .to_string();
        cron::tasks_file::write_automation_tasks_body(handle.fs.as_ref(), &cwd, &body)
            .await
            .unwrap();
        let task_path = cron::scheduled_tasks_path(&cwd);
        let before = std::fs::read(&task_path).unwrap();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let admission =
            super::super::MobileCronAdmission::new(Box::new(PausedRejectBeforeCronClaim {
                started: started.clone(),
                release: release.clone(),
            }));
        let child_handle = handle.clone();
        let child_admission = admission.clone();
        let fire = tokio::spawn(async move {
            child_handle
                .run_cron_task_now("d12345678".into(), child_admission)
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        let cancel = admission.cancel_and_wait();
        tokio::pin!(cancel);
        assert!(matches!(
            futures::poll!(&mut cancel),
            std::task::Poll::Pending
        ));
        release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(5), fire)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), &mut cancel)
            .await
            .unwrap();
        assert!(!result.retryable);
        assert!(
            matches!(result.status, super::super::CronFireStatusDto::Failed { ref message }
            if message.starts_with("cancelled:"))
        );
        assert_eq!(std::fs::read(&task_path).unwrap(), before);
        assert!(
            cron::list_owner_reservations_strict(handle.fs.as_ref(), &cwd)
                .await
                .unwrap()
                .is_empty()
        );
    });
}

#[test]
fn cron_owner_rejection_rechecks_scope_configuration_and_concurrent_claim() {
    for scenario in ["removed", "replaced", "edited", "malformed", "claimed"] {
        let tmp = tempfile::tempdir().unwrap();
        let (handle, _) = build_submit_handle_with_config(test_config(tmp.path()), tmp.path());
        handle.runtime().block_on(async {
            let cwd = std::path::PathBuf::from(handle.session_cwd());
            let body = serde_json::json!({"tasks":[{
                "id":"d12345678", "cron":"0 9 * * *", "prompt":"brief", "createdAt":1,
                "recurring":true, "automation":{
                    "version":2, "status":"active", "model":"provider/model",
                    "reasoning":{"type":"automatic"}, "runMode":"new_session",
                    "sessionMode":"code", "sessionScopeKey":"project.test", "notificationPolicy":"all"
                }
            }]}).to_string();
            cron::tasks_file::write_automation_tasks_body(handle.fs.as_ref(), &cwd, &body).await.unwrap();
            let task_path = cron::scheduled_tasks_path(&cwd);
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let admission = super::super::MobileCronAdmission::new(Box::new(PausedRejectBeforeCronClaim {
                started: started.clone(), release: release.clone(),
            }));
            let child_handle = handle.clone();
            let child_admission = admission.clone();
            let fire = tokio::spawn(async move {
                child_handle.run_cron_task_now_at("d12345678".into(), 123, child_admission).await
            });
            tokio::time::timeout(Duration::from_secs(5), started.notified()).await.unwrap();
            match scenario {
                "removed" => std::fs::remove_file(&task_path).unwrap(),
                "replaced" => {
                    let state_root = task_path.parent().unwrap();
                    std::fs::rename(state_root, cwd.join("prior-state")).unwrap();
                    std::fs::create_dir(state_root).unwrap();
                    std::fs::write(&task_path, &body).unwrap();
                }
                "edited" => {
                    let mut edited: serde_json::Value = serde_json::from_str(&body).unwrap();
                    edited["tasks"][0]["automation"]["sessionScopeKey"] = "project.other".into();
                    std::fs::write(&task_path, edited.to_string()).unwrap();
                }
                "malformed" => std::fs::write(&task_path, b"invalid document").unwrap(),
                "claimed" => {
                    assert!(cron::claim_automation_run_now_at(handle.fs.as_ref(), &cwd, "d12345678",
                        handle.firer_platform.clock().now(), 123).await.is_some());
                }
                _ => unreachable!(),
            }
            let after_change = std::fs::read(&task_path).ok();
            release.notify_one();
            let result = tokio::time::timeout(Duration::from_secs(5), fire).await.unwrap().unwrap();
            admission.cancel_and_wait().await;
            if scenario == "claimed" {
                assert!(result.is_none(), "concurrent claim must not be retried");
            } else {
                let result = result.expect("unverifiable source must be reported");
                assert!(!result.retryable, "{scenario}");
                assert!(matches!(result.status, super::super::CronFireStatusDto::OutcomeUnknown { .. }), "{scenario}");
            }
            assert_eq!(std::fs::read(&task_path).ok(), after_change, "{scenario}");
            assert!(cron::list_owner_reservations_strict(handle.fs.as_ref(), &cwd).await.unwrap().is_empty());
        });
    }
}

pub(super) fn wait_for_session_release(home: &Path, id: &str) {
    let session_id = SessionId::from_uuid(uuid::Uuid::parse_str(id).unwrap());
    let canonical = std::path::PathBuf::from(super::super::canonical_cwd_string(home));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let probe =
            crate::mobile::transcript::MobileTranscriptSessionSwitcher::new(canonical.clone());
        if probe.claim_initial(session_id).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "session owner did not drain for {id}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn duplicate_cold_mount_is_rejected_until_the_first_owner_drains() {
    let tmp = tempfile::tempdir().unwrap();
    let (id, _) = seed_replay_valid_session(tmp.path());
    let mut config = test_config(tmp.path());
    config.initial_session_id = Some(id.clone());
    config.initial_session_transcript_path = Some(session::jsonl::session_path(
        &config.lingxi_home,
        &config.cwd.to_string_lossy(),
        &id,
    ));
    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let first = build_mobile_engine(
        config.clone(),
        platform.clone(),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
    )
    .unwrap();
    let path = first.inner().session_writer.active_path();
    let before = std::fs::read(&path).unwrap();
    let duplicate = build_mobile_engine(
        config.clone(),
        platform.clone(),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
    );
    let error = match duplicate {
        Ok(_) => panic!("two engines must not own one conversation"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("already mounted by another live owner"),
        "{error}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    // A turn/background owner can outlive the foreign handle. Its retained
    // orchestrator must continue excluding a second mount during that drain.
    let retained_orchestrator = first.inner().orchestrator.clone();
    drop(first);
    let during_drain = build_mobile_engine(
        config.clone(),
        platform.clone(),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
    );
    assert!(
        matches!(during_drain, Err(error) if error.to_string().contains("already mounted by another live owner"))
    );
    drop(retained_orchestrator);
    wait_for_session_release(&config.lingxi_home, &id);
    let reopened = build_mobile_engine(
        config,
        platform,
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
    )
    .expect("a fully drained owner must be reopenable");
    reopened.runtime().block_on(async {
        assert_eq!(
            reopened
                .inner()
                .orchestrator
                .current_session_id()
                .await
                .as_uuid()
                .to_string(),
            id
        );
    });
}

#[test]
fn direct_mobile_runtime_retires_its_registry_and_releases_the_session() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(tmp.path());
    let executor = tokio::runtime::Runtime::new().unwrap();
    let runtime = executor
        .block_on(build_mobile(
            config.clone(),
            Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
            Arc::new(FakeListener::default()),
            Arc::new(RecordingPermissionSink::default()),
        ))
        .unwrap();
    let id = executor.block_on(async {
        runtime
            .orchestrator
            .current_session_id()
            .await
            .as_uuid()
            .to_string()
    });
    drop(runtime);
    wait_for_session_release(&config.lingxi_home, &id);
}

#[test]
fn cancelled_assembly_retires_the_partially_built_owner() {
    struct PausedListener(std::sync::mpsc::Sender<()>);

    #[async_trait]
    impl ClientEventListener for PausedListener {
        async fn on_event(&self, event: ClientEvent) {
            if matches!(event, ClientEvent::PermissionModeChanged { .. }) {
                let _ = self.0.send(());
                std::future::pending::<()>().await;
            }
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

    let tmp = tempfile::tempdir().unwrap();
    let (id, _) = seed_replay_valid_session(tmp.path());
    let mut config = test_config(tmp.path());
    config.initial_session_id = Some(id.clone());
    config.initial_session_transcript_path = Some(session::jsonl::session_path(
        &config.lingxi_home,
        &config.cwd.to_string_lossy(),
        &id,
    ));
    let executor = tokio::runtime::Runtime::new().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let build = executor.spawn(build_mobile(
        config.clone(),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        Arc::new(PausedListener(ready_tx)),
        Arc::new(RecordingPermissionSink::default()),
    ));
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("assembly must reach the post-registry event");
    let home = std::path::PathBuf::from(super::super::canonical_cwd_string(&config.lingxi_home));
    let session_id = SessionId::from_uuid(uuid::Uuid::parse_str(&id).unwrap());
    assert!(crate::mobile::transcript::session_owner_is_live(
        home, session_id
    ));
    build.abort();
    executor.block_on(async {
        assert!(matches!(build.await, Err(error) if error.is_cancelled()));
    });
    wait_for_session_release(&config.lingxi_home, &id);
}

#[test]
fn hot_resume_cannot_steal_a_live_owner_and_new_sessions_are_also_claimed() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(tmp.path());
    let (first, _) = build_submit_handle_with_config(config.clone(), tmp.path());
    let (second, _) = build_submit_handle_with_config(config.clone(), tmp.path());
    let first_id = first
        .runtime()
        .block_on(async { first.inner().orchestrator.current_session_id().await });
    let before = second
        .runtime()
        .block_on(async { second.inner().orchestrator.current_session_id().await });
    second.runtime().block_on(async {
        let error = second
            .resume_seeded_test_session(first_id.as_uuid().to_string())
            .await
            .expect_err("cannot resume a session owned by another engine");
        assert!(format!("{error:?}").contains("already mounted by another live owner"));
        assert_eq!(
            second.inner().orchestrator.current_session_id().await,
            before
        );
    });
    let next_id = first.runtime().block_on(async {
        first
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: None,
                mode: SessionModeDto::Code,
            })
            .await
            .unwrap();
        first.inner().orchestrator.current_session_id().await
    });
    assert_ne!(next_id, first_id);
    let canonical =
        std::path::PathBuf::from(super::super::canonical_cwd_string(&config.lingxi_home));
    let contender = crate::mobile::transcript::MobileTranscriptSessionSwitcher::new(canonical);
    assert!(
        contender.claim_initial(first_id).is_err(),
        "retired source remains protected while its owner exists"
    );
    assert!(
        contender.claim_initial(next_id).is_err(),
        "new session must be claimed before publication"
    );
    drop(contender);
    drop(first);
    wait_for_session_release(&config.lingxi_home, &first_id.as_uuid().to_string());
    wait_for_session_release(&config.lingxi_home, &next_id.as_uuid().to_string());
}

#[test]
fn same_uuid_is_independent_across_config_homes() {
    let tmp = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let first =
        crate::mobile::transcript::MobileTranscriptSessionSwitcher::new(tmp.path().join("first"));
    let second =
        crate::mobile::transcript::MobileTranscriptSessionSwitcher::new(tmp.path().join("second"));
    first.claim_initial(session_id).unwrap();
    second.claim_initial(session_id).unwrap();
}

#[tokio::test]
async fn release_receipt_only_succeeds_after_the_owner_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    let id = SessionId::new();
    let switcher = crate::mobile::transcript::MobileTranscriptSessionSwitcher::new(home.clone());
    switcher.claim_initial(id).unwrap();
    let home_text = home.to_string_lossy().into_owned();
    let id_text = id.as_uuid().to_string();
    assert!(
        !super::super::wait_for_mobile_session_release(home_text.clone(), id_text.clone(), 1).await
    );
    assert!(
        !super::super::wait_for_mobile_session_release(home_text.clone(), "invalid".into(), 1)
            .await
    );
    drop(switcher);
    assert!(super::super::wait_for_mobile_session_release(home_text, id_text, 10).await);
}

#[test]
fn idle_scheduled_run_borrows_the_live_owner_and_marks_it_for_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle_with_config(test_config(tmp.path()), tmp.path());
    handle.runtime().block_on(async {
        let id = handle.inner().orchestrator.current_session_id().await.as_uuid().to_string();
        let path = handle.inner().session_writer.active_path();
        handle.inner().session_writer.append_mobile_empty_session(&id, "Selected", &handle.session_cwd()).await.unwrap();
        let body = serde_json::json!({"id":"d12345678","cron":"0 9 * * *","prompt":"brief","createdAt":1,"recurring":false,"automation":{"version":2,"status":"active","model":"anthropic/claude-sonnet-4-6","reasoning":{"type":"invalid-test-reasoning"},"runMode":"selected_session","targetSessionId":id,"targetSessionTranscriptPath":path,"targetSessionMode":"code","targetSessionScopeKey":"project.test","notificationPolicy":"all"}}).to_string();
        let task: cron::CronTask = serde_json::from_str(&body).unwrap();
        let root = std::path::PathBuf::from(handle.session_cwd());
        let mut document = cron::ScheduledTasks::default();
        document.tasks.push(task);
        cron::tasks_file::write_automation_tasks_body(handle.fs.as_ref(), &root, &cron::serialize_tasks(&document)).await.unwrap();
        let request = cron::claim_automation_run_now(handle.fs.as_ref(), &root, "d12345678", std::time::SystemTime::now()).await.unwrap();
        let firer = super::super::MobileTurnFirer { cfg: handle.firer_cfg.clone(), platform: handle.firer_platform.clone() };
        let error = firer.fire_session_with_admission("scheduled".into(), request.task.automation.as_ref(), Some(&request), Some(super::super::automation::allow_cron_owner())).await.unwrap_err();
        // Invalid reasoning is checked after constructing the scheduled child;
        // reaching it proves the selected live owner was borrowed successfully.
        assert!(error.starts_with("paused:"), "{error}");
        assert!(!error.contains("already mounted"), "{error}");
        assert!(!error.contains("configuration is unavailable"), "{error}");
        assert!(handle.scheduled_reload.load(std::sync::atomic::Ordering::Acquire));
    });
}

#[test]
fn scheduled_borrow_rejects_same_path_replaced_after_live_owner_capture() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle_with_config(test_config(tmp.path()), tmp.path());
    handle.runtime().block_on(async {
        let id = handle
            .inner()
            .orchestrator
            .current_session_id()
            .await
            .as_uuid()
            .to_string();
        let path = handle.inner().session_writer.active_path();
        let old_authority = handle
            .inner()
            .session_writer
            .active_transcript_authority()
            .unwrap();
        let original = path.with_extension("captured");
        let bytes = std::fs::read(&path).unwrap();
        std::fs::rename(&path, &original).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let task: cron::CronTask = serde_json::from_value(serde_json::json!({
            "id":"d12345678", "cron":"0 9 * * *", "prompt":"brief", "createdAt":1, "recurring":true,
            "automation":{"version":2,"status":"active","model":"anthropic/claude-sonnet-4-6",
                "reasoning":{"type":"automatic"},"runMode":"selected_session","targetSessionId":id,
                "targetSessionTranscriptPath":path.to_string_lossy(),"targetSessionMode":"code",
                "targetSessionScopeKey":"project.test","notificationPolicy":"all"}
        }))
        .unwrap();
        let root = std::path::PathBuf::from(handle.session_cwd());
        let mut document = cron::ScheduledTasks::default();
        document.tasks.push(task);
        cron::tasks_file::write_automation_tasks_body(
            handle.fs.as_ref(),
            &root,
            &cron::serialize_tasks(&document),
        )
        .await
        .unwrap();
        let request = cron::claim_automation_run_now(
            handle.fs.as_ref(),
            &root,
            "d12345678",
            std::time::SystemTime::now(),
        )
        .await
        .unwrap();
        let firer = super::super::MobileTurnFirer {
            cfg: handle.firer_cfg.clone(),
            platform: handle.firer_platform.clone(),
        };
        let error = firer
            .fire_session_with_admission(
                "scheduled".into(),
                request.task.automation.as_ref(),
                Some(&request),
                Some(super::super::automation::allow_cron_owner()),
            )
            .await
            .unwrap_err();
        assert!(error.starts_with("busy:"), "{error}");
        assert_eq!(
            handle.inner().session_writer.active_transcript_authority(),
            Some(old_authority)
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read(&original).unwrap(), bytes);
        assert!(
            cron::list_owner_reservations_strict(handle.fs.as_ref(), &root)
                .await
                .unwrap()
                .is_empty()
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&original, &path).unwrap();
    });
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[test]
fn idle_scheduled_run_survives_default_thread_stack_in_isolated_process() {
    // A real stack overflow aborts the whole process. Run the existing
    // selected-session regression in a child with Tokio's default thread
    // budget so a missing product-side worker configuration fails this test
    // without terminating the rest of the suite.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env("RUST_MIN_STACK", "2097152")
        .args([
            "--exact",
            "mobile::host::tests::session_owner_tests::idle_scheduled_run_borrows_the_live_owner_and_marks_it_for_reload",
            "--test-threads=1",
        ])
        .output()
        .expect("run the scheduled session regression in an isolated process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "scheduled session failed on the production worker: status={}, stdout={stdout}, stderr={stderr}",
        output.status
    );
}

#[test]
fn run_now_can_target_its_own_idle_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle_with_config(test_config(tmp.path()), tmp.path());
    handle.runtime().block_on(async {
        let id = handle.inner().orchestrator.current_session_id().await.as_uuid().to_string();
        let path = handle.inner().session_writer.active_path();
        handle.inner().session_writer.append_mobile_empty_session(&id, "Selected", &handle.session_cwd()).await.unwrap();
        let body = serde_json::json!({"tasks":[{"id":"d12345678","cron":"0 9 * * *","prompt":"brief","createdAt":1,"recurring":false,"automation":{"version":2,"status":"active","model":"anthropic/claude-sonnet-4-6","reasoning":{"type":"invalid-test-reasoning"},"runMode":"selected_session","targetSessionId":id,"targetSessionTranscriptPath":path,"targetSessionMode":"code","targetSessionScopeKey":"project.test","notificationPolicy":"all"}}]}).to_string();
        cron::tasks_file::write_automation_tasks_body(handle.fs.as_ref(), Path::new(&handle.session_cwd()), &body).await.unwrap();
        let fired = handle.run_cron_task_now("d12345678".into(), super::super::automation::allow_cron_owner()).await.expect("task must be claimed");
        let super::super::CronFireStatusDto::Failed { message } = fired.status else { panic!("invalid reasoning must fail") };
        assert!(!message.contains("busy:"), "self-targeted cron must not deadlock: {message}");
        assert!(!message.contains("configuration is unavailable"), "live owner must be borrowed: {message}");
        assert!(message.starts_with("paused:"), "invalid reasoning must be reached after mount: {message}");
    });
}

#[test]
fn scheduled_reader_rechecks_active_source_after_transition_acquisition() {
    let tmp = tempfile::tempdir().unwrap();
    let (handle, _) = build_submit_handle_with_config(test_config(tmp.path()), tmp.path());
    handle.runtime().block_on(async {
        let source = handle.inner().session_writer.active_path();
        let cwd = handle.session_cwd();
        let id = handle.inner().orchestrator.current_session_id().await;
        assert!(super::super::automation::mobile_cron_reader_matches_source(
            &handle, &cwd, &source
        ));
        let transition = handle.loop_transition.clone().lock_owned().await;
        let other_dir = tmp.path().join("different-session-source");
        std::fs::create_dir_all(&other_dir).unwrap();
        let replacement = other_dir.join(source.file_name().unwrap());
        handle.inner().session_writer.retarget(replacement).await;
        assert_eq!(handle.inner().orchestrator.current_session_id().await, id);
        assert_eq!(handle.session_cwd(), cwd);
        assert!(
            !super::super::automation::mobile_cron_reader_matches_source(&handle, &cwd, &source)
        );
        handle.inner().session_writer.retarget(source).await;
        drop(transition);
    });
}
