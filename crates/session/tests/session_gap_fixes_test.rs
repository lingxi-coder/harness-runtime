//! Integration tests for session resume / picker gap fixes (gaps #1–#5).
//!
//! Covers:
//!  - Gap #1: `last-prompt` explicit tip override in `find_tip`
//!  - Gap #2: `sessionKind` daemon / daemon-worker filter in `collect_dir`
//!  - Gap #3: SDK-entrypoint (`sdk-cli`/`sdk-ts`/`sdk-py`) filter in `collect_dir`
//!  - Gap #4: `/loop` session filter in `collect_dir`
//!  - Gap #5 (selected): `route_lines` populates new side-maps
//!    (`tags`, `agent_names`, `agent_settings`, `modes`, `permission_modes`,
//!    `worktree_states`, `last_prompt_leaf_uuid`/`last_prompt_explicit`)

use lingxi_core::host::FileSystem;
use platform_posix::fs::PosixFileSystem;
use session::jsonl::loader::{find_tip, list_recent_sessions};
use session::jsonl::project_dir_name;
use session::jsonl::reader::{route_lines, LoadedTranscript};
use session::jsonl::LoaderError;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;
use uuid::Uuid;

// ── helpers ──────────────────────────────────────────────────────────────────

fn make_fs(root: &std::path::Path) -> Arc<dyn FileSystem> {
    Arc::new(PosixFileSystem::new(root.to_path_buf()))
}

/// Set up `<lingxi_home>/projects/<sanitize(cwd)>/` and return
/// `(tempdir, lingxi_home, cwd, project_subdir)`.
fn setup() -> (TempDir, std::path::PathBuf, String, std::path::PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let cwd = temp.path().join("proj").to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let project_subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    std::fs::create_dir_all(&project_subdir).expect("mkdir");
    (temp, lingxi_home, cwd, project_subdir)
}

/// Write a minimal JSONL session file to `dir/<uuid>.jsonl` with `extra_fields`
/// merged into the first (and only) line. Returns the uuid. The file's mtime is
/// set to `mtime`.
fn write_session_with_extras(
    dir: &std::path::Path,
    cwd: &str,
    extra: serde_json::Value,
    mtime: SystemTime,
) -> Uuid {
    let uuid = Uuid::new_v4();
    let mut line = serde_json::json!({
        "type": "user",
        "uuid": uuid.to_string(),
        "parentUuid": null,
        "sessionId": uuid.to_string(),
        "timestamp": "2026-05-25T12:00:00.000Z",
        "cwd": cwd,
        "version": "0.12.0",
        "isSidechain": false,
        "userType": "external",
        "message": {"role": "user", "content": "hello"},
    });
    if let (Some(obj), Some(extra_obj)) = (line.as_object_mut(), extra.as_object()) {
        for (k, v) in extra_obj {
            obj.insert(k.clone(), v.clone());
        }
    }
    let bytes = format!("{}\n", serde_json::to_string(&line).unwrap());
    let path = dir.join(format!("{uuid}.jsonl"));
    std::fs::write(&path, bytes).unwrap();
    filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(mtime)).unwrap();
    uuid
}

/// Write a plain session (no extra fields) with `isSidechain:false`.
fn write_plain_session(dir: &std::path::Path, cwd: &str, mtime: SystemTime) -> Uuid {
    write_session_with_extras(dir, cwd, serde_json::json!({}), mtime)
}

// ── Gap #2: sessionKind daemon filter ─────────────────────────────────────────

#[tokio::test]
async fn daemon_session_is_filtered_from_resume_picker() {
    let (temp, lingxi_home, cwd, dir) = setup();
    let base = SystemTime::now();

    // One normal session and one daemon session (newer mtime).
    let normal = write_plain_session(&dir, &cwd, base);
    let _daemon = write_session_with_extras(
        &dir,
        &cwd,
        serde_json::json!({"sessionKind": "daemon"}),
        base + Duration::from_secs(1),
    );

    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");

    assert_eq!(rows.len(), 1, "daemon session must be filtered out");
    assert_eq!(rows[0].uuid, normal);
}

#[tokio::test]
async fn daemon_worker_session_is_filtered_from_resume_picker() {
    let (temp, lingxi_home, cwd, dir) = setup();
    let base = SystemTime::now();

    let normal = write_plain_session(&dir, &cwd, base);
    let _daemon_wk = write_session_with_extras(
        &dir,
        &cwd,
        serde_json::json!({"sessionKind": "daemon-worker"}),
        base + Duration::from_secs(1),
    );

    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");

    assert_eq!(rows.len(), 1, "daemon-worker session must be filtered out");
    assert_eq!(rows[0].uuid, normal);
}

