//! `JsonlReader` full + lite parity.

use platform_api::FileSystem;
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Map};
use session::jsonl::reader::{route_lines, JsonlReader, SessionMetadata};
use session::jsonl::schema::JsonlMessage;
use session::jsonl::writer::JsonlWriter;
use std::sync::Arc;
use tempfile::tempdir;

fn user_msg(n: u8) -> JsonlMessage {
    JsonlMessage {
        message_type: "user".into(),
        uuid: format!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaa{n:02x}"),
        parent_uuid: None,
        session_id: "11111111-2222-3333-4444-555555555555".into(),
        timestamp: format!("2026-05-25T14:30:0{n}.000Z"),
        cwd: "/tmp/proj".into(),
        version: "0.6.0".into(),
        message: json!({"role":"user","content":format!("msg {n}")}),
        is_sidechain: false,
        user_type: Some("external".into()),
        git_branch: None,
        entrypoint: None,
        slug: None,
        prompt_id: None,
        logical_parent_uuid: None,
        extra: Map::default(),
    }
}

#[tokio::test]
async fn read_all_round_trips_writer_output() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = JsonlWriter::new(path.clone(), fs.clone());

    for n in 0..3 {
        writer.append(&user_msg(n)).await.expect("append");
    }

    let reader = JsonlReader::new(path, fs);
    let got = reader.read_all().await.expect("read_all");
    assert_eq!(got.len(), 3);
    for (n, msg) in got.iter().enumerate() {
        assert_eq!(msg.timestamp, format!("2026-05-25T14:30:0{n}.000Z"));
    }
}

#[tokio::test]
async fn read_lite_extracts_metadata_from_first_line_only() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = JsonlWriter::new(path.clone(), fs.clone());

    // First line is small + has all three lite fields.
    writer.append(&user_msg(0)).await.expect("append 0");
    // Pad with many subsequent lines so the file exceeds 64 KiB —
    // the lite read MUST NOT need to parse past the head buffer.
    let big_content = "x".repeat(2_000);
    for n in 1..40 {
        let mut m = user_msg(n);
        m.message = json!({"role":"user","content":big_content.clone()});
        writer.append(&m).await.expect("append n");
    }

    let file_size = std::fs::metadata(&path).expect("stat").len();
    assert!(
        file_size > 65_536,
        "test setup must produce a >64KiB file (got {file_size})"
    );

    let reader = JsonlReader::new(path, fs);
    let meta: SessionMetadata = reader.read_lite().await.expect("lite");
    assert_eq!(meta.session_id, "11111111-2222-3333-4444-555555555555");
    assert_eq!(meta.cwd, "/tmp/proj");
    assert_eq!(meta.first_type, "user");
}

#[tokio::test]
async fn read_lite_handles_escaped_chars_in_first_line() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = JsonlWriter::new(path.clone(), fs.clone());

    // cwd contains a quote + backslash (forced into the field by manual
    // construction; the writer escapes them in JSON).
    let mut m = user_msg(0);
    m.cwd = "/path/with \"quote\" and \\back".into();
    writer.append(&m).await.expect("append");

    let reader = JsonlReader::new(path, fs);
    let meta = reader.read_lite().await.expect("lite");
    assert_eq!(meta.cwd, "/path/with \"quote\" and \\back");
}

// ---- Tolerant reader (BLOCKING gap): real-transcript line-type tolerance ----

/// A realistic mixed-line transcript: chain participants
/// (`user`/`assistant`/`attachment`/`system`) interleaved with Tier-1 metadata
/// (`summary`/`ai-title`), Tier-2 metadata (`last-prompt`/`permission-mode`/
/// `file-history-snapshot`/`queue-operation`), and one MALFORMED line. The
/// reader must keep exactly the 4 chain participants in file order, populate the
/// side-maps, and NEVER error — mirroring `claude-code`'s `loadTranscriptFile`
/// (`sessionStorage.ts:3472`) over `parseJSONL` (`json.ts:155`).
const MIXED: &str = concat!(
    r#"{"type":"summary","summary":"prior session","leafUuid":"u-2"}"#,
    "\n",
    r#"{"type":"last-prompt","sessionId":"sid-1","prompt":"hello"}"#,
    "\n",
    r#"{"type":"user","uuid":"u-1","parentUuid":null,"sessionId":"sid-1","timestamp":"2026-05-25T12:00:00.000Z","cwd":"/p","version":"0.6.0","isSidechain":false,"userType":"external","message":{"role":"user","content":"hello"}}"#,
    "\n",
    r#"{not json at all"#,
    "\n",
    r#"{"type":"permission-mode","sessionId":"sid-1","mode":"acceptEdits"}"#,
    "\n",
    r#"{"type":"session-mode","sessionId":"sid-1","sessionMode":"chat"}"#,
    "\n",
    r#"{"type":"assistant","uuid":"u-2","parentUuid":"u-1","sessionId":"sid-1","timestamp":"2026-05-25T12:00:01.000Z","cwd":"/p","version":"0.6.0","isSidechain":false,"message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"hi"}]}}"#,
    "\n",
    r#"{"type":"file-history-snapshot","messageId":"snap-1","snapshot":{"trackedFileBackups":{}}}"#,
    "\n",
    r#"{"type":"attachment","uuid":"att-1","parentUuid":"u-2","sessionId":"sid-1","cwd":"/p","version":"0.6.0","attachment":{"type":"file","path":"/p/x"}}"#,
    "\n",
    r#"{"type":"ai-title","sessionId":"sid-1","aiTitle":"Greeting"}"#,
    "\n",
    r#"{"type":"system","uuid":"sys-1","parentUuid":"att-1","sessionId":"sid-1","content":"hook","subtype":"hook_result"}"#,
    "\n",
    r#"{"type":"queue-operation","operation":"enqueue","sessionId":"sid-1"}"#,
    "\n",
);

