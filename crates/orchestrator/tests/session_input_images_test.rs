//! Human input has one image owner even when it is carried as a queued input.
use std::sync::Arc;

use lingxi_core::types::{ContentBlock, ConversationMessage, ImageSource};
use orchestrator::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    noop_hook_executor, text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient,
    NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{scripted, ConversationOrchestrator, OrchestratorConfig};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn identified_inputs_append_each_attachment_once_without_deduplicating_user_choices() {
    for count in [1, 2] {
        let root = tempfile::tempdir().unwrap();
        let api = Arc::new(MockStreamingApiClient::with_turns(vec![scripted![
            message_start("reply", "test-model"),
            content_block_start_text(0),
            text_delta(0, "received"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop()
        ]]));
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(Vec::new())),
                api.clone(),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_path_buf(),
            )
            .with_config_home(root.path().join("home")),
        );
        let images = vec![
            ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "aW1hZ2U=".into()
            };
            count
        ];
        orch.run_turn_streaming_with_origin_and_row_token(
            "describe attachments",
            images,
            CancellationToken::new(),
            None,
            true,
            Some(format!("client-turn:{count}")),
        )
        .await
        .unwrap();
        let calls = api.captured_calls().await;
        let attachments = calls[0]
            .messages
            .iter()
            .filter_map(|message| match message {
                ConversationMessage::User { content, .. }
                    if message.text_content().contains("describe attachments") =>
                {
                    Some(content)
                }
                _ => None,
            })
            .flatten()
            .filter(|block| matches!(block, ContentBlock::Image { .. }))
            .count();
        assert_eq!(attachments, count);
    }
}