#[tokio::test]
async fn non_daemon_session_kind_is_not_filtered() {
    // sessionKind = "interactive" (or any non-daemon value) must NOT be filtered.
    let (temp, lingxi_home, cwd, dir) = setup();
    let base = SystemTime::now();

    let keep = write_session_with_extras(
        &dir,
        &cwd,
        serde_json::json!({"sessionKind": "interactive"}),
        base,
    );

    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");

    assert_eq!(rows.len(), 1, "non-daemon sessionKind must not be filtered");
    assert_eq!(rows[0].uuid, keep);
}

// ── Gap #3: SDK entrypoint filter ────────────────────────────────────────────

#[tokio::test]
async fn sdk_cli_session_is_filtered_from_resume_picker() {
    let (temp, lingxi_home, cwd, dir) = setup();
    let base = SystemTime::now();

    let normal = write_plain_session(&dir, &cwd, base);
    let _sdk = write_session_with_extras(
        &dir,
        &cwd,
        serde_json::json!({"entrypoint": "sdk-cli"}),
        base + Duration::from_secs(1),
    );

    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");

    assert_eq!(rows.len(), 1, "sdk-cli session must be filtered out");
    assert_eq!(rows[0].uuid, normal);
}

#[tokio::test]
async fn sdk_ts_session_is_filtered_from_resume_picker() {
    let (temp, lingxi_home, cwd, dir) = setup();
    let base = SystemTime::now();

    let normal = write_plain_session(&dir, &cwd, base);
    let _sdk = write_session_with_extras(
        &dir,
        &cwd,
        serde_json::json!({"entrypoint": "sdk-ts"}),
        base + Duration::from_secs(1),
    );

    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");

    assert_eq!(rows.len(), 1, "sdk-ts session must be filtered out");
    assert_eq!(rows[0].uuid, normal);
}

#[tokio::test]
async fn sdk_py_session_is_filtered_from_resume_picker() {
    let (temp, lingxi_home, cwd, dir) = setup();
    let base = SystemTime::now();

    let normal = write_plain_session(&dir, &cwd, base);
    let _sdk = write_session_with_extras(
        &dir,
        &cwd,
        serde_json::json!({"entrypoint": "sdk-py"}),
        base + Duration::from_secs(1),
    );

    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");

    assert_eq!(rows.len(), 1, "sdk-py session must be filtered out");
    assert_eq!(rows[0].uuid, normal);
}

#[tokio::test]
async fn cli_entrypoint_session_is_not_filtered() {
    // entrypoint = "cli" is the normal interactive session; must NOT be filtered.
    let (temp, lingxi_home, cwd, dir) = setup();
    let base = SystemTime::now();

    let keep =
        write_session_with_extras(&dir, &cwd, serde_json::json!({"entrypoint": "cli"}), base);

    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");

    assert_eq!(rows.len(), 1, "cli entrypoint must not be filtered");
    assert_eq!(rows[0].uuid, keep);
}

// ── Gap #4: /loop session filter ─────────────────────────────────────────────

#[tokio::test]
async fn loop_session_is_filtered_from_resume_picker() {
    // The /loop tag in the message content triggers the filter.
    // Binary detection: first line raw string contains
    // "<command-name>/loop</command-name>".
    let (temp, lingxi_home, cwd, dir) = setup();
    let base = SystemTime::now();

    let normal = write_plain_session(&dir, &cwd, base);

    // Write a session whose first user message content contains the /loop tag.
    let loop_uuid = Uuid::new_v4();
    let loop_line = serde_json::json!({
        "type": "user",
        "uuid": loop_uuid.to_string(),
        "parentUuid": null,
        "sessionId": loop_uuid.to_string(),
        "timestamp": "2026-05-25T12:00:00.000Z",
        "cwd": cwd,
        "version": "0.12.0",
        "isSidechain": false,
        "userType": "external",
        "message": {
            "role": "user",
            "content": "<command-name>/loop</command-name> echo hello"
        },
    });
    let loop_path = dir.join(format!("{loop_uuid}.jsonl"));
    std::fs::write(
        &loop_path,
        format!("{}\n", serde_json::to_string(&loop_line).unwrap()),
    )
    .unwrap();
    filetime::set_file_mtime(
        &loop_path,
        filetime::FileTime::from_system_time(base + Duration::from_secs(1)),
    )
    .unwrap();

    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");

    assert_eq!(rows.len(), 1, "/loop session must be filtered out");
    assert_eq!(rows[0].uuid, normal);
}

