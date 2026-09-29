//! T4 tests — populate a tempdir, assert `list_recent_sessions` returns sorted desc.

use lingxi_core::host::FileSystem;
use platform_posix::fs::PosixFileSystem;
use session::jsonl::{list_recent_sessions, project_dir_name, LoaderError};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;
use uuid::Uuid;

/// Build a tempdir that mimics `<lingxi_home>/projects/<sanitize(cwd)>/` and
/// returns (tempdir, `lingxi_home`, `cwd_string`).
async fn setup_project(file_count: usize) -> (TempDir, std::path::PathBuf, String) {
    let temp = TempDir::new().expect("tempdir");
    let cwd_path = temp.path().join("workproj");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let project_subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    tokio::fs::create_dir_all(&project_subdir).await.unwrap();

    let base = SystemTime::now();
    for i in 0..file_count {
        let uuid = Uuid::new_v4();
        let path = project_subdir.join(format!("{uuid}.jsonl"));
        let line = serde_json::json!({
            "type": "user",
            "uuid": uuid.to_string(),
            "parentUuid": null,
            "sessionId": uuid.to_string(),
            "timestamp": "2026-05-25T12:00:00.000Z",
            "cwd": cwd,
            "version": "0.6.0",
            "isSidechain": false,
            "userType": "external",
            "message": {"role": "user", "content": format!("prompt {i}")}
        });
        let bytes = format!("{}\n", serde_json::to_string(&line).unwrap());
        tokio::fs::write(&path, bytes).await.unwrap();
        // Stagger mtimes by 1 second so the desc sort has a deterministic order.
        let mtime = base + Duration::from_secs(i as u64);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(mtime)).unwrap();
    }
    (temp, lingxi_home, cwd)
}

fn make_fs(root: &std::path::Path) -> Arc<dyn FileSystem> {
    Arc::new(PosixFileSystem::new(root.to_path_buf()))
}

#[tokio::test]
async fn returns_up_to_limit_sorted_newest_first() {
    let (temp, lingxi_home, cwd) = setup_project(7).await;
    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");
    assert_eq!(rows.len(), 5);
    // Newest first — index 6 was created last with the highest mtime.
    for w in rows.windows(2) {
        assert!(w[0].modified >= w[1].modified);
    }
}

#[tokio::test]
async fn empty_project_dir_returns_empty_error() {
    let temp = TempDir::new().unwrap();
    let cwd_path = temp.path().join("noproj");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    // Create the projects dir but no jsonl in it.
    let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    tokio::fs::create_dir_all(&subdir).await.unwrap();
    let fs = make_fs(temp.path());
    match list_recent_sessions(&lingxi_home, &cwd, 5, fs).await {
        Err(LoaderError::EmptyDirectory) => {}
        other => panic!("expected EmptyDirectory, got {other:?}"),
    }
}

#[tokio::test]
async fn nonexistent_project_dir_returns_empty_error() {
    let temp = TempDir::new().unwrap();
    let cwd_path = temp.path().join("nope");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let fs = make_fs(temp.path());
    match list_recent_sessions(&lingxi_home, &cwd, 5, fs).await {
        Err(LoaderError::EmptyDirectory) => {}
        other => panic!("expected EmptyDirectory, got {other:?}"),
    }
}

#[tokio::test]
async fn skips_non_uuid_filenames() {
    let (temp, lingxi_home, cwd) = setup_project(2).await;
    let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    tokio::fs::write(subdir.join("not-a-uuid.jsonl"), "{}\n")
        .await
        .unwrap();
    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");
    assert_eq!(rows.len(), 2); // the 2 from setup_project, not the bogus one
}

// ---- Resume-picker title precedence: custom-title > ai-title > summary > -----
// ---- first-user-message. 1:1 with claude-code's `getLogDisplayTitle`     -----
// ---- (`customTitle || summary || firstPrompt`, `utils/log.ts:30`) composed --
// ---- with `readLiteMetadata`'s custom-over-ai rule (custom-title field wins --
// ---- over ai-title field, `sessionStorage.ts:4771-4775`). The side maps are --
// ---- produced by the tolerant reader's `read_routed`: `summaries` keyed by  --
// ---- the chain tip's `leafUuid`, `custom_titles`/`ai_titles` by sessionId.  --

/// Set up a single-session project dir whose ONE `.jsonl` file is built from the
/// supplied raw JSONL `lines` (already-serialized, one per element). The filename
/// stem is `sid` (== the picker's `sid` key for custom/ai titles). Returns the
/// `lingxi_home` + `cwd` so the caller can run `list_recent_sessions`.
async fn setup_one(lines: &[String], sid: Uuid) -> (TempDir, std::path::PathBuf, String) {
    let temp = TempDir::new().expect("tempdir");
    let cwd_path = temp.path().join("workproj");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    tokio::fs::create_dir_all(&subdir).await.unwrap();

    let mut body = String::new();
    for l in lines {
        body.push_str(l);
        body.push('\n');
    }
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();
    (temp, lingxi_home, cwd)
}

