//! Golden-fixture byte-equivalent write/read tests.
//! The writer's output, given the same `JsonlMessage` sequence, MUST be
//! byte-for-byte identical to the on-disk fixture after token substitution.

use platform_api::FileSystem;
use platform_posix::fs::PosixFileSystem;
use pretty_assertions::assert_eq;
use serde_json::{json, Map, Value};
use session::jsonl::reader::JsonlReader;
use session::jsonl::schema::JsonlMessage;
use session::jsonl::writer::JsonlWriter;
use std::sync::Arc;
use tempfile::tempdir;

const UUID1: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const UUID2: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const UUID3: &str = "cccccccc-cccc-cccc-cccc-cccccccccccc";
const UUID4: &str = "dddddddd-dddd-dddd-dddd-dddddddddddd";
const UUID5: &str = "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee";
const SESSION1: &str = "11111111-2222-3333-4444-555555555555";
const TS1: &str = "2026-05-25T14:30:00.000Z";
const TS2: &str = "2026-05-25T14:30:01.000Z";
const TS3: &str = "2026-05-25T14:30:02.000Z";
const TS4: &str = "2026-05-25T14:30:03.000Z";
const TS5: &str = "2026-05-25T14:30:04.000Z";

fn substitute(template: &str) -> String {
    template
        .replace("<UUID-1>", UUID1)
        .replace("<UUID-2>", UUID2)
        .replace("<UUID-3>", UUID3)
        .replace("<UUID-4>", UUID4)
        .replace("<UUID-5>", UUID5)
        .replace("<SESSION-1>", SESSION1)
        .replace("<TS-1>", TS1)
        .replace("<TS-2>", TS2)
        .replace("<TS-3>", TS3)
        .replace("<TS-4>", TS4)
        .replace("<TS-5>", TS5)
}

// ---------- T10: single-turn write ----------

#[tokio::test]
async fn writer_output_equals_single_turn_fixture() {
    let fixture = include_str!("fixtures/golden_sessions/single_turn_no_tools.jsonl");
    let expected = substitute(fixture);

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("out.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = JsonlWriter::new(path.clone(), fs);

    let user = JsonlMessage {
        message_type: "user".into(),
        uuid: UUID1.into(),
        parent_uuid: None,
        session_id: SESSION1.into(),
        timestamp: TS1.into(),
        cwd: "/tmp/golden".into(),
        version: "0.6.0".into(),
        message: json!({"role":"user","content":"say hi"}),
        is_sidechain: false,
        user_type: Some("external".into()),
        git_branch: None,
        entrypoint: None,
        slug: None,
        prompt_id: None,
        logical_parent_uuid: None,
        extra: Map::new(),
    };
    let assistant = JsonlMessage {
        message_type: "assistant".into(),
        uuid: UUID2.into(),
        parent_uuid: Some(UUID1.into()),
        session_id: SESSION1.into(),
        timestamp: TS2.into(),
        cwd: "/tmp/golden".into(),
        version: "0.6.0".into(),
        message: json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "claude-3-5-sonnet-latest",
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }),
        is_sidechain: false,
        user_type: None,
        git_branch: None,
        entrypoint: None,
        slug: None,
        prompt_id: None,
        logical_parent_uuid: None,
        extra: Map::new(),
    };

    writer.append(&user).await.expect("append user");
    writer.append(&assistant).await.expect("append assistant");

    let got = std::fs::read_to_string(&path).expect("read out.jsonl");
    assert_eq!(got, expected);
}

// ---------- T11: single-turn read ----------

#[tokio::test]
async fn reader_round_trips_single_turn_fixture() {
    let fixture = include_str!("fixtures/golden_sessions/single_turn_no_tools.jsonl");
    let expected_content = substitute(fixture);

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("in.jsonl");
    std::fs::write(&path, &expected_content).expect("seed");

    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let reader = JsonlReader::new(path.clone(), fs);
    let msgs = reader.read_all().await.expect("read_all");

    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].message_type, "user");
    assert_eq!(msgs[0].uuid, UUID1);
    assert_eq!(msgs[0].parent_uuid, None);
    assert_eq!(msgs[1].message_type, "assistant");
    assert_eq!(msgs[1].uuid, UUID2);
    assert_eq!(msgs[1].parent_uuid.as_deref(), Some(UUID1));
    assert_eq!(msgs[1].message["stop_reason"], "end_turn");
}

// ---------- T12: multi-turn write + read ----------

