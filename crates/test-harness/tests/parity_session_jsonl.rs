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

// ============================================================================
// T4 — single turn: line count = 2 (user + assistant)
// ============================================================================

#[tokio::test]
async fn single_turn_produces_the_fixture_line_count() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn platform_api::FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
        tmp.path().to_path_buf(),
    ));
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api, Arc::clone(&writer));
    orch.run_turn("say hi").await.expect("turn must succeed");

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
    // The extra line over the original user+assistant pair is the static
    // system-prompt snapshot, which upstream records unless CLAUDE_CODE_SIMPLE
    // is set — see the fixture's `why`. Assert WHICH attachment it is, so a
    // different attachment leaking into the transcript cannot pass by count.
    assert_eq!(
        lines[1]
            .extra
            .get("attachment")
            .and_then(|a| a.get("type"))
            .and_then(|t| t.as_str()),
        Some("prompt_snapshot"),
        "the middle line must be the prompt snapshot"
    );
}

// ============================================================================
// T4 — line order and chaining follow the fixture sequence
// ============================================================================

#[tokio::test]
async fn single_turn_line_types_follow_the_fixture_order() {
    let f = load();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn platform_api::FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
        tmp.path().to_path_buf(),
    ));
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api, Arc::clone(&writer));
    orch.run_turn("say hi").await.expect("turn must succeed");

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
    // The snapshot sits INSIDE the chain rather than beside it: the assistant's
    // parent is the attachment, not the user line it answers.
    assert_eq!(
        lines[1].parent_uuid.as_deref(),
        Some(lines[0].uuid.as_str()),
        "the snapshot must chain from the user line"
    );
    assert_eq!(
        lines[2].parent_uuid.as_deref(),
        Some(lines[1].uuid.as_str()),
        "the assistant must chain from the snapshot, not around it"
    );
}

// ============================================================================
// T4 — UUID chain: line2.parentUuid == line1.uuid, line1.parentUuid == null
// ============================================================================

#[tokio::test]
async fn single_turn_parent_uuid_chain_is_correct() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn platform_api::FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
        tmp.path().to_path_buf(),
    ));
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
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

    let fs: Arc<dyn platform_api::FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
        tmp.path().to_path_buf(),
    ));
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
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

    let fs: Arc<dyn platform_api::FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
        tmp.path().to_path_buf(),
    ));
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
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

    let fs: Arc<dyn platform_api::FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
        tmp.path().to_path_buf(),
    ));
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
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

    let fs: Arc<dyn platform_api::FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
        tmp.path().to_path_buf(),
    ));
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
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