/// A `user` first-prompt line whose `uuid` is also the chain tip (single-message
/// session — the tip leafUuid == this uuid, so a `summary` keyed by it links).
fn user_line(uuid: Uuid, sid: Uuid, cwd: &str, prompt: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "type": "user",
        "uuid": uuid.to_string(),
        "parentUuid": null,
        "sessionId": sid.to_string(),
        "timestamp": "2026-05-25T12:00:00.000Z",
        "cwd": cwd,
        "version": "0.12.0",
        "isSidechain": false,
        "userType": "external",
        "message": {"role": "user", "content": prompt},
    }))
    .unwrap()
}

fn summary_line(leaf_uuid: Uuid, summary: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "type": "summary",
        "summary": summary,
        "leafUuid": leaf_uuid.to_string(),
    }))
    .unwrap()
}

fn custom_title_line(sid: Uuid, custom_title: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "type": "custom-title",
        "sessionId": sid.to_string(),
        "customTitle": custom_title,
    }))
    .unwrap()
}

fn mobile_empty_title_line(sid: Uuid, custom_title: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "type": "custom-title",
        "sessionId": sid.to_string(),
        "customTitle": custom_title,
        "mobileEmptySession": 1,
    }))
    .unwrap()
}

fn ai_title_line(sid: Uuid, ai_title: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "type": "ai-title",
        "sessionId": sid.to_string(),
        "aiTitle": ai_title,
    }))
    .unwrap()
}

#[tokio::test]
async fn summary_for_tip_leaf_becomes_picker_title() {
    // A session whose transcript carries a `summary` line for its tip leaf → the
    // picker shows that summary, NOT the first user prompt.
    let sid = Uuid::new_v4();
    let tip = sid; // single-message session: the lone user line is the tip leaf
    let lines = vec![
        summary_line(tip, "Refactor the JSONL parser"),
        user_line(tip, sid, "/cwd-ignored", "please look at the parser"),
    ];
    let (temp, lingxi_home, cwd) = setup_one(&lines, sid).await;
    let fs = make_fs(temp.path());

    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "Refactor the JSONL parser");
}

#[tokio::test]
async fn metadata_only_title_file_is_not_a_session() {
    let sid = Uuid::new_v4();
    let (temp, lingxi_home, cwd) =
        setup_one(&[custom_title_line(sid, "Control Rename")], sid).await;
    assert!(matches!(
        list_recent_sessions(&lingxi_home, &cwd, 5, make_fs(temp.path())).await,
        Err(LoaderError::EmptyDirectory)
    ));
}

#[tokio::test]
async fn mobile_empty_anchor_is_listed_with_zero_messages_and_title() {
    let sid = Uuid::new_v4();
    let (temp, lingxi_home, cwd) =
        setup_one(&[mobile_empty_title_line(sid, "New mobile session")], sid).await;
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, make_fs(temp.path()))
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].uuid, sid);
    assert_eq!(rows[0].title, "New mobile session");
    assert_eq!(rows[0].message_count, 0);
}

#[tokio::test]
async fn later_unmarked_title_revokes_mobile_empty_anchor() {
    let sid = Uuid::new_v4();
    let lines = vec![
        mobile_empty_title_line(sid, "New mobile session"),
        custom_title_line(sid, "Renamed session"),
    ];
    let (temp, lingxi_home, cwd) = setup_one(&lines, sid).await;
    assert!(matches!(
        list_recent_sessions(&lingxi_home, &cwd, 5, make_fs(temp.path())).await,
        Err(LoaderError::EmptyDirectory)
    ));
}

#[tokio::test]
async fn custom_title_wins_over_ai_title_and_summary() {
    // custom-title is the highest-precedence source: it beats BOTH an ai-title
    // AND a summary present in the same transcript (logs.ts:69 "user renames
    // always win over AI titles"; getLogDisplayTitle puts customTitle first).
    let sid = Uuid::new_v4();
    let tip = sid;
    let lines = vec![
        summary_line(tip, "AUTO summary should lose"),
        ai_title_line(sid, "AI title should lose"),
        custom_title_line(sid, "My Renamed Session"),
        user_line(tip, sid, "/cwd", "the original first prompt"),
    ];
    let (temp, lingxi_home, cwd) = setup_one(&lines, sid).await;
    let fs = make_fs(temp.path());

    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "My Renamed Session");
}

