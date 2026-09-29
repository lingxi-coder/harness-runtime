//! `load_session` — tolerant, branch-aware load of a REAL `claude-code`
//! transcript.
//!
//! These tests replace the original STRICT linear-chain suite. `load_session`
//! used to enforce: msg[0].parent == None, every parent links to the previous
//! file row, all `sessionId` equal — and errored (`ChainBroken` /
//! `SessionIdMismatch`) otherwise. A genuine transcript violates all three
//! (leading `summary`, interleaved `attachment`/`system`, forked roots,
//! sidechains), so the loader now routes the file tolerantly and reconstructs
//! the main thread via a `parentUuid` DAG walk anchored at the newest
//! non-sidechain user/assistant leaf (`build_conversation_chain`), faithful to
//! `claude-code`'s `loadMessagesFromJsonlPath` (`conversationRecovery.ts:416`).
//! The strict error variants are RETAINED on `LoaderError` (other code matches
//! their `Display`) but are no longer produced from this path.

use lingxi_core::host::FileSystem;
use platform_posix::fs::PosixFileSystem;
use serde_json::json;
use session::jsonl::{load_session, project_dir_name, LoaderError};
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

async fn setup_cwd() -> (
    TempDir,
    std::path::PathBuf,
    String,
    std::path::PathBuf,
    Arc<dyn FileSystem>,
) {
    let temp = TempDir::new().unwrap();
    let cwd_path = temp.path().join("proj");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    tokio::fs::create_dir_all(&subdir).await.unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    (temp, lingxi_home, cwd, subdir, fs)
}

/// A `user`/`assistant` chain-participant line with an explicit timestamp (the
/// timestamp is load-bearing — the newest user/assistant leaf becomes the tip).
fn msg_line(ty: &str, uuid: &str, parent: Option<&str>, session: &str, ts: &str) -> String {
    let mut v = serde_json::Map::new();
    v.insert("type".into(), json!(ty));
    v.insert("uuid".into(), json!(uuid));
    v.insert(
        "parentUuid".into(),
        parent.map_or(json!(null), |p| json!(p)),
    );
    v.insert("sessionId".into(), json!(session));
    v.insert("timestamp".into(), json!(ts));
    v.insert("cwd".into(), json!("/proj"));
    v.insert("version".into(), json!("0.6.0"));
    v.insert("isSidechain".into(), json!(false));
    v.insert("userType".into(), json!("external"));
    v.insert("message".into(), json!({"role": ty, "content": "hi"}));
    format!("{}\n", serde_json::to_string(&v).unwrap())
}

/// A `user` line with a default timestamp (back-compat shape for the simple
/// happy-path test).
fn json_line(uuid: &str, parent: Option<&str>, session: &str) -> String {
    msg_line("user", uuid, parent, session, "2026-05-25T12:00:00.000Z")
}

#[tokio::test]
async fn loads_valid_two_message_session() {
    let (_temp, lingxi_home, cwd, subdir, fs) = setup_cwd().await;
    let sid = Uuid::new_v4();
    let m1 = Uuid::new_v4();
    let m2 = Uuid::new_v4();
    let mut body = msg_line(
        "user",
        &m1.to_string(),
        None,
        &sid.to_string(),
        "2026-05-25T12:00:00.000Z",
    );
    body.push_str(&msg_line(
        "assistant",
        &m2.to_string(),
        Some(&m1.to_string()),
        &sid.to_string(),
        "2026-05-25T12:00:01.000Z",
    ));
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();

    let messages = load_session(&lingxi_home, &cwd, sid, fs).await.expect("ok");
    assert_eq!(messages.len(), 2);
    // Returned root → tip in walk order.
    assert_eq!(messages[0].uuid, m1.to_string());
    assert_eq!(messages[1].uuid, m2.to_string());
}

#[tokio::test]
async fn missing_file_returns_session_not_found() {
    let (_temp, lingxi_home, cwd, _subdir, fs) = setup_cwd().await;
    let sid = Uuid::new_v4();
    let err = load_session(&lingxi_home, &cwd, sid, fs)
        .await
        .expect_err("err");
    match err {
        LoaderError::SessionNotFound { arg } => assert_eq!(arg, sid.to_string()),
        other => panic!("wrong variant: {other:?}"),
    }
}

// ---- TOLERANCE: former STRICT-error cases now load (no error) -------------

