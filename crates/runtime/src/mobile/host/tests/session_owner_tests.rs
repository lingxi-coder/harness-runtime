use super::*;
use lingxi_core::host::OrchestratorHandle;
use lingxi_core::types::SessionId;
use std::time::{Duration, Instant};

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
    assert!(crate::mobile::transcript::session_owner_is_live(home, session_id));
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
            .submit(ClientCommand::ResumeSession {
                session_id: first_id.as_uuid().to_string(),
                cwd: None,
                expected_mode: SessionModeDto::Code,
            })
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
        let body = format!(r#"{{"id":"d12345678","cron":"0 9 * * *","prompt":"brief","createdAt":1,"recurring":false,"automation":{{"version":2,"status":"active","model":"anthropic/claude-sonnet-4-6","reasoning":{{"type":"invalid-test-reasoning"}},"runMode":"selected_session","targetSessionId":"{id}","notificationPolicy":"all"}}}}"#);
        let task: cron::CronTask = serde_json::from_str(&body).unwrap();
        let firer = super::super::MobileTurnFirer { cfg: handle.firer_cfg.clone(), platform: handle.firer_platform.clone() };
        let error = firer.fire_session("scheduled".into(), task.automation.as_ref(), None).await.unwrap_err();
        // Invalid reasoning is checked after constructing the scheduled child;
        // reaching it proves the selected live owner was borrowed successfully.
        assert!(error.starts_with("paused:"), "{error}");
        assert!(!error.contains("already mounted"), "{error}");
        assert!(!error.contains("configuration is unavailable"), "{error}");
        assert!(handle.scheduled_reload.load(std::sync::atomic::Ordering::Acquire));
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
        let body = format!(r#"{{"tasks":[{{"id":"d12345678","cron":"0 9 * * *","prompt":"brief","createdAt":1,"recurring":false,"automation":{{"version":2,"status":"active","model":"anthropic/claude-sonnet-4-6","reasoning":{{"type":"invalid-test-reasoning"}},"runMode":"selected_session","targetSessionId":"{id}","notificationPolicy":"all"}}}}]}}"#);
        cron::tasks_file::write_automation_tasks_body(handle.fs.as_ref(), Path::new(&handle.session_cwd()), &body).await.unwrap();
        let fired = handle.run_cron_task_now("d12345678".into()).await.expect("task must be claimed");
        let super::super::CronFireStatusDto::Failed { message } = fired.status else { panic!("invalid reasoning must fail") };
        assert!(!message.contains("busy:"), "self-targeted cron must not deadlock: {message}");
        assert!(!message.contains("configuration is unavailable"), "live owner must be borrowed: {message}");
        assert!(message.starts_with("paused:"), "invalid reasoning must be reached after mount: {message}");
    });
}