#[tokio::test]
async fn ai_title_wins_over_summary() {
    // No custom-title → the ai-title is used, in preference to the summary.
    let sid = Uuid::new_v4();
    let tip = sid;
    let lines = vec![
        summary_line(tip, "summary should lose to ai-title"),
        ai_title_line(sid, "Generated Title"),
        user_line(tip, sid, "/cwd", "the first prompt"),
    ];
    let (temp, lingxi_home, cwd) = setup_one(&lines, sid).await;
    let fs = make_fs(temp.path());

    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "Generated Title");
}

#[tokio::test]
async fn plain_session_falls_back_to_first_user_message() {
    // Neither custom-title, ai-title, nor summary → the first-user-message title
    // (UNCHANGED from the pre-summary behavior).
    let sid = Uuid::new_v4();
    let tip = sid;
    let lines = vec![user_line(tip, sid, "/cwd", "what does this function do?")];
    let (temp, lingxi_home, cwd) = setup_one(&lines, sid).await;
    let fs = make_fs(temp.path());

    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "what does this function do?");
}

#[tokio::test]
async fn session_without_a_title_uses_uuid_prefix_fallback() {
    let sid = Uuid::new_v4();
    let line = serde_json::to_string(&serde_json::json!({
        "type": "system",
        "uuid": Uuid::new_v4().to_string(),
        "parentUuid": null,
        "sessionId": sid.to_string(),
        "timestamp": "2026-05-25T12:00:00.000Z",
        "cwd": "/cwd",
        "version": "0.12.0",
        "isSidechain": false,
        "message": {"role": "system", "content": "hook result"},
    }))
    .unwrap();
    let (temp, lingxi_home, cwd) = setup_one(&[line], sid).await;
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, make_fs(temp.path()))
        .await
        .expect("list");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, sid.to_string()[..8]);
    assert_eq!(rows[0].message_count, 0);
}

#[tokio::test]
async fn autonomous_tick_session_uses_claude_display_label() {
    let sid = Uuid::new_v4();
    let lines = vec![user_line(sid, sid, "/cwd", "<tick>12:30 PM</tick>")];
    let (temp, lingxi_home, cwd) = setup_one(&lines, sid).await;
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, make_fs(temp.path()))
        .await
        .expect("list");
    assert_eq!(rows[0].title, "Autonomous session");
}

#[tokio::test]
async fn summary_for_a_non_tip_leaf_is_not_used() {
    // A `summary` keyed by a leafUuid that is NOT the chain tip must NOT surface
    // (TS keys `summaries.get(leafMessage.uuid)` off the TIP only). With no
    // custom/ai title, the picker falls back to the first user message.
    let sid = Uuid::new_v4();
    let tip = sid;
    let other_leaf = Uuid::new_v4(); // not present as a message → never the tip
    let lines = vec![
        summary_line(other_leaf, "summary for some other leaf"),
        user_line(tip, sid, "/cwd", "the real first prompt"),
    ];
    let (temp, lingxi_home, cwd) = setup_one(&lines, sid).await;
    let fs = make_fs(temp.path());

    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].title, "the real first prompt",
        "a summary keyed by a non-tip leaf must not be used"
    );
}

#[tokio::test]
async fn branched_fixture_surfaces_ai_title() {
    // The committed `real_transcript_branched.jsonl` carries BOTH a `summary`
    // (leafUuid = the tip a2) and an `ai-title` ("Parser work"). With no
    // custom-title, the ai-title wins over the summary, exercising the precedence
    // end-to-end on a faithful multi-line transcript (leading summary, interleaved
    // attachment/system/metadata, a sidechain branch).
    let sid: Uuid = "11111111-1111-4111-8111-111111111111".parse().unwrap();
    let fixture = include_str!("fixtures/real_transcript_branched.jsonl");

    let temp = TempDir::new().expect("tempdir");
    let cwd_path = temp.path().join("workproj");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    tokio::fs::create_dir_all(&subdir).await.unwrap();
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), fixture)
        .await
        .unwrap();

    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].title, "Parser work",
        "ai-title wins over the summary"
    );
}