#[tokio::test]
async fn dangling_parent_link_no_longer_errors_and_returns_newest_branch() {
    // OLD behavior: m2 whose parentUuid points at a UUID not in the file failed
    // rule 2 → `ChainBroken`. NEW behavior: the dangling parent makes m2 its own
    // root; both m1 and m2 are non-sidechain user leaves, and the NEWER one (m2)
    // anchors the resumed thread. No error.
    let (_temp, lingxi_home, cwd, subdir, fs) = setup_cwd().await;
    let sid = Uuid::new_v4();
    let m1 = Uuid::new_v4();
    let m2 = Uuid::new_v4();
    let dangling = Uuid::new_v4();
    let mut body = msg_line(
        "user",
        &m1.to_string(),
        None,
        &sid.to_string(),
        "2026-05-25T12:00:00.000Z",
    );
    body.push_str(&msg_line(
        "user",
        &m2.to_string(),
        Some(&dangling.to_string()),
        &sid.to_string(),
        "2026-05-25T12:00:05.000Z", // newer → tip
    ));
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();

    let messages = load_session(&lingxi_home, &cwd, sid, fs)
        .await
        .expect("tolerant load must succeed (no ChainBroken)");
    // Walk from m2 stops immediately (its parent `dangling` is absent) → just m2.
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].uuid, m2.to_string());
}

#[tokio::test]
async fn first_message_with_parent_uuid_no_longer_errors() {
    // OLD: a sole first message carrying a parentUuid failed rule 1 →
    // `ChainBroken`. NEW: it is a single user leaf; the walk stops at the
    // missing parent and returns just that message.
    let (_temp, lingxi_home, cwd, subdir, fs) = setup_cwd().await;
    let sid = Uuid::new_v4();
    let m1 = Uuid::new_v4();
    let bogus_parent = Uuid::new_v4();
    let body = msg_line(
        "user",
        &m1.to_string(),
        Some(&bogus_parent.to_string()),
        &sid.to_string(),
        "2026-05-25T12:00:00.000Z",
    );
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();

    let messages = load_session(&lingxi_home, &cwd, sid, fs)
        .await
        .expect("tolerant load must succeed");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].uuid, m1.to_string());
    assert_eq!(
        messages[0].parent_uuid.as_deref(),
        Some(bogus_parent.to_string().as_str())
    );
}

#[tokio::test]
async fn differing_session_ids_no_longer_error() {
    // OLD: a second row claiming a different sessionId failed rule 3 →
    // `SessionIdMismatch`. NEW: forked-session shapes are legitimate — the load
    // succeeds and the leaf supplies the session id (covered in detail by the
    // forked-session test below). Here we just prove no error is raised.
    let (_temp, lingxi_home, cwd, subdir, fs) = setup_cwd().await;
    let sid = Uuid::new_v4();
    let other_sid = Uuid::new_v4();
    let m1 = Uuid::new_v4();
    let m2 = Uuid::new_v4();
    // m1 carries the SOURCE session id; m2 (the live continuation) carries the
    // file's own id — a real fork copies chain[0] verbatim from the source.
    let mut body = msg_line(
        "user",
        &m1.to_string(),
        None,
        &other_sid.to_string(),
        "2026-05-25T12:00:00.000Z",
    );
    body.push_str(&msg_line(
        "assistant",
        &m2.to_string(),
        Some(&m1.to_string()),
        &sid.to_string(),
        "2026-05-25T12:00:01.000Z",
    ));
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();

    let messages = load_session(&lingxi_home, &cwd, sid, fs)
        .await
        .expect("tolerant load must succeed (no SessionIdMismatch)");
    assert_eq!(messages.len(), 2);
}

// ---- GAP 2: real-transcript shape (leading summary + interleaved
//      attachment/system + a sidechain branch) → main thread only ----------

#[tokio::test]
async fn real_transcript_returns_only_newest_main_thread() {
    // A faithful committed fixture. The file:
    //   - opens with a `summary` metadata line (violates old rule 1),
    //   - splices `attachment` then `system` lines between turns (violates old
    //     rule 2's strict file-order parent chain),
    //   - has a SECOND, sidechain branch (`isSidechain:true`) with its own leaf.
    // The walk must return ONLY the main thread, anchored at the newest
    // non-sidechain user/assistant leaf, ignoring metadata + the sidechain.
    let (_temp, lingxi_home, cwd, subdir, fs) = setup_cwd().await;
    // Fixture is written with this exact session uuid as its filename stem.
    let sid: Uuid = "11111111-1111-4111-8111-111111111111".parse().unwrap();
    let fixture = include_str!("fixtures/real_transcript_branched.jsonl");
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), fixture)
        .await
        .unwrap();

    let messages = load_session(&lingxi_home, &cwd, sid, fs).await.expect("ok");

    // Main thread (root → tip): u1 → a1 → u2 → a2. The sidechain (s_user,
    // s_asst) and ALL metadata/attachment/system lines are excluded from the
    // returned conversation chain.
    let uuids: Vec<&str> = messages.iter().map(|m| m.uuid.as_str()).collect();
    assert_eq!(
        uuids,
        vec![
            "aaaaaaaa-0001-4001-8001-000000000001", // u1 (root user)
            "aaaaaaaa-0002-4002-8002-000000000002", // a1 (assistant)
            "aaaaaaaa-0003-4003-8003-000000000003", // u2 (user)
            "aaaaaaaa-0004-4004-8004-000000000004", // a2 (assistant tip — newest)
        ],
        "only the newest non-sidechain main thread is returned, root → tip"
    );
    // Every returned line is a user/assistant message — no system/attachment.
    for m in &messages {
        assert!(
            m.message_type == "user" || m.message_type == "assistant",
            "main thread must contain only user/assistant, got {}",
            m.message_type
        );
        assert!(!m.is_sidechain, "no sidechain message may appear");
    }
}

