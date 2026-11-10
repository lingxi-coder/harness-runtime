use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, MockStreamingApiClient,
    NoOpPermissionGate, StaticMemoryProvider,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Entry {
    Batched,
    Streaming,
    QueuedBatch,
    Images,
    ImagesWithMessageId,
    ImagesWithRowToken,
}

fn image_admission<'a>(
    entry: Entry,
    orch: &'a ConversationOrchestrator,
    image_paths: &'a [std::path::PathBuf],
    cancel: CancellationToken,
) -> Pin<Box<dyn Future<Output = Result<TurnOutcome, OrchestratorError>> + 'a>> {
    match entry {
        Entry::Images => Box::pin(orch.run_turn_streaming_with_cancel_images(
            "queued".into(),
            image_paths,
            cancel,
        )),
        Entry::ImagesWithMessageId => {
            Box::pin(orch.run_turn_streaming_with_cancel_images_and_message_id(
                "queued",
                image_paths,
                cancel,
                Some(lingxi_core::types::MessageId::new()),
            ))
        }
        Entry::ImagesWithRowToken => {
            Box::pin(orch.run_turn_streaming_with_cancel_images_and_row_token(
                "queued".into(),
                image_paths,
                cancel,
                "image-row".into(),
            ))
        }
        _ => panic!("expected an image-path entry"),
    }
}

async fn assert_cancelled_admission(entry: Entry, precancelled: bool) {
    let api = Arc::new(MockApiClient::new(Vec::new()));
    let streaming = Arc::new(MockStreamingApiClient::with_turns(Vec::new()));
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        crate::OrchestratorConfig::default(),
        api.clone(),
        streaming.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));
    // The prior turn keeps ownership throughout cancellation of the waiter.
    let _owner = orch.turn_gate.lock().await;
    let cancel = CancellationToken::new();
    if precancelled {
        cancel.cancel();
    }
    let image_root = tempfile::tempdir().unwrap();
    let image_paths = [image_root.path().join("attachment.png")];
    if !precancelled
        && matches!(
            entry,
            Entry::Images | Entry::ImagesWithMessageId | Entry::ImagesWithRowToken
        )
    {
        std::fs::write(&image_paths[0], b"\x89PNG").unwrap();
    }
    let mut waiter: Pin<Box<dyn Future<Output = Result<TurnOutcome, OrchestratorError>> + '_>> =
        match entry {
            Entry::Batched => Box::pin(orch.run_turn_with_cancel("queued", cancel.clone())),
            Entry::Streaming => {
                Box::pin(orch.run_turn_streaming_with_cancel("queued", cancel.clone()))
            }
            Entry::Images | Entry::ImagesWithMessageId | Entry::ImagesWithRowToken => {
                image_admission(entry, &orch, &image_paths, cancel.clone())
            }
            Entry::QueuedBatch => Box::pin(orch.run_queued_prompt_batch(
                vec![crate::QueuedPromptInput {
                    goal_retry_id: None,
                    text: "queued".into(),
                    images: Vec::new(),
                    is_meta: false,
                    turn_companion: false,
                    skill_identity: None,
                    trailing_history_rows: Vec::new(),
                    mod_origin: None,
                    message_origin: None,
                    prompt_source: None,
                    sdk_content_is_json_string: None,
                    prompt_ingress_kind: None,
                    skip_model_query: false,
                    message_id: None,
                    pending_prompt_identity: None,
                    transcript_row_token: None,
                    queue_priority: None,
                    scheduled_task_id: None,
                    scheduled_fire_id: None,
                }],
                cancel.clone(),
            )),
        };
    if !precancelled {
        tokio::select! {
            biased;
            result = &mut waiter => panic!("waiter bypassed an owned gate: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        cancel.cancel();
    }
    let outcome = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("cancelled waiter must finish while the prior turn still owns the gate")
        .unwrap();
    assert!(
        matches!(outcome, TurnOutcome::Cancelled),
        "{entry:?}: {outcome:?}"
    );
    assert!(orch.snapshot_history().await.is_empty());
    assert!(api.captured_msgs().await.is_empty());
    assert!(streaming.captured_calls().await.is_empty());
}

#[tokio::test]
async fn cancelled_batched_admission_does_not_wait_for_the_running_turn() {
    for precancelled in [false, true] {
        assert_cancelled_admission(Entry::Batched, precancelled).await;
    }
}

#[tokio::test]
async fn cancelled_streaming_admission_does_not_wait_for_the_running_turn() {
    for precancelled in [false, true] {
        assert_cancelled_admission(Entry::Streaming, precancelled).await;
    }
}

#[tokio::test]
async fn cancelled_queued_batch_admission_does_not_wait_for_the_running_turn() {
    for precancelled in [false, true] {
        assert_cancelled_admission(Entry::QueuedBatch, precancelled).await;
    }
}

#[tokio::test]
async fn cancelled_image_admission_does_not_read_attachments() {
    for entry in [
        Entry::Images,
        Entry::ImagesWithMessageId,
        Entry::ImagesWithRowToken,
    ] {
        assert_cancelled_admission(entry, true).await;
    }
}

#[tokio::test]
async fn cancelled_image_admission_does_not_wait_for_the_running_turn() {
    for entry in [
        Entry::Images,
        Entry::ImagesWithMessageId,
        Entry::ImagesWithRowToken,
    ] {
        assert_cancelled_admission(entry, false).await;
    }
}

#[tokio::test]
async fn failed_image_admission_does_not_append_or_call_the_api() {
    let root = tempfile::tempdir().unwrap();
    let paths = [
        root.path().join("first-missing.png"),
        root.path().join("later-missing.png"),
    ];
    for entry in [
        Entry::Images,
        Entry::ImagesWithMessageId,
        Entry::ImagesWithRowToken,
    ] {
        let api = Arc::new(MockApiClient::new(Vec::new()));
        let streaming = Arc::new(MockStreamingApiClient::with_turns(Vec::new()));
        let orch =
            ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
                crate::OrchestratorConfig::default(),
                api.clone(),
                streaming.clone(),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_path_buf(),
            ));
        let _owner = orch.turn_gate.lock().await;
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            image_admission(entry, &orch, &paths, CancellationToken::new()),
        )
        .await
        .expect("a read failure must finish before waiting for turn admission")
        .unwrap_err();
        assert!(
            error.to_string().contains("first-missing.png"),
            "{entry:?}: {error}"
        );
        assert!(orch.snapshot_history().await.is_empty());
        assert!(api.captured_msgs().await.is_empty());
        assert!(streaming.captured_calls().await.is_empty());
    }
}
