use super::*;
use tokio::sync::mpsc;

struct RejectCronOwner {
    begun: Arc<std::sync::Mutex<Vec<CronOwnerCandidateDto>>>,
    confirmed: Arc<std::sync::Mutex<Vec<CronOwnerReservationDto>>>,
}

struct RejectCronCandidate;

#[async_trait::async_trait]
impl MobileCronOwnerAdmission for RejectCronCandidate {
    async fn begin_owner(&self, _: CronOwnerCandidateDto) -> bool {
        false
    }
    async fn confirm_owner(&self, _: CronOwnerReservationDto) -> bool {
        panic!("rejected candidate must never be confirmed")
    }
}

#[async_trait::async_trait]
impl MobileCronOwnerAdmission for RejectCronOwner {
    async fn begin_owner(&self, candidate: CronOwnerCandidateDto) -> bool {
        self.begun.lock().unwrap().push(candidate);
        true
    }
    async fn confirm_owner(&self, reservation: CronOwnerReservationDto) -> bool {
        self.confirmed.lock().unwrap().push(reservation);
        false
    }
}

fn task(status: &str) -> cron::CronTask {
    let body = format!(
        r#"{{"id":"d12345678","cron":"0 9 * * *","prompt":"brief","createdAt":1,"recurring":false,"automation":{{"version":2,"status":"{status}","model":"anthropic/claude-sonnet-4-6","reasoning":{{"type":"automatic"}},"runMode":"new_session","sessionMode":"code","sessionScopeKey":"project.test","notificationPolicy":"all"}}}}"#
    );
    serde_json::from_str(&body).unwrap()
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
async fn task_session_retries_with_new_owner_after_child_creation_failure() {
    use crate::mobile::test_support::{test_config, HostFakePlatform};
    let (temp, fs, mut request) = claimed_test_run().await;
    let mut saved = read_cron_tasks(fs.as_ref(), temp.path()).await;
    let automation = saved.tasks[0].automation.as_mut().unwrap();
    automation.run_mode = cron::RunMode::TaskSession;
    automation.reasoning = serde_json::json!({"type":"invalid-test-reasoning"});
    request.task.automation.as_mut().unwrap().run_mode = automation.run_mode;
    request.task.automation.as_mut().unwrap().reasoning = automation.reasoning.clone();
    cron::tasks_file::write_automation_tasks_body(
        fs.as_ref(),
        temp.path(),
        &cron::serialize_tasks(&saved),
    )
    .await
    .unwrap();
    let mut cfg = test_config(temp.path());
    cfg.lingxi_home = temp.path().join("session-home");
    std::fs::create_dir_all(&cfg.lingxi_home).unwrap();
    // Assembly cannot create the target bucket, but the scheduler store is
    // healthy. This deterministically fails before the first transcript row.
    std::fs::write(cfg.lingxi_home.join("projects"), b"not a directory").unwrap();
    let firer = MobileTurnFirer {
        cfg: cfg.clone(),
        platform: Arc::new(HostFakePlatform::new(temp.path().to_path_buf())),
    };
    let error = firer
        .fire_session_with_admission(
            "test".into(),
            request.task.automation.as_ref(),
            Some(&request),
            Some(super::automation::allow_cron_owner()),
        )
        .await
        .unwrap_err();
    assert!(error.starts_with("paused:"), "{error}");
    let ledger = cron::list_owner_reservations_strict(fs.as_ref(), temp.path())
        .await
        .unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].session_mode, "code");
    assert_eq!(ledger[0].session_scope_key, "project.test");
    assert!(!std::path::Path::new(&ledger[0].session_transcript_path).exists());
    let saved = read_cron_tasks(fs.as_ref(), temp.path()).await;
    let automation = saved.tasks[0].automation.as_ref().unwrap();
    assert!(automation.owned_session_id.is_none());
    assert!(automation.runs[0].session_id.is_none());

    // Repairing the build must allocate a fresh owner, rather than attempting
    // to restore the reserved source which was never materialized.
    std::fs::remove_file(cfg.lingxi_home.join("projects")).unwrap();
    let error = firer
        .fire_session_with_admission(
            "test".into(),
            request.task.automation.as_ref(),
            Some(&request),
            Some(super::automation::allow_cron_owner()),
        )
        .await
        .unwrap_err();
    assert!(error.starts_with("paused:"), "{error}");
    let ledger_after = cron::list_owner_reservations_strict(fs.as_ref(), temp.path())
        .await
        .unwrap();
    assert_eq!(ledger_after.len(), 2);
    assert_eq!(ledger_after[0], ledger[0]);
    assert_ne!(ledger_after[1].session_id, ledger[0].session_id);
    assert!(std::path::Path::new(&ledger_after[1].session_transcript_path).is_file());
    assert!(!std::path::Path::new(&ledger[0].session_transcript_path).exists());
    let saved = read_cron_tasks(fs.as_ref(), temp.path()).await;
    let automation = saved.tasks[0].automation.as_ref().unwrap();
    assert_eq!(
        automation.owned_session_id.as_deref(),
        Some(ledger_after[1].session_id.as_str())
    );
    assert_eq!(automation.runs[0].session_id, automation.owned_session_id);
    assert_eq!(
        automation.runs[0].session_transcript_path.as_deref(),
        Some(ledger_after[1].session_transcript_path.as_str())
    );
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
                    session_transcript_path: None,
                    session_mode: None,
                    session_scope_key: None,
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
async fn supervisor_reports_unknown_without_replaying_after_persistent_commit_failure() {
    let (temp, fs, request) = claimed_test_run().await;
    let task_path = cron::scheduled_tasks_path(temp.path());
    let saved_path = temp.path().join("unavailable-scheduled-tasks.json");
    let hidden_task_path = task_path.clone();
    let hidden_saved_path = saved_path.clone();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = executions.clone();
    let session_id = uuid::Uuid::new_v4().to_string();
    let result_id = session_id.clone();
    let dto = tokio::time::timeout(
        std::time::Duration::from_secs(25),
        supervise_mobile_automation(
            fs.clone(),
            temp.path().into(),
            Arc::new(platform_posix_minimal::PosixClock::new()),
            std::future::ready(Some(request)),
            move |_| async move {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::fs::rename(hidden_task_path, hidden_saved_path).unwrap();
                Ok(cron::AutomationRunResult {
                    session_id: result_id,
                    session_transcript_path: Some("/source/session.jsonl".into()),
                    session_mode: Some("code".into()),
                    session_scope_key: Some("project.test".into()),
                    summary: "the turn executed".into(),
                })
            },
        ),
    )
    .await
    .unwrap()
    .expect("a claimed execution cannot be reported as skipped");
    assert!(matches!(
        dto.status,
        CronFireStatusDto::OutcomeUnknown { .. }
    ));
    assert!(!dto.retryable);
    assert_eq!(dto.session_id.as_deref(), Some(session_id.as_str()));
    assert_eq!(
        dto.session_transcript_path.as_deref(),
        Some("/source/session.jsonl")
    );
    assert_eq!(dto.session_mode.as_deref(), Some("code"));
    assert_eq!(dto.session_scope_key.as_deref(), Some("project.test"));
    std::fs::rename(saved_path, task_path).unwrap();
    assert!(cron::claim_automation_run_now(
        fs.as_ref(),
        temp.path(),
        "d12345678",
        std::time::SystemTime::now(),
    )
    .await
    .is_none());
    let saved = read_cron_tasks(fs.as_ref(), temp.path()).await;
    assert_eq!(
        saved.tasks[0].automation.as_ref().unwrap().runs[0].status,
        cron::AutomationRunStatus::Running
    );
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn strict_store_listing_distinguishes_missing_valid_and_damaged_receipts() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix_minimal::PosixFileSystem::new(
        temp.path().to_path_buf(),
    ));
    let store = MobileCronStoreHandle::new(
        temp.path().to_path_buf(),
        fs.clone(),
        Arc::new(platform_posix_minimal::PosixClock::new()),
    );
    assert!(store.list_strict().await.unwrap().is_empty());
    let mut doc = cron::ScheduledTasks::default();
    let mut saved = task("active");
    let run: cron::AutomationRun = serde_json::from_value(serde_json::json!({
        "id":"run", "taskId":"d12345678", "scheduledAt":1, "startedAt":1,
        "finishedAt":2, "status":"succeeded", "model":"anthropic/model",
        "reasoning":{"type":"automatic"},
        "sessionId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "sessionTranscriptPath":"/source/session.jsonl", "sessionMode":"code",
        "sessionScopeKey":"project.test", "summary":"done", "error":null
    }))
    .unwrap();
    saved.automation.as_mut().unwrap().runs.push(run);
    doc.tasks.push(saved);
    cron::tasks_file::write_automation_tasks_body(
        fs.as_ref(),
        temp.path(),
        &cron::serialize_tasks(&doc),
    )
    .await
    .unwrap();
    assert_eq!(store.list_strict().await.unwrap().len(), 1);
    let path = cron::scheduled_tasks_path(temp.path());
    let mut partial: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    partial["tasks"][0]["automation"]["runs"][0]["sessionId"] = serde_json::Value::Null;
    std::fs::write(&path, partial.to_string()).unwrap();
    assert!(
        store.list_strict().await.is_err(),
        "partial receipts must fail closed"
    );
    partial["tasks"][0]["cron"] = serde_json::json!("not a cron");
    std::fs::write(&path, partial.to_string()).unwrap();
    assert!(
        store.list_strict().await.is_err(),
        "unmodeled tasks must fail closed"
    );
    std::fs::write(&path, "{broken").unwrap();
    assert!(store.list_strict().await.is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "{broken");
}

