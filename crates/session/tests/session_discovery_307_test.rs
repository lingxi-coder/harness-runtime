//! Current directory links, explicit scopes, and unambiguous UUID lookup.

use lingxi_core::host::FileSystem;
use platform_posix::fs::PosixFileSystem;
use session::jsonl::{
    describe_session_source, find_session_exact_with_diagnostics,
    find_session_in_scope_with_diagnostics, list_recent_sessions, list_recent_sessions_in_scope,
    load_routed_session_across_worktrees_with_path, load_routed_session_from_path_with_source,
    load_session, project_dir_name, record_session_alias, record_session_cwd_alias,
    resolve_job_transcript_raw, resolve_session_path, resolve_session_path_in_scope,
    resolve_session_path_raw, transcript_workspace_path, JobTranscriptResolveOptions, LoaderError,
    SessionDiscoveryScope,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

fn bucket(home: &Path, cwd: &Path) -> PathBuf {
    home.join("projects")
        .join(project_dir_name(&cwd.to_string_lossy()))
}

fn write_session(home: &Path, cwd: &Path, id: Uuid, seconds: u8) -> PathBuf {
    let path = bucket(home, cwd).join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let row = serde_json::json!({
        "type":"user", "uuid":Uuid::new_v4(), "sessionId":id, "parentUuid":null,
        "timestamp":format!("2026-10-21T12:00:{seconds:02}.000Z"),
        "cwd":cwd.to_string_lossy(), "message":{"role":"user","content":format!("message-{id}")}
    });
    let header = serde_json::json!({"type":"fixed-session-mode", "sessionId":id, "sessionMode":"code", "instructionEpoch":0});
    std::fs::write(&path, format!("{header}\n{row}\n")).unwrap();
    path
}

fn fs(root: &Path) -> Arc<dyn FileSystem> {
    Arc::new(PosixFileSystem::new(root.to_path_buf()))
}

fn append_relocated(path: &Path, id: Uuid, cwd: &Path) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type":"relocated", "sessionId":id, "relocatedCwd":cwd.to_string_lossy()
        })
    )
    .unwrap();
    file.sync_all().unwrap();
}

#[tokio::test]
async fn exact_source_description_keeps_the_selected_copy_and_never_falls_back() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let id = Uuid::new_v4();
    let selected = write_session(&home, &a, id, 1);
    let sibling = write_session(&home, &b, id, 9);
    record_session_cwd_alias(&home, &selected, &b)
        .await
        .unwrap();
    append_relocated(&selected, id, &b);
    let row = describe_session_source(
        &home,
        &b.to_string_lossy(),
        SessionDiscoveryScope::Current,
        &selected,
        id,
    )
    .await
    .unwrap();
    assert_eq!(row.metadata.path, selected);
    assert_eq!(row.metadata.workspace_path.as_deref(), b.to_str());
    assert_eq!(row.metadata.mode, session::jsonl::SessionMode::Code);
    assert!(!row.empty_session);
    assert!(describe_session_source(
        &home,
        &b.to_string_lossy(),
        SessionDiscoveryScope::Current,
        &selected,
        Uuid::new_v4()
    )
    .await
    .is_err());
    std::fs::remove_file(&selected).unwrap();
    assert!(sibling.exists());
    assert!(describe_session_source(
        &home,
        &b.to_string_lossy(),
        SessionDiscoveryScope::Current,
        &selected,
        id
    )
    .await
    .is_err());
}

#[tokio::test]
async fn exact_source_description_preserves_a_valid_empty_anchor() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let id = Uuid::new_v4();
    let source = bucket(&home, &a).join(format!("{id}.jsonl"));
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let header = serde_json::json!({"type":"fixed-session-mode", "sessionId":id, "sessionMode":"chat", "instructionEpoch":0});
    let anchor = serde_json::json!({"type":"custom-title", "sessionId":id, "customTitle":"Scheduled", "mobileEmptySession":1, "cwd":a});
    std::fs::write(&source, format!("{header}\n{anchor}\n")).unwrap();
    record_session_cwd_alias(&home, &source, &b).await.unwrap();
    append_relocated(&source, id, &b);
    let described = describe_session_source(
        &home,
        &b.to_string_lossy(),
        SessionDiscoveryScope::Current,
        &source,
        id,
    )
    .await
    .unwrap();
    assert!(described.empty_session);
    assert_eq!(described.metadata.path, source);
    assert_eq!(described.metadata.workspace_path.as_deref(), b.to_str());
    assert_eq!(described.metadata.mode, session::jsonl::SessionMode::Chat);
}

