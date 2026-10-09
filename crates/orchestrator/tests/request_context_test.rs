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
use orchestrator::{scripted, ConversationOrchestrator, OrchestratorConfig, TurnOutcome};
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
                        assert_eq!(
                            source_text(&call.messages[index + 1]),
                            Some("/unknown".encode_utf16().collect())
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
