use super::*;
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

#[test]
fn configured_chat_is_durable_before_startup_hook_and_cold_resume() {
    let tmp = tempfile::tempdir().unwrap();
    write_project_hook(tmp.path(), "SessionStart");
    let mut config = test_config(tmp.path());
    config.session_mode = session::jsonl::SessionMode::Chat;
    config.workspace_trusted = true;
    let process = Arc::new(ObserveStartupProfile::default());
    let platform: Arc<dyn lingxi_core::host::Platform> = Arc::new(
        HostFakePlatform::new(tmp.path().to_path_buf())
            .with_process_runner(process.clone())
            .with_sandbox(Arc::new(
                crate::mobile::test_support::FakeProcessAdmittingSandbox,
            )),
    );
    let listener = Arc::new(FakeListener::default());
    let first = build_mobile_engine(
        config,
        platform,
        listener,
        Arc::new(RecordingPermissionSink::default()),
    )
    .unwrap();

    let (id, transcript_path) = first.runtime().block_on(async {
        let snapshot = first.inner().orchestrator.conversation_mode().await;
        assert_eq!(snapshot.active, session::jsonl::SessionMode::Chat);
        assert_eq!(snapshot.selected, session::jsonl::SessionMode::Chat);
        assert_eq!(snapshot.revision, 1);
        assert!(!snapshot.pending);
        (
            snapshot.session_id,
            first.inner().session_writer.active_path(),
        )
    });
    let calls = process.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "startup hook must run once");
    let (input, at_hook) = &calls[0];
    assert_eq!(input["source"], "startup");
    assert_eq!(input["session_id"], id);
    assert_eq!(input["transcript_path"].as_str(), transcript_path.to_str());
    let at_hook = at_hook.as_deref().expect("profile exists before hook");
    let profile = session::JsonlWriter::session_mode_transition_from_source(at_hook, &id)
        .expect("startup hook observes durable profile");
    assert_eq!(profile.active, session::jsonl::SessionMode::Chat);
    assert_eq!(profile.selected, session::jsonl::SessionMode::Chat);
    assert_eq!(
        (
            profile.revision,
            profile.active_epoch,
            profile.selected_epoch
        ),
        (1, 1, 1)
    );
    assert!(!profile.pending);
    let source = std::fs::read_to_string(&transcript_path).unwrap();
    assert!(source.contains("startup-profile-probe"));
    drop(first);

    // Restart from the implicit Code default; the captured startup transcript
    // must restore Chat and its epoch without any preceding selection command.
    std::fs::remove_file(tmp.path().join(".lingxi/settings.json")).unwrap();
    let (second, listener) = build_submit_handle(tmp.path());
    second.runtime().block_on(async {
        second
            .submit(ClientCommand::ResumeSession {
                session_id: id.clone(),
                cwd: None,
            })
            .await
            .expect("resume startup-created history");
        let snapshot = second.inner().orchestrator.conversation_mode().await;
        assert_eq!(snapshot.session_id, id);
        assert_eq!(snapshot.active, session::jsonl::SessionMode::Chat);
        assert_eq!(snapshot.selected, session::jsonl::SessionMode::Chat);
        assert_eq!(snapshot.revision, 1);
        assert!(!snapshot.pending);
        assert!(drained(&listener).await.iter().any(|event| matches!(event,
            Ev::SessionResumed { session_id, mode: SessionModeDto::Chat, .. } if session_id == &id
        )));
        let restored_source = std::fs::read_to_string(&transcript_path).unwrap();
        let restored =
            session::JsonlWriter::session_mode_transition_from_source(&restored_source, &id)
                .expect("restored profile remains durable");
        assert_eq!((restored.active_epoch, restored.selected_epoch), (1, 1));
    });
}

#[test]
fn configured_chat_is_persisted_even_when_no_hook_or_command_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = test_config(tmp.path());
    config.session_mode = session::jsonl::SessionMode::Chat;
    let (handle, _) = build_submit_handle_with_config(config, tmp.path());
    handle.runtime().block_on(async {
        let state = handle.inner().orchestrator.conversation_mode().await;
        let source = std::fs::read_to_string(handle.inner().session_writer.active_path()).unwrap();
        let profile =
            session::JsonlWriter::session_mode_transition_from_source(&source, &state.session_id)
                .expect("initial Chat must be written by the constructor");
        assert_eq!(profile.active, session::jsonl::SessionMode::Chat);
        assert_eq!(profile.selected, session::jsonl::SessionMode::Chat);
        assert_eq!(
            (
                profile.revision,
                profile.active_epoch,
                profile.selected_epoch
            ),
            (1, 1, 1)
        );
        assert!(!profile.pending);
    });
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
        assert_eq!(state.selected, session::jsonl::SessionMode::Chat);
        assert_eq!(state.revision, 0);
        assert!(!handle.inner().session_writer.active_path().exists());
    });
}
