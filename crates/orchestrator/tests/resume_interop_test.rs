//! append a 3rd message using the replayed `last_message_uuid` as parent,
//! re-read via `JsonlReader`, confirm the full 3-link chain.

use orchestrator::replay_session_state;
use platform_api::FileSystem;
use platform_posix::fs::PosixFileSystem;
use serde_json::json;
use session::jsonl::{project_dir_name, session_path, JsonlMessage, JsonlReader, JsonlWriter};
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

fn build_msg(
    type_str: &str,
    uuid: Uuid,
    parent: Option<Uuid>,
    sid: Uuid,
    cwd: &str,
    body: &str,
) -> JsonlMessage {
    serde_json::from_value(json!({
        "type": type_str,
        "uuid": uuid.to_string(),
        "parentUuid": parent.map(|p| p.to_string()),
        "sessionId": sid.to_string(),
        "timestamp": "2026-05-25T12:00:00.000Z",
        "cwd": cwd,
        "version": "0.6.0",
        "isSidechain": false,
        "userType": "external",
        "message": {"role": type_str, "content": body}
    }))
    .expect("jsonl message")
}

#[tokio::test]
async fn write_then_resume_then_append_then_read_full_chain_intact() {
    let temp = TempDir::new().unwrap();
    let cwd_path = temp.path().join("proj");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    tokio::fs::create_dir_all(&subdir).await.unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));

    let sid = Uuid::new_v4();
    let m1 = Uuid::new_v4();
    let m2 = Uuid::new_v4();
    let path = session_path(&lingxi_home, &cwd, &sid.to_string());

    // (1) initial 2-turn write via the M5-07 writer.
    let writer = Arc::new(JsonlWriter::new(path.clone(), fs.clone()));
    writer
        .append(&build_msg("user", m1, None, sid, &cwd, "hi"))
        .await
        .unwrap();
    writer
        .append(&build_msg("assistant", m2, Some(m1), sid, &cwd, "hello"))
        .await
        .unwrap();

    // (2) load via M5-08.
    let replayed = replay_session_state(&lingxi_home, &cwd, sid, fs.clone())
        .await
        .unwrap();
    assert_eq!(replayed.messages.len(), 2);
    assert_eq!(replayed.last_message_uuid, Some(m2));

    // (3) append 3rd message — parent MUST be m2.
    let m3 = Uuid::new_v4();
    let next = build_msg(
        "user",
        m3,
        replayed.last_message_uuid,
        replayed.state.session_id.as_uuid(),
        &cwd,
        "next",
    );
    writer.append(&next).await.unwrap();

    // (4) re-read full file with the M5-07 reader.
    let reader = JsonlReader::new(path, fs);
    let all = reader.read_all().await.unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].uuid, m1.to_string());
    assert_eq!(all[0].parent_uuid, None);
    assert_eq!(all[1].parent_uuid.as_deref(), Some(m1.to_string().as_str()));
    assert_eq!(all[2].parent_uuid.as_deref(), Some(m2.to_string().as_str()));
    for m in &all {
        assert_eq!(m.session_id, sid.to_string());
    }
}