#[tokio::test]
async fn cwd_alias_discovers_only_the_exact_session_with_the_current_marker() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    let c = temp.path().join("C");
    for path in [&a, &b, &c] {
        std::fs::create_dir_all(path).unwrap();
    }
    let [x, y, z] = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    let selected = write_session(&home, &a, x, 1);
    write_session(&home, &a, y, 2);
    write_session(&home, &b, z, 3);
    assert!(record_session_cwd_alias(&home, &selected, &b)
        .await
        .unwrap());
    assert!(!record_session_cwd_alias(&home, &selected, &b)
        .await
        .unwrap());

    // A durable alias written before a failed marker is not authorization.
    let before = list_recent_sessions(&home, &b.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(before.iter().map(|row| row.uuid).collect::<Vec<_>>(), [z]);
    append_relocated(&selected, x, &b);
    let rows = list_recent_sessions(&home, &b.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.uuid == x
        && row.path == selected
        && row.workspace_path.as_deref() == b.to_str()));
    assert!(rows.iter().any(|row| row.uuid == z));
    assert!(
        !rows.iter().any(|row| row.uuid == y),
        "the rest of A's bucket must not appear in B"
    );
    let found = find_session_in_scope_with_diagnostics(
        &home,
        &b.to_string_lossy(),
        SessionDiscoveryScope::Current,
        x,
        fs(temp.path()),
    )
    .await
    .unwrap();
    assert_eq!(found.sessions[0].path, selected);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&selected)
        .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type":"tag", "sessionId":x, "tag":"z".repeat(70_000)
        })
    )
    .unwrap();
    file.sync_all().unwrap();
    let large = list_recent_sessions(&home, &b.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert!(
        large.iter().any(|row| row.uuid == x
            && row.path == selected
            && row.workspace_path.as_deref() == b.to_str()),
        "a large later row must not hide the relocation marker"
    );
    assert_eq!(
        resolve_session_path_in_scope(
            &home,
            &b.to_string_lossy(),
            SessionDiscoveryScope::Current,
            x
        )
        .await
        .unwrap(),
        selected
    );

    record_session_cwd_alias(&home, &selected, &c)
        .await
        .unwrap();
    append_relocated(&selected, x, &c);
    let old = list_recent_sessions(&home, &b.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(old.iter().map(|row| row.uuid).collect::<Vec<_>>(), [z]);
    let new = list_recent_sessions(&home, &c.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(new[0].path, selected);
    assert_eq!(new[0].workspace_path.as_deref(), c.to_str());
}

#[cfg(unix)]
#[tokio::test]
async fn cwd_alias_preserves_canonical_source_through_a_symlinked_home() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let id = Uuid::new_v4();
    let source = write_session(&home, &a, id, 1).canonicalize().unwrap();
    let alias_home = temp.path().join("home-link");
    symlink(&home, &alias_home).unwrap();
    record_session_cwd_alias(&alias_home, &source, &b)
        .await
        .unwrap();
    append_relocated(&source, id, &b);
    let rows = list_recent_sessions(&alias_home, &b.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, source);
}

#[tokio::test]
async fn cwd_alias_does_not_promote_its_original_bucket_during_collision_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let [x, y] = [Uuid::new_v4(), Uuid::new_v4()];
    write_session(&home, &a, y, 1);
    let source = write_session(&home, &a, x, 2);
    record_session_cwd_alias(&home, &source, &b).await.unwrap();
    append_relocated(&source, x, &b);
    let rows = list_recent_sessions(&home, &b.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].uuid, x);
    assert_eq!(rows[0].path, source);
    assert!(!rows.iter().any(|row| row.uuid == y));
}