// ---- GAP 2: forked session — chain[0].sessionId != filename uuid ----------

#[tokio::test]
async fn forked_session_loads_and_uses_leaf_session_id() {
    // A fork copies the source transcript's first row verbatim, so chain[0]'s
    // `sessionId` is the SOURCE session — different from this file's own uuid.
    // The old strict loader rejected this with `SessionIdMismatch`; the new
    // loader must succeed and (per `loadMessagesFromJsonlPath`) the leaf — not
    // the root — supplies the session id.
    let (_temp, lingxi_home, cwd, subdir, fs) = setup_cwd().await;
    let file_sid = Uuid::new_v4(); // this file's own session id (filename stem)
    let source_sid = Uuid::new_v4(); // the session the fork was branched FROM
    let m_root = Uuid::new_v4();
    let m_tip = Uuid::new_v4();

    // Root row carries the SOURCE session id (copied from the source transcript).
    let mut body = msg_line(
        "user",
        &m_root.to_string(),
        None,
        &source_sid.to_string(),
        "2026-05-25T12:00:00.000Z",
    );
    // Continuation row carries THIS file's session id and is the newest leaf.
    body.push_str(&msg_line(
        "assistant",
        &m_tip.to_string(),
        Some(&m_root.to_string()),
        &file_sid.to_string(),
        "2026-05-25T12:00:09.000Z",
    ));
    tokio::fs::write(subdir.join(format!("{file_sid}.jsonl")), body)
        .await
        .unwrap();

    let messages = load_session(&lingxi_home, &cwd, file_sid, fs)
        .await
        .expect("forked session must load (no SessionIdMismatch)");
    assert_eq!(messages.len(), 2, "the full forked chain is returned");
    assert_eq!(messages[0].uuid, m_root.to_string());
    assert_eq!(messages[1].uuid, m_tip.to_string());
}

// ---- recoverOrphanedParallelToolResults (sessionStorage.ts:2096) --------
// PARALLEL tool calls stream as N one-block assistant messages with DISTINCT
// uuid but the SAME `message.id`; each tool_result's parentUuid points at its
// OWN sibling assistant. The tip→root walk keeps only ONE branch, orphaning the
// off-chain siblings + their tool_results. The post-pass re-attaches them right
// after the group's on-chain anchor without reordering the main chain.

/// An `assistant` line carrying an inner `message.id` (the shared parallel-group
/// id) and a single `tool_use` block — the shape a streamed parallel turn writes.
fn assistant_tooluse_line(
    uuid: &str,
    parent: Option<&str>,
    session: &str,
    ts: &str,
    message_id: &str,
    tool_use_id: &str,
) -> String {
    let v = json!({
        "type": "assistant",
        "uuid": uuid,
        "parentUuid": parent,
        "sessionId": session,
        "timestamp": ts,
        "cwd": "/proj",
        "version": "0.6.0",
        "isSidechain": false,
        "message": {
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "content": [{"type": "tool_use", "id": tool_use_id, "name": "Bash", "input": {}}],
            "model": "claude-3-5-sonnet-latest",
            "stop_reason": "tool_use",
        },
    });
    format!("{}\n", serde_json::to_string(&v).unwrap())
}

/// A plain `assistant` text line with an inner `message.id` (the final answer).
fn assistant_text_line(
    uuid: &str,
    parent: Option<&str>,
    session: &str,
    ts: &str,
    message_id: &str,
) -> String {
    let v = json!({
        "type": "assistant",
        "uuid": uuid,
        "parentUuid": parent,
        "sessionId": session,
        "timestamp": ts,
        "cwd": "/proj",
        "version": "0.6.0",
        "isSidechain": false,
        "message": {
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "done"}],
            "model": "claude-3-5-sonnet-latest",
            "stop_reason": "end_turn",
        },
    });
    format!("{}\n", serde_json::to_string(&v).unwrap())
}

/// A `user` line whose inner `message.content` is a `tool_result` array, with
/// `parentUuid` pointing at the assistant whose `tool_use` it answers (the
/// write-time `sourceToolAssistantUUID` override).
fn tool_result_line(
    uuid: &str,
    parent: &str,
    session: &str,
    ts: &str,
    tool_use_id: &str,
) -> String {
    let v = json!({
        "type": "user",
        "uuid": uuid,
        "parentUuid": parent,
        "sessionId": session,
        "timestamp": ts,
        "cwd": "/proj",
        "version": "0.6.0",
        "isSidechain": false,
        "userType": "external",
        "message": {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": "ok"}],
        },
    });
    format!("{}\n", serde_json::to_string(&v).unwrap())
}

