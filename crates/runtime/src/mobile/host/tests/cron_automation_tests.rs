use super::*;
use tokio::sync::mpsc;

fn task(status: &str) -> cron::CronTask {
    serde_json::from_value(serde_json::json!({
        "id": "d12345678", "cron": "0 9 * * *", "prompt": "brief", "createdAt": 1,
        "recurring": false,
        "automation": { "version": 2, "status": status, "model": "anthropic/claude-sonnet-4-6", "reasoning": {"type":"automatic"}, "runMode":"new_session", "notificationPolicy":"all" }
    })).unwrap()
}

async fn claimed_test_run() -> (
    tempfile::TempDir,
    Arc<dyn FileSystem>,
    cron::AutomationRunRequest,
) {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join(".lingxi")).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix_minimal::PosixFileSystem::new(
        temp.path().to_path_buf(),
    ));
    let mut recurring = task("active");
    recurring.recurring = Some(true);
    let mut doc = cron::ScheduledTasks::default();
    doc.tasks.push(recurring);
    cron::tasks_file::write_automation_tasks_body(
        fs.as_ref(),
        temp.path(),
        &cron::serialize_tasks(&doc),
    )
    .await
    .unwrap();
    let request = cron::claim_automation_run_now(
        fs.as_ref(),
        temp.path(),
        "d12345678",
        std::time::SystemTime::now(),
    )
    .await
    .unwrap();
    (temp, fs, request)
}

#[tokio::test]
async fn supervisor_retries_transient_commit_io_without_reexecuting_turn() {
    let (temp, fs, request) = claimed_test_run().await;
    let task_path = cron::scheduled_tasks_path(temp.path());
    let saved_path = temp.path().join("temporarily-unavailable-tasks.json");
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = executions.clone();
    let dto = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        supervise_mobile_automation(
            fs.clone(),
            temp.path().into(),
            Arc::new(platform_posix_minimal::PosixClock::new()),
            std::future::ready(Some(request)),
            move |_| async move {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::fs::rename(&task_path, &saved_path).unwrap();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    std::fs::rename(saved_path, task_path).unwrap();
                });
                Ok(cron::AutomationRunResult {
                    session_id: "actual-session".into(),
                    summary: "actual completed result".into(),
                })
            },
        ),
    )
    .await
    .unwrap()
    .expect("saved execution must not be reported as skipped");
    assert!(matches!(dto.status, CronFireStatusDto::Ok));
    assert_eq!(dto.result_text.as_deref(), Some("actual completed result"));
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);
    let stored = read_cron_tasks(fs.as_ref(), temp.path()).await;
    let run = &stored.tasks[0].automation.as_ref().unwrap().runs[0];
    assert_eq!(run.status, cron::AutomationRunStatus::Succeeded);
    assert_eq!(run.summary.as_deref(), Some("actual completed result"));
}

