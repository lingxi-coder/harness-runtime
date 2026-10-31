use super::*;
use crate::mobile::host::canonical_cwd_string;
use lingxi_core::host::OrchestratorHandle;

#[derive(Default)]
struct ObserveStartupProfile {
    calls: StdMutex<Vec<(serde_json::Value, Option<String>)>>,
}

#[async_trait]
impl lingxi_core::host::ProcessRunner for ObserveStartupProfile {
    async fn run(
        &self,
        command: &lingxi_core::host::SandboxedCommand,
    ) -> Result<mobile_linux_api::ProcessOutput, mobile_linux_api::ProcessError> {
        let input: serde_json::Value =
            serde_json::from_str(command.inner().stdin.as_deref().unwrap()).unwrap();
        let transcript = input["transcript_path"]
            .as_str()
            .and_then(|path| std::fs::read_to_string(path).ok());
        self.calls.lock().unwrap().push((input, transcript));
        // A real startup hook can create a resumable history before any native
        // command arrives. Exercise that path without a network or child process.
        Ok(mobile_linux_api::ProcessOutput {
            stdout: r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","initialUserMessage":"startup-profile-probe"}}"#.into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        })
    }

    async fn spawn_background(
        &self,
        _command: &lingxi_core::host::SandboxedCommand,
    ) -> Result<lingxi_core::host::ProcessHandle, mobile_linux_api::ProcessError> {
        Err(mobile_linux_api::ProcessError::Unsupported)
    }

    async fn kill(
        &self,
        _handle: &lingxi_core::host::ProcessHandle,
    ) -> Result<(), mobile_linux_api::ProcessError> {
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

fn profile_test_platform(
    root: &std::path::Path,
    process: Arc<ObserveStartupProfile>,
) -> Arc<dyn lingxi_core::host::Platform> {
    Arc::new(
        HostFakePlatform::new(root.to_path_buf())
            .with_process_runner(process)
            .with_sandbox(Arc::new(
                crate::mobile::test_support::FakeProcessAdmittingSandbox,
            )),
    )
}

#[test]
fn configured_chat_creates_and_cold_resumes_the_original_session_once() {
    let tmp = tempfile::tempdir().unwrap();
    write_project_hook(tmp.path(), "SessionStart");
    let process = Arc::new(ObserveStartupProfile::default());
    let platform = profile_test_platform(tmp.path(), process.clone());
    let mut config = test_config(tmp.path());
    config.session_mode = session::jsonl::SessionMode::Chat;
    config.workspace_trusted = true;
    let first_listener = Arc::new(FakeListener::default());
    let first = build_mobile_engine(
        config.clone(),
        platform.clone(),
        first_listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
    )
    .unwrap();
    let (id, path) = first.runtime().block_on(async {
        let state = first.inner().orchestrator.conversation_mode().await;
        assert_eq!(state.active, session::jsonl::SessionMode::Chat);
        assert_eq!(state.selected, state.active);
        assert_eq!(state.revision, 0);
        let events = drained(&first_listener).await;
        assert!(events.iter().any(|event| matches!(event,
            Ev::SessionStarted { session_id, mode: SessionModeDto::Chat } if session_id == &state.session_id
        )));
        (state.session_id, first.inner().session_writer.active_path())
    });
    {
        let calls = process.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0["source"], "startup");
        let source = calls[0].1.as_deref().expect("mode persisted before hook");
        assert_eq!(
            session::JsonlWriter::fixed_session_mode_from_source(source, &id)
                .unwrap()
                .active,
            session::jsonl::SessionMode::Chat
        );
    }
    drop(first);
    super::session_owner_tests::wait_for_session_release(&config.lingxi_home, &id);
    config.initial_session_id = Some(id.clone());
    let listener = Arc::new(FakeListener::default());
    let resumed = build_mobile_engine(
        config,
        platform,
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
    )
    .unwrap();
    resumed.runtime().block_on(async {
        let state = resumed.inner().orchestrator.conversation_mode().await;
        assert_eq!(state.session_id, id);
        assert_eq!(state.active, session::jsonl::SessionMode::Chat);
        assert_eq!(resumed.inner().session_writer.active_path(), path);
        let events = drained(&listener).await;
        assert!(events.iter().any(|event| matches!(event,
            Ev::SessionResumed { session_id, mode: SessionModeDto::Chat, .. } if session_id == &id
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ev::SessionStarted { .. })));
    });
    let calls = process.calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "exactly one hook for each actual mount");
    assert_eq!(calls[1].0["source"], "resume");
    assert_eq!(calls[1].0["session_id"], id);
}

#[test]
fn cold_resume_restores_recorded_workspace_and_full_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("another-workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config = test_config(tmp.path());
    let (id, message_count) = seed_replay_valid_session(tmp.path());
    let old_path =
        session::jsonl::session_path(&config.lingxi_home, &config.cwd.to_string_lossy(), &id);
    let new_path =
        session::jsonl::session_path(&config.lingxi_home, &workspace.to_string_lossy(), &id);
    std::fs::create_dir_all(new_path.parent().unwrap()).unwrap();
    let source = std::fs::read_to_string(&old_path)
        .unwrap()
        .lines()
        .map(|line| {
            let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
            value["cwd"] = serde_json::json!(workspace.to_string_lossy());
            format!("{value}\n")
        })
        .collect::<String>();
    std::fs::write(&new_path, source).unwrap();
    std::fs::remove_file(old_path).unwrap();
    let mut config = config;
    config.initial_session_id = Some(id.clone());
    let listener = Arc::new(FakeListener::default());
    let resumed = build_mobile_engine(
        config,
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
    )
    .unwrap();
    resumed.runtime().block_on(async {
        assert_eq!(resumed.session_cwd(), canonical_cwd_string(&workspace));
        assert_eq!(resumed.inner().session_writer.active_path(), new_path);
        let events = drained(&listener).await;
        assert!(events.iter().any(|event| matches!(event,
            Ev::SessionResumed { session_id, mode: SessionModeDto::Code, messages }
                if session_id == &id && messages.len() == message_count
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ev::SessionStarted { .. })));
    });
}

#[test]
fn cold_resume_repairs_only_the_torn_final_record_before_hooks_or_appends() {
    for suffix in [
        b"{\"type\":\"assistant\"".as_slice(),
        b"{\"type\":\"assistant\",\"text\":\"\xf0\x9f".as_slice(),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = test_config(tmp.path());
        let (id, message_count) = seed_replay_valid_session(tmp.path());
        let path =
            session::jsonl::session_path(&config.lingxi_home, &config.cwd.to_string_lossy(), &id);
        let committed = std::fs::read(&path).unwrap();
        let mut crashed = committed.clone();
        crashed.extend_from_slice(suffix);
        std::fs::write(&path, crashed).unwrap();
        config.initial_session_id = Some(id.clone());
        let listener = Arc::new(FakeListener::default());
        let resumed = build_mobile_engine(
            config,
            Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
            listener.clone(),
            Arc::new(RecordingPermissionSink::default()),
        )
        .unwrap();
        resumed.runtime().block_on(async {
            let events = drained(&listener).await;
            assert!(events.iter().any(|event| matches!(event,
                Ev::SessionResumed { session_id, messages, .. } if session_id == &id && messages.len() == message_count)));
            let bytes = std::fs::read(&path).unwrap();
            assert!(bytes.starts_with(&committed));
            let source = String::from_utf8(bytes).unwrap();
            assert_eq!(session::jsonl::route_lines(&source).malformed_line_count, 0);
            resumed.inner().session_writer.append_custom_title(&id, "after recovery".into()).await.unwrap();
            let appended = std::fs::read_to_string(&path).unwrap();
            assert_eq!(session::jsonl::route_lines(&appended).malformed_line_count, 0);
        });
    }
}

#[test]
fn cold_resume_mode_mismatch_has_no_writer_or_hook_effects() {
    let tmp = tempfile::tempdir().unwrap();
    write_project_hook(tmp.path(), "SessionStart");
    let process = Arc::new(ObserveStartupProfile::default());
    let platform = profile_test_platform(tmp.path(), process.clone());
    let mut config = test_config(tmp.path());
    config.session_mode = session::jsonl::SessionMode::Chat;
    config.workspace_trusted = true;
    let first = build_mobile_engine(
        config.clone(),
        platform.clone(),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
    )
    .unwrap();
    let (id, path) = first.runtime().block_on(async {
        (
            first
                .inner()
                .orchestrator
                .current_session_id()
                .await
                .as_uuid()
                .to_string(),
            first.inner().session_writer.active_path(),
        )
    });
    drop(first);
    let original = std::fs::read(&path).unwrap();
    let before_entries = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
    config.initial_session_id = Some(id);
    config.session_mode = session::jsonl::SessionMode::Code;
    let error = match build_mobile_engine(
        config,
        platform,
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
    ) {
        Ok(_) => panic!("wrong mode must be rejected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("session mode mismatch"));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        before_entries
    );
    assert_eq!(
        process.calls.lock().unwrap().len(),
        1,
        "mismatch must not run a hook"
    );
}

#[test]
fn isolated_app_agent_keeps_initial_chat_without_a_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = test_config(tmp.path());
    config.app_agent_host = true;
    config.session_mode = session::jsonl::SessionMode::Chat;
    let (handle, _) = build_submit_handle_with_config(config, tmp.path());
    handle.runtime().block_on(async {
        let state = handle.inner().orchestrator.conversation_mode().await;
        assert_eq!(state.active, session::jsonl::SessionMode::Chat);
        assert_eq!(state.revision, 0);
        assert!(!handle.inner().session_writer.active_path().exists());
    });
}
