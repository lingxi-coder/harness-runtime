//! cc 2.1.263 Oer: retry from pre-attempt history plus the clean meta nudge.

use lingxi_core::types::{ContentBlock, ConversationMessage};
use llm_runtime::HistoryEvent;
use orchestrator::test_support::{
    content_block_start_text, content_block_start_thinking, content_block_stop, message_delta_stop,
    message_start, message_stop, noop_hook_executor, text_delta, thinking_delta, MockApiClient,
    MockOutputStream, MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use std::{path::PathBuf, sync::Arc};
use tool_api::registry::ToolRegistry;

fn attempt(stop: &str, thinking: bool) -> Vec<HistoryEvent> {
    vec![
        message_start("attempt", "claude-opus-4-7"),
        if thinking {
            content_block_start_thinking(0)
        } else {
            content_block_start_text(0)
        },
        if thinking {
            thinking_delta(0, "private unfinished thought")
        } else {
            text_delta(0, "<invoke name=invalid>")
        },
        content_block_stop(0),
        message_delta_stop(stop),
        message_stop(),
    ]
}

#[tokio::test]
async fn retry_request_omits_malformed_and_thinking_only_attempts() {
    for (stop, thinking, expected_nudge) in [
        ("tool_use", false, "The previous response failed to produce a valid tool call. Please retry the tool call now."),
        ("end_turn", true, "[Your previous response had no visible output. Please continue and produce a user-visible response.]"),
        ("stop_sequence", true, "[Your previous response had no visible output. Please continue and produce a user-visible response.]"),
    ] {
        let api = Arc::new(MockStreamingApiClient::with_turns(vec![
            attempt(stop, thinking),
            vec![message_start("answer", "claude-opus-4-7"), content_block_start_text(0),
                text_delta(0, "Complete answer"), content_block_stop(0),
                message_delta_stop("end_turn"), message_stop()],
        ]));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
            path.clone(), Arc::new(platform_posix::fs::PosixFileSystem::new(dir.path().to_path_buf()))));
        let output = Arc::new(MockOutputStream::new());
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(), Arc::new(MockApiClient::new(vec![])), api.clone(),
            Arc::new(ToolRegistry::new()), noop_hook_executor(), Arc::new(NoOpPermissionGate),
            output.clone(), Arc::new(StaticMemoryProvider::empty()), PathBuf::from("/tmp"),
        ).with_jsonl_writer(writer));
        assert!(matches!(orch.run_turn_streaming("answer the question").await.unwrap(),
            ConversationOutcome::EndTurn { .. }));
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&path).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert!(!rows.iter().any(|row| row["type"] == "tombstone"), "transient events must not become transcript rows");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("private unfinished thought"));
        assert!(!std::fs::read_to_string(&path).unwrap().contains("<invoke name=invalid>"));
        let events = output.snapshot().await;
        assert_eq!(events.iter().filter(|event| matches!(event,
            lingxi_core::host::OutputEvent::MessageRetracted { .. })).count(), 1);
        let calls = api.captured_calls().await;
        assert_eq!(calls.len(), 2, "{stop}");
        assert!(!calls[1].messages.iter().any(|message| matches!(message,
            ConversationMessage::Assistant { .. }
        )), "failed attempt must be removed from the retry request: {stop}");
        assert!(calls[1].messages.iter().any(|message| matches!(message,
            ConversationMessage::User { content, is_meta: true, .. }
                if content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == expected_nudge))
        )), "byte-exact clean meta nudge missing: {stop}");
    }
}

#[tokio::test]
async fn terminal_streaming_errors_persist_api_error_envelopes() {
    for (stop, attempts, expected_error, inner_stop) in [
        ("tool_use", 2, None, "stop_sequence"),
        ("max_tokens", 4, Some("max_output_tokens"), "stop_sequence"),
        (
            "model_context_window_exceeded",
            1,
            Some("max_output_tokens"),
            "stop_sequence",
        ),
        ("refusal", 1, Some("invalid_request"), "refusal"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                dir.path().to_path_buf(),
            )),
        ));
        let api = Arc::new(MockStreamingApiClient::with_turns(
            (0..attempts).map(|_| attempt(stop, false)).collect(),
        ));
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                api,
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/tmp"),
            )
            .with_jsonl_writer(writer),
        );
        orch.run_turn_streaming("answer the question")
            .await
            .unwrap();
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let terminal = rows
            .iter()
            .rev()
            .find(|row| row["type"] == "assistant")
            .unwrap();
        assert_eq!(terminal["isApiErrorMessage"], true, "{stop}: {terminal}");
        assert_eq!(
            terminal.get("error").and_then(|value| value.as_str()),
            expected_error,
            "{stop}"
        );
        assert_eq!(terminal["message"]["stop_reason"], inner_stop, "{stop}");
    }
}
