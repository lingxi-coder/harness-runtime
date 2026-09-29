use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use platform_posix::fs::PosixFileSystem;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

fn failing_orchestrator(root: &std::path::Path) -> (ConversationOrchestrator, MockOutputStream) {
    // A directory at the transcript's file path deterministically makes
    // every append fail without relying on platform permission semantics.
    let transcript_path = root.join("transcript.jsonl");
    std::fs::create_dir(&transcript_path).expect("create blocking directory");
    let fs: Arc<dyn lingxi_core::host::FileSystem> =
        Arc::new(PosixFileSystem::new(root.to_path_buf()));
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        transcript_path,
        fs,
    ));
    let output = MockOutputStream::new();
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(output.clone()),
        Arc::new(StaticMemoryProvider::empty()),
        root.to_path_buf(),
    )
    .with_jsonl_writer(writer);
    (orch, output)
}

#[tokio::test]
async fn transcript_append_failure_is_silent_to_the_user() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (orch, output) = failing_orchestrator(dir.path());

    let user = ConversationMessage::user(MessageId::new(), "hello".into());
    orch.persist_message_to_jsonl(&user).await;
    orch.persist_active_goal_state_to_jsonl(None).await;

    let (boundary, metadata) = compaction::create_compact_boundary(
        compaction::CompactTrigger::Manual,
        1,
        None,
        None,
        Some(1),
        &[],
    );
    orch.persist_compact_boundary_to_jsonl(&boundary, &metadata)
        .await;

    let per_block = ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![
            lingxi_core::types::ContentBlock::Text {
                text: "first".into(),
            },
            lingxi_core::types::ContentBlock::Text {
                text: "second".into(),
            },
        ],
        stop_reason: Some("end_turn".into()),
    };
    orch.persist_assistant_per_block(&per_block, None, None)
        .await;

    let merged = ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: "merged".into(),
        }],
        stop_reason: Some("end_turn".into()),
    };
    orch.persist_assistant_merged(&merged, None, None).await;

    // CC 2.1.218 shows NO user-visible notice for a transcript-append
    // failure — the handling is log + telemetry only. Every persist path
    // above failed against the failing writer; none may surface a
    // SystemNotice to the user.
    let notice_count = output
        .snapshot()
        .await
        .into_iter()
        .filter(|event| matches!(event, lingxi_core::host::OutputEvent::SystemNotice { .. }))
        .count();
    assert_eq!(
        notice_count, 0,
        "transcript-append failures must not surface a user-visible notice"
    );
}