#[tokio::test]
async fn recovers_orphaned_parallel_tool_result_from_sibling_branch() {
    let (_temp, lingxi_home, cwd, subdir, fs) = setup_cwd().await;
    let sid = Uuid::new_v4().to_string();

    // u0 → a1(tool_use#1, id=msg_par) → tr1(parent=a1)  [walk's branch]
    //        a2(tool_use#2, id=msg_par, parent=a1)        [orphan sibling]
    //          tr2(parent=a2)                              [orphan tool_result]
    //      a3(text, id=msg_final, parent=tr1)              [tip — newest]
    let u0 = "00000000-0000-4000-8000-000000000000";
    let a1 = "a1000000-0000-4000-8000-000000000001";
    let a2 = "a2000000-0000-4000-8000-000000000002";
    let tr1 = "71000000-0000-4000-8000-000000000071";
    let tr2 = "72000000-0000-4000-8000-000000000072";
    let a3 = "a3000000-0000-4000-8000-000000000003";

    let mut body = String::new();
    body.push_str(&msg_line(
        "user",
        u0,
        None,
        &sid,
        "2026-05-25T12:00:00.000Z",
    ));
    body.push_str(&assistant_tooluse_line(
        a1,
        Some(u0),
        &sid,
        "2026-05-25T12:00:01.000Z",
        "msg_par",
        "tool_1",
    ));
    // Sibling assistant: SAME message.id (msg_par), chained off a1, OFF the walk.
    body.push_str(&assistant_tooluse_line(
        a2,
        Some(a1),
        &sid,
        "2026-05-25T12:00:02.000Z",
        "msg_par",
        "tool_2",
    ));
    // tr1 answers a1 → ON the walk (a3's parent chain runs a3→tr1→a1→u0).
    body.push_str(&tool_result_line(
        tr1,
        a1,
        &sid,
        "2026-05-25T12:00:03.000Z",
        "tool_1",
    ));
    // tr2 answers a2 → ORPHAN (its carrier a2 is off-chain).
    body.push_str(&tool_result_line(
        tr2,
        a2,
        &sid,
        "2026-05-25T12:00:04.000Z",
        "tool_2",
    ));
    // Final answer is the newest leaf → the tip.
    body.push_str(&assistant_text_line(
        a3,
        Some(tr1),
        &sid,
        "2026-05-25T12:00:05.000Z",
        "msg_final",
    ));

    let sid_uuid = Uuid::parse_str(&sid).unwrap();
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();

    let chain = load_session(&lingxi_home, &cwd, sid_uuid, fs)
        .await
        .expect("loads");

    let uuids: Vec<&str> = chain.iter().map(|m| m.uuid.as_str()).collect();

    // The orphaned sibling a2 AND its orphaned tool_result tr2 are recovered.
    assert!(
        uuids.contains(&a2),
        "orphaned sibling assistant a2 recovered: {uuids:?}"
    );
    assert!(
        uuids.contains(&tr2),
        "orphaned tool_result tr2 recovered: {uuids:?}"
    );

    // Main chain is NOT reordered: u0, a1, tr1, a3 keep their relative order,
    // and the recovered group [a2, tr2] is spliced right after the anchor a1.
    assert_eq!(
        uuids,
        vec![u0, a1, a2, tr2, tr1, a3],
        "recovered group inserted after anchor a1; main chain order preserved",
    );
}

#[tokio::test]
async fn no_parallel_calls_chain_is_unchanged_by_recovery() {
    // A linear transcript (no shared message.id, no sibling branches) must be
    // returned byte-identical — the recovery pass is a strict no-op.
    let (_temp, lingxi_home, cwd, subdir, fs) = setup_cwd().await;
    let sid = Uuid::new_v4().to_string();
    let u0 = "00000000-0000-4000-8000-0000000000a0";
    let a1 = "a1000000-0000-4000-8000-0000000000a1";
    let tr1 = "71000000-0000-4000-8000-0000000000b1";
    let a2 = "a2000000-0000-4000-8000-0000000000a2";

    let mut body = String::new();
    body.push_str(&msg_line(
        "user",
        u0,
        None,
        &sid,
        "2026-05-25T12:00:00.000Z",
    ));
    body.push_str(&assistant_tooluse_line(
        a1,
        Some(u0),
        &sid,
        "2026-05-25T12:00:01.000Z",
        "msg_one",
        "tool_x",
    ));
    body.push_str(&tool_result_line(
        tr1,
        a1,
        &sid,
        "2026-05-25T12:00:02.000Z",
        "tool_x",
    ));
    body.push_str(&assistant_text_line(
        a2,
        Some(tr1),
        &sid,
        "2026-05-25T12:00:03.000Z",
        "msg_two",
    ));

    let sid_uuid = Uuid::parse_str(&sid).unwrap();
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();

    let chain = load_session(&lingxi_home, &cwd, sid_uuid, fs)
        .await
        .expect("loads");
    let uuids: Vec<&str> = chain.iter().map(|m| m.uuid.as_str()).collect();
    assert_eq!(uuids, vec![u0, a1, tr1, a2], "linear chain unchanged");
}

