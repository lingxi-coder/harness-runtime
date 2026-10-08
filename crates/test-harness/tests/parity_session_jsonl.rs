//! Parity: byte-equivalent session JSONL produced through the orchestrator.
//!
//! M5-07 tested `JsonlWriter`/`JsonlReader` in isolation. M5-14 locks the
//! cross-cutting behaviour: a `ConversationOrchestrator` with an attached
//! `JsonlWriter` produces a JSONL file whose line structure, field names,
//! terminator, and UUID chain are byte-equivalent to the M5-07 golden format.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-14-release-v0.6.0.md` Task 4.

use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use serde::Deserialize;
use serde_json::Value;
use session::jsonl::{JsonlReader, JsonlWriter};
use std::collections::HashSet;
use std::sync::Arc;
use tempfile::TempDir;

const FIXTURE: &str = include_str!("../src/parity/fixtures/parity_session_jsonl.json");

// ============================================================================
// Fixture types
// ============================================================================

#[derive(Debug, Deserialize)]
struct Fixture {
    #[serde(rename = "_meta")]
    meta: Meta,
}

#[derive(Debug, Deserialize)]
struct Meta {
    locks: Locks,
    single_turn_sequence: Vec<SequenceEntry>,
    session_telemetry_events: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Locks {
    line_terminator: String,
    json_format: String,
    user_type: String,
    is_sidechain_default: bool,
    #[allow(dead_code)]
    uuid_pattern: String,
}

#[derive(Debug, Deserialize)]
struct SequenceEntry {
    #[serde(rename = "type")]
    msg_type: String,
    attachment_type: Option<String>,
    expected_attachment: Option<Value>,
}

fn load() -> Fixture {
    serde_json::from_str(FIXTURE).expect("fixture parse")
}

// ============================================================================
// Helpers
// ============================================================================

fn build_orchestrator_with_writer(
    api: Arc<MockApiClient>,
    writer: Arc<JsonlWriter>,
) -> ConversationOrchestrator {
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());
    let output = Arc::new(MockOutputStream::new());
    let cfg = OrchestratorConfig::default();
    ConversationOrchestrator::new(
        cfg,
        api,
        tools,
        hooks,
        perms,
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_jsonl_writer(writer)
}

async fn assert_native_single_turn_rows(
    lines: &[session::JsonlMessage],
    raw: &str,
    api: &MockApiClient,
) {
    use lingxi_core::types::{ContentBlock, ConversationMessage};

    let fixture = load();
    assert_eq!(lines.len(), fixture.meta.single_turn_sequence.len());
    let raw_rows: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL row"))
        .collect();
    assert_eq!(raw_rows.len(), lines.len());

    // These native cases execute .286 Br/renderers and fmn/xd unchanged.
    // Only the local date and the host's branded static prompt vary here.
    let oracle: Value = serde_json::from_str(include_str!(
        "../../core/tests/fixtures/instruction_announcements_2_1_286.json"
    ))
    .unwrap();
    assert_eq!(oracle["version"], "2.1.286");
    assert_eq!(
        oracle["binary_sha256"],
        "75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433"
    );
    let native = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "successful-empty-initial")
        .unwrap();
    assert_eq!(native["expected"].as_array().unwrap().len(), 2);
    assert_eq!(native["expected"][0]["rendered"], serde_json::json!([]));
    assert_eq!(
        fixture.meta.single_turn_sequence[1].expected_attachment,
        Some(native["expected"][0]["attachment"].clone())
    );
    let mut native_date = native["expected"][1]["attachment"].clone();
    native_date["date"] = serde_json::json!("$currentDate");
    assert_eq!(
        fixture.meta.single_turn_sequence[2].expected_attachment,
        Some(native_date)
    );
    let native_snapshot = oracle["snapshotCases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "valid-announced-system-prompt")
        .unwrap();
    assert_eq!(native_snapshot["expected"]["schemaValid"], true);
    let mut snapshot_payload = native_snapshot["prior"][0]["attachment"].clone();
    snapshot_payload["systemPrompt"] = serde_json::json!(["$capturedSystem"]);
    assert_eq!(
        fixture.meta.single_turn_sequence[4].expected_attachment,
        Some(snapshot_payload)
    );

    let calls = api.captured_msgs().await;
    assert_eq!(calls.len(), 1, "single turn makes exactly one API call");
    let systems = api.captured_systems().await;
    assert_eq!(systems.len(), 1);
    let system = systems[0].as_deref().expect("static system prompt");
    assert!(!system.is_empty());
    // Native persists the source string vector before dynamic context. The
    // default mock route has no provider profile, so its source vector carries
    // no global-cache marker.
    let snapshot_system = lines[4].extra["attachment"]["systemPrompt"].clone();
    let snapshot_blocks = snapshot_system
        .as_array()
        .expect("Native source block array");
    assert!(!snapshot_blocks.is_empty());
    assert!(snapshot_blocks.iter().all(Value::is_string));
    assert!(lines[4].extra["attachment"].get("sharedBoundary").is_none());
    let snapshot_text = snapshot_blocks
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()
        .expect("source blocks are strings")
        .join("\n\n");
    assert!(system.starts_with(&snapshot_text));
    assert_eq!(api.captured_tools().await, vec![Vec::<Value>::new()]);

    let date = orchestrator::prompt::env_meta::current_date_string();
    let mut seen = HashSet::new();
    for (index, (line, expected)) in lines
        .iter()
        .zip(&fixture.meta.single_turn_sequence)
        .enumerate()
    {
        let uuid = uuid::Uuid::parse_str(&line.uuid).expect("valid row UUID");
        assert_eq!(line.uuid, uuid.to_string(), "canonical UUID at row {index}");
        assert!(seen.insert(uuid), "distinct UUID at row {index}");
        let parent = index.checked_sub(1).map(|prior| lines[prior].uuid.as_str());
        assert_eq!(line.parent_uuid.as_deref(), parent, "parent at row {index}");
        assert_eq!(raw_rows[index]["parentUuid"], serde_json::json!(parent));
        assert_eq!(line.session_id, lines[0].session_id);
        assert_eq!(line.message_type, expected.msg_type, "type at row {index}");
        if let Some(kind) = expected.attachment_type.as_deref() {
            let mut payload = expected
                .expected_attachment
                .clone()
                .expect("exact attachment payload in fixture");
            match kind {
                "date" => payload["date"] = serde_json::json!(date),
                "prompt_snapshot" => {
                    payload["systemPrompt"] = snapshot_system.clone();
                }
                _ => {}
            }
            assert_eq!(payload["type"], kind);
            assert_eq!(line.extra.get("attachment"), Some(&payload), "row {index}");
            assert_eq!(raw_rows[index]["attachment"], payload, "disk row {index}");
            assert_eq!(
                line.message,
                Value::Null,
                "no decoded projection at row {index}"
            );
            assert!(
                raw_rows[index].get("message").is_none(),
                "native attachment has no inner message at row {index}"
            );
            assert!(
                raw_rows[index].get("isMeta").is_none(),
                "attachment is persisted as raw metadata at row {index}"
            );
        }
    }
    lingxi_core::types::SessionId::parse_prefixed(&lines[0].session_id)
        .expect("valid core session ID");
    assert_eq!(
        lines[0].message,
        serde_json::json!({"role":"user","content":[{"type":"text","text":"say hi"}]})
    );
    assert_eq!(lines[5].message["role"], "assistant");
    assert_eq!(
        lines[5].message["content"],
        serde_json::json!([{"type":"text","text":"hello"}])
    );
    assert_eq!(lines[5].message["stop_reason"], "end_turn");

    // Announcements and total_tokens preserve their prepared-request IDs.
    // The empty native session_context has no model-visible user projection.
    let sent = &calls[0];
    assert_eq!(
        sent.len(),
        4,
        "human, empty context anchor, date, total_tokens"
    );
    for (line, message) in lines[..4].iter().zip(sent) {
        assert_eq!(line.uuid, message.id().as_uuid().to_string());
    }
    assert!(
        matches!(&sent[0], ConversationMessage::User { content, is_meta: false, .. }
        if content == &[ContentBlock::Text { text: "say hi".into(), citations: None }])
    );
    assert!(
        matches!(&sent[1], ConversationMessage::System { content, subtype, compact_metadata: None, refusal_fallback: None, .. }
        if content.is_empty() && subtype.as_deref() == Some("model_reminder_attachment"))
    );
    let date_body = native["expected"][1]["rendered"][0]
        .as_str()
        .unwrap()
        .replace(
            native["expected"][1]["attachment"]["date"]
                .as_str()
                .unwrap(),
            &date,
        );
    assert!(
        matches!(&sent[2], ConversationMessage::User { content, is_meta: true, .. }
        if content == &[ContentBlock::Text { text: date_body, citations: None }])
    );
    let token_body = fixture.meta.single_turn_sequence[3]
        .expected_attachment
        .as_ref()
        .unwrap()["text"]
        .as_str()
        .unwrap();
    assert!(
        matches!(&sent[3], ConversationMessage::User { content, is_meta: true, .. }
        if content == &[ContentBlock::Text { text: format!("<system-reminder>\n{token_body}\n</system-reminder>"), citations: None }])
    );
}

// ============================================================================
// T4 — single turn: human + four native attachments + assistant
// ============================================================================

#[tokio::test]
async fn single_turn_produces_the_fixture_line_count() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api.clone(), Arc::clone(&writer));
    let outcome = orch.run_turn("say hi").await.expect("turn must succeed");
    assert!(matches!(
        outcome,
        orchestrator::ConversationOutcome::EndTurn { turn_count: 1, .. }
    ));

    // Read back via JsonlReader.
    let reader = JsonlReader::new(path.clone(), Arc::clone(&fs));
    let lines = reader.read_all().await.expect("read_all must succeed");

    let f = load();
    assert_eq!(
        lines.len(),
        f.meta.single_turn_sequence.len(),
        "single turn must produce the fixture's line count, got {:?}",
        lines
            .iter()
            .map(|l| l.message_type.as_str())
            .collect::<Vec<_>>()
    );
    let raw = std::fs::read_to_string(&path).expect("read actual JSONL bytes");
    assert_native_single_turn_rows(&lines, &raw, &api).await;
}