#[test]
fn route_lines_keeps_only_transcript_messages_and_skips_malformed() {
    let routed = route_lines(MIXED);

    assert_eq!(routed.malformed_line_count, 1);

    // 4 chain participants in FILE ORDER: user, assistant, attachment, system.
    let order: Vec<(&str, &str)> = routed
        .messages_in_order
        .iter()
        .map(|m| (m.message_type.as_str(), m.uuid.as_str()))
        .collect();
    assert_eq!(
        order,
        vec![
            ("user", "u-1"),
            ("assistant", "u-2"),
            ("attachment", "att-1"),
            ("system", "sys-1"),
        ],
        "only user/assistant/attachment/system survive, in file order"
    );

    // by_uuid index mirrors the participants.
    assert_eq!(routed.by_uuid.len(), 4);
    assert!(routed.by_uuid.contains_key("att-1"));
    assert!(routed.by_uuid.contains_key("sys-1"));

    // Tolerant defaults: the attachment/system lines lacked `message` /
    // `timestamp` / some outer fields — they parsed anyway.
    let att = &routed.by_uuid["att-1"];
    assert!(att.message.is_null(), "absent `message` defaults to null");
    assert_eq!(att.timestamp, "", "absent `timestamp` defaults to empty");

    // Tier-1 side-maps populated.
    assert_eq!(
        routed.summaries.get("u-2").map(String::as_str),
        Some("prior session")
    );
    assert_eq!(
        routed.custom_titles.len(),
        0,
        "no custom-title lines present"
    );
    assert_eq!(
        routed.ai_titles.get("sid-1").map(String::as_str),
        Some("Greeting")
    );
    assert_eq!(
        routed.session_modes.get("sid-1").map(String::as_str),
        Some("chat")
    );
}

#[test]
fn route_lines_tracks_mobile_empty_anchor_with_last_write_wins() {
    let routed = route_lines(concat!(
        r#"{"type":"custom-title","sessionId":"sid-1","customTitle":"Empty","mobileEmptySession":1}"#,
        "\n",
        r#"{"type":"custom-title","sessionId":"sid-2","customTitle":"Empty 2","mobileEmptySession":1}"#,
        "\n",
        r#"{"type":"custom-title","sessionId":"sid-1","customTitle":"Renamed"}"#,
        "\n",
    ));

    assert!(!routed.mobile_empty_sessions.contains("sid-1"));
    assert!(routed.mobile_empty_sessions.contains("sid-2"));
    assert_eq!(
        routed.custom_titles.get("sid-1"),
        Some(&"Renamed".to_string())
    );
}

#[test]
fn route_lines_projects_pr_link_metadata() {
    let routed = route_lines(
        r#"{"type":"pr-link","sessionId":"sid-1","prNumber":"42","prUrl":"https://github.com/acme/repo/pull/42","prRepository":"acme/repo"}"#,
    );
    assert_eq!(routed.pr_numbers.get("sid-1"), Some(&42));
    assert_eq!(
        routed.pr_urls.get("sid-1").map(String::as_str),
        Some("https://github.com/acme/repo/pull/42")
    );
    assert_eq!(
        routed.pr_repositories.get("sid-1").map(String::as_str),
        Some("acme/repo")
    );
}

#[test]
fn route_lines_backfills_pr_number_from_legacy_url_only_record() {
    let routed = route_lines(
        r#"{"type":"pr-link","sessionId":"sid-legacy","prUrl":"https://github.com/acme/repo/pull/73"}"#,
    );
    assert_eq!(routed.pr_numbers.get("sid-legacy"), Some(&73));
    assert_eq!(
        routed.pr_urls.get("sid-legacy").map(String::as_str),
        Some("https://github.com/acme/repo/pull/73")
    );
}