// Keep the legacy single-message happy path (still valid, exercises the
// `json_line` default-timestamp shape used elsewhere in the suite).
#[tokio::test]
async fn loads_single_user_message() {
    let (_temp, lingxi_home, cwd, subdir, fs) = setup_cwd().await;
    let sid = Uuid::new_v4();
    let m1 = Uuid::new_v4();
    let body = json_line(&m1.to_string(), None, &sid.to_string());
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();

    let messages = load_session(&lingxi_home, &cwd, sid, fs).await.expect("ok");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].uuid, m1.to_string());
}

// ---------- P1-05: compact-boundary chain reset + preserved-tail re-splice ----------

/// A claude-2.1.207-shaped `subtype:"compact_boundary"` system line:
/// `parentUuid: null` (chain reset), the real parent in `logicalParentUuid`,
/// flattened `content`/`level` envelope, and the given `compactMetadata`.
fn boundary_line(
    uuid: &str,
    logical_parent: &str,
    session: &str,
    ts: &str,
    compact_metadata: serde_json::Value,
) -> String {
    let mut v = serde_json::Map::new();
    v.insert("parentUuid".into(), json!(null));
    v.insert("logicalParentUuid".into(), json!(logical_parent));
    v.insert("isSidechain".into(), json!(false));
    v.insert("type".into(), json!("system"));
    v.insert("subtype".into(), json!("compact_boundary"));
    v.insert("content".into(), json!("Conversation compacted"));
    v.insert("level".into(), json!("info"));
    v.insert("compactMetadata".into(), compact_metadata);
    v.insert("uuid".into(), json!(uuid));
    v.insert("timestamp".into(), json!(ts));
    v.insert("sessionId".into(), json!(session));
    v.insert("cwd".into(), json!("/proj"));
    v.insert("version".into(), json!("0.6.0"));
    format!("{}\n", serde_json::to_string(&v).unwrap())
}

/// An assistant line carrying an inner `message.id` (the per-block shared id).
fn assistant_line_with_inner_id(
    uuid: &str,
    parent: Option<&str>,
    session: &str,
    ts: &str,
    inner_id: &str,
) -> String {
    let mut v = serde_json::Map::new();
    v.insert("type".into(), json!("assistant"));
    v.insert("uuid".into(), json!(uuid));
    v.insert(
        "parentUuid".into(),
        parent.map_or(json!(null), |p| json!(p)),
    );
    v.insert("sessionId".into(), json!(session));
    v.insert("timestamp".into(), json!(ts));
    v.insert("cwd".into(), json!("/proj"));
    v.insert("version".into(), json!("0.6.0"));
    v.insert("isSidechain".into(), json!(false));
    v.insert(
        "message".into(),
        json!({"id": inner_id, "role": "assistant", "content": [{"type":"text","text":"block"}]}),
    );
    format!("{}\n", serde_json::to_string(&v).unwrap())
}

/// Boundary without preserved metadata: the tip→root walk stops at the
/// boundary's `parentUuid: null`, so the summarized pre-compact prefix never
/// re-enters the chain (claude's fast-path chain reset).
#[tokio::test]
async fn chain_stops_at_compact_boundary_parent_null() {
    let (_temp, lingxi_home, cwd, subdir, fs) = setup_cwd().await;
    let sid = Uuid::new_v4().to_string();
    let (u1, a2, b3, s4, a5) = (
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
    );
    let mut body = msg_line("user", &u1, None, &sid, "2026-07-13T10:00:00.000Z");
    body.push_str(&msg_line(
        "assistant",
        &a2,
        Some(&u1),
        &sid,
        "2026-07-13T10:00:01.000Z",
    ));
    body.push_str(&boundary_line(
        &b3,
        &a2,
        &sid,
        "2026-07-13T10:00:02.000Z",
        json!({"trigger":"auto","preTokens":100}),
    ));
    body.push_str(&msg_line(
        "user",
        &s4,
        Some(&b3),
        &sid,
        "2026-07-13T10:00:03.000Z",
    ));
    body.push_str(&msg_line(
        "assistant",
        &a5,
        Some(&s4),
        &sid,
        "2026-07-13T10:00:04.000Z",
    ));
    let sid_uuid = Uuid::parse_str(&sid).unwrap();
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();

    let messages = load_session(&lingxi_home, &cwd, sid_uuid, fs)
        .await
        .expect("ok");
    let uuids: Vec<&str> = messages.iter().map(|m| m.uuid.as_str()).collect();
    assert_eq!(
        uuids,
        vec![b3.as_str(), s4.as_str(), a5.as_str()],
        "walk must stop at the boundary's parentUuid:null; pre-compact prefix dropped"
    );
}