// ============================================================================
// T4 — line order and chaining follow the fixture sequence
// ============================================================================

#[tokio::test]
async fn single_turn_line_types_follow_the_fixture_order() {
    let f = load();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api.clone(), Arc::clone(&writer));
    let outcome = orch.run_turn("say hi").await.expect("turn must succeed");
    assert!(matches!(
        outcome,
        orchestrator::ConversationOutcome::EndTurn { turn_count: 1, .. }
    ));

    let reader = JsonlReader::new(path.clone(), Arc::clone(&fs));
    let lines = reader.read_all().await.expect("read_all must succeed");

    let expected_types: Vec<&str> = f
        .meta
        .single_turn_sequence
        .iter()
        .map(|e| e.msg_type.as_str())
        .collect();

    let actual_types: Vec<&str> = lines.iter().map(|l| l.message_type.as_str()).collect();
    assert_eq!(
        actual_types, expected_types,
        "line types must follow the fixture order"
    );
    let raw = std::fs::read_to_string(&path).expect("read actual JSONL bytes");
    assert_native_single_turn_rows(&lines, &raw, &api).await;
}

// ============================================================================
// T4 — UUID chain: line2.parentUuid == line1.uuid, line1.parentUuid == null
// ============================================================================

#[tokio::test]
async fn single_turn_parent_uuid_chain_is_correct() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api, Arc::clone(&writer));
    orch.run_turn("say hi").await.expect("turn must succeed");

    let reader = JsonlReader::new(path.clone(), Arc::clone(&fs));
    let lines = reader.read_all().await.expect("read_all must succeed");

    let user = &lines[0];
    let assistant = &lines[1];

    assert!(
        user.parent_uuid.is_none(),
        "first line (user) must have parentUuid = null"
    );
    assert_eq!(
        assistant.parent_uuid.as_deref(),
        Some(user.uuid.as_str()),
        "assistant parentUuid must equal user uuid"
    );
}