#[tokio::test]
async fn writer_output_equals_multi_turn_fixture() {
    let fixture = include_str!("fixtures/golden_sessions/multi_turn_with_tools.jsonl");
    let expected = substitute(fixture);

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("multi.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = JsonlWriter::new(path.clone(), fs);

    let messages: Vec<JsonlMessage> = vec![
        JsonlMessage {
            message_type: "user".into(),
            uuid: UUID1.into(),
            parent_uuid: None,
            session_id: SESSION1.into(),
            timestamp: TS1.into(),
            cwd: "/tmp/golden".into(),
            version: "0.6.0".into(),
            message: json!({"role":"user","content":"read /etc/hostname"}),
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: Map::new(),
        },
        JsonlMessage {
            message_type: "assistant".into(),
            uuid: UUID2.into(),
            parent_uuid: Some(UUID1.into()),
            session_id: SESSION1.into(),
            timestamp: TS2.into(),
            cwd: "/tmp/golden".into(),
            version: "0.6.0".into(),
            message: json!({
                "id":"msg_01","type":"message","role":"assistant",
                "content":[{"type":"tool_use","id":"toolu_01","name":"Read","input":{"file_path":"/etc/hostname"}}],
                "model":"claude-3-5-sonnet-latest","stop_reason":"tool_use",
                "stop_sequence":Value::Null,
                "usage":{"input_tokens":50,"output_tokens":20}
            }),
            is_sidechain: false,
            user_type: None,
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: Map::new(),
        },
        JsonlMessage {
            message_type: "user".into(),
            uuid: UUID3.into(),
            parent_uuid: Some(UUID2.into()),
            session_id: SESSION1.into(),
            timestamp: TS3.into(),
            cwd: "/tmp/golden".into(),
            version: "0.6.0".into(),
            message: json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"myhostname\n","is_error":false}]}),
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: Map::new(),
        },
        JsonlMessage {
            message_type: "assistant".into(),
            uuid: UUID4.into(),
            parent_uuid: Some(UUID3.into()),
            session_id: SESSION1.into(),
            timestamp: TS4.into(),
            cwd: "/tmp/golden".into(),
            version: "0.6.0".into(),
            message: json!({
                "id":"msg_02","type":"message","role":"assistant",
                "content":[{"type":"text","text":"The hostname is myhostname."}],
                "model":"claude-3-5-sonnet-latest","stop_reason":"end_turn",
                "stop_sequence":Value::Null,
                "usage":{"input_tokens":80,"output_tokens":10}
            }),
            is_sidechain: false,
            user_type: None,
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: Map::new(),
        },
        JsonlMessage {
            message_type: "user".into(),
            uuid: UUID5.into(),
            parent_uuid: Some(UUID4.into()),
            session_id: SESSION1.into(),
            timestamp: TS5.into(),
            cwd: "/tmp/golden".into(),
            version: "0.6.0".into(),
            message: json!({"role":"user","content":"thanks"}),
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: Map::new(),
        },
    ];

    for m in &messages {
        writer.append(m).await.expect("append");
    }
    let got = std::fs::read_to_string(&path).expect("read multi.jsonl");
    assert_eq!(got, expected);
}

#[tokio::test]
async fn reader_round_trips_multi_turn_fixture() {
    let fixture = include_str!("fixtures/golden_sessions/multi_turn_with_tools.jsonl");
    let content = substitute(fixture);
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("multi.jsonl");
    std::fs::write(&path, &content).expect("seed");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let reader = JsonlReader::new(path, fs);
    let msgs = reader.read_all().await.expect("read_all");
    assert_eq!(msgs.len(), 5);
    assert_eq!(msgs[1].message["content"][0]["name"], "Read");
    assert_eq!(msgs[2].message["content"][0]["type"], "tool_result");
}