#[tokio::test]
async fn sanitized_dir_collision_filters_rows_by_transcript_cwd() {
    let temp = TempDir::new().expect("tempdir");
    let lingxi_home = temp.path().join("home");
    let cwd_a = "/tmp/collision-a_b".to_string();
    let cwd_b = "/tmp/collision-a-b".to_string();
    assert_eq!(
        project_dir_name(&cwd_a),
        project_dir_name(&cwd_b),
        "fixture must collide in the sanitized project dir"
    );
    let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd_a));
    tokio::fs::create_dir_all(&subdir).await.unwrap();

    let sid_a = Uuid::new_v4();
    tokio::fs::write(
        subdir.join(format!("{sid_a}.jsonl")),
        format!(
            "{}\n",
            serde_json::to_string(&serde_json::json!({
                "type": "user",
                "uuid": sid_a.to_string(),
                "parentUuid": null,
                "sessionId": sid_a.to_string(),
                "timestamp": "2026-05-25T12:00:00.000Z",
                "cwd": cwd_a,
                "version": "0.12.0",
                "isSidechain": false,
                "userType": "external",
                "message": {"role": "user", "content": "from a_b"},
            }))
            .unwrap()
        ),
    )
    .await
    .unwrap();

    let sid_b = Uuid::new_v4();
    tokio::fs::write(
        subdir.join(format!("{sid_b}.jsonl")),
        format!(
            "{}\n",
            serde_json::to_string(&serde_json::json!({
                "type": "user",
                "uuid": sid_b.to_string(),
                "parentUuid": null,
                "sessionId": sid_b.to_string(),
                "timestamp": "2026-05-25T12:00:00.000Z",
                "cwd": cwd_b,
                "version": "0.12.0",
                "isSidechain": false,
                "userType": "external",
                "message": {"role": "user", "content": "from a-b"},
            }))
            .unwrap()
        ),
    )
    .await
    .unwrap();

    let fs = make_fs(temp.path());

    let rows_a = list_recent_sessions(&lingxi_home, &cwd_a, 5, fs.clone())
        .await
        .expect("list a");
    assert_eq!(rows_a.len(), 1);
    assert_eq!(rows_a[0].uuid, sid_a);
    assert_eq!(rows_a[0].title, "from a_b");

    let rows_b = list_recent_sessions(&lingxi_home, &cwd_b, 5, fs)
        .await
        .expect("list b");
    assert_eq!(rows_b.len(), 1);
    assert_eq!(rows_b[0].uuid, sid_b);
    assert_eq!(rows_b[0].title, "from a-b");
}

#[tokio::test]
async fn sorts_by_transcript_activity_not_file_mtime() {
    let temp = TempDir::new().expect("tempdir");
    let cwd_path = temp.path().join("workproj");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    tokio::fs::create_dir_all(&subdir).await.unwrap();

    let old_sid = Uuid::new_v4();
    let old_user = Uuid::new_v4();
    let old_assistant = Uuid::new_v4();
    tokio::fs::write(
        subdir.join(format!("{old_sid}.jsonl")),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&serde_json::json!({
                "type": "user",
                "uuid": old_user.to_string(),
                "parentUuid": null,
                "sessionId": old_sid.to_string(),
                "timestamp": "2026-05-25T12:00:00.000Z",
                "cwd": cwd,
                "version": "0.12.0",
                "isSidechain": false,
                "userType": "external",
                "message": {"role": "user", "content": "older logical activity"},
            }))
            .unwrap(),
            serde_json::to_string(&serde_json::json!({
                "type": "assistant",
                "uuid": old_assistant.to_string(),
                "parentUuid": old_user.to_string(),
                "sessionId": old_sid.to_string(),
                "timestamp": "2026-05-25T12:05:00.000Z",
                "cwd": cwd,
                "version": "0.12.0",
                "isSidechain": false,
                "userType": "external",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "done"}]},
            }))
            .unwrap()
        ),
    )
    .await
    .unwrap();

    let new_sid = Uuid::new_v4();
    let new_user = Uuid::new_v4();
    let new_assistant = Uuid::new_v4();
    tokio::fs::write(
        subdir.join(format!("{new_sid}.jsonl")),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&serde_json::json!({
                "type": "user",
                "uuid": new_user.to_string(),
                "parentUuid": null,
                "sessionId": new_sid.to_string(),
                "timestamp": "2026-05-25T12:01:00.000Z",
                "cwd": cwd,
                "version": "0.12.0",
                "isSidechain": false,
                "userType": "external",
                "message": {"role": "user", "content": "newer logical activity"},
            }))
            .unwrap(),
            serde_json::to_string(&serde_json::json!({
                "type": "assistant",
                "uuid": new_assistant.to_string(),
                "parentUuid": new_user.to_string(),
                "sessionId": new_sid.to_string(),
                "timestamp": "2026-05-25T12:10:00.000Z",
                "cwd": cwd,
                "version": "0.12.0",
                "isSidechain": false,
                "userType": "external",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "done"}]},
            }))
            .unwrap()
        ),
    )
    .await
    .unwrap();

    let old_path = subdir.join(format!("{old_sid}.jsonl"));
    let new_path = subdir.join(format!("{new_sid}.jsonl"));
    let base = SystemTime::now();
    filetime::set_file_mtime(&old_path, filetime::FileTime::from_system_time(base)).unwrap();
    filetime::set_file_mtime(
        &new_path,
        filetime::FileTime::from_system_time(base - Duration::from_secs(300)),
    )
    .unwrap();

    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].uuid, new_sid,
        "newer transcript activity must win even when its file mtime is older"
    );
    assert_eq!(rows[1].uuid, old_sid);
}