#[tokio::test]
async fn session_with_loop_text_in_non_content_context_is_not_filtered() {
    // A session whose user message is a plain string that happens to contain
    // the word "loop" but NOT the full XML tag must NOT be filtered.
    let (temp, lingxi_home, cwd, dir) = setup();
    let base = SystemTime::now();

    let keep_uuid = Uuid::new_v4();
    let line = serde_json::json!({
        "type": "user",
        "uuid": keep_uuid.to_string(),
        "parentUuid": null,
        "sessionId": keep_uuid.to_string(),
        "timestamp": "2026-05-25T12:00:00.000Z",
        "cwd": cwd,
        "version": "0.12.0",
        "isSidechain": false,
        "userType": "external",
        "message": {"role": "user", "content": "please loop over the files"},
    });
    let path = dir.join(format!("{keep_uuid}.jsonl"));
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&line).unwrap()),
    )
    .unwrap();
    filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(base)).unwrap();

    let fs = make_fs(temp.path());
    let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
        .await
        .expect("list");

    assert_eq!(
        rows.len(),
        1,
        "plain 'loop' text must not trigger the filter"
    );
    assert_eq!(rows[0].uuid, keep_uuid);
}

// ── Gap #1: last-prompt explicit tip override in find_tip ───────────────────

/// Build a minimal `LoadedTranscript` for `find_tip` unit testing.
/// `messages` = list of (uuid, parent_uuid, type, timestamp, is_sidechain).
fn make_transcript(messages: &[(&str, Option<&str>, &str, &str, bool)]) -> LoadedTranscript {
    let mut t = LoadedTranscript::default();
    for &(uuid, parent, ty, ts, is_sc) in messages {
        let msg = serde_json::json!({
            "type": ty,
            "uuid": uuid,
            "parentUuid": parent,
            "sessionId": "sess-1",
            "timestamp": ts,
            "cwd": "/p",
            "version": "0.12.0",
            "isSidechain": is_sc,
            "userType": "external",
            "message": {"role": ty, "content": "hi"},
        });
        let m: session::jsonl::JsonlMessage = serde_json::from_value(msg).unwrap();
        t.by_uuid.insert(uuid.to_string(), m.clone());
        t.messages_in_order.push(m);
    }
    t
}

#[test]
fn find_tip_with_explicit_last_prompt_overrides_timestamp_selection() {
    // Two branches: branch-A (newer timestamp) and branch-B (older timestamp).
    // Without last-prompt, branch-A would be selected. With an explicit
    // last-prompt pointing at branch-B's leaf, branch-B must win.
    //
    //  root → a1 (ts=12:00:02, newer — would normally be tip)
    //       → b1 (ts=12:00:01, older)
    let mut t = make_transcript(&[
        ("root", None, "user", "2026-05-25T12:00:00.000Z", false),
        (
            "a1",
            Some("root"),
            "user",
            "2026-05-25T12:00:02.000Z",
            false,
        ),
        (
            "b1",
            Some("root"),
            "user",
            "2026-05-25T12:00:01.000Z",
            false,
        ),
    ]);

    // Without last-prompt, a1 wins (newer ts).
    let tip = find_tip(&t, "test").expect("tip");
    assert_eq!(tip.uuid, "a1", "without last-prompt, newest timestamp wins");

    // Set explicit last-prompt pointing at b1.
    t.last_prompt_leaf_uuid = Some("b1".to_string());
    t.last_prompt_explicit = true;

    let tip = find_tip(&t, "test").expect("tip with last-prompt");
    assert_eq!(
        tip.uuid, "b1",
        "explicit last-prompt must override timestamp-based selection"
    );
}

