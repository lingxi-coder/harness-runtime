use super::*;
use crate::mobile::test_support::new_engine_with_streaming;

struct PendingFallbackProvider {
    entered: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
    pending_open: bool,
}

#[async_trait::async_trait]
impl orchestrator::StreamingApiClient for PendingFallbackProvider {
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
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        if self.pending_open {
            std::future::pending().await
        } else {
            Ok(Box::pin(futures_util::stream::pending()))
        }
    }
}

#[test]
fn mobile_turn_step_is_admitted_before_side_effect_and_pause_cold_resume() {
    let tmp = tempfile::tempdir().expect("temporary mobile workspace");
    let module = tmp.path().join("turn-step.js");
    let marker = tmp.path().join("turn-step-effect.txt");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.step', async function* ($, event, next) {
            const path = 'turn-step-effect.txt';
            const prior = await $.fs.exists(path) ? await $.fs.read(path) : '';
            await $.fs.write(path, prior + 'ran\n');
            const below = next(event);
            for await (const chunk of below) yield chunk;
            return await below.result;
          });
        }"#,
    )
    .unwrap();

    let (entered, provider_started) = tokio::sync::oneshot::channel();
    let provider = Arc::new(PendingPromptStreamClient::new(entered));
    let first = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
        Some(provider),
    )
    .expect("build initial engine");
    let session_id = first.runtime().block_on(async {
        let host = hooks::mods::ModHost::start(None)
            .await
            .expect("start Mod worker");
        host.load(
            "turn-step-recovery",
            tmp.path(),
            &module,
            Utf16JsonProjection::from_serde_value(&serde_json::json!({})).unwrap(),
        )
        .await
        .expect("register turn.step");
        first
            .inner()
            .app_agent_factory
            .host_hooks
            .write()
            .await
            .set_mod_host(host);
        let session_id = first.active_session_id();
        first
            .inner()
            .session_writer
            .append_mobile_empty_session(&session_id, "turn.step recovery", &first.session_cwd())
            .await
            .expect("anchor session");
        first
            .submit(ClientCommand::SendPrompt {
                session_id: Some(first.active_session_id()),
                text: "run turn.step".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(892),
                visualization_context: None,
            })
            .await
            .expect("start turn");
        tokio::time::timeout(std::time::Duration::from_secs(30), provider_started)
            .await
            .expect("provider reached")
            .expect("provider entry signal");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran\n");
        assert!(
            !first
                .durable_turns
                .load(&session_id, 892)
                .unwrap()
                .safe_to_resume
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            first.submit(ClientCommand::PauseTurn {
                session_id: first.active_session_id(),
                turn_id: 892,
                reason: "cold_restart".into(),
            }),
        )
        .await
        .expect("pause must not wait indefinitely for the pending provider")
        .expect("pause turn");
        let paused = first.durable_turns.load(&session_id, 892).unwrap();
        assert_eq!(paused.state, Some(TurnRecoveryStateDto::PausedRecoverable));
        assert!(!paused.safe_to_resume);
        session_id
    });
    drop(first);

    let (entered, _provider_started) = tokio::sync::oneshot::channel();
    let provider = Arc::new(PendingPromptStreamClient::new(entered));
    let resumed = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        Arc::new(FakeListener::default()),
        Arc::new(RecordingPermissionSink::default()),
        Some(provider.clone()),
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
                turn_id: 892,
            })
            .await
            .expect("request recovery");
        let checkpoint = resumed.durable_turns.load(&session_id, 892).unwrap();
        assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::WaitingForUser));
        assert!(provider.captured_calls().is_empty());
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran\n");
    });
}

#[test]
fn mobile_turn_step_fallback_releases_owner_when_provider_is_pending() {
    for pending_open in [true, false] {
        let tmp = tempfile::tempdir().expect("temporary mobile workspace");
        let module = tmp.path().join("failing-step.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('turn.step', async function* () { throw new Error('trigger fallback'); });
            }"#,
        )
        .unwrap();
        let (entered, provider_started) = tokio::sync::oneshot::channel();
        let provider = Arc::new(PendingFallbackProvider {
            entered: StdMutex::new(Some(entered)),
            pending_open,
        });
        let engine = new_engine_with_streaming(
            test_config(tmp.path()),
            Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
            Arc::new(FakeListener::default()),
            Arc::new(RecordingPermissionSink::default()),
            Some(provider),
        )
        .expect("build engine");
        engine.runtime().block_on(async {
            let host = hooks::mods::ModHost::start(None)
                .await
                .expect("start Mod worker");
            host.load(
                "failing-step",
                tmp.path(),
                &module,
                Utf16JsonProjection::from_serde_value(&serde_json::json!({})).unwrap(),
            )
            .await
            .expect("register turn.step");
            engine
                .inner()
                .app_agent_factory
                .host_hooks
                .write()
                .await
                .set_mod_host(host);
            let session_id = engine.active_session_id();
            engine
                .submit(ClientCommand::SendPrompt {
                    session_id: Some(engine.active_session_id()),
                    text: "trigger fallback".into(),
                    prompt_mode: None,
                    images: Vec::new(),
                    turn_id: Some(893),
                    visualization_context: None,
                })
                .await
                .expect("start turn");
            tokio::time::timeout(std::time::Duration::from_secs(30), provider_started)
                .await
                .expect("fallback provider reached")
                .expect("provider entry signal");
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                engine.submit(ClientCommand::PauseTurn {
                    session_id: engine.active_session_id(),
                    turn_id: 893,
                    reason: "background".into(),
                }),
            )
            .await
            .expect("pause must return")
            .expect("pause turn");

            // The fallback worker may observe the closed receiver on its next
            // poll. It must then release the old execution reservation so a
            // different turn in the same session can actually be admitted.
            let next_owner = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    match engine
                        .durable_turns
                        .reserve_live_owner(&session_id, Some(894))
                    {
                        Ok(owner) => break owner,
                        Err(crate::mobile::turn_durability::DurableTurnStoreError::LiveOwner {
                            ..
                        }) => {
                            tokio::task::yield_now().await;
                        }
                        Err(error) => panic!("reserve next turn: {error}"),
                    }
                }
            })
            .await
            .expect("pending fallback must not retain the session owner");
            drop(next_owner);
        });
    }
}