#[test]
fn route_lines_tracks_content_replacements_and_marble_state_exactly() {
    let routed = route_lines(concat!(
        r#"{"type":"content-replacement","sessionId":"sid-1","replacements":[{"path":"a.txt","text":"A"}]}"#,
        "\n",
        r#"{"type":"content-replacement","sessionId":"sid-1","replacements":[{"path":"c.txt","text":"C"}]}"#,
        "\n",
        r#"{"type":"marble-origami-commit","sessionId":"sid-1","commit":"pre-reset"}"#,
        "\n",
        r#"{"type":"marble-origami-snapshot","sessionId":"sid-1","snapshot":"pre-reset"}"#,
        "\n",
        r#"{"type":"marble-origami-reset","sessionId":"sid-1","reason":"manual"}"#,
        "\n",
        r#"{"type":"content-replacement","sessionId":"sid-1","agentId":"agent-1","replacements":[{"path":"b.txt","text":"B"}]}"#,
        "\n",
        r#"{"type":"marble-origami-commit","sessionId":"sid-1","commit":"post-reset"}"#,
        "\n",
        r#"{"type":"marble-origami-snapshot","sessionId":"sid-1","snapshot":"post-reset"}"#,
        "\n",
    ));

    assert_eq!(
        vec![
            routed.content_replacements.get("sid-1").unwrap()[0]["path"]
                .as_str()
                .unwrap(),
            routed.content_replacements.get("sid-1").unwrap()[1]["path"]
                .as_str()
                .unwrap(),
        ],
        vec!["a.txt", "c.txt"],
        "session-level content replacements must flatten in encounter order",
    );
    assert_eq!(
        routed.agent_content_replacements.get("agent-1").unwrap()[0]["path"],
        "b.txt",
        "agent-level replacements must stay separate from the session map",
    );
    assert_eq!(routed.context_collapse_commits.len(), 1);
    assert_eq!(
        routed.context_collapse_commits[0]["commit"], "post-reset",
        "reset must clear earlier marble commits",
    );
    assert_eq!(
        routed.context_collapse_snapshot.as_ref().unwrap()["snapshot"],
        "post-reset",
        "only the live marble snapshot survives cold load",
    );
}

#[test]
fn route_lines_compact_boundary_clears_pre_boundary_marble_state() {
    let routed = route_lines(concat!(
        r#"{"type":"marble-origami-commit","sessionId":"sid-1","commit":"pre-boundary"}"#,
        "\n",
        r#"{"type":"marble-origami-snapshot","sessionId":"sid-1","snapshot":"pre-boundary"}"#,
        "\n",
        r#"{"type":"system","subtype":"compact_boundary","uuid":"11111111-1111-4111-8111-111111111111","parentUuid":null,"sessionId":"11111111-1111-4111-8111-111111111111","cwd":"/tmp","timestamp":"2026-08-25T00:00:00.000Z","message":{"role":"system","content":"Conversation compacted"}}"#,
        "\n",
        r#"{"type":"marble-origami-commit","sessionId":"sid-1","commit":"post-boundary"}"#,
        "\n",
        r#"{"type":"marble-origami-snapshot","sessionId":"sid-1","snapshot":"post-boundary"}"#,
        "\n",
    ));

    assert_eq!(routed.context_collapse_commits.len(), 1);
    assert_eq!(
        routed.context_collapse_commits[0]["commit"],
        "post-boundary"
    );
    assert_eq!(
        routed.context_collapse_snapshot.as_ref().unwrap()["snapshot"],
        "post-boundary"
    );
}

#[test]
fn route_lines_tracks_branch_sidecars_with_oracle_validation() {
    let routed = route_lines(concat!(
        r#"{"type":"relocated","sessionId":"sid-1","relocatedCwd":"/old"}"#,
        "\n",
        r#"{"type":"relocated","sessionId":"sid-1","relocatedCwd":""}"#,
        "\n",
        r#"{"type":"relocated","sessionId":"sid-1","relocatedCwd":"/new"}"#,
        "\n",
        r#"{"type":"history-suppression","sessionId":"sid-1","cause":"manual","ts":"2026-08-25T00:00:00.000Z"}"#,
        "\n",
        r#"{"type":"atis-latch","sessionId":"sid-1","atis":"valid-token"}"#,
        "\n",
        "{\"type\":\"atis-latch\",\"sessionId\":\"sid-1\",\"atis\":\"bad\\u0020token\"}",
        "\n",
        r#"{"type":"atis-latch","sessionId":"sid-2","atis":""}"#,
        "\n",
    ));

    assert_eq!(
        routed.relocated_cwds.get("sid-1").map(String::as_str),
        Some("/new"),
        "relocated is last-valid-write-wins and ignores empty cwd values",
    );
    assert!(routed.session_history_suppressed);
    assert_eq!(
        routed.atis_latches.get("sid-1").map(String::as_str),
        Some("valid-token"),
        "atis values containing bytes outside [0x21, 0x7e] are ignored",
    );
    assert_eq!(
        routed.atis_latches.get("sid-2").map(String::as_str),
        Some(""),
        "the oracle's /^[\\x21-\\x7e]*$/ validator accepts an empty latch",
    );
}

#[tokio::test]
async fn read_all_is_tolerant_of_metadata_and_malformed_lines() {
    // Same mixed content, but exercised through the public `JsonlReader::read_all`
    // I/O surface — it must NOT error and must return the 4 chain participants.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("mixed.jsonl");
    std::fs::write(&path, MIXED).expect("seed");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));

    let reader = JsonlReader::new(path, fs);
    let msgs = reader
        .read_all()
        .await
        .expect("tolerant read must not error on metadata/malformed lines");
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0].uuid, "u-1");
    assert_eq!(msgs[3].uuid, "sys-1");
}
