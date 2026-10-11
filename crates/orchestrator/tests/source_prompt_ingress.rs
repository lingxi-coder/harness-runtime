//! Verify exact UTF-16 prompt units reach orchestrator history and model requests through each text-entry API.

use std::sync::Arc;

use lingxi_core::host::OrchestratorHandle;
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage};
use orchestrator::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};
use orchestrator::{scripted, ConversationOrchestrator, OrchestratorConfig};
use tokio_util::sync::CancellationToken;

fn assert_exact_prompt(messages: &[ConversationMessage], expected: &[u16]) {
    assert!(messages.iter().any(|message| matches!(
        message,
        ConversationMessage::User { content, .. }
            if content.iter().any(|block| matches!(
                block,
                ContentBlock::TextJsUtf16 { utf16_code_units, .. }
                    if utf16_code_units == expected
            ))
    )));
}

#[tokio::test]
async fn source_prompt_reaches_history_and_request_through_every_text_entry() {
    let units = vec![0x61, 0xd800, 0x62, 0xdc00, 0xd83d, 0xde00];
    let prompt =
        Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units.clone()).unwrap();

    for entry in 0..4 {
        let batched = Arc::new(MockApiClient::new(vec![mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "done".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        )]));
        let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![scripted![
            message_start("source-input", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "done"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]]));
        let orch =
            ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig::default(),
                batched.clone(),
                streaming.clone(),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            ));
        match entry {
            0 => {
                orch.run_turn(&prompt).await.unwrap();
            }
            1 => {
                orch.run_turn_with_cancel(&prompt, CancellationToken::new())
                    .await
                    .unwrap();
            }
            2 => {
                orch.run_turn_streaming(&prompt).await.unwrap();
            }
            _ => {
                OrchestratorHandle::run_turn_streaming_with_cancel(
                    orch.as_ref(),
                    &prompt,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            }
        }
        assert_exact_prompt(&orch.snapshot_history().await, &units);
        if entry < 2 {
            assert_exact_prompt(&batched.captured_msgs().await[0], &units);
        } else {
            assert_exact_prompt(&streaming.captured_calls().await[0].messages, &units);
        }
    }
}
