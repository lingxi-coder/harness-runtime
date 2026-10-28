use super::*;
use crate::mobile::test_support::{new_engine_with_streaming, test_config};
use client::protocol::mod_session_event::ModSessionEventPayload;
use orchestrator::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta,
};

#[derive(Default)]
struct FirstTurnThenPending {
    calls: AtomicUsize,
    second_entered: Notify,
}

#[async_trait]
impl orchestrator::StreamingApiClient for FirstTurnThenPending {
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
        if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
            let events = vec![
                message_start("mod-origin-a", "test"),
                content_block_start_text(0),
                text_delta(0, "answer A"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ];
            Ok(Box::pin(futures_util::stream::iter(
                events.into_iter().map(Ok),
            )))
        } else {
            self.second_entered.notify_one();
            Ok(Box::pin(futures_util::stream::pending()))
        }
    }
}

/// A real queued turn.complete Mod must keep A's identity while B is active.
#[test]
fn queued_mod_output_keeps_origin_when_next_mobile_turn_is_active() {
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("origin-mod.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.complete', async ($, event) => {
            await $.fs.write('mod-a-started', event.turnId);
            while (!(await $.fs.exists('mod-a-release'))) {
              await $.clock.sleep(10);
            }
            $.ui.log('old log', { to: 'transcript' });
            $.ui.toast('old toast');
            $.ui.notify('old notify');
            $.ui.status('old status');
            return { text: 'old notice' };
          });
        }"#,
    )
    .unwrap();
    let provider = Arc::new(FirstTurnThenPending::default());
    let listener = Arc::new(BlockingSequenceListener::default());
    let handle = new_engine_with_streaming(
        test_config(tmp.path()),
        Arc::new(HostFakePlatform::new(tmp.path().to_path_buf())),
        listener.clone(),
        Arc::new(RecordingPermissionSink::default()),
        Some(provider.clone()),
    )
    .unwrap();
    handle.runtime().block_on(async {
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            "origin-mod",
            tmp.path(),
            &module,
            Utf16JsonProjection::from_serde_value(&serde_json::json!({})).unwrap(),
        )
        .await
        .unwrap();
        handle
            .inner()
            .app_agent_factory
            .host_hooks
            .write()
            .await
            .set_mod_host(host);
        let session_id = handle.active_session_id();
        handle
            .submit(ClientCommand::SendPrompt { session_id: Some(handle.active_session_id()),
                text: "question A".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(273),
                visualization_context: None,
            })
            .await
            .unwrap();
        let started = tmp.path().join("mod-a-started");
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while !started.exists() || handle.active_cancel.lock().await.is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("A must settle while its complete hook is blocked");
        let mod_turn_id = std::fs::read_to_string(&started).unwrap();
        assert!(!mod_turn_id.is_empty());
        handle
            .submit(ClientCommand::SendPrompt { session_id: Some(handle.active_session_id()),
                text: "question B".into(),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(274),
                visualization_context: None,
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), provider.second_entered.notified())
            .await
            .expect("B must run while A's complete hook is blocked");
        std::fs::write(tmp.path().join("mod-a-release"), "go").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if listener.events.lock().await.iter().any(|event| matches!(
                    event,
                    ClientEvent::ModSessionEvent {
                        session_id: origin_session,
                        mod_turn_id: Some(origin_turn),
                        payload: ModSessionEventPayload::Notice { message, .. },
                    } if origin_session == &session_id && origin_turn == &mod_turn_id
                        && message.display().contains("old notice")
                )) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("A's output must carry its own source identity");
        let events = listener.events.lock().await;
        for needle in ["old log", "old toast", "old notify", "old status", "old notice"] {
            assert!(events.iter().any(|event| {
                matches!(event, ClientEvent::ModSessionEvent { session_id: origin, mod_turn_id: Some(turn), .. }
                    if origin == &session_id && turn == &mod_turn_id)
                    && serde_json::to_string(event).unwrap().contains(needle)
            }), "missing source-bound output: {needle}");
        }
        drop(events);
        let b = handle.durable_turns.load(&session_id, 274).unwrap();
        assert!(b.events.iter().all(|event| !event.event_json.contains("old ")));
        handle
            .submit(ClientCommand::PauseTurn { session_id: handle.active_session_id(),
                turn_id: 274,
                reason: "test complete".into(),
            })
            .await
            .unwrap();
    });
}
