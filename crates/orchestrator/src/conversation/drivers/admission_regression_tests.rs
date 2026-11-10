use super::*;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};

#[tokio::test]
async fn queued_images_reach_each_submit_hook_history_provider_and_transcript() {
    for streaming in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let module = root.path().join("queued-images.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
          on('prompt.submit', ($, e, next) => {
            const expected = e.text === 'first' ? 'image/png' : 'image/jpeg';
            if (e.attachments?.length !== 1 || e.attachments[0].mediaType !== expected) {
              return {drop:'wrong queued image metadata'};
            }
            return next({...e, text:`seen:${e.text}`});
          });
        }"#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            "queued-images",
            root.path(),
            &module,
            Utf16JsonProjection::parse("{}").unwrap(),
        )
        .await
        .unwrap();
        let mut registry = hooks::HookRegistry::new();
        registry.set_mod_host(host);
        let api = Arc::new(MockApiClient::new(vec![mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "done".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        )]));
        let stream = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
            message_start("queued-images", "model"),
            content_block_start_text(0),
            text_delta(0, "done"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]]));
        let path = root.path().join("session.jsonl");
        let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix::fs::PosixFileSystem::new(root.path().to_path_buf()),
        );
        let orch = ConversationOrchestrator::new_with_streaming(
            crate::OrchestratorConfig::default(),
            api.clone(),
            stream.clone(),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)))
        .with_jsonl_writer(Arc::new(session::jsonl::writer::JsonlWriter::new(
            path.clone(),
            fs,
        )));
        let orch = ConversationOrchestrator::into_shared(orch);
        let ids = [MessageId::new(), MessageId::new()];
        let specs = [
            ("first", "image/png", "aW1hZ2Utb25l"),
            ("second", "image/jpeg", "aW1hZ2UtdHdv"),
        ];
        let inputs = specs
            .iter()
            .zip(ids)
            .map(|((text, media, data), id)| QueuedPromptInput {
                text: (*text).into(),
                message_id: Some(id),
                images: vec![ImageSource::Base64 {
                    media_type: (*media).into(),
                    data: (*data).into(),
                }],
                ..Default::default()
            })
            .collect();
        let result = if streaming {
            orch.run_queued_prompt_batch(inputs, CancellationToken::new())
                .await
        } else {
            orch.run_queued_prompt_batch_batched(inputs, CancellationToken::new())
                .await
        };
        assert_eq!(result.unwrap(), TurnOutcome::EndTurn);
        let calls = if streaming {
            stream
                .captured_calls()
                .await
                .into_iter()
                .map(|call| call.messages)
                .collect::<Vec<_>>()
        } else {
            api.captured_msgs().await
        };
        assert_eq!(calls.len(), 1);
        let history = orch.snapshot_history().await;
        let rows = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        for (index, (text, media, data)) in specs.iter().enumerate() {
            for messages in [&history, &calls[0]] {
                let message = messages
                    .iter()
                    .find(|message| message.id() == ids[index])
                    .unwrap();
                assert_eq!(message.text_content(), format!("seen:{text}"));
                let ConversationMessage::User { content, .. } = message else {
                    panic!("user row")
                };
                assert!(content.iter().any(|block| matches!(block,
                    ContentBlock::Image { source:ImageSource::Base64 {media_type,data:actual} }
                        if media_type.as_str()==*media && actual.as_str()==*data)));
            }
            let uuid = ids[index].as_uuid().to_string();
            let row = rows
                .iter()
                .find(|row| row["uuid"].as_str() == Some(uuid.as_str()))
                .unwrap();
            assert!(row.to_string().contains(data));
            assert!(row.to_string().contains(&format!("seen:{text}")));
        }
    }
}
