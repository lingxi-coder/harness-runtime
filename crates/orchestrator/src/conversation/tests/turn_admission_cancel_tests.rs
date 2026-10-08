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
    let image_paths = [image_root.path().join("missing.png")];
    let mut waiter: Pin<Box<dyn Future<Output = Result<TurnOutcome, OrchestratorError>>>> =
        match entry {
            Entry::Batched => Box::pin(orch.run_turn_with_cancel("queued", cancel.clone())),
            Entry::Streaming => {
                Box::pin(orch.run_turn_streaming_with_cancel("queued", cancel.clone()))
            }
            Entry::Images => Box::pin(orch.run_turn_streaming_with_cancel_images(
                "queued",
                &image_paths,
                cancel.clone(),
            )),
            Entry::QueuedBatch => Box::pin(orch.run_queued_prompt_batch(
                vec![crate::QueuedPromptInput {
                    goal_retry_id: None,
                    text: "queued".into(),
                    is_meta: false,
                    mod_origin: None,
                    message_id: None,
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
    assert_cancelled_admission(Entry::Images, true).await;
}