#[tokio::test]
async fn retargeted_cron_runtime_rebinds_workflows_permissions_and_transcript_identity() {
    use crate::mobile::test_support::{test_config, FakeListener, HostFakePlatform};
    use platform_api::PermissionGate as _;
    let temp = tempfile::tempdir().unwrap();
    let config = test_config(temp.path());
    let home = config.lingxi_home.clone();
    let cwd = canonical_cwd_string(temp.path());
    let (sender, mut requests) = mpsc::unbounded_channel();
    let runtime = build_mobile(
        config,
        Arc::new(HostFakePlatform::new(temp.path().into())),
        Arc::new(FakeListener::default()),
        Arc::new(ImmediateDenyPermissionSink { sender }),
    )
    .await
    .unwrap();
    let original = runtime.active_session_uuid.lock().unwrap().clone();
    let target = protocol::SessionId::new();
    let target_uuid = target.as_uuid().to_string();
    for (task_id, session_uuid, run_id) in [
        ("wold00001", original, "wf_old"),
        ("wnew00001", target_uuid.clone(), "wf_new"),
    ] {
        runtime
            .task_registry
            .register_adopted_workflow(tasks::registry::AdoptedWorkflow {
                task_id: task_id.into(),
                session_uuid: Some(session_uuid),
                workflow_id: task_id.into(),
                run_id: run_id.into(),
                script_path: "script.ts".into(),
                args: None,
                transcript_dir: temp.path().join(run_id).to_string_lossy().into_owned(),
                description: task_id.into(),
                start_time: std::time::SystemTime::now(),
            })
            .await
            .unwrap();
    }
    runtime
        .orchestrator
        .resume_session(
            target,
            Vec::new(),
            None,
            None,
            platform_api::ResumeRuntimeSnapshot::default(),
        )
        .await
        .unwrap();
    runtime.retarget_session_context(&home, target, &cwd).await;
    assert_eq!(*runtime.active_session_uuid.lock().unwrap(), target_uuid);
    assert_eq!(
        runtime.session_writer.active_path(),
        orchestrator::transcript_paths::main_transcript_path(&home, &cwd, &target_uuid)
    );
    let workflows = platform_api::task_registry::TaskRegistryHandle::list_workflows(
        runtime.task_registry.as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(workflows.len(), 1);
    assert_eq!(workflows[0].task_id, "wnew00001");
    let gate = runtime.permission_gate.clone();
    let request_task = tokio::spawn(async move {
        gate.check_with_worker(
            "Write",
            &serde_json::json!({"file_path":"test.txt"}),
            Some(permission::gate::PromptWorker {
                name: "cron-worker".into(),
                team: None,
                is_async: true,
            }),
        )
        .await
    });
    let request = tokio::time::timeout(std::time::Duration::from_secs(2), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        request
            .owner
            .as_ref()
            .and_then(|owner| owner.session_id.as_deref()),
        Some(target_uuid.as_str())
    );
    runtime
        .permission_gate
        .resolve(request.request_id, PermissionResponseDto::Deny, "Write")
        .await;
    request_task.await.unwrap();
}

#[tokio::test]
async fn binding_cancelled_claim_returns_durable_cancellation_to_native_host() {
    for expired in [false, true] {
        let (temp, fs, request) = claimed_test_run().await;
        let execute_fs = fs.clone();
        let root = temp.path().to_path_buf();
        let execute_root = root.clone();
        let dto = supervise_mobile_automation(
            fs.clone(),
            root,
            Arc::new(platform_posix_minimal::PosixClock::new()),
            std::future::ready(Some(request)),
            move |request| async move {
                {
                    let _guard = cron::lock_cron_file().await;
                    let _file =
                        cron::tasks_file::lock_automation_tasks(execute_fs.as_ref(), &execute_root)
                            .await
                            .unwrap();
                    let mut document = read_cron_tasks(execute_fs.as_ref(), &execute_root).await;
                    if expired {
                        document.tasks[0].expires_at = Some(1);
                    } else {
                        document.tasks[0].automation.as_mut().unwrap().status =
                            cron::AutomationStatus::Paused;
                    }
                    cron::tasks_file::write_automation_tasks_body(
                        execute_fs.as_ref(),
                        &execute_root,
                        &cron::serialize_tasks(&document),
                    )
                    .await
                    .unwrap();
                }
                cron::bind_automation_run_session(
                    execute_fs.as_ref(),
                    &execute_root,
                    &request,
                    "never-started",
                )
                .await?;
                panic!("paused or expired task must not begin executing");
            },
        )
        .await
        .expect("durable cancellation must be returned");
        assert!(!dto.retryable);
        assert!(
            matches!(dto.status, CronFireStatusDto::Failed { message } if message.starts_with(cron::AUTOMATION_CANCELLED_PREFIX) && message.contains(if expired { "expired" } else { "stopped" }))
        );
        let saved = read_cron_tasks(fs.as_ref(), temp.path()).await;
        assert_eq!(
            saved.tasks[0].automation.as_ref().unwrap().runs[0].status,
            cron::AutomationRunStatus::Cancelled
        );
    }
}

#[tokio::test]
async fn supervisor_does_not_surface_cancellation_from_a_reclaimed_generation() {
    let (temp, fs, request) = claimed_test_run().await;
    let execute_fs = fs.clone();
    let root = temp.path().to_path_buf();
    let execute_root = root.clone();
    let dto = supervise_mobile_automation(
        fs,
        root,
        Arc::new(platform_posix_minimal::PosixClock::new()),
        std::future::ready(Some(request)),
        move |request| async move {
            let _guard = cron::lock_cron_file().await;
            let _file = cron::tasks_file::lock_automation_tasks(execute_fs.as_ref(), &execute_root)
                .await
                .unwrap();
            let mut document = read_cron_tasks(execute_fs.as_ref(), &execute_root).await;
            let run = &mut document.tasks[0].automation.as_mut().unwrap().runs[0];
            run.claim_generation = Some(request.claim_generation + 1);
            run.status = cron::AutomationRunStatus::Cancelled;
            run.error = Some("new claimant cancelled".into());
            cron::tasks_file::write_automation_tasks_body(
                execute_fs.as_ref(),
                &execute_root,
                &cron::serialize_tasks(&document),
            )
            .await
            .unwrap();
            Err("cancelled: stale execution".into())
        },
    )
    .await;
    assert!(dto.is_none());
}

#[tokio::test]
async fn explicit_manual_token_survives_busy_retry_and_blocks_terminal_replay() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join(".lingxi")).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix_minimal::PosixFileSystem::new(
        temp.path().into(),
    ));
    let clock: Arc<dyn Clock> = Arc::new(platform_posix_minimal::PosixClock::new());
    let now = clock.now();
    let token = now
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut document = cron::ScheduledTasks::default();
    let mut recurring = task("active");
    recurring.recurring = Some(true);
    document.tasks.push(recurring);
    cron::tasks_file::write_automation_tasks_body(
        fs.as_ref(),
        temp.path(),
        &cron::serialize_tasks(&document),
    )
    .await
    .unwrap();
    let first =
        cron::claim_automation_run_now_at(fs.as_ref(), temp.path(), "d12345678", now, token)
            .await
            .unwrap();
    let run_id = first.run_id.clone();
    let pending = supervise_mobile_automation(
        fs.clone(),
        temp.path().into(),
        clock.clone(),
        std::future::ready(Some(first)),
        |_| async { Err("busy: Target has an active turn".into()) },
    )
    .await
    .unwrap();
    assert!(pending.retryable);
    let retry = cron::claim_automation_run_now_at(
        fs.as_ref(),
        temp.path(),
        "d12345678",
        now + std::time::Duration::from_secs(30),
        token,
    )
    .await
    .unwrap();
    assert_eq!(retry.run_id, run_id);
    let completed = supervise_mobile_automation(
        fs.clone(),
        temp.path().into(),
        clock.clone(),
        std::future::ready(Some(retry)),
        |_| async {
            Ok(cron::AutomationRunResult {
                session_id: "result-session".into(),
                summary: "done".into(),
            })
        },
    )
    .await
    .unwrap();
    assert!(matches!(completed.status, CronFireStatusDto::Ok));
    assert!(cron::claim_automation_run_now_at(
        fs.as_ref(),
        temp.path(),
        "d12345678",
        now + std::time::Duration::from_secs(60),
        token
    )
    .await
    .is_none());
    let saved = read_cron_tasks(fs.as_ref(), temp.path()).await;
    let runs = &saved.tasks[0].automation.as_ref().unwrap().runs;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].scheduled_at, token);
    assert_eq!(runs[0].status, cron::AutomationRunStatus::Succeeded);
}

