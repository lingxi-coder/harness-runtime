//! Request-only context survives model continuation without becoming history.

use lingxi_core::host::FileSystem;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, ToolUseId,
};
use orchestrator::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, noop_hook_executor, text_delta, MockApiClient,
    MockOutputStream, MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    scripted, ConversationOrchestrator, OrchestratorConfig, RequestContextTranscriptAttachment,
    TurnOutcome,
};
use platform_posix::fs::PosixFileSystem;
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tool_api::registry::ToolRegistry;

fn source_text(row: &ConversationMessage) -> Option<Vec<u16>> {
    let ConversationMessage::User { content, .. } = row else {
        return None;
    };
    match content.first()? {
        ContentBlock::Text { text, .. } => Some(text.encode_utf16().collect()),
        ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => Some(utf16_code_units.clone()),
        _ => None,
    }
}

#[test]
fn request_context_attachment_persists_as_a_rendered_transcript_sidecar() {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    use orchestrator::test_support::mock_message_response;
                    let response = mock_message_response(
                        vec![llm_runtime::ContentBlock::Text {
                            text: "done".into(),
                            cache_control: None,
                            citations: None,
                        }],
                        Some("end_turn"),
                    );
                    let api = Arc::new(MockApiClient::new(vec![response]));
                    let dir = tempfile::tempdir().unwrap();
                    let transcript = dir.path().join("fallback-sidecar.jsonl");
                    let fs: Arc<dyn FileSystem> =
                        Arc::new(PosixFileSystem::new(dir.path().to_owned()));
                    let orch = ConversationOrchestrator::into_shared(
                        ConversationOrchestrator::new(
                            OrchestratorConfig::default(),
                            api.clone(),
                            Arc::new(ToolRegistry::new()),
                            noop_hook_executor(),
                            Arc::new(NoOpPermissionGate),
                            Arc::new(MockOutputStream::new()),
                            Arc::new(StaticMemoryProvider::empty()),
                            dir.path().to_owned(),
                        )
                        .with_jsonl_writer(Arc::new(JsonlWriter::new(
                            transcript.clone(),
                            fs.clone(),
                        ))),
                    );
                    let reminder_text = "<system-reminder>\nUnknown slash fallback\n</system-reminder>";
                    let reminder = Utf16JsonProjection::parse(
                        &serde_json::to_string(reminder_text).unwrap(),
                    )
                    .unwrap();
                    let payload = Utf16JsonProjection::parse(
                        r#"{"type":"unknown_command_fallback","commandName":"probe","availableCommandNames":["run"],"availableCommandCount":1}"#,
                    )
                    .unwrap();
                    let rendered = Utf16JsonProjection::parse(
                        &format!(
                            "[{{\"content\":{}}}]",
                            serde_json::to_string(reminder_text).unwrap()
                        ),
                    )
                    .unwrap();
                    let attachment = RequestContextTranscriptAttachment {
                        message_id: lingxi_core::types::MessageId::new(),
                        payload,
                        rendered,
                        rendered_role: "user",
                    };

                    assert_eq!(
                        orch.run_turn_with_request_context(
                            "/probe fixed-argument",
                            vec![reminder],
                            Some(attachment),
                            CancellationToken::new(),
                        )
                        .await
                        .unwrap(),
                        TurnOutcome::EndTurn,
                    );

                    let calls = api.captured_msgs().await;
                    assert_eq!(calls.len(), 1);
                    let outbound_reminders = calls[0]
                        .iter()
                        .filter(|row| row.text_content().contains("Unknown slash fallback"))
                        .collect::<Vec<_>>();
                    assert_eq!(outbound_reminders.len(), 1);
                    assert!(matches!(
                        outbound_reminders[0],
                        ConversationMessage::User { is_meta: false, .. }
                    ));
                    let fallback_request_index = calls[0]
                        .iter()
                        .position(|row| row.id() == outbound_reminders[0].id())
                        .unwrap();
                    let prompt_request_index = calls[0]
                        .iter()
                        .position(|row| row.text_content() == "/probe fixed-argument")
                        .expect("original slash prompt is in the same API request");
                    assert!(
                        fallback_request_index < prompt_request_index,
                        "the API projection places the fallback context before the original prompt"
                    );

                    let rows = JsonlReader::new(transcript, fs).read_all().await.unwrap();
                    let fallback_rows = rows
                        .iter()
                        .enumerate()
                        .filter(|(_, row)| {
                            row.message_type == "attachment"
                                && row
                                    .attachment_projection
                                    .as_ref()
                                    .and_then(|payload| payload.string_at("/type"))
                                    == Some("unknown_command_fallback")
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(fallback_rows.len(), 1);
                    let (fallback_index, fallback) = fallback_rows[0];
                    assert_eq!(
                        outbound_reminders[0].id().as_uuid().to_string(),
                        fallback.uuid,
                        "the API fallback row and typed transcript sidecar share one identity"
                    );
                    assert_eq!(fallback.extra.string_at("/renderedRole"), Some("user"));
                    assert_eq!(fallback.extra.array_len("/rendered"), Some(1));
                    assert_eq!(
                        fallback.extra.string_at("/rendered/0/content"),
                        Some(reminder_text)
                    );
                    assert_eq!(
                        fallback
                            .attachment_projection
                            .as_ref()
                            .and_then(|payload| payload.string_at("/commandName")),
                        Some("probe")
                    );
                    assert!(fallback_index > 0);
                    let prompt_transcript_index = rows
                        .iter()
                        .position(|row| {
                            row.message_type == "user"
                                && row.message.string_at("/content/0/text")
                                    == Some("/probe fixed-argument")
                        })
                        .expect("original slash prompt is persisted as a user row");
                    assert!(
                        prompt_transcript_index < fallback_index,
                        "JSONL writes the user prompt before its ingress/fallback attachments"
                    );
                    assert_eq!(
                        fallback.parent_uuid.as_deref(),
                        Some(rows[fallback_index - 1].uuid.as_str()),
                        "the sidecar advances the durable transcript parent chain"
                    );
                    assert!(rows[fallback_index + 1..]
                        .iter()
                        .any(|row| row.message_type == "assistant"));
                });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn context_repeats_on_continuation_but_never_persists_or_leaks_to_the_next_turn() {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let tool_step = scripted![
                        message_start("first", "claude-sonnet-4-5"),
                        content_block_start_tool_use(0, ToolUseId::from("missing"), "MissingTool"),
                        input_json_delta(0, "{}"),
                        content_block_stop(0),
                        message_delta_stop("tool_use"),
                        message_stop(),
                    ];
                    let final_step = scripted![
                        message_start("final", "claude-sonnet-4-5"),
                        content_block_start_text(0),
                        text_delta(0, "done"),
                        content_block_stop(0),
                        message_delta_stop("end_turn"),
                        message_stop(),
                    ];
                    let api = Arc::new(MockStreamingApiClient::with_turns(vec![
                        tool_step,
                        final_step.clone(),
                        final_step,
                    ]));
                    let dir = tempfile::tempdir().unwrap();
                    let transcript = dir.path().join("session.jsonl");
                    let fs: Arc<dyn FileSystem> =
                        Arc::new(PosixFileSystem::new(dir.path().to_owned()));
                    let orch = ConversationOrchestrator::into_shared(
                        ConversationOrchestrator::new_with_streaming(
                            OrchestratorConfig {
                                max_turns: 3,
                                ..Default::default()
                            },
                            Arc::new(MockApiClient::new(Vec::new())),
                            api.clone(),
                            Arc::new(ToolRegistry::new()),
                            noop_hook_executor(),
                            Arc::new(NoOpPermissionGate),
                            Arc::new(MockOutputStream::new()),
                            Arc::new(StaticMemoryProvider::empty()),
                            dir.path().to_owned(),
                        )
                        .with_jsonl_writer(Arc::new(JsonlWriter::new(
                            transcript.clone(),
                            fs.clone(),
                        ))),
                    );
                    let context =
                        Utf16JsonProjection::parse(r#""ONLY_REQUEST_CONTEXT \ud800""#).unwrap();
                    let units = context.string_units("").unwrap();
                    assert_eq!(
                        orch.run_turn_streaming_with_request_context(
                            "/unknown",
                            vec![context],
                            None,
                            CancellationToken::new(),
                        )
                        .await
                        .unwrap(),
                        TurnOutcome::EndTurn,
                    );
                    orch.run_turn_streaming_with_cancel("next turn", CancellationToken::new())
                        .await
                        .unwrap();
                    let calls = api.captured_calls().await;
                    assert_eq!(calls.len(), 3);
                    for call in &calls[..2] {
                        let matches = call
                            .messages
                            .iter()
                            .enumerate()
                            .filter(|(_, row)| source_text(row).as_ref() == Some(&units))
                            .collect::<Vec<_>>();
                        assert_eq!(matches.len(), 1);
                        let (index, row) = matches[0];
                        assert!(matches!(
                            row,
                            ConversationMessage::User { is_meta: true, .. }
                        ));
                        let prompt_index = call
                            .messages
                            .iter()
                            .position(|message| {
                                source_text(message) == Some("/unknown".encode_utf16().collect())
                            })
                            .expect("unknown slash prompt survives the request projection");
                        assert!(
                            index < prompt_index,
                            "request-only context stays before the prompt, after turn reminders"
                        );
                    }
                    assert!(calls[2]
                        .messages
                        .iter()
                        .all(|row| source_text(row).as_ref() != Some(&units)));
                    let rows = JsonlReader::new(transcript, fs).read_all().await.unwrap();
                    assert!(rows.iter().all(|row| row
                        .message
                        .string_units("/content/0/text")
                        .as_ref()
                        != Some(&units)));
                    assert!(rows
                        .iter()
                        .any(|row| row.message.string_at("/content/0/text") == Some("/unknown")));
                });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn batched_context_stays_request_owned_and_next_turn_keeps_images() {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    use orchestrator::test_support::mock_message_response;
                    let finish = mock_message_response(
                        vec![llm_runtime::ContentBlock::Text {
                            text: "done".into(),
                            cache_control: None,
                            citations: None,
                        }],
                        Some("end_turn"),
                    );
                    let api = Arc::new(MockApiClient::new(vec![
                        mock_message_response(
                            vec![llm_runtime::ContentBlock::ToolCall {
                                id: "missing".into(),
                                name: "MissingTool".into(),
                                input: lingxi_llm_client::protocol::JsonPayload::parse("{}").unwrap(),
                            }],
                            Some("tool_use"),
                        ),
                        finish.clone(),
                        finish,
                    ]));
                    let dir = tempfile::tempdir().unwrap();
                    let transcript = dir.path().join("batched-session.jsonl");
                    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_owned()));
                    let orch = ConversationOrchestrator::into_shared(
                        ConversationOrchestrator::new(
                            OrchestratorConfig { max_turns: 3, ..Default::default() },
                            api.clone(),
                            Arc::new(ToolRegistry::new()),
                            noop_hook_executor(),
                            Arc::new(NoOpPermissionGate),
                            Arc::new(MockOutputStream::new()),
                            Arc::new(StaticMemoryProvider::empty()),
                            dir.path().to_owned(),
                        ).with_jsonl_writer(Arc::new(JsonlWriter::new(transcript.clone(), fs.clone()))),
                    );
                    let context = Utf16JsonProjection::parse(r#""BATCHED_CONTEXT \ud800""#).unwrap();
                    let units = context.string_units("").unwrap();
                    assert_eq!(
                        orch.run_turn_with_request_context("/unknown", vec![context], None, CancellationToken::new()).await.unwrap(),
                        TurnOutcome::EndTurn,
                    );
                    let image = lingxi_core::types::ImageSource::Url { url: "https://example.invalid/fixture.png".into() };
                    orch.run_turn_with_cancel_image_sources("image turn", vec![image.clone()], CancellationToken::new()).await.unwrap();
                    let calls = api.captured_msgs().await;
                    assert_eq!(calls.len(), 3);
                    for call in &calls[..2] {
                        let indices = call.iter().enumerate().filter(|(_, row)| source_text(row).as_ref() == Some(&units)).map(|(index, _)| index).collect::<Vec<_>>();
                        assert_eq!(indices.len(), 1);
                        let prompt_index = call
                            .iter()
                            .position(|row| {
                                source_text(row) == Some("/unknown".encode_utf16().collect())
                            })
                            .expect("unknown slash prompt survives the batched projection");
                        assert!(
                            indices[0] < prompt_index,
                            "request-only context stays before the prompt, after turn reminders"
                        );
                    }
                    assert!(calls[2].iter().all(|row| source_text(row).as_ref() != Some(&units)));
                    let image_row = calls[2].iter().find(|row| source_text(row) == Some("image turn".encode_utf16().collect())).unwrap();
                    assert!(matches!(image_row, ConversationMessage::User { content, .. }
                        if content.iter().any(|block| matches!(block, ContentBlock::Image { source } if source == &image))));
                    let rows = JsonlReader::new(transcript, fs).read_all().await.unwrap();
                    assert!(rows.iter().all(|row| row.message.string_units("/content/0/text").as_ref() != Some(&units)));
                    assert!(rows.iter().any(|row| row.message.string_at("/content/1/source/url") == Some("https://example.invalid/fixture.png")));
                });
        }).unwrap().join().unwrap();
}