#[tokio::test]
async fn cwd_and_explicit_directory_aliases_can_share_a_source_bucket() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let b = b.canonicalize().unwrap();
    let [x, y] = [Uuid::new_v4(), Uuid::new_v4()];
    write_session(&home, &a, y, 1);
    let source = write_session(&home, &a, x, 2);
    record_session_cwd_alias(&home, &source, &b).await.unwrap();
    append_relocated(&source, x, &b);
    let before = list_recent_sessions(&home, &b.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(before.iter().map(|row| row.uuid).collect::<Vec<_>>(), [x]);
    record_session_alias(&home, source.parent().unwrap(), &b)
        .await
        .unwrap();
    let after = list_recent_sessions(&home, &b.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(after.len(), 2);
    assert!(after.iter().any(|row| row.uuid == x));
    assert!(after.iter().any(|row| row.uuid == y));
}

#[tokio::test]
async fn cwd_alias_ignores_unterminated_and_escaping_records() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let [x, y] = [Uuid::new_v4(), Uuid::new_v4()];
    let source = write_session(&home, &a, x, 1);
    write_session(&home, &b, y, 2);
    append_relocated(&source, x, &b);
    assert!(
        record_session_cwd_alias(&home, &temp.path().join(format!("{x}.jsonl")), &b)
            .await
            .is_err()
    );
    let relative = source.strip_prefix(home.join("projects")).unwrap();
    let valid = serde_json::json!({"source": relative.to_str().unwrap(), "canonical_home": false});
    let escape = serde_json::json!({"source": format!("../{x}.jsonl"), "canonical_home": false});
    let alias_path =
        bucket(&home, &b.canonicalize().unwrap()).join(session::jsonl::SESSION_CWD_ALIASES_FILE);
    std::fs::create_dir_all(alias_path.parent().unwrap()).unwrap();
    std::fs::write(&alias_path, format!("{escape}\n{valid}")).unwrap();
    let before = list_recent_sessions(&home, &b.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(before.iter().map(|row| row.uuid).collect::<Vec<_>>(), [y]);
    assert!(
        record_session_cwd_alias(&home, &source, &b).await.is_err(),
        "an incomplete tail cannot be appended to"
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(alias_path)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    let after = list_recent_sessions(&home, &b.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert!(after.iter().any(|row| row.uuid == x && row.path == source));
    assert_eq!(after.len(), 2);
}

#[tokio::test]
async fn selected_source_path_reads_one_full_parser_snapshot_without_uuid_rediscovery() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    let unrelated = temp.path().join("outside");
    let id = Uuid::new_v4();
    let selected = write_session(&home, &a, id, 1);
    write_session(&home, &b, id, 9);
    assert!(matches!(
        resolve_session_path(&home, &unrelated.to_string_lossy(), id).await,
        Err(LoaderError::SessionNotFound { .. })
    ));
    let source = std::fs::read_to_string(&selected)
        .unwrap()
        .replace("\"type\":\"user\"", "\"type\": \"user\"");
    std::fs::write(&selected, &source).unwrap();
    let (path, captured, loaded) =
        load_routed_session_from_path_with_source(&selected, id, fs(temp.path()))
            .await
            .unwrap();
    assert_eq!(path, selected);
    assert_eq!(captured, source);
    assert_eq!(loaded.messages_in_order.len(), 1);
    assert!(matches!(
        load_routed_session_from_path_with_source(&selected, Uuid::new_v4(), fs(temp.path())).await,
        Err(LoaderError::SessionNotFound { .. })
    ));
}

#[tokio::test]
async fn current_links_are_directional_direct_and_open_the_selected_source() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    let c = temp.path().join("C");
    let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    let a_path = write_session(&home, &a, ids[0], 1);
    write_session(&home, &b, ids[1], 2);
    write_session(&home, &c, ids[2], 3);
    record_session_alias(&home, a_path.parent().unwrap(), &b)
        .await
        .unwrap();
    record_session_alias(&home, &bucket(&home, &b), &c)
        .await
        .unwrap();

    let current = list_recent_sessions(&home, &b.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(
        current.iter().map(|row| row.uuid).collect::<Vec<_>>(),
        [ids[1], ids[0]]
    );
    assert_eq!(current[1].path, a_path);
    assert_eq!(current[1].workspace_path.as_deref(), a.to_str());
    let from_a = list_recent_sessions(&home, &a.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(from_a.len(), 1, "the link must not become reciprocal");
    let from_c = list_recent_sessions(&home, &c.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(
        from_c.iter().map(|row| row.uuid).collect::<Vec<_>>(),
        [ids[2], ids[1]],
        "links must not be traversed transitively"
    );

    let (selected, loaded) = load_routed_session_across_worktrees_with_path(
        &home,
        &b.to_string_lossy(),
        ids[0],
        fs(temp.path()),
    )
    .await
    .unwrap();
    assert_eq!(selected, a_path);
    assert_eq!(
        transcript_workspace_path(&selected, &loaded).as_deref(),
        a.to_str()
    );
    assert_eq!(
        load_session(&home, &b.to_string_lossy(), ids[0], fs(temp.path()))
            .await
            .unwrap()
            .len(),
        1
    );
    let exact =
        find_session_exact_with_diagnostics(&home, &b.to_string_lossy(), ids[0], fs(temp.path()))
            .await
            .unwrap();
    assert_eq!(exact.sessions[0].path, a_path);
}

#[tokio::test]
async fn current_scope_isolates_sanitized_collisions_and_all_is_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("same-path");
    let b = temp.path().join("same_path");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let a_id = Uuid::new_v4();
    let b_id = Uuid::new_v4();
    assert_eq!(bucket(&home, &a), bucket(&home, &b));
    write_session(&home, &a, a_id, 1);
    write_session(&home, &b, b_id, 2);
    let rows = list_recent_sessions(&home, &a.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(rows.iter().map(|row| row.uuid).collect::<Vec<_>>(), [a_id]);
    let all = list_recent_sessions_in_scope(
        &home,
        &a.to_string_lossy(),
        SessionDiscoveryScope::All,
        20,
        fs(temp.path()),
    )
    .await
    .unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn uuid_lookup_prefers_local_then_requires_one_global_message_transcript() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    let c = temp.path().join("C");
    let id = Uuid::new_v4();
    let a_path = write_session(&home, &a, id, 1);
    assert_eq!(
        resolve_session_path(&home, &c.to_string_lossy(), id)
            .await
            .unwrap(),
        a_path
    );
    write_session(&home, &b, id, 9);
    let ambiguous = resolve_job_transcript_raw(
        &home,
        &c.to_string_lossy(),
        &id.to_string(),
        &JobTranscriptResolveOptions::default(),
    )
    .await
    .unwrap();
    assert!(!ambiguous.has_messages);
    assert_eq!(
        ambiguous.path,
        bucket(&home, &c).join(format!("{id}.jsonl"))
    );
    assert_eq!(
        resolve_session_path(&home, &a.to_string_lossy(), id)
            .await
            .unwrap(),
        a_path,
        "an unrelated newer copy cannot replace the current session"
    );
    assert!(matches!(
        resolve_session_path_in_scope(
            &home,
            &c.to_string_lossy(),
            SessionDiscoveryScope::Current,
            id
        )
        .await,
        Err(LoaderError::SessionNotFound { .. })
    ));
}

#[tokio::test]
async fn job_uuid_probe_uses_native_raw_type_markers() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let a = temp.path().join("A");
    let c = temp.path().join("C");
    let id = Uuid::new_v4();
    let path = bucket(&home, &a).join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let header = serde_json::json!({"type":"fixed-session-mode", "sessionId":id, "sessionMode":"code", "instructionEpoch":0});
    std::fs::write(&path, format!("{header}\n{{\"type\": \"user\"}}\n")).unwrap();
    let unresolved = resolve_job_transcript_raw(
        &home,
        &c.to_string_lossy(),
        &id.to_string(),
        &JobTranscriptResolveOptions::default(),
    )
    .await
    .unwrap();
    assert!(!unresolved.has_messages);
    assert_eq!(
        unresolved.path,
        bucket(&home, &c).join(format!("{id}.jsonl"))
    );
    std::fs::write(
        &path,
        format!("{header}\nnot parsed JSON: \"type\":\"user\"\n"),
    )
    .unwrap();
    assert_eq!(
        resolve_job_transcript_raw(
            &home,
            &c.to_string_lossy(),
            &id.to_string(),
            &JobTranscriptResolveOptions::default()
        )
        .await
        .unwrap()
        .path,
        path
    );
    assert!(
        matches!(
            resolve_session_path(&home, &c.to_string_lossy(), id).await,
            Err(LoaderError::SessionNotFound { .. })
        ),
        "CLI must full-parse the unique global candidate after its raw probe"
    );
}

#[tokio::test]
async fn malformed_linked_paths_do_not_hide_the_current_catalog() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("current");
    let id = Uuid::new_v4();
    let transcript = write_session(&home, &cwd, id, 1);
    std::fs::write(
        transcript.parent().unwrap().join(".session-aliases"),
        "\0\n\r\nmissing\n",
    )
    .unwrap();
    let rows = list_recent_sessions(&home, &cwd.to_string_lossy(), 20, fs(temp.path()))
        .await
        .unwrap();
    assert_eq!(rows.iter().map(|row| row.uuid).collect::<Vec<_>>(), [id]);
}

#[tokio::test]
async fn job_raw_uuid_case_and_false_scoped_candidates_remain_observable() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("current");
    let upper = "ABCDEFAB-0000-4000-8000-000000000001";
    let path = bucket(&home, &cwd).join(format!("{upper}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{\"type\":\"summary\"}\n").unwrap();
    let options = JobTranscriptResolveOptions::default();
    let empty = resolve_job_transcript_raw(&home, &cwd.to_string_lossy(), upper, &options)
        .await
        .unwrap();
    assert_eq!(empty.path, path);
    assert!(!empty.has_messages);
    std::fs::write(&path, "bad JSON \"type\":\"user\"\n").unwrap();
    let actual = resolve_job_transcript_raw(&home, &cwd.to_string_lossy(), upper, &options)
        .await
        .unwrap();
    assert_eq!(
        actual.path.file_name().unwrap().to_str().unwrap(),
        format!("{upper}.jsonl")
    );
    assert!(actual.has_messages);
    let invalid = resolve_job_transcript_raw(&home, &cwd.to_string_lossy(), "not-a-uuid", &options)
        .await
        .unwrap();
    assert_eq!(invalid.path.file_name().unwrap(), "invalid-resume-id.jsonl");
    assert!(!invalid.has_messages);
}

#[tokio::test]
async fn cli_full_parser_accepts_spaced_types_before_global_message_probing() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("current");
    let id = Uuid::new_v4();
    let path = write_session(&home, &cwd, id, 1);
    write_session(&home, &temp.path().join("unrelated"), id, 9);
    let compact = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        compact.replace("\"type\":\"user\"", "\"type\": \"user\""),
    )
    .unwrap();
    assert_eq!(
        load_session(&home, &cwd.to_string_lossy(), id, fs(temp.path()))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        resolve_session_path(&home, &cwd.to_string_lossy(), id)
            .await
            .unwrap(),
        path
    );
}

#[tokio::test]
async fn cli_raw_uuid_lookup_keeps_the_original_filename_case() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("current");
    let raw = "ABCDEFAB-0000-4000-8000-000000000001";
    let id = Uuid::parse_str(raw).unwrap();
    let lower = write_session(&home, &cwd, id, 1);
    let upper = lower.with_file_name(format!("{raw}.jsonl"));
    std::fs::rename(&lower, &upper).unwrap();
    assert_eq!(
        resolve_session_path_raw(&home, &cwd.to_string_lossy(), raw)
            .await
            .unwrap(),
        upper
    );
}