#[tokio::test]
async fn busy_returns_retryable_dto_and_manual_retry_reuses_pending_run() {
    let (temp, fs, request) = claimed_test_run().await;
    let id = request.run_id.clone();
    let dto = supervise_mobile_automation(
        fs.clone(),
        temp.path().into(),
        Arc::new(platform_posix_minimal::PosixClock::new()),
        std::future::ready(Some(request)),
        |_| async { Err("busy: Conversation has an active turn".into()) },
    )
    .await
    .unwrap();
    assert!(dto.retryable);
    assert!(
        matches!(dto.status, CronFireStatusDto::Failed { message } if message.starts_with("busy:"))
    );
    let retry = cron::claim_automation_run_now(
        fs.as_ref(),
        temp.path(),
        "d12345678",
        std::time::SystemTime::now(),
    )
    .await
    .unwrap();
    assert_eq!(retry.run_id, id);
    let dto = supervise_mobile_automation(
        fs.clone(),
        temp.path().into(),
        Arc::new(platform_posix_minimal::PosixClock::new()),
        std::future::ready(Some(retry)),
        |_| async {
            Ok(cron::AutomationRunResult {
                session_id: "result-session".into(),
                summary: "done".into(),
            })
        },
    )
    .await
    .unwrap();
    assert!(!dto.retryable);
    assert!(matches!(dto.status, CronFireStatusDto::Ok));
}