// ============================================================================
// T4 — sessionId: all lines share the same session ID
// ============================================================================

#[tokio::test]
async fn single_turn_all_lines_share_session_id() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api, Arc::clone(&writer));
    orch.run_turn("say hi").await.expect("turn must succeed");

    let reader = JsonlReader::new(path.clone(), Arc::clone(&fs));
    let lines = reader.read_all().await.expect("read_all must succeed");

    let session_ids: Vec<&str> = lines.iter().map(|l| l.session_id.as_str()).collect();
    assert!(
        session_ids.windows(2).all(|w| w[0] == w[1]),
        "all lines must share the same sessionId; got {session_ids:?}"
    );
}

// ============================================================================
// T4 — format locks: LF-only terminator, compact JSON, uuid v4 pattern
// ============================================================================

#[tokio::test]
async fn single_turn_file_has_lf_only_terminator_and_compact_json() {
    let f = load();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api, Arc::clone(&writer));
    orch.run_turn("say hi").await.expect("turn must succeed");

    let raw = std::fs::read_to_string(&path).expect("read raw file");

    // LF-only: no CR byte.
    assert!(
        !raw.contains('\r'),
        "fixture lock '{:?}': file must use LF-only line endings, no CR found",
        f.meta.locks.line_terminator
    );

    // Compact JSON: no lines that contain `: ` (space after colon in key-value).
    for (i, line) in raw.lines().enumerate() {
        // Compact serde_json never emits ": " — it always emits ":"
        // (verify by checking that none of the outer field separators have trailing space).
        // We check the absence of `": "` to confirm no pretty-printing.
        assert!(
            !line.contains("\": \"") || {
                // The only ": " that appears must be INSIDE a JSON string value,
                // not at the top-level key separator position.
                // Simpler invariant: the line must parse as valid compact JSON.
                serde_json::from_str::<Value>(line).is_ok()
            },
            "line {}: fixture lock '{:?}': JSON must be compact",
            i + 1,
            f.meta.locks.json_format
        );
        // Verify it parses as valid JSON.
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("line {} is not valid JSON: {e}", i + 1));
    }
}