#[tokio::test]
async fn writer_output_equals_compacted_fixture() {
    let fixture = include_str!("fixtures/golden_sessions/compacted_session.jsonl");
    let expected = substitute(fixture);

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("compact.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = JsonlWriter::new(path.clone(), fs);

    // The boundary line carries claude's FLATTENED system envelope as OUTER
    // fields via `extra` — `subtype`/`content`/`level`/`compactMetadata` (the
    // serializer's compact-boundary arm emits them at claude's 2.1.207
    // positions: type, subtype, content, level, compactMetadata, uuid,
    // timestamp) — plus `parentUuid: null` + `logicalParentUuid` (the chain
    // reset) and NO inner `message`.
    let mut boundary_extra: Map<String, Value> = Map::new();
    boundary_extra.insert("subtype".into(), Value::String("compact_boundary".into()));
    boundary_extra.insert(
        "content".into(),
        Value::String("Conversation compacted".into()),
    );
    boundary_extra.insert("level".into(), Value::String("info".into()));
    boundary_extra.insert(
        "compactMetadata".into(),
        json!({"trigger": "manual", "preTokens": 50}),
    );

    // The summary user line carries the top-level compact-summary flags,
    // emitted in claude's on-disk order between `message` and `uuid`.
    let mut summary_extra: Map<String, Value> = Map::new();
    summary_extra.insert("isVisibleInTranscriptOnly".into(), Value::Bool(true));
    summary_extra.insert("isCompactSummary".into(), Value::Bool(true));

    let messages: Vec<JsonlMessage> = vec![
        JsonlMessage {
            message_type: "user".into(),
            uuid: UUID1.into(),
            parent_uuid: None,
            session_id: SESSION1.into(),
            timestamp: TS1.into(),
            cwd: "/tmp/golden".into(),
            version: "0.6.0".into(),
            message: json!({"role":"user","content":"hi"}),
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: Map::new(),
        },
        JsonlMessage {
            message_type: "assistant".into(),
            uuid: UUID2.into(),
            parent_uuid: Some(UUID1.into()),
            session_id: SESSION1.into(),
            timestamp: TS2.into(),
            cwd: "/tmp/golden".into(),
            version: "0.6.0".into(),
            message: json!({
                "id":"msg_01","type":"message","role":"assistant",
                "content":[{"type":"text","text":"hello"}],
                "model":"claude-3-5-sonnet-latest","stop_reason":"end_turn",
                "stop_sequence":Value::Null,
                "usage":{"input_tokens":5,"output_tokens":3}
            }),
            is_sidechain: false,
            user_type: None,
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: Map::new(),
        },
        JsonlMessage {
            message_type: "system".into(),
            uuid: UUID3.into(),
            // Chain reset: `parentUuid: null`, real parent in logicalParentUuid.
            parent_uuid: None,
            session_id: SESSION1.into(),
            timestamp: TS3.into(),
            cwd: "/tmp/golden".into(),
            version: "0.6.0".into(),
            // Boundary lines carry NO inner `message` (skipped on write).
            message: Value::Null,
            is_sidechain: false,
            user_type: None,
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: Some(UUID2.into()),
            extra: boundary_extra,
        },
        JsonlMessage {
            message_type: "user".into(),
            uuid: UUID4.into(),
            parent_uuid: Some(UUID3.into()),
            session_id: SESSION1.into(),
            timestamp: TS4.into(),
            cwd: "/tmp/golden".into(),
            version: "0.6.0".into(),
            message: json!({"role":"user","content":"This session is being continued from a previous conversation. Summary:\nS"}),
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: summary_extra,
        },
    ];

    for m in &messages {
        writer.append(m).await.expect("append");
    }
    let got = std::fs::read_to_string(&path).expect("read compact.jsonl");
    assert_eq!(got, expected);
}

#[tokio::test]
async fn reader_round_trips_compacted_fixture() {
    let fixture = include_str!("fixtures/golden_sessions/compacted_session.jsonl");
    let content = substitute(fixture);
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("compact.jsonl");
    std::fs::write(&path, &content).expect("seed");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let reader = JsonlReader::new(path, fs);
    let msgs = reader.read_all().await.expect("read_all");
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[2].message_type, "system");
    assert_eq!(
        msgs[2].extra.get("subtype").and_then(|v| v.as_str()),
        Some("compact_boundary")
    );
    // Chain reset: parentUuid null, real parent in logicalParentUuid.
    assert_eq!(msgs[2].parent_uuid, None);
    assert_eq!(msgs[2].logical_parent_uuid.as_deref(), Some(UUID2));
    assert_eq!(
        msgs[2].extra.get("content").and_then(|v| v.as_str()),
        Some("Conversation compacted")
    );
    assert_eq!(
        msgs[2]
            .extra
            .get("compactMetadata")
            .and_then(|v| v.get("preTokens"))
            .and_then(serde_json::Value::as_i64),
        Some(50)
    );
    // The summary line's flags round-trip as outer `extra` fields.
    assert_eq!(
        msgs[3].extra.get("isCompactSummary"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        msgs[3].extra.get("isVisibleInTranscriptOnly"),
        Some(&Value::Bool(true))
    );
}