#[test]
fn find_tip_non_explicit_last_prompt_does_not_override() {
    // `last-prompt` entry with `explicit===false` must NOT override.
    // The TS condition is `L && O && n.has(O) && !n.get(O)?.isSidechain`
    // where L = last_prompt_explicit. When L is false, the timestamp race runs.
    let mut t = make_transcript(&[
        ("root", None, "user", "2026-05-25T12:00:00.000Z", false),
        (
            "a1",
            Some("root"),
            "user",
            "2026-05-25T12:00:02.000Z",
            false,
        ),
        (
            "b1",
            Some("root"),
            "user",
            "2026-05-25T12:00:01.000Z",
            false,
        ),
    ]);

    // last_prompt_explicit = false → does NOT override.
    t.last_prompt_leaf_uuid = Some("b1".to_string());
    t.last_prompt_explicit = false;

    let tip = find_tip(&t, "test").expect("tip");
    assert_eq!(
        tip.uuid, "a1",
        "non-explicit last-prompt must not override; timestamp wins"
    );
}

#[test]
fn find_tip_last_prompt_pointing_at_sidechain_falls_through_to_timestamp() {
    // If the explicit last-prompt points at a sidechain leaf, `V` in the binary
    // is false (isSidechain check), so timestamp selection runs instead.
    let mut t = make_transcript(&[
        ("root", None, "user", "2026-05-25T12:00:00.000Z", false),
        (
            "a1",
            Some("root"),
            "user",
            "2026-05-25T12:00:02.000Z",
            false,
        ),
        (
            "sc1",
            Some("root"),
            "user",
            "2026-05-25T12:00:05.000Z",
            true,
        ), // sidechain, newer
    ]);

    // Explicit last-prompt at sc1 — but sc1 is a sidechain → should fall through.
    t.last_prompt_leaf_uuid = Some("sc1".to_string());
    t.last_prompt_explicit = true;

    let tip = find_tip(&t, "test").expect("tip");
    assert_eq!(
        tip.uuid, "a1",
        "last-prompt pointing at a sidechain leaf must fall through to timestamp selection"
    );
}

#[test]
fn find_tip_last_prompt_pointing_at_unknown_uuid_falls_through() {
    // If the explicit last-prompt points at a uuid not in by_uuid, skip it.
    let mut t = make_transcript(&[
        ("root", None, "user", "2026-05-25T12:00:00.000Z", false),
        (
            "a1",
            Some("root"),
            "user",
            "2026-05-25T12:00:02.000Z",
            false,
        ),
    ]);

    t.last_prompt_leaf_uuid = Some("nonexistent-uuid".to_string());
    t.last_prompt_explicit = true;

    let tip = find_tip(&t, "test").expect("tip");
    assert_eq!(
        tip.uuid, "a1",
        "last-prompt pointing at unknown uuid must fall through"
    );
}

// ── Gap #1: route_lines parses last-prompt entries ───────────────────────────

#[test]
fn route_lines_parses_last_prompt_explicit_true() {
    // Binary: `{type:"last-prompt", leafUuid, explicit:true}` sets L=true, O=uuid.
    let content = concat!(
        r#"{"type":"last-prompt","leafUuid":"leaf-1","explicit":true,"sessionId":"s1"}"#,
        "\n",
        r#"{"type":"user","uuid":"leaf-1","parentUuid":null,"sessionId":"s1","timestamp":"2026-05-25T12:00:00.000Z","cwd":"/p","version":"0.12.0","isSidechain":false,"userType":"external","message":{"role":"user","content":"hi"}}"#,
        "\n",
    );
    let t = route_lines(content);
    assert_eq!(
        t.last_prompt_leaf_uuid.as_deref(),
        Some("leaf-1"),
        "last-prompt leafUuid must be captured"
    );
    assert!(t.last_prompt_explicit, "explicit:true must be captured");
}

#[test]
fn route_lines_parses_last_prompt_explicit_false() {
    let content = concat!(
        r#"{"type":"last-prompt","leafUuid":"leaf-2","explicit":false,"sessionId":"s1"}"#,
        "\n",
    );
    let t = route_lines(content);
    assert_eq!(t.last_prompt_leaf_uuid.as_deref(), Some("leaf-2"));
    assert!(
        !t.last_prompt_explicit,
        "explicit:false → last_prompt_explicit stays false"
    );
}

#[test]
fn route_lines_last_prompt_accumulation_same_uuid() {
    // Binary TS: `L = N.explicit===true || L && N.leafUuid===O`
    // Two entries with the same leafUuid: first explicit=false, second explicit=false.
    // L stays false (never true). Only leafUuid is updated.
    let content = concat!(
        r#"{"type":"last-prompt","leafUuid":"leaf-a","explicit":false}"#,
        "\n",
        r#"{"type":"last-prompt","leafUuid":"leaf-a","explicit":false}"#,
        "\n",
    );
    let t = route_lines(content);
    assert_eq!(t.last_prompt_leaf_uuid.as_deref(), Some("leaf-a"));
    assert!(!t.last_prompt_explicit);
}

