//! Regression coverage for immutable skill history and active-only recovery.
use super::*;
use crate::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    mock_message_response, noop_hook_executor, text_delta, MockApiClient, MockOutputStream,
    MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

struct RunningInputGate {
    first: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    queued: tokio::sync::Mutex<Option<Utf16JsonProjection>>,
}

#[async_trait::async_trait]
impl crate::prompt::mid_turn_input::MidTurnInputSource for RunningInputGate {
    async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection> {
        self.queued.lock().await.take()
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::NewDiagnosticsSource for RunningInputGate {
    async fn take_new_diagnostics_block(&self) -> Option<Utf16JsonProjection> {
        if self.first.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        None
    }
}

fn fixture(
    cwd: &Path,
) -> (
    ConversationOrchestrator,
    Arc<MockApiClient>,
    Arc<MockStreamingApiClient>,
) {
    let mut done = mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "done".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    done.model = "gpt-6.1-sol".into();
    let events = vec![
        message_start(&done.id, &done.model),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let batched = Arc::new(MockApiClient::new(vec![done.clone(), done.clone(), done]));
    let streamed = Arc::new(MockStreamingApiClient::with_turns(vec![
        events.clone(),
        events.clone(),
        events,
    ]));
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: "gpt-6.1-sol".into(),
            ..Default::default()
        },
        batched.clone(),
        streamed.clone(),
        Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd.to_owned(),
    )
    .with_config_home(cwd.join("config"));
    (orch, batched, streamed)
}

fn individual(id: MessageId, name: &str, body: &str) -> ConversationMessage {
    ConversationMessage::user_meta(id, body.into()).with_origin(
        compaction::invoked_skills::skill_activation_origin_with_skill(
            0,
            &name.into(),
            Path::new("/skills"),
        ),
    )
}

fn register(
    scope: compaction::invoked_skills::InvokedSkillScopeRef<'_>,
    id: MessageId,
    name: &str,
    body: &str,
) {
    compaction::invoked_skills::register_activation_with_message(
        &name.into(),
        Path::new("/skills"),
        &body.into(),
        None,
        scope,
        id,
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn skill_retirement_survives_cancel_then_continue_in_both_drivers_and_cold_recovery() {
    let _guard = compaction::invoked_skills::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for streaming in [false, true] {
        for cold_recovery in [false, true] {
            compaction::invoked_skills::reset_for_test();
            let root = tempfile::tempdir().unwrap();
            let (orch, batched, streamed) = fixture(root.path());
            let orch = ConversationOrchestrator::into_shared(orch);
            orch.initialize_conversation_mode().await.unwrap();
            let sid = orch.current_session_id().await;
            let scoped = sid.to_string();
            let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scoped), None);
            let old_id = MessageId::new();
            let new_id = MessageId::new();
            let old = individual(old_id, "review", "old task arguments");
            let quote = ConversationMessage::user(MessageId::new(), "old task arguments".into());
            let current = individual(new_id, "review", "current task arguments");
            register(scope, old_id, "review", "old task arguments");
            register(scope, new_id, "review", "current task arguments");
            orch.session
                .lock()
                .await
                .history
                .extend([old.clone(), quote.clone(), current]);
            orch.refresh_effective_skill_projection().await.unwrap();
            let committed = orch.session.lock().await.model_context_history();
            assert_eq!(committed[0], old, "old bytes stay in place");
            assert_eq!(
                committed[1], quote,
                "quoted user evidence is not reclassified"
            );
            assert_eq!(retired_skill_sources(&committed, 0).len(), 1);

            // Cancellation happens after the durable update's publication but
            // before opening a model request. Retry must consume that same row.
            let cancel = tokio_util::sync::CancellationToken::new();
            cancel.cancel();
            let outcome = if streaming {
                orch.run_turn_streaming_with_cancel("cancelled request", cancel)
                    .await
            } else {
                orch.run_turn_with_cancel("cancelled request", cancel).await
            }
            .unwrap();
            assert!(matches!(outcome, crate::TurnOutcome::Cancelled));
            assert!(batched.captured_msgs().await.is_empty());
            assert!(streamed.captured_calls().await.is_empty());

            let cold_fixture;
            let active = if cold_recovery {
                let saved = orch.session.lock().await.model_context_history();
                compaction::invoked_skills::reset_for_test();
                let (cold, _, _) = fixture(root.path());
                cold_fixture = ConversationOrchestrator::into_shared(cold.with_session_id(sid));
                cold_fixture.session.lock().await.history = saved.clone();
                cold_fixture.initialize_conversation_mode().await.unwrap();
                assert_eq!(
                    cold_fixture.session.lock().await.model_context_history(),
                    saved
                );
                &cold_fixture
            } else {
                &orch
            };
            let metadata = compaction::invoked_skills::filter_for_scope(scope);
            assert_eq!(
                metadata.len(),
                1,
                "recovery must not reactivate old invocation"
            );
            assert_eq!(
                metadata[0].content.string_at(""),
                Some("current task arguments")
            );
            active.refresh_effective_skill_projection().await.unwrap();
            let after = active.session.lock().await.model_context_history();
            assert_eq!(retired_skill_sources(&after, 0).len(), 1);
            assert_eq!(
                after
                    .iter()
                    .filter(|row| row.text_content().contains("Skill activation update."))
                    .count(),
                1
            );
            if streaming {
                active.run_turn_streaming("continue").await.unwrap();
            } else {
                active.run_turn("continue").await.unwrap();
            }
            let sent = active.session.lock().await.model_context_history();
            assert_eq!(sent.iter().find(|row| row.id() == old_id), Some(&old));
            assert_eq!(
                sent.iter()
                    .filter(|row| row.text_content().contains("Skill activation update."))
                    .count(),
                1
            );
        }
    }
    compaction::invoked_skills::reset_for_test();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn completed_skill_stays_retired_after_cold_recovery() {
    let _guard = compaction::invoked_skills::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    compaction::invoked_skills::reset_for_test();
    let root = tempfile::tempdir().unwrap();
    let (orch, _, _) = fixture(root.path());
    orch.initialize_conversation_mode().await.unwrap();
    let sid = orch.current_session_id().await;
    let scoped = sid.to_string();
    let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scoped), None);
    let id = MessageId::new();
    let old = individual(id, "review", "old task arguments");
    register(scope, id, "review", "old task arguments");
    orch.session.lock().await.history.push(old.clone());
    compaction::invoked_skills::complete_task(scope);
    orch.refresh_effective_skill_projection().await.unwrap();
    let history = orch.session.lock().await.model_context_history();
    assert_eq!(history[0], old);
    assert!(history
        .last()
        .unwrap()
        .text_content()
        .contains("no active invocation"));
    compaction::invoked_skills::reset_for_test();
    let (cold, _, _) = fixture(root.path());
    let cold = cold.with_session_id(sid);
    cold.session.lock().await.history = history.clone();
    cold.initialize_conversation_mode().await.unwrap();
    assert!(compaction::invoked_skills::filter_for_scope(scope).is_empty());
    cold.refresh_effective_skill_projection().await.unwrap();
    assert_eq!(cold.session.lock().await.model_context_history(), history);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn retirement_metadata_preserves_exact_utf16_skill_names() {
    let _guard = compaction::invoked_skills::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    compaction::invoked_skills::reset_for_test();
    let root = tempfile::tempdir().unwrap();
    let (orch, _, _) = fixture(root.path());
    orch.initialize_conversation_mode().await.unwrap();
    let scoped = orch.current_session_id().await.to_string();
    let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scoped), None);
    let name = Utf16JsonProjection::parse(r#""\ud800""#).unwrap();
    let old_id = MessageId::new();
    let new_id = MessageId::new();
    for (id, body) in [(old_id, "old"), (new_id, "new")] {
        compaction::invoked_skills::register_activation_with_message(
            &name,
            Path::new("/skills"),
            &body.into(),
            None,
            scope,
            id,
        );
        let origin = compaction::invoked_skills::skill_activation_origin_with_skill(
            0,
            &name,
            Path::new("/skills"),
        );
        orch.session
            .lock()
            .await
            .history
            .push(ConversationMessage::user_meta(id, body.into()).with_origin(origin));
    }
    orch.refresh_effective_skill_projection().await.unwrap();
    let history = orch.session.lock().await.model_context_history();
    assert!(retired_skill_sources(&history, 0).contains(&(old_id, vec![0xd800])));
    let serialized = serde_json::to_string(&history.last().unwrap()).unwrap();
    let restored: ConversationMessage = serde_json::from_str(&serialized).unwrap();
    assert!(retired_skill_sources(&[restored], 0).contains(&(old_id, vec![0xd800])));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn user_message_appended_while_running_preserves_skill_updates_in_both_drivers() {
    let _guard = compaction::invoked_skills::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for streaming in [false, true] {
        compaction::invoked_skills::reset_for_test();
        let root = tempfile::tempdir().unwrap();
        let gate = Arc::new(RunningInputGate {
            first: AtomicBool::new(true),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            queued: tokio::sync::Mutex::new(None),
        });
        let (orch, batched, streamed) = fixture(root.path());
        let orch = ConversationOrchestrator::into_shared(
            orch.with_new_diagnostics_source(gate.clone())
                .with_mid_turn_input(gate.clone()),
        );
        orch.initialize_conversation_mode().await.unwrap();
        let scoped = orch.current_session_id().await.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scoped), None);
        let old_id = MessageId::new();
        let current_id = MessageId::new();
        let old = individual(old_id, "review", "old task arguments");
        register(scope, old_id, "review", "old task arguments");
        register(scope, current_id, "review", "current task arguments");
        orch.session.lock().await.history.extend([
            old.clone(),
            individual(current_id, "review", "current task arguments"),
        ]);
        orch.refresh_effective_skill_projection().await.unwrap();
        let mut running = Box::pin(async {
            if streaming {
                orch.run_turn_streaming("start").await
            } else {
                orch.run_turn("start").await
            }
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                () = gate.entered.notified() => {},
                result = &mut running => panic!("turn finished before running input gate: {result:?}"),
            }
        }).await.expect("request preparation reaches the gate");
        *gate.queued.lock().await = Some("new user input during execution".into());
        gate.release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), &mut running)
            .await
            .unwrap()
            .unwrap();
        drop(running);
        if streaming {
            orch.run_turn_streaming("continue").await.unwrap();
        } else {
            orch.run_turn("continue").await.unwrap();
        }
        let requests = if streaming {
            streamed
                .captured_calls()
                .await
                .into_iter()
                .map(|call| call.messages)
                .collect::<Vec<_>>()
        } else {
            batched.captured_msgs().await
        };
        assert!((2..=3).contains(&requests.len()));
        let last = requests.last().unwrap();
        assert_eq!(
            last.iter()
                .filter(|row| row
                    .text_content()
                    .contains("new user input during execution"))
                .count(),
            1
        );
        assert_eq!(last.iter().find(|row| row.id() == old_id), Some(&old));
        assert_eq!(
            last.iter()
                .filter(|row| row.text_content().contains("Skill activation update."))
                .count(),
            1
        );
        assert_eq!(
            compaction::invoked_skills::filter_for_scope(scope)[0]
                .content
                .string_at(""),
            Some("current task arguments")
        );
    }
}