#[tokio::test]
async fn retargeted_cron_runtime_rebinds_workflows_permissions_and_transcript_identity() {
    use crate::mobile::test_support::{test_config, FakeListener, HostFakePlatform};
    use lingxi_core::host::PermissionGate as _;
    let temp = tempfile::tempdir().unwrap();
    let mut config = test_config(temp.path());
    config.session_mode = session::jsonl::SessionMode::Chat;
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
    let target = lingxi_core::types::SessionId::new();
    let target_uuid = target.as_uuid().to_string();
    let target_path = session::jsonl::session_path(&home, &cwd, &target_uuid);
    let target_writer = session::jsonl::JsonlWriter::new(
        target_path.clone(),
        Arc::new(platform_posix_minimal::PosixFileSystem::new(
            temp.path().to_path_buf(),
        )),
    );
    target_writer
        .initialize_fixed_session_mode(&target_uuid, session::jsonl::SessionMode::Chat, 3)
        .await
        .unwrap();
    target_writer
        .append_mobile_empty_session(&target_uuid, "Scheduled", &cwd)
        .await
        .unwrap();
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
            lingxi_core::host::ResumeRuntimeSnapshot {
                transcript_path: Some(target_path.clone()),
                transcript_metadata: Some(session::JsonlWriter::capture_session_metadata_source(
                    &std::fs::read_to_string(&target_path).unwrap(),
                    &target_uuid,
                )),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    runtime
        .retarget_session_context(&home, target, &cwd)
        .await
        .unwrap();
    let restored = runtime.orchestrator.conversation_mode().await;
    assert_eq!(restored.mode, session::jsonl::SessionMode::Chat);
    assert_eq!(*runtime.active_session_uuid.lock().unwrap(), target_uuid);
    assert_eq!(
        runtime.session_writer.active_path(),
        orchestrator::transcript_paths::main_transcript_path(&home, &cwd, &target_uuid)
    );
    let workflows = lingxi_core::host::task_registry::TaskRegistryHandle::list_workflows(
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
            &lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &serde_json::json!({"file_path":"test.txt"}),
            )
            .expect("test permission input is valid JSON"),
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
async fn mounted_workspace_scheduled_run_reuses_the_selected_host_transcript() {
    use crate::mobile::test_support::{test_config, HostFakePlatform, MountTableRuntime};
    use mobile_linux_api::MobileLinuxRuntime;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime");
    let dirs = crate::mobile::mobile_user_directories(root.display().to_string()).unwrap();
    dirs.prepare().unwrap();
    let workspace = std::path::Path::new(&dirs.projects).join("scheduled-test");
    std::fs::create_dir_all(&workspace).unwrap();
    let guest = format!("{}/scheduled-test", dirs.projects_guest_path);
    let linux = Arc::new(MountTableRuntime::default());
    linux
        .configure_mounts(vec![
            mobile_linux_api::MountSpec {
                host_path: std::path::PathBuf::from(&dirs.projects),
                guest_path: dirs.projects_guest_path.clone(),
                read_only: false,
                purpose: mobile_linux_api::MountPurpose::Workspace,
            },
            mobile_linux_api::MountSpec {
                host_path: std::path::PathBuf::from(&dirs.config_home),
                guest_path: dirs.config_guest_path.clone(),
                read_only: false,
                purpose: mobile_linux_api::MountPurpose::Workspace,
            },
        ])
        .await
        .unwrap();
    let platform: Arc<dyn Platform> =
        Arc::new(HostFakePlatform::new(temp.path().to_path_buf()).with_mobile_linux(linux.clone()));
    let model_fs = platform.filesystem();
    assert!(model_fs
        .write_file("/tmp/unmounted-guest-file", "forbidden")
        .await
        .is_err());
    let host_fs = model_fs.host_filesystem().unwrap();
    let mut cfg = test_config(&workspace);
    cfg.logical_cwd = Some(guest.clone().into());
    cfg.lingxi_home = dirs.config_home.clone().into();
    let session = uuid::Uuid::new_v4();
    let path = session::jsonl::session_path(&cfg.lingxi_home, &guest, &session.to_string());
    let writer = session::jsonl::JsonlWriter::new(path.clone(), host_fs.clone());
    writer
        .initialize_fixed_session_mode(&session.to_string(), session::jsonl::SessionMode::Code, 0)
        .await
        .unwrap();
    writer
        .append_mobile_empty_session(&session.to_string(), "Scheduled source", &guest)
        .await
        .unwrap();
    let original_source = std::fs::read(&path).unwrap();
    let mut scheduled = task("active");
    scheduled.recurring = Some(true);
    let automation = scheduled.automation.as_mut().unwrap();
    automation.run_mode = cron::RunMode::SelectedSession;
    automation.target_session_id = Some(session.to_string());
    automation.target_session_transcript_path = Some(path.to_string_lossy().into_owned());
    automation.target_session_mode = Some("code".into());
    automation.target_session_scope_key = Some("project.test".into());
    // Fail after binding and before model I/O, so the test stays deterministic.
    automation.reasoning = serde_json::json!({"type":"invalid-test-reasoning"});
    let mut doc = cron::ScheduledTasks::default();
    doc.tasks.push(scheduled);
    cron::tasks_file::write_automation_tasks_body(
        host_fs.as_ref(),
        &workspace,
        &cron::serialize_tasks(&doc),
    )
    .await
    .unwrap();
    let request = cron::claim_automation_run_now(
        host_fs.as_ref(),
        &workspace,
        "d12345678",
        std::time::SystemTime::now(),
    )
    .await
    .unwrap();
    let firer = MobileTurnFirer { cfg, platform };
    let expected_error = format!(
        "paused: {}",
        serde_json::from_value::<lingxi_core::host::ReasoningSelection>(
            request.task.automation.as_ref().unwrap().reasoning.clone(),
        )
        .unwrap_err()
    );
    for _ in 0..2 {
        let error = firer
            .fire_session_with_admission(
                "scheduled".into(),
                request.task.automation.as_ref(),
                Some(&request),
                Some(super::automation::allow_cron_owner()),
            )
            .await
            .unwrap_err();
        assert_eq!(error, expected_error);
        let stored = read_cron_tasks(host_fs.as_ref(), &workspace).await;
        let run = &stored.tasks[0].automation.as_ref().unwrap().runs[0];
        assert_eq!(
            run.session_id.as_deref(),
            Some(session.to_string().as_str())
        );
        assert!(std::fs::read(&path).unwrap().starts_with(&original_source));
    }
    let relative = path.strip_prefix(&firer.cfg.lingxi_home).unwrap();
    let guest_source = std::path::Path::new(&dirs.config_guest_path)
        .join(relative)
        .to_string_lossy()
        .into_owned();
    let mut updated = read_cron_tasks(host_fs.as_ref(), &workspace).await;
    let updated_automation = updated.tasks[0].automation.as_mut().unwrap();
    updated_automation.runs.clear();
    updated_automation.target_session_transcript_path = Some(guest_source.clone());
    cron::tasks_file::write_automation_tasks_body(
        host_fs.as_ref(),
        &workspace,
        &cron::serialize_tasks(&updated),
    )
    .await
    .unwrap();
    let guest_request = cron::claim_automation_run_now(
        host_fs.as_ref(),
        &workspace,
        "d12345678",
        std::time::SystemTime::now(),
    )
    .await
    .unwrap();
    let before_conflict = std::fs::read(cron::scheduled_tasks_path(&workspace)).unwrap();
    let source_before_conflict = std::fs::read(&path).unwrap();
    let ledger_before_conflict = cron::list_owner_reservations_strict(host_fs.as_ref(), &workspace)
        .await
        .unwrap();
    let conflict = firer
        .fire_session_with_admission(
            "scheduled".into(),
            guest_request.task.automation.as_ref(),
            Some(&guest_request),
            Some(super::automation::allow_cron_owner()),
        )
        .await
        .unwrap_err();
    assert!(
        conflict.contains("different owner reservation"),
        "{conflict}"
    );
    assert_eq!(
        std::fs::read(cron::scheduled_tasks_path(&workspace)).unwrap(),
        before_conflict
    );
    assert_eq!(std::fs::read(&path).unwrap(), source_before_conflict);
    assert_eq!(
        cron::list_owner_reservations_strict(host_fs.as_ref(), &workspace)
            .await
            .unwrap(),
        ledger_before_conflict
    );

    // A different owner may be admitted with guest coordinates. The selected
    // spelling is retained throughout that owner's binding and receipt.
    let guest_session = uuid::Uuid::new_v4();
    let guest_path =
        session::jsonl::session_path(&firer.cfg.lingxi_home, &guest, &guest_session.to_string());
    let guest_writer = session::jsonl::JsonlWriter::new(guest_path.clone(), host_fs.clone());
    guest_writer
        .initialize_fixed_session_mode(
            &guest_session.to_string(),
            session::jsonl::SessionMode::Code,
            0,
        )
        .await
        .unwrap();
    guest_writer
        .append_mobile_empty_session(&guest_session.to_string(), "Guest source", &guest)
        .await
        .unwrap();
    let guest_relative = guest_path.strip_prefix(&firer.cfg.lingxi_home).unwrap();
    let guest_source = std::path::Path::new(&dirs.config_guest_path)
        .join(guest_relative)
        .to_string_lossy()
        .into_owned();
    let mut updated = read_cron_tasks(host_fs.as_ref(), &workspace).await;
    let automation = updated.tasks[0].automation.as_mut().unwrap();
    automation.runs.clear();
    automation.target_session_id = Some(guest_session.to_string());
    automation.target_session_transcript_path = Some(guest_source.clone());
    cron::tasks_file::write_automation_tasks_body(
        host_fs.as_ref(),
        &workspace,
        &cron::serialize_tasks(&updated),
    )
    .await
    .unwrap();
    let guest_request = cron::claim_automation_run_now(
        host_fs.as_ref(),
        &workspace,
        "d12345678",
        std::time::SystemTime::now(),
    )
    .await
    .unwrap();
    for _ in 0..2 {
        assert_eq!(
            firer
                .fire_session_with_admission(
                    "scheduled".into(),
                    guest_request.task.automation.as_ref(),
                    Some(&guest_request),
                    Some(super::automation::allow_cron_owner()),
                )
                .await
                .unwrap_err(),
            expected_error
        );
    }
    let stored = read_cron_tasks(host_fs.as_ref(), &workspace).await;
    let run = stored.tasks[0]
        .automation
        .as_ref()
        .unwrap()
        .runs
        .first()
        .unwrap();
    assert_eq!(
        run.session_transcript_path.as_deref(),
        Some(guest_source.as_str())
    );
    let before = std::fs::read(cron::scheduled_tasks_path(&workspace)).unwrap();
    linux
        .configure_mounts(vec![
            mobile_linux_api::MountSpec {
                host_path: std::path::PathBuf::from(&dirs.projects),
                guest_path: dirs.projects_guest_path.clone(),
                read_only: true,
                purpose: mobile_linux_api::MountPurpose::Workspace,
            },
            mobile_linux_api::MountSpec {
                host_path: std::path::PathBuf::from(&dirs.config_home),
                guest_path: dirs.config_guest_path.clone(),
                read_only: false,
                purpose: mobile_linux_api::MountPurpose::Workspace,
            },
        ])
        .await
        .unwrap();
    let error = firer
        .fire_session_with_admission(
            "scheduled".into(),
            guest_request.task.automation.as_ref(),
            Some(&guest_request),
            Some(super::automation::allow_cron_owner()),
        )
        .await
        .unwrap_err();
    assert!(error.contains("read-only"), "{error}");
    assert_eq!(
        std::fs::read(cron::scheduled_tasks_path(&workspace)).unwrap(),
        before
    );
}

#[tokio::test]
async fn global_selected_conversation_retains_exact_source_and_relocated_cwd() {
    use crate::mobile::test_support::{test_config, HostFakePlatform};
    let temp = tempfile::tempdir().unwrap();
    let controller = temp.path().join(".atlas-workspaces/scheduled");
    let original_cwd = temp.path().join("ordinary-global");
    let moved_cwd = temp.path().join("relocated-global");
    for path in [&controller, &original_cwd, &moved_cwd] {
        std::fs::create_dir_all(path).unwrap();
    }
    let mut cfg = test_config(&controller);
    cfg.brand_runtime_profile =
        mobile_linux_api::BrandRuntimeProfile::new("atlas", "atlas_user", ".atlas");
    cfg.lingxi_home = temp.path().join(".atlas");
    let platform: Arc<dyn Platform> = Arc::new(HostFakePlatform::new(temp.path().to_path_buf()));
    let fs = platform.filesystem();
    let session = uuid::Uuid::new_v4();
    let path = session::jsonl::session_path(
        &cfg.lingxi_home,
        &original_cwd.to_string_lossy(),
        &session.to_string(),
    );
    let durable = Arc::new(session::jsonl::DurableTranscriptWriter::open(temp.path()).unwrap());
    let writer =
        session::jsonl::JsonlWriter::new(path.clone(), fs.clone()).with_durable_lock(durable);
    writer
        .activate_session_target(
            lingxi_core::types::SessionId::from_uuid(session),
            path.clone(),
            original_cwd.clone(),
        )
        .unwrap();
    writer
        .initialize_fixed_session_mode(&session.to_string(), session::jsonl::SessionMode::Chat, 0)
        .await
        .unwrap();
    writer
        .append_mobile_empty_session(
            &session.to_string(),
            "Ordinary Global",
            &original_cwd.to_string_lossy(),
        )
        .await
        .unwrap();
    writer
        .retarget_with_relocation(
            path.clone(),
            &session.to_string(),
            &moved_cwd.to_string_lossy(),
        )
        .await
        .unwrap();
    let source = std::fs::read(&path).unwrap();
    let duplicate_dir = cfg.lingxi_home.join("projects").join("other-global");
    std::fs::create_dir_all(&duplicate_dir).unwrap();
    let duplicate = duplicate_dir.join(format!("{session}.jsonl"));
    std::fs::write(&duplicate, &source).unwrap();
    let mut scheduled = task("active");
    scheduled.recurring = Some(true);
    let automation = scheduled.automation.as_mut().unwrap();
    automation.run_mode = cron::RunMode::SelectedSession;
    automation.target_session_id = Some(session.to_string());
    automation.target_session_transcript_path = Some(path.to_string_lossy().into_owned());
    automation.target_session_mode = Some("chat".into());
    automation.target_session_scope_key = Some("global".into());
    automation.reasoning = serde_json::json!({"type":"invalid-test-reasoning"});
    let mut doc = cron::ScheduledTasks::default();
    doc.tasks.push(scheduled);
    cron::tasks_file::write_automation_tasks_body(
        fs.as_ref(),
        &controller,
        &cron::serialize_tasks(&doc),
    )
    .await
    .unwrap();
    let request = cron::claim_automation_run_now(
        fs.as_ref(),
        &controller,
        "d12345678",
        std::time::SystemTime::now(),
    )
    .await
    .unwrap();
    let firer = MobileTurnFirer { cfg, platform };
    let early_owner = super::automation::allow_cron_owner();
    let lease = super::automation::reserve_mobile_cron_owner(
        early_owner.begin().unwrap(),
        request.task.automation.as_ref().unwrap(),
    )
    .await
    .unwrap();
    let mut changed = request.clone();
    changed
        .task
        .automation
        .as_mut()
        .unwrap()
        .target_session_transcript_path = Some(duplicate.to_string_lossy().into_owned());
    let error = firer
        .fire_automation_with_cancel(&changed, tokio_util::sync::CancellationToken::new(), lease)
        .await
        .unwrap_err();
    assert!(
        error.contains("source changed after owner reservation"),
        "{error}"
    );
    assert!(
        cron::list_owner_reservations_strict(fs.as_ref(), &controller)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read(&path).unwrap(), source);
    assert_eq!(std::fs::read(&duplicate).unwrap(), source);
    let rejected = MobileCronAdmission::new(Box::new(RejectCronCandidate));
    let error = firer
        .fire_session_with_admission(
            "scheduled".into(),
            request.task.automation.as_ref(),
            Some(&request),
            Some(rejected.clone()),
        )
        .await
        .unwrap_err();
    assert!(error.starts_with("busy:"), "{error}");
    rejected.cancel_and_wait().await;
    assert!(
        cron::list_owner_reservations_strict(fs.as_ref(), &controller)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read(&path).unwrap(), source);
    assert_eq!(std::fs::read(&duplicate).unwrap(), source);
    let mut wrong_mode = request.task.automation.clone().unwrap();
    wrong_mode.target_session_mode = Some("code".into());
    assert!(firer
        .fire_session_with_admission(
            "scheduled".into(),
            Some(&wrong_mode),
            Some(&request),
            Some(super::automation::allow_cron_owner())
        )
        .await
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), source);
    assert_eq!(std::fs::read(&duplicate).unwrap(), source);
    let confirmed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let begun = Arc::new(std::sync::Mutex::new(Vec::new()));
    let denied = MobileCronAdmission::new(Box::new(RejectCronOwner {
        begun: begun.clone(),
        confirmed: confirmed.clone(),
    }));
    let error = firer
        .fire_session_with_admission(
            "scheduled".into(),
            request.task.automation.as_ref(),
            Some(&request),
            Some(denied.clone()),
        )
        .await
        .unwrap_err();
    assert!(error.starts_with("cancelled:"), "{error}");
    denied.cancel_and_wait().await;
    assert_eq!(
        *begun.lock().unwrap(),
        vec![CronOwnerCandidateDto {
            session_id: session.to_string(),
            session_transcript_path: Some(path.to_string_lossy().into_owned()),
            session_mode: "chat".into(),
            session_scope_key: "global".into(),
        }]
    );
    let confirmed = confirmed.lock().unwrap().clone();
    assert_eq!(
        confirmed,
        vec![CronOwnerReservationDto {
            session_id: session.to_string(),
            session_transcript_path: path.to_string_lossy().into_owned(),
            session_mode: "chat".into(),
            session_scope_key: "global".into(),
        }]
    );
    let ledger = cron::list_owner_reservations_strict(fs.as_ref(), &controller)
        .await
        .unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].session_id, session.to_string());
    assert_eq!(std::fs::read(&duplicate).unwrap(), source);
    let error = firer
        .fire_session_with_admission(
            "scheduled".into(),
            request.task.automation.as_ref(),
            Some(&request),
            Some(super::automation::allow_cron_owner()),
        )
        .await
        .unwrap_err();
    assert!(error.starts_with("paused:"), "{error}");
    let stored = read_cron_tasks(fs.as_ref(), &controller).await;
    let run = &stored.tasks[0].automation.as_ref().unwrap().runs[0];
    assert_eq!(
        run.session_id.as_deref(),
        Some(session.to_string().as_str())
    );
    assert_eq!(run.session_transcript_path.as_deref(), path.to_str());
    assert_eq!(run.session_mode.as_deref(), Some("chat"));
    assert_eq!(run.session_scope_key.as_deref(), Some("global"));
    assert_eq!(std::fs::read(&duplicate).unwrap(), source);
    assert!(!session::jsonl::session_path(
        &firer.cfg.lingxi_home,
        &controller.to_string_lossy(),
        &session.to_string()
    )
    .exists());
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
                cron::bind_automation_run_session_with_source(
                    execute_fs.as_ref(),
                    &execute_root,
                    &request,
                    "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                    "/source/session.jsonl",
                    "code",
                    Some("project.test"),
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
                session_transcript_path: None,
                session_mode: None,
                session_scope_key: None,
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
                session_transcript_path: None,
                session_mode: None,
                session_scope_key: None,
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
async fn scheduled_existing_source_validates_exact_path_and_mode() {
    use crate::mobile::test_support::{test_config, HostFakePlatform};
    use platform_posix_minimal::PosixFileSystem;
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut config = test_config(&cwd);
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    let platform: Arc<dyn Platform> = Arc::new(HostFakePlatform::new(temp.path().to_path_buf()));
    for mode in [
        session::jsonl::SessionMode::Chat,
        session::jsonl::SessionMode::Code,
    ] {
        let id = uuid::Uuid::new_v4();
        let path = session::jsonl::session_path(
            &config.lingxi_home,
            &cwd.to_string_lossy(),
            &id.to_string(),
        );
        let writer = session::jsonl::JsonlWriter::new(path.clone(), fs.clone());
        writer
            .initialize_fixed_session_mode(&id.to_string(), mode, 0)
            .await
            .unwrap();
        writer
            .append_mobile_empty_session(
                &id.to_string(),
                "Scheduled source",
                &cwd.to_string_lossy(),
            )
            .await
            .unwrap();
        config.initial_session_id = Some(id.to_string());
        config.initial_session_transcript_path = Some(path.clone());
        config.session_mode = mode;
        let captured = super::restoration::capture_initial_mobile_session(&config, &platform)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(captured.mode, mode);
        assert_eq!(
            captured.replayed.transcript_path,
            path.canonicalize().unwrap()
        );
        config.session_mode = match mode {
            session::jsonl::SessionMode::Chat => session::jsonl::SessionMode::Code,
            session::jsonl::SessionMode::Code => session::jsonl::SessionMode::Chat,
        };
        assert!(
            super::restoration::capture_initial_mobile_session(&config, &platform)
                .await
                .is_err()
        );
        std::fs::remove_file(&path).unwrap();
        config.session_mode = mode;
        assert!(
            super::restoration::capture_initial_mobile_session(&config, &platform)
                .await
                .is_err()
        );
    }
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
async fn no_project_cron_allocates_brand_workspace_without_moving_its_claim_ledger() {
    use crate::mobile::test_support::{test_config, HostFakePlatform};
    let root = tempfile::tempdir().unwrap();
    let controller = root.path().join(".atlas-workspaces/scheduled");
    std::fs::create_dir_all(&controller).unwrap();
    let mut cfg = test_config(&controller);
    cfg.brand_runtime_profile =
        mobile_linux_api::BrandRuntimeProfile::new("atlas", "atlas_user", ".atlas");
    cfg.lingxi_home = root.path().join(".atlas");
    cfg.scheduler_controller = true;
    std::fs::create_dir_all(&cfg.lingxi_home).unwrap();
    let platform: Arc<dyn Platform> = Arc::new(HostFakePlatform::new(root.path().to_path_buf()));
    let fs = platform.filesystem();
    let mut saved = task("active");
    saved.automation.as_mut().unwrap().session_scope_key = Some("scheduled".into());
    saved.automation.as_mut().unwrap().reasoning =
        serde_json::json!({"type":"invalid-test-reasoning"});
    let mut document = cron::ScheduledTasks::default();
    document.tasks.push(saved);
    cron::tasks_file::write_automation_tasks_body(
        fs.as_ref(),
        &controller,
        &cron::serialize_tasks(&document),
    )
    .await
    .unwrap();
    let request = cron::claim_automation_run_now(
        fs.as_ref(),
        &controller,
        "d12345678",
        std::time::SystemTime::now(),
    )
    .await
    .unwrap();
    let firer = MobileTurnFirer {
        cfg: cfg.clone(),
        platform,
    };
    let error = firer
        .fire_session_with_admission(
            "task prompt".into(),
            request.task.automation.as_ref(),
            Some(&request),
            Some(super::automation::allow_cron_owner()),
        )
        .await
        .unwrap_err();
    assert!(error.starts_with("paused:"), "{error}");
    let stored = read_cron_tasks(fs.as_ref(), &controller).await;
    assert_eq!(
        stored.tasks[0].automation.as_ref().unwrap().runs[0]
            .session_scope_key
            .as_deref(),
        Some("scheduled")
    );
    let id = stored.tasks[0].automation.as_ref().unwrap().runs[0]
        .session_id
        .clone()
        .expect("session bound in the control ledger");
    let workspace = super::scheduled_workspaces::ScheduledWorkspace::prepare(
        &cfg,
        "d12345678",
        "title",
        Some(uuid::Uuid::parse_str(&id).unwrap()),
    )
    .unwrap()
    .unwrap();
    assert!(workspace.host.starts_with(
        branding::workspace_home(&std::fs::canonicalize(&cfg.lingxi_home).unwrap()).join("cron")
    ));
    let transcript =
        session::jsonl::session_path(&cfg.lingxi_home, &workspace.logical.to_string_lossy(), &id);
    assert!(
        transcript.exists(),
        "session anchor must use the generated cwd"
    );
    assert!(
        !cron::scheduled_tasks_path(&workspace.host).exists(),
        "execution directory must not get a duplicate schedule"
    );
}

#[tokio::test]
async fn project_scope_at_scheduled_cwd_never_receives_scheduled_workspace() {
    use crate::mobile::test_support::{test_config, HostFakePlatform};
    use session::jsonl::SessionMode;
    use sha2::Digest;

    for run_mode in [
        cron::RunMode::NewSession,
        cron::RunMode::TaskSession,
        cron::RunMode::SelectedSession,
    ] {
        let root = tempfile::tempdir().unwrap();
        let controller = root.path().join(".atlas-workspaces/scheduled");
        std::fs::create_dir_all(&controller).unwrap();
        let controller = controller.canonicalize().unwrap();
        let mut cfg = test_config(&controller);
        cfg.brand_runtime_profile =
            mobile_linux_api::BrandRuntimeProfile::new("atlas", "atlas_user", ".atlas");
        cfg.lingxi_home = root.path().join(".atlas");
        std::fs::create_dir_all(&cfg.lingxi_home).unwrap();
        cfg.scheduler_controller = true;
        let platform: Arc<dyn Platform> =
            Arc::new(HostFakePlatform::new(root.path().to_path_buf()));
        let fs = platform.filesystem();
        let mut saved = task("active");
        let automation = saved.automation.as_mut().unwrap();
        automation.run_mode = run_mode;
        automation.reasoning = serde_json::json!({"type":"invalid-test-reasoning"});
        let selected_source = if run_mode == cron::RunMode::SelectedSession {
            let id = uuid::Uuid::new_v4();
            let path = session::jsonl::session_path(
                &cfg.lingxi_home,
                &controller.to_string_lossy(),
                &id.to_string(),
            );
            let writer = session::jsonl::JsonlWriter::new(path.clone(), fs.clone());
            writer
                .initialize_fixed_session_mode(&id.to_string(), SessionMode::Code, 0)
                .await
                .unwrap();
            writer
                .append_mobile_empty_session(
                    &id.to_string(),
                    "Project session",
                    &controller.to_string_lossy(),
                )
                .await
                .unwrap();
            automation.target_session_id = Some(id.to_string());
            automation.target_session_transcript_path = Some(path.to_string_lossy().into_owned());
            automation.target_session_mode = Some("code".into());
            automation.target_session_scope_key = Some("project.test".into());
            Some(path)
        } else {
            None
        };
        let mut document = cron::ScheduledTasks::default();
        document.tasks.push(saved);
        cron::tasks_file::write_automation_tasks_body(
            fs.as_ref(),
            &controller,
            &cron::serialize_tasks(&document),
        )
        .await
        .unwrap();
        let request = cron::claim_automation_run_now(
            fs.as_ref(),
            &controller,
            "d12345678",
            std::time::SystemTime::now(),
        )
        .await
        .unwrap();
        let firer = MobileTurnFirer {
            cfg: cfg.clone(),
            platform,
        };
        let error = firer
            .fire_session_with_admission(
                "task prompt".into(),
                request.task.automation.as_ref(),
                Some(&request),
                Some(super::automation::allow_cron_owner()),
            )
            .await
            .unwrap_err();
        assert!(error.starts_with("paused:"), "{run_mode:?}: {error}");
        let stored = read_cron_tasks(fs.as_ref(), &controller).await;
        let run = &stored.tasks[0].automation.as_ref().unwrap().runs[0];
        assert_eq!(
            run.session_scope_key.as_deref(),
            Some("project.test"),
            "{run_mode:?}"
        );
        let source = std::path::PathBuf::from(
            run.session_transcript_path
                .as_deref()
                .expect("session source bound"),
        );
        if let Some(selected) = selected_source {
            assert_eq!(source, selected, "selected source must be retained");
        }
        let content = std::fs::read_to_string(source).unwrap();
        assert!(content
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|value| value.get("cwd").and_then(|value| value.as_str()) == controller.to_str()));
        assert!(!branding::workspace_home(&cfg.lingxi_home)
            .join("cron")
            .exists());
        let memory = cfg
            .lingxi_home
            .join("automations")
            .join(format!("{:x}", sha2::Sha256::digest(b"d12345678")))
            .join("memory.md");
        assert!(!memory.exists());
    }
}

#[tokio::test]
async fn cancelled_automation_waits_for_non_abortable_cleanup_before_persisting() {
    let (temp, fs, request) = claimed_test_run().await;
    let id = request.run_id.clone();
    let started = Arc::new(tokio::sync::Notify::new());
    let cancelling = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let caller = tokio::spawn(supervise_mobile_automation_with_cancel(
        fs.clone(),
        temp.path().into(),
        Arc::new(platform_posix_minimal::PosixClock::new()),
        std::future::ready(Some(request)),
        {
            let started = started.clone();
            let cancelling = cancelling.clone();
            let release = release.clone();
            move |_, cancel| async move {
                started.notify_one();
                cancel.cancelled().await;
                cancelling.notify_one();
                release.notified().await;
                Ok(cron::AutomationRunResult {
                    session_transcript_path: None,
                    session_mode: None,
                    session_scope_key: None,
                    session_id: "same".into(),
                    summary: String::new(),
                })
            }
        },
        None,
    ));
    started.notified().await;
    caller.abort();
    let _ = caller.await;
    tokio::time::timeout(std::time::Duration::from_secs(2), cancelling.notified())
        .await
        .unwrap();
    let stored = read_cron_tasks(fs.as_ref(), temp.path()).await;
    let run = stored.tasks[0]
        .automation
        .as_ref()
        .unwrap()
        .runs
        .iter()
        .find(|run| run.id == id)
        .unwrap();
    assert_ne!(run.status, cron::AutomationRunStatus::Cancelled);
    assert!(run.finished_at.is_none());
    assert!(cron::claim_automation_run_now(
        fs.as_ref(),
        temp.path(),
        "d12345678",
        std::time::SystemTime::now()
    )
    .await
    .is_none());
    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let stored = read_cron_tasks(fs.as_ref(), temp.path()).await;
            if stored.tasks[0]
                .automation
                .as_ref()
                .unwrap()
                .runs
                .iter()
                .any(|run| run.id == id && run.status == cron::AutomationRunStatus::Cancelled)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
