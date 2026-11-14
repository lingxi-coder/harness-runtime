//! Exercise the public recovery entrypoints, including their session reset and
//! JSONL loading. Re-running only the final restore helper misses the hot reset.

use super::*;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};
use lingxi_core::host::{NativeSettingsAgentContext, OrchestratorHandle};
use platform_posix::fs::PosixFileSystem;
use session::jsonl::reader::JsonlReader;

const MODEL: &str = "claude-opus-4-7";
const DEFERRED_RAW: &str = "<system-reminder>\nRESUME_DEFERRED_SOURCE\n</system-reminder>";

#[derive(Clone, Copy, Debug)]
enum Reminder {
    Deferred,
    Language,
    OutputStyle,
}

impl Reminder {
    fn kind(self) -> &'static str {
        match self {
            Self::Deferred => "deferred_tools_delta",
            Self::Language => "language",
            Self::OutputStyle => "output_style_instructions",
        }
    }

    fn config(self) -> OrchestratorConfig {
        let mut config = OrchestratorConfig {
            model: MODEL.into(),
            system_prompt_override: Some("resume attachment fixture".into()),
            ..Default::default()
        };
        match self {
            Self::Deferred => {}
            Self::Language => config.language = Some("English".into()),
            Self::OutputStyle => config.output_style = Some("Explanatory".into()),
        }
        config
    }
}

#[derive(Clone, Copy, Debug)]
enum Recovery {
    Hot,
    Cold,
}

fn run_with_large_stack(
    build: impl FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send + 'static,
) {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(build());
        })
        .unwrap()
        .join()
        .unwrap();
}

fn replies(count: usize) -> Vec<llm_runtime::HistoryResponse> {
    (0..count)
        .map(|_| {
            mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "accepted resume fixture".into(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            )
        })
        .collect()
}

fn streams(count: usize) -> Vec<Vec<llm_runtime::HistoryEvent>> {
    (0..count)
        .map(|index| {
            vec![
                message_start(&format!("resume-attachment-{index}"), MODEL),
                content_block_start_text(0),
                text_delta(0, "accepted resume fixture"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ]
        })
        .collect()
}

async fn captures(
    api: &MockApiClient,
    stream: &MockStreamingApiClient,
    streaming: bool,
) -> Vec<Vec<ConversationMessage>> {
    if streaming {
        stream
            .captured_calls()
            .await
            .into_iter()
            .map(|call| call.messages)
            .collect()
    } else {
        api.captured_msgs().await
    }
}

fn row_texts(messages: &[ConversationMessage], id: MessageId) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.id() == id)
        .map(ConversationMessage::text_content)
        .collect()
}

async fn run_prompt(orch: &ConversationOrchestrator, streaming: bool, prompt: &str) {
    if streaming {
        orch.run_turn_streaming(prompt).await.unwrap();
    } else {
        orch.run_turn(prompt).await.unwrap();
    }
}

