use super::*;
use crate::mobile::test_support::new_engine_with_streaming;

struct RejectAsyncHookSpawn;

#[async_trait]
impl lingxi_core::host::RuntimeSpawner for RejectAsyncHookSpawn {
    async fn spawn(
        &self,
        _name: &str,
        _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError> {
        Err(lingxi_core::host::RuntimeError::Internal(
            "injected registration failure".into(),
        ))
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn cancel(
        &self,
        _handle: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), lingxi_core::host::RuntimeError> {
        Ok(())
    }
}

#[tokio::test]
async fn real_marker_hook_retains_work_when_registry_missing_or_rejects_spawn() {
    use hooks::{HookDefinition, HookEvent, HookEventType, HookExecutor, HookSource};
    use lingxi_core::host::Platform as _;

    for rejecting_registry in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("hook-child.pid");
        let mut registry = hooks::HookRegistry::new();
        registry.register(HookDefinition {
            id: lingxi_core::types::HookId::new(),
            name: "real marker".into(),
            events: vec![HookEventType::UserPromptSubmit],
            if_condition: None,
            executor: HookExecutor::Command {
                command: format!("echo '{{\"async\":true,\"asyncTimeout\":800}}'; sleep 30 & echo $! > '{}'; wait", pid_path.display()),
                args: Vec::new(), env: HashMap::new(), cwd: None, shell: None,
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true, timeout: None, priority: 0, once: false,
            status_message: None, async_rewake: false, async_timeout: None,
            rewake_message: None, on_failure: None,
        });
        let platform = HostFakePlatform::new(tmp.path().to_path_buf());
        let runtime = Arc::new(RejectAsyncHookSpawn);
        let mut exec = hooks::HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            platform.http(),
            runtime.clone(),
        )
        .with_process_runner(
            Arc::new(platform_posix::PosixProcess::new()),
            platform.sandbox(),
        );
        let (_completion_tx, _completion_rx) = tokio::sync::mpsc::channel(1);
        if rejecting_registry {
            exec = exec.with_async_registry(Arc::new(hooks::AsyncHookRegistry::new(
                runtime,
                _completion_tx,
            )));
        }
        let work = Arc::new(());
        let weak = Arc::downgrade(&work);
        let result = exec
            .execute_recovery_checked(
                HookEvent::UserPromptSubmit {
                    prompt: source_text("marker"),
                },
                hooks::HookContext {
                    cwd: tmp.path().to_path_buf(),
                    async_work_lease: Some(work.clone()),
                    ..Default::default()
                },
            )
            .await
            .expect("marker accepted");
        assert_eq!(result.decision, None);
        drop(work);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !pid_path.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("same-group child starts");
        assert!(
            weak.upgrade().is_some(),
            "actual child must own the turn even without registry fold-back"
        );
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while weak.upgrade().is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("turn identity releases after verified group cleanup");
    }
}

#[test]
fn mobile_prompt_hook_side_effect_is_not_repeated_on_cold_resume() {
    let tmp = tempfile::tempdir().expect("temporary mobile workspace");
    let settings = tmp.path().join(".lingxi");
    std::fs::create_dir_all(&settings).unwrap();
    std::fs::write(
        settings.join("settings.json"),
        serde_json::to_vec(&serde_json::json!({
            "hooks": { "UserPromptSubmit": [{
                "hooks": [{ "type": "command", "command": "printf 'hook\\n' >> \"$LINGXI_PROJECT_DIR/side-effect.txt\"" }]
            }] }
        })).unwrap(),
    ).unwrap();

    let (entered, provider_started) = tokio::sync::oneshot::channel();
    let first_provider = Arc::new(PendingPromptStreamClient::new(entered));
    let first = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(
            HostFakePlatform::new(tmp.path().to_path_buf())
                .with_process_runner(Arc::new(platform_posix::PosixProcess::new())),
        ),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
        Some(first_provider),
    )
    .expect("build initial engine");
    let session_id = first.runtime().block_on(async {
        let session_id = first.active_session_id();
        first
            .inner()
            .session_writer
            .append_mobile_empty_session(&session_id, "hook recovery", &first.session_cwd())
            .await
            .expect("anchor session");
        first
            .submit(ClientCommand::SendPrompt {
                session_id: Some(first.active_session_id()),
                text: "run the hook".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(891),
                visualization_context: None,
            })
            .await
            .expect("start turn");
        tokio::time::timeout(std::time::Duration::from_secs(30), provider_started)
            .await
            .expect("provider reached")
            .expect("provider entry signal");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("side-effect.txt")).unwrap(),
            "hook\n"
        );
        assert!(
            !first
                .durable_turns
                .load(&session_id, 891)
                .unwrap()
                .safe_to_resume
        );
        first
            .submit(ClientCommand::PauseTurn {
                session_id: first.active_session_id(),
                turn_id: 891,
                reason: "cold_restart".into(),
            })
            .await
            .expect("pause turn");
        let paused = first.durable_turns.load(&session_id, 891).unwrap();
        assert_eq!(paused.state, Some(TurnRecoveryStateDto::PausedRecoverable));
        assert!(!paused.safe_to_resume);
        session_id
    });
    drop(first);

    let (entered, _provider_started) = tokio::sync::oneshot::channel();
    let resumed_provider = Arc::new(PendingPromptStreamClient::new(entered));
    let resumed = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(
            HostFakePlatform::new(tmp.path().to_path_buf())
                .with_process_runner(Arc::new(platform_posix::PosixProcess::new())),
        ),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
        Some(resumed_provider.clone()),
    )
    .expect("rebuild engine");
    resumed.runtime().block_on(async {
        resumed
            .submit(ClientCommand::ResumeSession {
                session_id: session_id.clone(),
                cwd: None,
            })
            .await
            .expect("restore session");
        resumed
            .submit(ClientCommand::ResumeTurn {
                session_id: resumed.active_session_id(),
                turn_id: 891,
            })
            .await
            .expect("request recovery");
        let checkpoint = resumed.durable_turns.load(&session_id, 891).unwrap();
        assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::WaitingForUser));
        assert!(!checkpoint.safe_to_resume);
        assert!(resumed_provider.captured_calls().is_empty());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("side-effect.txt")).unwrap(),
            "hook\n"
        );
    });
}