#[tokio::test]
async fn last_foreground_runtime_borrow_is_safely_dropped_before_and_during_cancellation() {
    struct RuntimeOwner {
        runtime: Option<tokio::runtime::Runtime>,
        dropped: Option<tokio::sync::oneshot::Sender<()>>,
    }
    impl Drop for RuntimeOwner {
        fn drop(&mut self) {
            // Sending after shutdown proves the runtime destructor did not
            // panic because its final reference was on an async worker.
            drop(self.runtime.take());
            let _ = self.dropped.take().unwrap().send(());
        }
    }
    fn borrowed_runtime() -> (
        MobileCronBorrow<RuntimeOwner>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (dropped, receiver) = tokio::sync::oneshot::channel();
        let owner = Arc::new(RuntimeOwner {
            runtime: Some(tokio::runtime::Runtime::new().unwrap()),
            dropped: Some(dropped),
        });
        (MobileCronBorrow(Some(owner)), receiver)
    }
    let (temp, fs, request) = claimed_test_run().await;
    let run_id = request.run_id.clone();
    let (nonmatching, nonmatching_dropped) = borrowed_runtime();
    let (matching, matching_dropped) = borrowed_runtime();
    let (started, ready) = tokio::sync::oneshot::channel();
    let caller = tokio::spawn(supervise_mobile_automation(
        fs.clone(),
        temp.path().into(),
        Arc::new(platform_posix_minimal::PosixClock::new()),
        std::future::ready(Some(request)),
        move |_| async move {
            // Mirrors filtering unrelated upgraded foreground handles.
            drop(nonmatching);
            let _matching_reader = matching;
            let _ = started.send(());
            std::future::pending().await
        },
    ));
    ready.await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), nonmatching_dropped)
        .await
        .unwrap()
        .unwrap();
    caller.abort();
    let _ = caller.await;
    tokio::time::timeout(std::time::Duration::from_secs(3), matching_dropped)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let doc = read_cron_tasks(fs.as_ref(), temp.path()).await;
            if doc.tasks[0]
                .automation
                .as_ref()
                .unwrap()
                .runs
                .iter()
                .any(|run| run.id == run_id && run.status == cron::AutomationRunStatus::Cancelled)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelled_claim_settles_after_foreign_runtime_is_destroyed_without_executing() {
    let (temp, fs, _) = claimed_test_run().await;
    let mut doc = read_cron_tasks(fs.as_ref(), temp.path()).await;
    doc.tasks[0].automation.as_mut().unwrap().runs.clear();
    cron::tasks_file::write_automation_tasks_body(
        fs.as_ref(),
        temp.path(),
        &cron::serialize_tasks(&doc),
    )
    .await
    .unwrap();
    let root = temp.path().to_path_buf();
    let claim_root = root.clone();
    let claim_fs = fs.clone();
    let supervisor_fs = fs.clone();
    let (claim_started, started) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let executed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_execution = executed.clone();
    std::thread::spawn(move || {
        let foreign_runtime = tokio::runtime::Runtime::new().unwrap();
        foreign_runtime.block_on(async {
            let caller = tokio::spawn(supervise_mobile_automation(
                supervisor_fs,
                root,
                Arc::new(platform_posix_minimal::PosixClock::new()),
                async move {
                    let _ = claim_started.send(());
                    released.await.unwrap();
                    cron::claim_automation_run_now(
                        claim_fs.as_ref(),
                        &claim_root,
                        "d12345678",
                        std::time::SystemTime::now(),
                    )
                    .await
                },
                move |_| async move {
                    observed_execution.store(true, std::sync::atomic::Ordering::Release);
                    std::future::pending().await
                },
            ));
            started.await.unwrap();
            caller.abort();
            let _ = caller.await;
        });
        drop(foreign_runtime);
    })
    .join()
    .unwrap();
    // The durable claim completes only after both caller and runtime died.
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let doc = read_cron_tasks(fs.as_ref(), temp.path()).await;
            if doc.tasks[0]
                .automation
                .as_ref()
                .unwrap()
                .runs
                .iter()
                .any(|run| run.status == cron::AutomationRunStatus::Cancelled)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!executed.load(std::sync::atomic::Ordering::Acquire));
    assert!(cron::claim_automation_run_now(
        fs.as_ref(),
        temp.path(),
        "d12345678",
        std::time::SystemTime::now()
    )
    .await
    .is_some());
}