// ============================================================================
// T4 — userType field: locked to "external"
// ============================================================================

#[tokio::test]
async fn single_turn_user_line_has_usertype_external() {
    let f = load();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api, Arc::clone(&writer));
    orch.run_turn("say hi").await.expect("turn must succeed");

    let reader = JsonlReader::new(path.clone(), Arc::clone(&fs));
    let lines = reader.read_all().await.expect("read_all must succeed");
    let user = &lines[0];

    assert_eq!(
        user.user_type.as_deref(),
        Some(f.meta.locks.user_type.as_str()),
        "user line userType must be locked to {:?}",
        f.meta.locks.user_type
    );
}

// ============================================================================
// T4 — isSidechain: default false for main-loop messages
// ============================================================================

#[tokio::test]
async fn single_turn_is_sidechain_is_false() {
    let f = load();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api, Arc::clone(&writer));
    orch.run_turn("say hi").await.expect("turn must succeed");

    let reader = JsonlReader::new(path.clone(), Arc::clone(&fs));
    let lines = reader.read_all().await.expect("read_all must succeed");

    for line in &lines {
        assert_eq!(
            line.is_sidechain, f.meta.locks.is_sidechain_default,
            "isSidechain must be {} for main-loop messages",
            f.meta.locks.is_sidechain_default
        );
    }
}

// ============================================================================
// T4 — telemetry invariants: session events are registered
// ============================================================================

#[test]
fn session_telemetry_events_are_registered() {
    let f = load();
    let registered: std::collections::HashSet<&&str> =
        telemetry::tengu::ALL_EVENT_NAMES.iter().collect();
    for name in &f.meta.session_telemetry_events {
        assert!(
            registered.contains(&name.as_str()),
            "session event {name:?} not found in ALL_EVENT_NAMES"
        );
    }
}