#[test]
fn route_lines_last_prompt_explicit_sticks_to_same_uuid() {
    // First entry: explicit=true, leafUuid=A → L=true, O=A.
    // Second entry: explicit=false, leafUuid=A → L = false || (true && A==A) = true. Still true.
    let content = concat!(
        r#"{"type":"last-prompt","leafUuid":"leaf-a","explicit":true}"#,
        "\n",
        r#"{"type":"last-prompt","leafUuid":"leaf-a","explicit":false}"#,
        "\n",
    );
    let t = route_lines(content);
    assert_eq!(t.last_prompt_leaf_uuid.as_deref(), Some("leaf-a"));
    assert!(
        t.last_prompt_explicit,
        "explicit carries over when same leafUuid"
    );
}

#[test]
fn route_lines_last_prompt_explicit_resets_on_new_uuid() {
    // First entry: explicit=true, leafUuid=A → L=true, O=A.
    // Second entry: explicit=false, leafUuid=B → L = false || (true && B==A) = false.
    // A new leafUuid breaks the carry.
    let content = concat!(
        r#"{"type":"last-prompt","leafUuid":"leaf-a","explicit":true}"#,
        "\n",
        r#"{"type":"last-prompt","leafUuid":"leaf-b","explicit":false}"#,
        "\n",
    );
    let t = route_lines(content);
    assert_eq!(t.last_prompt_leaf_uuid.as_deref(), Some("leaf-b"));
    assert!(
        !t.last_prompt_explicit,
        "explicit resets when leafUuid changes"
    );
}

#[test]
fn route_lines_last_prompt_without_leaf_uuid_is_ignored() {
    // A last-prompt line without a leafUuid field is ignored (no update).
    let content = concat!(
        r#"{"type":"last-prompt","sessionId":"s1","explicit":true}"#,
        "\n",
    );
    let t = route_lines(content);
    assert!(
        t.last_prompt_leaf_uuid.is_none(),
        "last-prompt without leafUuid must not update the map"
    );
    assert!(!t.last_prompt_explicit);
}

// ── Gap #5: route_lines populates new side-maps ───────────────────────────────

#[test]
fn route_lines_parses_tag_entries() {
    // Binary 2.1.215: `a.set(J.sessionId, J.tag)` — LAST-WRITE-WINS single value
    // per session (a re-tag overwrites; an earlier build accumulated an array).
    let content = concat!(
        r#"{"type":"tag","sessionId":"s1","tag":"important"}"#,
        "\n",
        r#"{"type":"tag","sessionId":"s1","tag":"work"}"#,
        "\n",
        r#"{"type":"tag","sessionId":"s2","tag":"personal"}"#,
        "\n",
    );
    let t = route_lines(content);
    // s1 re-tagged: only the LATEST ("work") is kept.
    assert_eq!(t.tags.get("s1").map(String::as_str), Some("work"));
    assert_eq!(t.tags.get("s2").map(String::as_str), Some("personal"));
}

#[test]
fn route_lines_parses_agent_name_entries() {
    // Binary v2.1.193 @211658974: `agentNames.set(N.sessionId, N.agentName)` —
    // keyed by sessionId (the write side carries no agentId).
    let content = concat!(
        r#"{"type":"agent-name","sessionId":"s1","agentName":"Alice"}"#,
        "\n",
        r#"{"type":"agent-name","sessionId":"s2","agentName":"Bob"}"#,
        "\n",
    );
    let t = route_lines(content);
    assert_eq!(t.agent_names.get("s1").map(String::as_str), Some("Alice"));
    assert_eq!(t.agent_names.get("s2").map(String::as_str), Some("Bob"));
}

#[test]
fn route_lines_parses_agent_setting_entries() {
    // Binary v2.1.193 @211659124: `agentSettings.set(N.sessionId, N.agentSetting)`
    // — keyed by sessionId, stores the inner `agentSetting` payload.
    let content = concat!(
        r#"{"type":"agent-setting","sessionId":"s1","agentSetting":{"model":"opus","value":42}}"#,
        "\n",
    );
    let t = route_lines(content);
    let setting = t.agent_settings.get("s1").expect("agent setting");
    assert_eq!(setting.get("model").and_then(|v| v.as_str()), Some("opus"));
    assert_eq!(setting.get("value").and_then(|v| v.as_i64()), Some(42));
}

