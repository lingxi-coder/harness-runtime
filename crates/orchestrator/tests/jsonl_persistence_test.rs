//!
//! Drives turns through the public `run_turn` API with a `JsonlWriter`
//! attached, then reads the file back via `JsonlReader` and asserts the
//! `parentUuid` chain — proves on-disk persistence is wired correctly and
//! the chain is monotonic.
use lingxi_core::host::FileSystem;
use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use platform_posix::fs::PosixFileSystem;
use session::jsonl::reader::JsonlReader;
use session::jsonl::writer::JsonlWriter;
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn two_turns_persist_user_assistant_messages_with_parent_uuid_chain() {
    let dir = tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = Arc::new(JsonlWriter::new(session_path.clone(), fs.clone()));

    // Two scripted batched responses, both `end_turn` — drives two independent
    // run_turn calls that share the same orchestrator (and thus the same
    // `last_jsonl_uuid` chain).
    let r1 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "first reply".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    );
    let r2 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "second reply".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    );
    let api = Arc::new(MockApiClient::new(vec![r1, r2]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        tools,
        hooks,
        perms,
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_jsonl_writer(writer);

    // Drive two turns through the public API.
    let _ = orch.run_turn("prompt one").await.expect("turn 1");
    let _ = orch.run_turn("prompt two").await.expect("turn 2");

    // Read back the JSONL.
    let reader = JsonlReader::new(session_path, fs);
    let msgs = reader.read_all().await.expect("read_all");

    // The first Normal/Announced query records the empty session-context anchor
    // and date before its token reminder, then persists the write-once static
    // prompt snapshot. The second query correctly deduplicates the unchanged
    // context/date and snapshot, while recording its own token reminder.
    assert_eq!(
        msgs.iter()
            .map(|m| m.message_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "user",
            "attachment",
            "attachment",
            "attachment",
            "attachment",
            "assistant",
            "user",
            "attachment",
            "assistant"
        ],
        "unexpected JSONL entry sequence"
    );
    let attachment_types = msgs
        .iter()
        .filter(|message| message.message_type == "attachment")
        .map(|message| {
            message
                .extra
                .get("attachment")
                .and_then(|attachment| attachment.get("type"))
                .and_then(|kind| kind.as_str())
                .expect("attachment row has a typed attachment")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        attachment_types,
        vec![
            "session_context",
            "date",
            "total_tokens_reminder",
            "prompt_snapshot",
            "total_tokens_reminder"
        ],
        "Normal context/date rows precede turn reminders, and unchanged announcements dedupe"
    );
    assert_eq!(
        msgs[1].extra["attachment"]["context"],
        serde_json::json!({}),
        "empty instruction sources still persist the typed session-context anchor"
    );
    assert!(msgs[2].extra["attachment"]["date"].is_string());
    assert_eq!(
        msgs[4].extra["attachment"]["contextRendering"], "announced",
        "the one-time prompt snapshot records the selected context rendering"
    );
    assert_eq!(
        msgs.len(),
        9,
        "expected 9 JSONL entries (2 user + 2 assistant + 5 typed attachments), got {} — entries: {:?}",
        msgs.len(),
        msgs.iter().map(|m| &m.message_type).collect::<Vec<_>>(),
    );

    // First entry's parent_uuid is None (start of chain).
    assert_eq!(msgs[0].parent_uuid, None, "first parent_uuid must be None");

    // Every subsequent entry's parent_uuid equals the previous entry's uuid.
    for i in 1..msgs.len() {
        assert_eq!(
            msgs[i].parent_uuid.as_deref(),
            Some(msgs[i - 1].uuid.as_str()),
            "broken chain at index {i}: prev.uuid={:?}, this.parent_uuid={:?}",
            msgs[i - 1].uuid,
            msgs[i].parent_uuid,
        );
    }

    // Sanity: every UUID must match the schema's expected format
    // (8-4-4-4-12 lowercase hex — see `jsonl::uuid::validate_uuid`).
    for (i, m) in msgs.iter().enumerate() {
        assert!(
            session::jsonl::uuid::validate_uuid(&m.uuid),
            "entry {i} uuid {:?} fails validate_uuid",
            m.uuid,
        );
    }

    // Outcome sanity: both turns reached end_turn.
    let snap = output.snapshot().await;
    let end_turns = snap
        .iter()
        .filter(|e| matches!(e, lingxi_core::host::OutputEvent::EndTurn { .. }))
        .count();
    assert_eq!(end_turns, 2, "expected 2 EndTurn events; got snap {snap:?}");
}

#[tokio::test]
async fn orchestrator_without_writer_creates_no_file() {
    // Sanity: with `None` for jsonl_writer (the default constructor),
    // the orchestrator does NOT touch the filesystem under tempdir.
    let dir = tempdir().expect("tempdir");

    let response = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    );
    let api = Arc::new(MockApiClient::new(vec![response]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        tools,
        hooks,
        perms,
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    );
    // NOTE: no `.with_jsonl_writer(...)` call.

    let _ = orch.run_turn("hi").await.expect("turn");

    // Walk the temp dir — must be empty (orchestrator created no files).
    let count = std::fs::read_dir(dir.path()).expect("readdir").count();
    assert_eq!(
        count, 0,
        "orchestrator without writer must not create any files; found {count}"
    );
}

#[tokio::test]
async fn batched_run_turn_persists_the_real_model_and_usage_not_synthetic() {
    // Regression (found via `--bg` / `--print` smoke): the NON-streaming
    // (batched) `run_turn` path — used by `--print` and the `--bg` daemon
    // worker — must persist the assistant line as a FULL BetaMessage envelope
    // carrying the real model + usage, exactly like the streaming path's
    // `persist_assistant_per_block`. It had regressed to the model-less
    // `persist_message_to_jsonl`, so every headless reply was recorded as
    // `model:"<synthetic>"` with `usage` dropped (cost lost, resume/telemetry
    // mis-attributed) even though the API call succeeded.
    let dir = tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = Arc::new(JsonlWriter::new(session_path.clone(), fs.clone()));

    let r1 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hi there".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    );
    let api = Arc::new(MockApiClient::new(vec![r1]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        tools,
        hooks,
        perms,
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_jsonl_writer(writer);

    let _ = orch.run_turn("hi").await.expect("turn");

    // Inspect the RAW on-disk assistant line's inner BetaMessage.
    let body = std::fs::read_to_string(&session_path).expect("read session file");
    let assistant = body
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "assistant")
        .expect("an assistant line on disk");
    let inner = &assistant["message"];
    assert_ne!(
        inner["model"],
        serde_json::json!("<synthetic>"),
        "batched assistant line must carry the REAL model, not the synthetic sentinel: {inner}"
    );
    assert!(
        inner["model"].is_string() && !inner["model"].as_str().unwrap().is_empty(),
        "assistant line must carry a non-empty model: {inner}"
    );
    assert!(
        inner.get("usage").is_some() && !inner["usage"].is_null(),
        "batched assistant line must carry `usage` (cost is derived from it): {inner}"
    );
}

#[tokio::test]
async fn assistant_line_records_session_effort_level_2_1_212() {
    // 2.1.212: with a session effort configured (CLI `--effort`), every REAL
    // assistant transcript line records it as a top-level `effort` LEVEL string
    // — 1:1 with claude's `...effort!==void 0&&{effort}` spread. USER lines never
    // carry it, and a session with NO effort omits the field entirely.
    let dir = tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = Arc::new(JsonlWriter::new(session_path.clone(), fs.clone()));

    let r1 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "reply".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    );
    let api = Arc::new(MockApiClient::new(vec![r1]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());

    let cfg = OrchestratorConfig {
        effort: Some("high".into()),
        ..OrchestratorConfig::default()
    };
    let orch = ConversationOrchestrator::new(
        cfg,
        api.clone(),
        tools,
        hooks,
        perms,
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_jsonl_writer(writer);

    let _ = orch.run_turn("hi").await.expect("turn");

    let body = std::fs::read_to_string(&session_path).expect("read session file");
    let lines: Vec<serde_json::Value> = body
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect();

    let assistant = lines
        .iter()
        .find(|v| v["type"] == "assistant")
        .expect("an assistant line on disk");
    assert_eq!(
        assistant.get("effort").and_then(|v| v.as_str()),
        Some("high"),
        "assistant line must record the session effort level: {assistant}"
    );

    let user = lines
        .iter()
        .find(|v| v["type"] == "user")
        .expect("a user line on disk");
    assert!(
        user.get("effort").is_none(),
        "user lines must NOT carry effort: {user}"
    );
}

#[tokio::test]
async fn assistant_line_omits_effort_when_session_has_none_2_1_212() {
    // Parity default: no `--effort` ⟹ NO `effort` field on any line (claude's
    // `!==void 0` guard), so default-session transcripts stay byte-identical.
    let dir = tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = Arc::new(JsonlWriter::new(session_path.clone(), fs.clone()));

    let r1 = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "reply".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    );
    let api = Arc::new(MockApiClient::new(vec![r1]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        tools,
        hooks,
        perms,
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_jsonl_writer(writer);

    let _ = orch.run_turn("hi").await.expect("turn");

    let body = std::fs::read_to_string(&session_path).expect("read session file");
    for line in body.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        assert!(
            v.get("effort").is_none(),
            "no line may carry effort when the session has none: {v}"
        );
    }
}