async fn check_recovery(recovery: Recovery, reminder: Reminder, streaming: bool, suppress: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let cwd = root.join("workspace");
    let home = root.join("home");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let kind = reminder.kind();
    let kind_json = serde_json::to_string(kind).unwrap();
    let action = if suppress {
        "($,e) => { if(e.origin.kind !== 'engine') throw new Error('wrong origin'); return {text:null}; }"
    } else {
        "($,e,next) => { if(e.origin.kind !== 'engine') throw new Error('wrong origin'); return next({...e,text:'MOD: '+e.text}); }"
    };
    let module = root.join("attachment-mod.js");
    std::fs::write(
        &module,
        format!("export function register(on) {{ on('prompt.attachment', {{type:{kind_json}}}, {action}); }}"),
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "resume-attachment",
        &root,
        &module,
        tool_api::tool_trait::generated_builtin_result(&serde_json::json!({})),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let registry = Arc::new(tokio::sync::RwLock::new(registry));
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(root));
    let session_id = SessionId::new();
    let cwd_string = cwd.to_string_lossy().into_owned();
    let transcript =
        session::jsonl::path::session_path(&home, &cwd_string, &session_id.as_uuid().to_string());
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let config = reminder.config();
    let warm_api = Arc::new(MockApiClient::new(replies(3)));
    let warm_stream = Arc::new(MockStreamingApiClient::with_turns(streams(3)));
    let warm = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            config.clone(),
            warm_api.clone(),
            warm_stream.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_session_id(session_id)
        .with_jsonl_writer(Arc::new(JsonlWriter::new(transcript.clone(), fs.clone())))
        .with_hook_registry(registry.clone())
        .with_native_settings_agent_context(NativeSettingsAgentContext::Subagent {
            is_main_session: true,
        }),
    );
    warm.initialize_conversation_mode().await.unwrap();
    if matches!(reminder, Reminder::Deferred) {
        let attachment = Some(crate::RequestContextTranscriptAttachment {
            message_id: MessageId::new(),
            payload: Utf16JsonProjection::parse(
                r#"{"type":"deferred_tools_delta","addedNames":[],"addedLines":[],"removedNames":[],"wireHiddenNames":[],"surfacedNames":[],"replacedNames":[],"readdedNames":[]}"#,
            )
            .unwrap(),
            rendered: Utf16JsonProjection::from_serde_value(&serde_json::json!([
                {"content": DEFERRED_RAW}
            ]))
            .unwrap(),
            rendered_role: "system",
        });
        if streaming {
            warm.run_turn_streaming_with_request_context(
                "warm deferred source",
                vec![DEFERRED_RAW.into()],
                attachment,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        } else {
            warm.run_turn_with_request_context(
                "warm deferred source",
                vec![DEFERRED_RAW.into()],
                attachment,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        }
    } else {
        // Language and style rows come from the actual native settings producer.
        run_prompt(&warm, streaming, "warm native settings source").await;
    }
    let rows = JsonlReader::new(transcript.clone(), fs.clone())
        .read_all()
        .await
        .unwrap();
    let attachment_rows = rows
        .iter()
        .filter(|row| {
            row.message_type == "attachment"
                && row
                    .attachment_projection
                    .as_ref()
                    .and_then(|payload| payload.string_at("/type"))
                    == Some(kind)
        })
        .collect::<Vec<_>>();
    assert_eq!(attachment_rows.len(), 1, "{recovery:?} {reminder:?}");
    let row = attachment_rows[0];
    let id = MessageId::from_uuid(Uuid::parse_str(&row.uuid).unwrap());
    let payload = row.attachment_projection.clone().unwrap();
    let raw = row
        .extra
        .string_at("/rendered/0/content")
        .unwrap()
        .to_owned();
    assert!(!payload.contains_property("hookEvent"));
    let expected = if suppress {
        Vec::new()
    } else {
        let inner = raw
            .strip_prefix("<system-reminder>\n")
            .unwrap()
            .strip_suffix("\n</system-reminder>")
            .unwrap();
        vec![format!(
            "<system-reminder>\nMOD: {inner}\n</system-reminder>"
        )]
    };
    let warm_calls = captures(&warm_api, &warm_stream, streaming).await;
    assert_eq!(warm_calls.len(), 1);
    assert_eq!(row_texts(&warm_calls[0], id), expected);
    let raw_history = warm.snapshot_history().await;
    assert_eq!(row_texts(&raw_history, id), vec![raw.clone()]);
    let durable_ids = raw_history
        .iter()
        .map(ConversationMessage::id)
        .collect::<HashSet<_>>();
    let prior_request_ids = warm_calls[0]
        .iter()
        .map(ConversationMessage::id)
        .filter(|id| durable_ids.contains(id))
        .collect::<Vec<_>>();
    let prior_request_id_set = prior_request_ids.iter().copied().collect::<HashSet<_>>();
    let source_prefix = std::fs::read_to_string(&transcript).unwrap();

    let (subject, api, stream, first_post_resume_index) = match recovery {
        Recovery::Hot => {
            let replayed =
                replay_session_state(&home, &cwd_string, session_id.as_uuid(), fs.clone())
                    .await
                    .unwrap();
            let runtime = replayed.handle_runtime_snapshot();
            // This public entry performs reset_session_scoped_runtime before
            // rebuilding state from the loader's authoritative raw sidecars.
            OrchestratorHandle::resume_session(
                &*warm,
                session_id,
                replayed.state.history,
                replayed.last_message_uuid.map(|uuid| uuid.to_string()),
                None,
                runtime,
            )
            .await
            .unwrap();
            (warm, warm_api, warm_stream, 1)
        }
        Recovery::Cold => {
            let api = Arc::new(MockApiClient::new(replies(2)));
            let stream = Arc::new(MockStreamingApiClient::with_turns(streams(2)));
            let mut cold = ConversationOrchestrator::with_resume(
                config,
                session_id.as_uuid(),
                home,
                cwd_string,
                fs.clone(),
                api.clone(),
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                cwd,
                Some(Arc::new(JsonlWriter::new(transcript.clone(), fs.clone()))),
            )
            .await
            .unwrap()
            .with_hook_registry(registry)
            .with_native_settings_agent_context(NativeSettingsAgentContext::Subagent {
                is_main_session: true,
            });
            // with_resume configures NoStreamingApiClient. Replace only that
            // transport dependency; all restored state came from its public
            // loader/constructor, never a private restoration helper.
            cold.streaming_api = stream.clone();
            (ConversationOrchestrator::into_shared(cold), api, stream, 0)
        }
    };
    assert!(host.has_event("prompt.attachment"));
    assert_eq!(
        row_texts(&subject.snapshot_history().await, id),
        vec![raw.clone()]
    );
    for prompt in ["after actual resume", "follow-up request"] {
        run_prompt(&subject, streaming, prompt).await;
    }
    let requests = captures(&api, &stream, streaming).await;
    assert_eq!(requests.len(), first_post_resume_index + 2);
    for request in &requests[first_post_resume_index..] {
        assert_eq!(
            row_texts(request, id),
            expected,
            "{recovery:?} {reminder:?}, streaming={streaming}, suppress={suppress}"
        );
        let restored_order = request
            .iter()
            .map(ConversationMessage::id)
            .filter(|id| prior_request_id_set.contains(id))
            .collect::<Vec<_>>();
        assert_eq!(
            restored_order, prior_request_ids,
            "historical request order stays stable"
        );
    }
    assert_eq!(row_texts(&subject.snapshot_history().await, id), vec![raw]);
    assert!(std::fs::read_to_string(&transcript)
        .unwrap()
        .starts_with(&source_prefix));
    let rows = JsonlReader::new(transcript, fs).read_all().await.unwrap();
    let original = rows
        .iter()
        .filter(|row| row.uuid == id.as_uuid().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        original.len(),
        1,
        "resume never duplicates the durable reminder UUID"
    );
    assert_eq!(original[0].attachment_projection.as_ref(), Some(&payload));
}

fn check_matrix(recovery: Recovery, reminders: Vec<Reminder>) {
    run_with_large_stack(move || {
        Box::pin(async move {
            for reminder in reminders {
                for streaming in [false, true] {
                    for suppress in [false, true] {
                        check_recovery(recovery, reminder, streaming, suppress).await;
                    }
                }
            }
        })
    });
}

#[test]
fn hot_resume_keeps_deferred_mod_views_for_both_drivers() {
    check_matrix(Recovery::Hot, vec![Reminder::Deferred]);
}

#[test]
fn cold_with_resume_keeps_deferred_mod_views_for_both_drivers() {
    check_matrix(Recovery::Cold, vec![Reminder::Deferred]);
}

#[test]
fn hot_resume_keeps_native_settings_mod_views_for_both_drivers() {
    check_matrix(
        Recovery::Hot,
        vec![Reminder::Language, Reminder::OutputStyle],
    );
}

#[test]
fn cold_with_resume_keeps_native_settings_mod_views_for_both_drivers() {
    check_matrix(
        Recovery::Cold,
        vec![Reminder::Language, Reminder::OutputStyle],
    );
}

#[test]
fn descriptor_preserves_typed_engine_hook_and_plugin_origins() {
    for kind in [
        "sandbox_instructions",
        "attention_budget",
        "environment",
        "model",
        "output_style_instructions",
        "language",
        "deferred_tools_delta",
        "mcp_instructions_delta",
        "mcp_dropped_tools_delta",
        "agent_listing_delta",
        "skill_listing",
        "nested_memory",
        "edited_text_file",
        "proactivity",
        "auto_mode",
        "auto_mode_exit",
        "total_tokens_reminder",
        "budget_usd",
        "unknown_command_fallback",
    ] {
        let source =
            Utf16JsonProjection::from_serde_value(&serde_json::json!({"type":kind})).unwrap();
        assert_eq!(
            mod_attachment_descriptor_from_payload(&source),
            Some((kind.into(), serde_json::json!({"kind":"engine"})))
        );
    }
    for event in ["prompt.submit", "tool.call"] {
        let source = Utf16JsonProjection::from_serde_value(&serde_json::json!({
            "type":"hook_additional_context", "hookName":event, "hookEvent":"UserPromptSubmit"
        }))
        .unwrap();
        assert_eq!(
            mod_attachment_descriptor_from_payload(&source),
            Some((
                "hook_additional_context".into(),
                serde_json::json!({"kind":"plugin","event":event})
            ))
        );
    }
    let hook =
        Utf16JsonProjection::parse(r#"{"type":"hook_success","hookEvent":"UserPromptExpansion"}"#)
            .unwrap();
    assert_eq!(
        mod_attachment_descriptor_from_payload(&hook),
        Some((
            "hook_success".into(),
            serde_json::json!({"kind":"hook","event":"UserPromptExpansion"})
        ))
    );
    for source in [
        r#"{"type":"hook_success"}"#,
        r#"{"type":"deferred_tools_record","hookEvent":"pretend"}"#,
        r#"{"type":"hook_error_during_execution","hookEvent":"PostToolUse"}"#,
        r#"{"type":"queued_command","commandMode":"prompt"}"#,
    ] {
        assert!(mod_attachment_descriptor_from_payload(
            &Utf16JsonProjection::parse(source).unwrap()
        )
        .is_none());
    }
}