/// Boundary WITH `compactMetadata.preservedMessages`: the `E$_` re-parent pass
/// splices the verbatim kept tail back in AFTER the summary, so the chain is
/// [boundary, summary, ...tail, ...post-compact] and the summarized prefix is
/// dropped — even though the post-compact lines physically parent off the
/// tail's last on-disk line.
#[tokio::test]
async fn compact_boundary_preserved_tail_resplices_after_summary() {
    let (_temp, lingxi_home, cwd, subdir, fs) = setup_cwd().await;
    let sid = Uuid::new_v4().to_string();
    let (u1, a2, u3, a4, b5, s6, a7) = (
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
    );
    let mut body = msg_line("user", &u1, None, &sid, "2026-07-13T10:00:00.000Z");
    body.push_str(&msg_line(
        "assistant",
        &a2,
        Some(&u1),
        &sid,
        "2026-07-13T10:00:01.000Z",
    ));
    // Preserved tail (already on disk, parenting into the pre-compact chain).
    body.push_str(&msg_line(
        "user",
        &u3,
        Some(&a2),
        &sid,
        "2026-07-13T10:00:02.000Z",
    ));
    body.push_str(&msg_line(
        "assistant",
        &a4,
        Some(&u3),
        &sid,
        "2026-07-13T10:00:03.000Z",
    ));
    body.push_str(&boundary_line(
        &b5,
        &a4,
        &sid,
        "2026-07-13T10:00:04.000Z",
        json!({
            "trigger": "auto",
            "preTokens": 100,
            "preservedSegment": {"headUuid": u3, "anchorUuid": s6, "tailUuid": a4},
            "preservedMessages": {"anchorUuid": s6, "uuids": [u3, a4], "allUuids": [u3, a4]},
        }),
    ));
    body.push_str(&msg_line(
        "user",
        &s6,
        Some(&b5),
        &sid,
        "2026-07-13T10:00:05.000Z",
    ));
    // Post-compact line chains off the tail's LAST on-disk line (claude shape).
    body.push_str(&msg_line(
        "assistant",
        &a7,
        Some(&a4),
        &sid,
        "2026-07-13T10:00:06.000Z",
    ));
    let sid_uuid = Uuid::parse_str(&sid).unwrap();
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();

    let messages = load_session(&lingxi_home, &cwd, sid_uuid, fs)
        .await
        .expect("ok");
    let uuids: Vec<&str> = messages.iter().map(|m| m.uuid.as_str()).collect();
    assert_eq!(
        uuids,
        vec![
            b5.as_str(),
            s6.as_str(),
            u3.as_str(),
            a4.as_str(),
            a7.as_str()
        ],
        "preserved tail must re-splice after the summary; summarized prefix dropped"
    );
    // The spliced tail head's parentUuid is patched onto the anchor (summary).
    let tail_head = messages.iter().find(|m| m.uuid == u3).unwrap();
    assert_eq!(tail_head.parent_uuid.as_deref(), Some(s6.as_str()));
}