#[test]
fn route_lines_parses_mode_entries() {
    // Binary: `modes.set(N.sessionId, N.mode)`.
    let content = concat!(
        r#"{"type":"mode","sessionId":"s1","mode":"auto"}"#,
        "\n",
        r#"{"type":"mode","sessionId":"s1","mode":"plan"}"#, // last-write-wins
        "\n",
    );
    let t = route_lines(content);
    assert_eq!(t.modes.get("s1").map(String::as_str), Some("plan"));
}

#[test]
fn route_lines_parses_permission_mode_entries() {
    // Binary: `permissionModes.set(N.sessionId, N.permissionMode)`.
    let content = concat!(
        r#"{"type":"permission-mode","sessionId":"s1","permissionMode":"acceptEdits"}"#,
        "\n",
    );
    let t = route_lines(content);
    assert_eq!(
        t.permission_modes.get("s1").map(String::as_str),
        Some("acceptEdits")
    );
}

#[test]
fn route_lines_parses_worktree_state_entries() {
    // Binary (2.1.212 `Yle`): `worktreeStates.set(N.sessionId, N.worktreeSession)`
    // — keyed by `sessionId`, storing the inner `worktreeSession` payload.
    let content = concat!(
        r#"{"type":"worktree-state","sessionId":"s1","worktreeSession":{"worktreePath":"/repo/.lingxi/worktrees/feat","worktreeBranch":"worktree-feat"}}"#,
        "\n",
    );
    let t = route_lines(content);
    let ws = t.worktree_states.get("s1").expect("worktree state");
    assert_eq!(
        ws.get("worktreePath").and_then(|v| v.as_str()),
        Some("/repo/.lingxi/worktrees/feat")
    );
}

#[test]
fn route_lines_worktree_state_null_supersedes_active() {
    // The ExitWorktree clear record persists `worktreeSession: null`; last-write
    // -wins means it supersedes an earlier active session for the same sessionId.
    let content = concat!(
        r#"{"type":"worktree-state","sessionId":"s1","worktreeSession":{"worktreePath":"/repo/.lingxi/worktrees/feat","worktreeBranch":"worktree-feat"}}"#,
        "\n",
        r#"{"type":"worktree-state","sessionId":"s1","worktreeSession":null}"#,
        "\n",
    );
    let t = route_lines(content);
    let ws = t.worktree_states.get("s1").expect("worktree state present");
    assert!(ws.is_null(), "later null record supersedes the active one");
}

#[test]
fn route_lines_all_side_maps_empty_by_default() {
    // A plain transcript with no metadata entries → all new side-maps are empty.
    let content = r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s1","timestamp":"2026-05-25T12:00:00.000Z","cwd":"/p","version":"0.12.0","isSidechain":false,"userType":"external","message":{"role":"user","content":"hi"}}"#;
    let t = route_lines(&format!("{content}\n"));
    assert!(t.tags.is_empty());
    assert!(t.agent_names.is_empty());
    assert!(t.agent_settings.is_empty());
    assert!(t.modes.is_empty());
    assert!(t.permission_modes.is_empty());
    assert!(t.worktree_states.is_empty());
    assert!(t.last_prompt_leaf_uuid.is_none());
    assert!(!t.last_prompt_explicit);
}

// ── All-filtered → EmptyDirectory ────────────────────────────────────────────

#[tokio::test]
async fn all_sessions_filtered_returns_empty_directory() {
    // When all sessions are filtered out (daemon + sdk-cli + sdk-ts + sdk-py +
    // loop), the result is EmptyDirectory (same surface as a dir with no .jsonl).
    let (temp, lingxi_home, cwd, dir) = setup();
    let base = SystemTime::now();

    write_session_with_extras(
        &dir,
        &cwd,
        serde_json::json!({"sessionKind": "daemon"}),
        base,
    );
    write_session_with_extras(
        &dir,
        &cwd,
        serde_json::json!({"entrypoint": "sdk-cli"}),
        base + Duration::from_secs(1),
    );

    let fs = make_fs(temp.path());
    match list_recent_sessions(&lingxi_home, &cwd, 5, fs).await {
        Err(LoaderError::EmptyDirectory) => {}
        other => panic!("expected EmptyDirectory when all sessions filtered, got {other:?}"),
    }
}
