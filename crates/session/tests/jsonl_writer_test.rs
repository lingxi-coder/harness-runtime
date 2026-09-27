//! `JsonlWriter` raw-byte assertion — three appends produce three lines, one
//! `\n` per line, no extra whitespace.

use platform_api::FileSystem;
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Map};
use session::jsonl::schema::JsonlMessage;
use session::jsonl::writer::JsonlWriter;
use std::sync::Arc;
use tempfile::tempdir;

fn make_msg(uuid: &str, parent: Option<&str>, n: u8) -> JsonlMessage {
    JsonlMessage {
        message_type: "user".into(),
        uuid: uuid.into(),
        parent_uuid: parent.map(str::to_string),
        session_id: "11111111-2222-3333-4444-555555555555".into(),
        timestamp: format!("2026-05-25T14:30:0{n}.000Z"),
        cwd: "/tmp/proj".into(),
        version: "0.6.0".into(),
        message: json!({"role":"user","content":format!("hello {n}")}),
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
async fn three_appends_produce_three_lines_one_lf_each() {
    let dir = tempdir().expect("tempdir");
    let path = dir
        .path()
        .join("nested")
        .join("subdir")
        .join("11111111-2222-3333-4444-555555555555.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = JsonlWriter::new(path.clone(), fs.clone());

    writer
        .append(&make_msg("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", None, 0))
        .await
        .expect("append 0");
    writer
        .append(&make_msg(
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            1,
        ))
        .await
        .expect("append 1");
    writer
        .append(&make_msg(
            "cccccccc-cccc-cccc-cccc-cccccccccccc",
            Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
            2,
        ))
        .await
        .expect("append 2");

    // Direct OS read — we want the RAW bytes, not whatever FileSystem trait
    // returns. We open the file directly and assert byte-for-byte.
    let raw = std::fs::read(&path).expect("file written");
    let text = std::string::String::from_utf8(raw).expect("UTF-8");

    // `split('\n')` on "a\nb\nc\n" yields ["a","b","c",""].
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(
        lines.len(),
        4,
        "expected 3 lines + trailing empty, got {lines:?}"
    );
    assert_eq!(lines[3], "");

    for (idx, line) in lines[..3].iter().enumerate() {
        let parsed: JsonlMessage = serde_json::from_str(line).expect("parse");
        assert_eq!(parsed.timestamp, format!("2026-05-25T14:30:0{idx}.000Z"));
    }

    // Total bytes = 3 lines + 3 LF.
    let expected_byte_count: usize = lines[..3].iter().map(|l| l.len() + 1).sum();
    assert_eq!(text.len(), expected_byte_count, "no extra whitespace");

    // No CRLF anywhere.
    assert!(!text.contains("\r\n"), "writer must use LF, not CRLF");
}

// ---- Writer-field fidelity (§G gap 4) ----------------------------------
// On write, claude-code's `insertMessageChain` (sessionStorage.ts:1039-1064)
// stamps `gitBranch` / `entrypoint` / `slug` on every line, `promptId` on USER
// lines only, and `logicalParentUuid` on compact boundaries only. All five use
// `skip_serializing_if = Option::is_none`, so an unset value is OMITTED (TS
// `undefined`), never emitted as `null`.

/// Object keys present on a serialized `JsonlMessage`.
fn keys_of(msg: &JsonlMessage) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::to_value(msg).unwrap() {
        serde_json::Value::Object(m) => m,
        other => panic!("JsonlMessage did not serialize to an object: {other:?}"),
    }
}

#[test]
fn populated_writer_fields_serialize_with_their_keys() {
    // A USER line with all five fidelity fields populated.
    let mut user = make_msg("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", None, 0);
    user.git_branch = Some("main".into());
    user.entrypoint = Some("cli".into());
    user.slug = Some("my-plan".into());
    user.prompt_id = Some("prompt-123".into());
    user.logical_parent_uuid = Some("parent-xyz".into());

    let m = keys_of(&user);
    assert_eq!(m.get("gitBranch").and_then(|v| v.as_str()), Some("main"));
    assert_eq!(m.get("entrypoint").and_then(|v| v.as_str()), Some("cli"));
    assert_eq!(m.get("slug").and_then(|v| v.as_str()), Some("my-plan"));
    assert_eq!(
        m.get("promptId").and_then(|v| v.as_str()),
        Some("prompt-123")
    );
    assert_eq!(
        m.get("logicalParentUuid").and_then(|v| v.as_str()),
        Some("parent-xyz"),
    );
    // Renames, not snake_case, must reach the wire.
    assert!(!m.contains_key("git_branch"));
    assert!(!m.contains_key("prompt_id"));
    assert!(!m.contains_key("logical_parent_uuid"));
}

#[test]
fn unset_writer_fields_are_omitted_not_null() {
    // A non-user line with promptId UNSET and logicalParentUuid UNSET (the
    // common case): both keys must be ABSENT (TS `undefined`), never `null`.
    let mut assistant = make_msg("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", None, 1);
    assistant.message_type = "assistant".into();
    // entrypoint/gitBranch present, the per-line-conditional fields absent.
    assistant.entrypoint = Some("cli".into());
    assistant.git_branch = Some("main".into());

    let m = keys_of(&assistant);
    assert!(
        !m.contains_key("promptId"),
        "non-user line omits promptId entirely",
    );
    assert!(
        !m.contains_key("logicalParentUuid"),
        "non-boundary line omits logicalParentUuid entirely",
    );
    assert!(!m.contains_key("slug"), "unset slug omitted");
    // And the populated ones are present.
    assert!(m.contains_key("entrypoint"));
    assert!(m.contains_key("gitBranch"));
}

#[tokio::test]
async fn fidelity_fields_round_trip_write_then_tolerant_read() {
    use session::jsonl::reader::route_lines;

    let dir = tempdir().expect("tempdir");
    let path = dir
        .path()
        .join("11111111-2222-3333-4444-555555555555.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = JsonlWriter::new(path.clone(), fs.clone());

    // USER line: gitBranch + entrypoint + slug + promptId set.
    let mut user = make_msg("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", None, 0);
    user.git_branch = Some("main".into());
    user.entrypoint = Some("cli".into());
    user.slug = Some("my-plan".into());
    user.prompt_id = Some("prompt-123".into());

    // Compact-boundary assistant line: logicalParentUuid set, promptId unset.
    let mut boundary = make_msg(
        "cccccccc-cccc-cccc-cccc-cccccccccccc",
        Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        1,
    );
    boundary.message_type = "assistant".into();
    boundary.git_branch = Some("main".into());
    boundary.entrypoint = Some("cli".into());
    boundary.logical_parent_uuid = Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into());

    writer.append(&user).await.expect("append user");
    writer.append(&boundary).await.expect("append boundary");

    // Read back through the SAME tolerant reader the loader uses.
    let raw = std::fs::read_to_string(&path).expect("read");
    let loaded = route_lines(&raw);

    let ru = loaded
        .by_uuid
        .get("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
        .expect("user round-tripped");
    assert_eq!(ru.git_branch.as_deref(), Some("main"));
    assert_eq!(ru.entrypoint.as_deref(), Some("cli"));
    assert_eq!(ru.slug.as_deref(), Some("my-plan"));
    assert_eq!(ru.prompt_id.as_deref(), Some("prompt-123"));
    assert_eq!(ru.logical_parent_uuid, None);

    let rb = loaded
        .by_uuid
        .get("cccccccc-cccc-cccc-cccc-cccccccccccc")
        .expect("boundary round-tripped");
    assert_eq!(
        rb.logical_parent_uuid.as_deref(),
        Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
    );
    assert_eq!(
        rb.prompt_id, None,
        "non-user boundary line carries no promptId"
    );
    assert_eq!(rb.git_branch.as_deref(), Some("main"));
}