/// A preserved id that matches no outer line uuid resolves to the per-block
/// assistant lines sharing that inner `message.id` (this engine's write-side
/// split), in file order.
#[tokio::test]
async fn preserved_tail_assistant_blocks_resolved_by_inner_message_id() {
    for segment_head in ["legacy", "user", "assistant"] {
        let (_temp, lingxi_home, cwd, subdir, fs) = setup_cwd().await;
        let sid = Uuid::new_v4().to_string();
        let inner_id = Uuid::new_v4().to_string(); // in-memory assistant id
        let (u1, a2, u3, a4a, a4b, b5, s6, a7) = (
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
        );
        let mut body = msg_line("user", &u1, None, &sid, "2026-07-13T10:00:00.000Z");
        body.push_str(&msg_line(
            "assistant",
            &a2,
            Some(&u1),
            &sid,
            "2026-07-13T10:00:01.000Z",
        ));
        body.push_str(&msg_line(
            "user",
            &u3,
            Some(&a2),
            &sid,
            "2026-07-13T10:00:02.000Z",
        ));
        // The preserved assistant turn was persisted per-block: two lines with
        // fresh outer uuids sharing the inner `message.id`.
        body.push_str(&assistant_line_with_inner_id(
            &a4a,
            Some(&u3),
            &sid,
            "2026-07-13T10:00:03.000Z",
            &inner_id,
        ));
        body.push_str(&assistant_line_with_inner_id(
            &a4b,
            Some(&a4a),
            &sid,
            "2026-07-13T10:00:04.000Z",
            &inner_id,
        ));
        let mut metadata = json!({
            "trigger": "auto", "preTokens": 100,
            "preservedMessages": {"anchorUuid": s6, "uuids": [u3, inner_id], "allUuids": [u3, inner_id]},
        });
        if segment_head != "legacy" {
            metadata["preservedSegment"] = json!({
                "headUuid": if segment_head == "assistant" { &inner_id } else { &u3 },
                "anchorUuid": s6, "tailUuid": inner_id,
            });
        }
        body.push_str(&boundary_line(
            &b5,
            &a4b,
            &sid,
            "2026-07-13T10:00:05.000Z",
            metadata,
        ));
        body.push_str(&msg_line(
            "user",
            &s6,
            Some(&b5),
            &sid,
            "2026-07-13T10:00:06.000Z",
        ));
        body.push_str(&msg_line(
            "assistant",
            &a7,
            Some(&a4b),
            &sid,
            "2026-07-13T10:00:07.000Z",
        ));
        // On-disk usage predates compaction for both preserved response blocks.
        // A fresh response after the boundary must retain its own usage.
        let stale_usage = json!({
            "input_tokens": 120_000, "output_tokens": 300,
            "cache_creation_input_tokens": 20_000, "cache_read_input_tokens": 50_000,
            "server_tool_use": {"web_search_requests": 1}
        });
        let fresh_usage = json!({"input_tokens": 2_000, "output_tokens": 20});
        body = body
            .lines()
            .map(|line| {
                let mut row: serde_json::Value = serde_json::from_str(line).unwrap();
                if row["uuid"] == a4a || row["uuid"] == a4b {
                    row["message"]["usage"] = stale_usage.clone();
                } else if row["uuid"] == a7 {
                    row["message"]["usage"] = fresh_usage.clone();
                }
                format!("{row}\n")
            })
            .collect();
        let sid_uuid = Uuid::parse_str(&sid).unwrap();
        tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
            .await
            .unwrap();

        let messages = load_session(&lingxi_home, &cwd, sid_uuid, fs)
            .await
            .expect("ok");
        let uuids: Vec<&str> = messages.iter().map(|m| m.uuid.as_str()).collect();
        let mut expected = vec![b5.as_str(), s6.as_str()];
        if segment_head != "assistant" {
            expected.push(u3.as_str());
        }
        expected.extend([a4a.as_str(), a4b.as_str(), a7.as_str()]);
        assert_eq!(
            uuids, expected,
            "per-block assistant tail lines must resolve via inner message.id"
        );
        for uuid in [&a4a, &a4b] {
            let usage = &messages.iter().find(|m| &m.uuid == uuid).unwrap().message["usage"];
            assert_eq!(
                usage,
                &json!({
                    "input_tokens": 0, "output_tokens": 0,
                    "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                    "server_tool_use": {"web_search_requests": 1}
                }),
                "preserved assistant usage must not revive the precompact context"
            );
        }
        assert_eq!(messages.last().unwrap().message["usage"], fresh_usage);
    }
}

#[tokio::test]
async fn preserved_segment_resume_usage_and_boundary_validation() {
    for variant in ["live", "fresh", "stale", "broken"] {
        let (_temp, home, cwd, subdir, fs) = setup_cwd().await;
        let sid = Uuid::new_v4().to_string();
        let mut rows = vec![
            json!({"type":"user", "uuid":"old", "parentUuid":null}),
            json!({"type":"user", "uuid":"head", "parentUuid":"old"}),
            json!({"type":"assistant", "uuid":"tail", "parentUuid":"head",
                "message":{"role":"assistant", "content":"kept", "usage":{
                    "input_tokens":120000, "output_tokens":300,
                    "cache_creation_input_tokens":20000, "cache_read_input_tokens":50000}}}),
            json!({"type":"system", "uuid":"boundary", "parentUuid":null,
                "subtype":"compact_boundary", "compactMetadata":{"preservedSegment":{
                    "headUuid":if variant == "broken" {"missing"} else {"head"},
                    "tailUuid":"tail", "anchorUuid":"summary"}}}),
            json!({"type":"user", "uuid":"summary", "parentUuid":"boundary"}),
        ];
        if variant == "fresh" {
            rows.push(json!({"type":"assistant", "uuid":"fresh", "parentUuid":"summary",
                "message":{"role":"assistant", "content":"new", "usage":{"input_tokens":2000,"output_tokens":20}}}));
        } else if variant == "stale" {
            rows.push(
                json!({"type":"system", "uuid":"boundary2", "parentUuid":null,
                "subtype":"compact_boundary", "compactMetadata":{}}),
            );
            rows.push(json!({"type":"user", "uuid":"summary2", "parentUuid":"boundary2"}));
        } else if variant == "broken" {
            rows.push(json!({"type":"user", "uuid":"continuation", "parentUuid":"tail"}));
        }
        let body: String = rows
            .iter_mut()
            .enumerate()
            .map(|(i, row)| {
                row["sessionId"] = json!(sid);
                row["timestamp"] = json!(format!("2026-09-20T10:00:{i:02}.000Z"));
                row["cwd"] = json!(cwd);
                row["version"] = json!("2.1.270");
                if row.get("message").is_none() {
                    row["message"] = json!({"role":"user", "content":"text"});
                }
                format!("{row}\n")
            })
            .collect();
        tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
            .await
            .unwrap();
        let messages = load_session(&home, &cwd, Uuid::parse_str(&sid).unwrap(), fs)
            .await
            .unwrap();
        let ids: Vec<_> = messages.iter().map(|m| m.uuid.as_str()).collect();
        let expected = match variant {
            "live" => vec!["boundary", "summary", "head", "tail"],
            "fresh" => vec!["boundary", "summary", "head", "tail", "fresh"],
            "stale" => vec!["boundary2", "summary2"],
            _ => vec!["old", "head", "tail", "continuation"],
        };
        assert_eq!(ids, expected, "{variant}");
        if let Some(tail) = messages.iter().find(|m| m.uuid == "tail") {
            for (field, old) in [
                ("input_tokens", 120000),
                ("output_tokens", 300),
                ("cache_creation_input_tokens", 20000),
                ("cache_read_input_tokens", 50000),
            ] {
                assert_eq!(
                    tail.message["usage"][field],
                    if variant == "broken" { old } else { 0 },
                    "{variant} {field}"
                );
            }
        }
        if variant == "fresh" {
            assert_eq!(
                messages.last().unwrap().parent_uuid.as_deref(),
                Some("tail")
            );
            assert_eq!(
                messages.last().unwrap().message["usage"]["input_tokens"],
                2000
            );
        }
    }
}