fn openai_input(messages: Vec<ConversationMessage>, streaming: bool) -> serde_json::Value {
    use lingxi_llm_client::codecs::{CodecContext, EncodeRequest, WireCodec};
    let normalized = llm_runtime::convert::normalize_messages_for_api(messages);
    let messages = llm_runtime::convert::to_llm_messages(normalized).unwrap();
    let (request, _) = llm_runtime::convert::history_input(
        "gpt-6.1-sol",
        &messages,
        &[],
        &[],
        llm_runtime::ProtocolFamily::OpenAiResponses,
    )
    .unwrap();
    let profile = serde_json::from_value(serde_json::json!({
        "provider_id":"openai", "profile_name":"fixture", "auth":"none",
        "protocol":"open_ai_responses", "base_url":"https://api.openai.com/v1",
        "models":[{"display_model":"gpt-6.1-sol", "request_model":"gpt-6.1-sol", "billing_model":"gpt-6.1-sol"}]
    })).unwrap();
    let mode = if streaming {
        lingxi_llm_client::RequestMode::Stream
    } else {
        lingxi_llm_client::RequestMode::Complete
    };
    let context = CodecContext::new(&profile, "gpt-6.1-sol", mode);
    let wire = lingxi_llm_client::codecs::openai::responses::OpenAiResponsesCodec
        .encode_request(EncodeRequest::new(&request), &context)
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&wire.body).unwrap()["input"].clone()
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn skill_reinvocation_keeps_the_final_openai_input_prefix_in_both_modes() {
    let _guard = compaction::invoked_skills::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for streaming in [false, true] {
        compaction::invoked_skills::reset_for_test();
        let root = tempfile::tempdir().unwrap();
        let (orch, _, _) = fixture(root.path());
        orch.initialize_conversation_mode().await.unwrap();
        let scoped = orch.current_session_id().await.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scoped), None);
        let aggregate_id = MessageId::new();
        register(scope, aggregate_id, "a", "old a");
        register(scope, aggregate_id, "b", "unchanged b");
        let skills = vec![
            compaction::RestoredSkill {
                name: "a".into(),
                path: "/skills".into(),
                content: "old a".into(),
            },
            compaction::RestoredSkill {
                name: "b".into(),
                path: "/skills".into(),
                content: "unchanged b".into(),
            },
        ];
        let body = compaction::render_invoked_skills_attachment(&skills).unwrap();
        let aggregate = ConversationMessage::user_meta_js_utf16(
            aggregate_id,
            body.string_at("").unwrap().into(),
            body.string_units("").unwrap(),
        )
        .with_origin(aggregate_origin(0, &skills, Some(scope)));
        orch.transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap()
            .insert(
                aggregate_id,
                skills.iter().map(|skill| skill.content.clone()).collect(),
            );
        orch.session.lock().await.history.extend([
            aggregate,
            ConversationMessage::Assistant {
                per_turn_effort: None,
                id: MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "earlier answer".into(),
                    citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                resumable: None,
            },
            ConversationMessage::user(MessageId::new(), "current task".into()),
        ]);
        let before = openai_input(orch.session.lock().await.model_context_history(), streaming);
        let id = MessageId::new();
        register(scope, id, "a", "new a arguments");
        orch.session.lock().await.history.extend([
            ConversationMessage::Assistant {
                per_turn_effort: None,
                id: MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "invoke a again".into(),
                    citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                resumable: None,
            },
            individual(id, "a", "new a arguments"),
        ]);
        orch.refresh_effective_skill_projection().await.unwrap();
        let after = openai_input(orch.session.lock().await.model_context_history(), streaming);
        let before = before.as_array().unwrap();
        let after = after.as_array().unwrap();
        assert_eq!(
            &after[..before.len()],
            before,
            "final encoded historical input must be byte-equivalent before the appended activation"
        );
        let rendered = serde_json::to_string(&after).unwrap();
        assert_eq!(
            rendered.matches("unchanged b").count(),
            1,
            "unaffected aggregate member is not reinserted"
        );
    }
}