#[tokio::test]
async fn dropping_host_future_finishes_claim_after_dropping_execution_and_releases_next_run() {
    let (temp, fs, request) = claimed_test_run().await;
    let id = request.run_id.clone();
    let gate = Arc::new(Mutex::new(()));
    let execution_gate = gate.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let caller = tokio::spawn(supervise_mobile_automation(
        fs.clone(),
        temp.path().into(),
        Arc::new(platform_posix_minimal::PosixClock::new()),
        std::future::ready(Some(request)),
        |_| async move {
            let _lease = execution_gate.lock().await;
            let _ = started_tx.send(());
            std::future::pending().await
        },
    ));
    started_rx.await.unwrap();
    assert!(gate.try_lock().is_err());
    caller.abort();
    let _ = caller.await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let doc = read_cron_tasks(fs.as_ref(), temp.path()).await;
            let run = doc.tasks[0]
                .automation
                .as_ref()
                .unwrap()
                .runs
                .iter()
                .find(|run| run.id == id)
                .unwrap();
            if run.status == cron::AutomationRunStatus::Cancelled {
                assert!(
                    gate.try_lock().is_ok(),
                    "terminal must follow execution teardown"
                );
                assert!(run.finished_at.is_some());
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let next = cron::claim_automation_run_now(
        fs.as_ref(),
        temp.path(),
        "d12345678",
        std::time::SystemTime::now(),
    )
    .await
    .unwrap();
    assert_ne!(next.run_id, id);
}

#[tokio::test]
async fn scheduled_empty_anchor_is_replayable_before_first_prompt() {
    use platform_posix_minimal::PosixFileSystem;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join(".lingxi");
    let cwd = temp.path().join("scheduled/workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let id = uuid::Uuid::new_v4();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    let path = orchestrator::transcript_paths::main_transcript_path(
        &home,
        &cwd.to_string_lossy(),
        &id.to_string(),
    );
    let writer = session::jsonl::writer::JsonlWriter::new(path, fs.clone());
    writer
        .append_mobile_empty_session(&id.to_string(), "Scheduled task")
        .await
        .unwrap();
    writer
        .append_session_mode(session::jsonl::SessionMode::Code.as_str())
        .await
        .unwrap();
    assert!(
        cron_replay_session(&home, &cwd.to_string_lossy(), id, fs.clone())
            .await
            .unwrap()
            .is_none()
    );
    let path = session::jsonl::session_path(&home, &cwd.to_string_lossy(), &id.to_string());
    std::fs::remove_file(&path).unwrap();
    assert!(
        cron_replay_session(&home, &cwd.to_string_lossy(), id, fs.clone())
            .await
            .is_err()
    );
    std::fs::write(&path, "").unwrap();
    assert!(cron_replay_session(&home, &cwd.to_string_lossy(), id, fs)
        .await
        .is_err());
}

#[tokio::test]
async fn scheduled_existing_session_uses_its_persisted_mode() {
    use platform_posix_minimal::PosixFileSystem;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join(".lingxi");
    let cwd = temp.path().join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    for mode in [
        session::jsonl::SessionMode::Chat,
        session::jsonl::SessionMode::Code,
    ] {
        let id = uuid::Uuid::new_v4();
        let path = session::jsonl::session_path(&home, &cwd.to_string_lossy(), &id.to_string());
        let writer = session::jsonl::JsonlWriter::new(path, fs.clone());
        writer
            .append_mobile_empty_session(&id.to_string(), "Existing session")
            .await
            .unwrap();
        writer.append_session_mode(mode.as_str()).await.unwrap();
        assert_eq!(
            cron_target_session_mode(&home, &cwd.to_string_lossy(), id, fs.clone())
                .await
                .unwrap(),
            mode
        );
    }
    assert!(
        cron_target_session_mode(&home, &cwd.to_string_lossy(), uuid::Uuid::new_v4(), fs)
            .await
            .unwrap_err()
            .starts_with("paused:")
    );
}

#[test]
fn only_naturally_completed_turns_are_successful() {
    use orchestrator::conversation::TurnOutcome;
    assert!(cron_scheduled_turn_outcome(TurnOutcome::EndTurn).is_ok());
    assert!(cron_scheduled_turn_outcome(TurnOutcome::Cancelled)
        .unwrap_err()
        .starts_with(cron::AUTOMATION_CANCELLED_PREFIX));
    let exhausted = cron_scheduled_turn_outcome(TurnOutcome::MaxTurns).unwrap_err();
    assert!(exhausted.contains("maximum turns"));
    assert!(!exhausted.starts_with(cron::AUTOMATION_CANCELLED_PREFIX));
}

#[test]
fn inactive_automations_never_offer_a_mobile_wake() {
    for status in ["paused", "completed"] {
        let dto = cron_task_dto(task(status), std::time::SystemTime::now());
        assert!(dto.next_fire_ms.is_none());
        assert!(dto.automation_json.unwrap().contains(status));
    }
}

#[test]
fn completing_one_shot_retains_automation_configuration() {
    let mut document = cron::ScheduledTasks::default();
    document.tasks.push(task("active"));
    finalize_cron_occurrence(&mut document, "d12345678", 3000);
    assert_eq!(document.tasks.len(), 1);
    assert_eq!(document.tasks[0].last_fired_at, Some(3000));
    assert_eq!(
        document.tasks[0].automation.as_ref().unwrap().status,
        cron::AutomationStatus::Completed
    );
}

#[test]
fn malformed_configuration_cannot_replace_a_saved_task() {
    assert!(decode_cron_automation("{}").is_err());
    assert!(decode_cron_automation(r#"{"version":2,"model":""}"#).is_err());
    assert!(decode_cron_automation("").unwrap().is_none());
}
#[tokio::test]
async fn legacy_scope_migration_keeps_loop_sentinels_without_reviving_them() {
    use platform_posix_minimal::{PosixClock, PosixFileSystem};
    for pending_marker in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let cwd = root.join("scheduled/workspace");
        std::fs::create_dir_all(root.join(".lingxi")).unwrap();
        std::fs::create_dir_all(cwd.join(".lingxi")).unwrap();
        let sentinels = [
            cron::AUTONOMOUS_LOOP_SENTINEL,
            cron::AUTONOMOUS_LOOP_DYNAMIC_SENTINEL,
            cron::LOOP_FILE_SENTINEL,
            cron::LOOP_FILE_DYNAMIC_SENTINEL,
        ];
        let mut original = cron::ScheduledTasks::default();
        for (index, prompt) in sentinels.iter().enumerate() {
            let mut legacy = task("active");
            legacy.id = format!("loop-{index}");
            legacy.prompt = (*prompt).into();
            legacy.automation = None;
            original.tasks.push(legacy);
        }
        let mut ordinary = task("active");
        ordinary.automation = None;
        original.tasks.push(ordinary);
        let snapshot = cron::serialize_tasks(&original);
        std::fs::write(cron::scheduled_tasks_path(root), &snapshot).unwrap();
        if pending_marker {
            std::fs::write(
                root.join(".lingxi/cron-v2-migration.json"),
                serde_json::json!({"version":2,"completed":false,"source":snapshot}).to_string(),
            )
            .unwrap();
        }
        let store = MobileCronStoreHandle::new(
            cwd.clone(),
            Arc::new(PosixFileSystem::new(root.to_path_buf())),
            Arc::new(PosixClock::new()),
        );
        store.migrate_legacy_scope().await.unwrap();
        store.migrate_legacy_scope().await.unwrap();
        let remaining = cron::tasks_file::parse_automation_tasks(
            &std::fs::read_to_string(cron::scheduled_tasks_path(root)).unwrap(),
        );
        assert_eq!(remaining.tasks.len(), 4);
        for (index, task) in remaining.tasks.iter().enumerate() {
            assert_eq!(task.id, format!("loop-{index}"));
            assert_eq!(task.prompt, sentinels[index]);
            assert!(task.automation.is_none());
        }
        let destination = cron::tasks_file::parse_automation_tasks(
            &std::fs::read_to_string(cron::scheduled_tasks_path(&cwd)).unwrap(),
        );
        assert_eq!(destination.tasks.len(), 1);
        assert_eq!(destination.tasks[0].id, "d12345678");
        assert_eq!(destination.tasks[0].prompt, "brief");
        assert_eq!(
            destination.tasks[0].automation.as_ref().unwrap().status,
            cron::AutomationStatus::Paused
        );
    }
}

#[tokio::test]
async fn migration_marker_recovers_each_publication_boundary_without_duplicate_tasks() {
    use platform_posix_minimal::{PosixClock, PosixFileSystem};
    for phase in 0..3 {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let cwd = root.join("scheduled/workspace");
        std::fs::create_dir_all(root.join(".lingxi")).unwrap();
        std::fs::create_dir_all(cwd.join(".lingxi")).unwrap();
        let mut original = cron::ScheduledTasks::default();
        let mut legacy_task = task("active");
        legacy_task.automation = None;
        original.tasks.push(legacy_task);
        let snapshot = cron::serialize_tasks(&original);
        std::fs::write(
            root.join(".lingxi/cron-v2-migration.json"),
            serde_json::json!({"version":2,"completed":false,"source":snapshot}).to_string(),
        )
        .unwrap();
        let empty = cron::serialize_tasks(&cron::ScheduledTasks::default());
        std::fs::write(
            cron::scheduled_tasks_path(root),
            if phase == 0 { &snapshot } else { &empty },
        )
        .unwrap();
        if phase == 2 {
            std::fs::write(cron::scheduled_tasks_path(&cwd), &snapshot).unwrap();
        }
        let store = MobileCronStoreHandle::new(
            cwd,
            Arc::new(PosixFileSystem::new(root.to_path_buf())),
            Arc::new(PosixClock::new()),
        );
        store
            .set_migration_defaults(
                "anthropic/claude-sonnet-4-6".into(),
                r#"{"type":"automatic"}"#.into(),
            )
            .await
            .unwrap();
        assert_eq!(store.list().await.len(), 1);
        assert_eq!(store.list().await.len(), 1);
        let migrated = store.list().await.remove(0);
        let automation: cron::CronAutomation =
            serde_json::from_str(&migrated.automation_json.unwrap()).unwrap();
        assert_eq!(automation.status, cron::AutomationStatus::Active);
        assert_eq!(automation.model, "anthropic/claude-sonnet-4-6");
        assert!(cron::tasks_file::parse_automation_tasks(
            &std::fs::read_to_string(cron::scheduled_tasks_path(root)).unwrap()
        )
        .tasks
        .is_empty());
        let marker: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".lingxi/cron-v2-migration.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(marker["completed"], true);
    }
}