#[tokio::test]
async fn preserved_segment_keeps_parallel_results_and_split_blocks_only_from_latest_boundary() {
    for superseded in [false, true] {
        let (_temp, home, cwd, subdir, fs) = setup_cwd().await;
        let sid = Uuid::new_v4().to_string();
        let mut rows = vec![
            json!({"type":"user", "uuid":"old", "parentUuid":null}),
            json!({"type":"system", "uuid":"old-boundary", "parentUuid":null,
                "subtype":"compact_boundary", "compactMetadata":{"preservedMessages":{
                    "anchorUuid":"old-summary", "uuids":["discarded-result"], "allUuids":["discarded-result"]}}}),
            json!({"type":"user", "uuid":"old-summary", "parentUuid":"old-boundary"}),
            json!({"type":"user", "uuid":"head", "parentUuid":"old-summary"}),
            json!({"type":"assistant", "uuid":"block-a", "parentUuid":"head",
                "message":{"id":"response", "role":"assistant", "content":[{"type":"tool_use", "id":"tool-a", "name":"Bash", "input":{}}], "usage":{"input_tokens":120000}}}),
            json!({"type":"assistant", "uuid":"block-b", "parentUuid":"block-a",
                "message":{"id":"response", "role":"assistant", "content":[{"type":"tool_use", "id":"tool-b", "name":"Bash", "input":{}}], "usage":{"input_tokens":120000}}}),
            // Each result points to its source assistant, so result-a is not
            // on the result-b -> head parent chain.
            json!({"type":"user", "uuid":"result-a", "parentUuid":"block-a",
                "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"tool-a", "content":"a"}]}}),
            json!({"type":"user", "uuid":"discarded-result", "parentUuid":"block-a",
                "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"discarded", "content":"old"}]}}),
            json!({"type":"user", "uuid":"result-b", "parentUuid":"block-b",
                "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"tool-b", "content":"b"}]}}),
            json!({"type":"system", "uuid":"boundary", "parentUuid":null,
                "subtype":"compact_boundary", "compactMetadata":{
                    "preservedSegment":{"headUuid":"head", "tailUuid":"result-b", "anchorUuid":"summary"},
                    "preservedMessages":{"anchorUuid":"summary", "uuids":["head", "response", "result-a", "result-b"], "allUuids":["head", "response", "result-a", "result-b"]}}}),
            json!({"type":"user", "uuid":"summary", "parentUuid":"boundary"}),
        ];
        if superseded {
            rows.push(
                json!({"type":"system", "uuid":"new-boundary", "parentUuid":null,
                "subtype":"compact_boundary", "compactMetadata":{}}),
            );
            rows.push(json!({"type":"user", "uuid":"new-summary", "parentUuid":"new-boundary"}));
        }
        let body: String = rows
            .iter_mut()
            .enumerate()
            .map(|(i, row)| {
                row["sessionId"] = json!(sid);
                row["timestamp"] = json!(format!("2026-09-20T10:00:{i:02}.000Z"));
                row["cwd"] = json!(cwd);
                row["version"] = json!("2.1.270");
                if row.get("message").is_none() {
                    row["message"] = json!({"role":"user", "content":"text"});
                }
                format!("{row}\n")
            })
            .collect();
        tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
            .await
            .unwrap();
        let messages = load_session(&home, &cwd, Uuid::parse_str(&sid).unwrap(), fs)
            .await
            .unwrap();
        let ids: Vec<_> = messages.iter().map(|m| m.uuid.as_str()).collect();
        if superseded {
            assert_eq!(ids, ["new-boundary", "new-summary"]);
        } else {
            assert_eq!(
                ids,
                ["boundary", "summary", "head", "block-a", "block-b", "result-a", "result-b"]
            );
            for message in messages.iter().filter(|m| m.message_type == "assistant") {
                for field in [
                    "input_tokens",
                    "output_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                ] {
                    assert_eq!(message.message["usage"][field], 0);
                }
            }
        }
    }
}
