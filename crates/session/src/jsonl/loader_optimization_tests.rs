use super::*;
use platform_posix::fs::PosixFileSystem;
use serde_json::json;

fn row(id: &str, millis: i64, sidechain: bool) -> JsonlMessage {
    let source = json!({
        "type": "user", "uuid": id, "parentUuid": null,
        "sessionId": "11111111-2222-4333-8444-555555555555",
        "timestamp": chrono::DateTime::from_timestamp_millis(millis).unwrap().to_rfc3339(),
        "cwd": "/workspace", "version": "0.12.0", "isSidechain": sidechain,
        "message": {"role": "user", "content": "hello"}
    });
    route_lines(&source.to_string())
        .messages_in_order
        .pop()
        .unwrap()
}

fn bytes(row: &JsonlMessage) -> Vec<u8> {
    crate::jsonl::exact_json::native_message_bytes(row).unwrap()
}

#[test]
fn snapshot_indexes_borrow_latest_payload_and_mutate_only_on_write() {
    let mut first = row("duplicate", 1_000, false);
    first.message = Utf16JsonProjection::parse(r#"{"z":"old\ud800","a":"first"}"#).unwrap();
    let middle = row("middle", 2_000, true);
    let mut latest = row("duplicate", 3_000, false);
    latest.message = Utf16JsonProjection::parse(r#"{"z":"new\udfff","a":"latest"}"#).unwrap();
    let expected = [bytes(&first), bytes(&middle), bytes(&latest)];
    let mut loaded = LoadedTranscript::default();
    for message in [first, middle, latest] {
        loaded.push_message(message);
    }
    assert_eq!(loaded.messages_in_order.len(), 3);
    assert_eq!(loaded.message_count(), 2);
    assert!(std::ptr::eq(
        loaded.message("duplicate").unwrap(),
        &loaded.messages_in_order[2]
    ));
    assert!(std::ptr::eq(
        loaded.message("middle").unwrap(),
        &loaded.messages_in_order[1]
    ));
    assert_eq!(
        loaded
            .messages_in_order
            .iter()
            .map(bytes)
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        loaded
            .message("duplicate")
            .unwrap()
            .message
            .string_units("/z"),
        Some(vec![110, 101, 119, 0xdfff])
    );
    let text = String::from_utf8(expected[2].clone()).unwrap();
    assert!(text.find("\"z\"").unwrap() < text.find("\"a\"").unwrap());

    loaded.message_mut("duplicate").unwrap().parent_uuid = Some("edited".into());
    assert_eq!(
        loaded.message("duplicate").unwrap().parent_uuid.as_deref(),
        Some("edited")
    );
    assert!(!std::ptr::eq(
        loaded.message("duplicate").unwrap(),
        &loaded.messages_in_order[2]
    ));
    assert_eq!(
        loaded
            .messages_in_order
            .iter()
            .map(bytes)
            .collect::<Vec<_>>(),
        expected,
        "a mutable UUID lookup must retain the old map's source-row isolation"
    );
    let index = loaded.messages_by_uuid();
    assert_eq!(
        index.get("duplicate").unwrap().parent_uuid.as_deref(),
        Some("edited")
    );
    let owned = loaded.messages_in_order;
    assert_eq!(
        owned.iter().map(bytes).collect::<Vec<_>>(),
        expected,
        "owned physical replay rows remain byte-identical after the index is dropped"
    );
}

#[test]
fn snapshot_reindexes_explicit_physical_edits_and_replaces_overrides() {
    let mut loaded = LoadedTranscript::default();
    loaded.push_message(row("same", 1_000, false));
    loaded.message_mut("same").unwrap().parent_uuid = Some("isolated".into());
    loaded.push_message(row("same", 2_000, true));
    assert_eq!(loaded.message("same").unwrap().parent_uuid, None);
    assert!(loaded.message("same").unwrap().is_sidechain);
    loaded.messages_in_order.reverse();
    loaded.reindex_messages();
    assert!(!loaded.message("same").unwrap().is_sidechain);
    assert!(std::ptr::eq(
        loaded.message("same").unwrap(),
        &loaded.messages_in_order[1]
    ));
}

#[test]
fn timestamp_index_retains_ties_duplicates_sidechains_and_inclusive_gap() {
    let mut loaded = LoadedTranscript::default();
    loaded.push_message(row("first", 1_000, false));
    loaded.push_message(row("second", 1_000, false));
    loaded.push_message(row("sidechain", 6_000, true));
    let mut latest_first = row("first", 1_000, false);
    latest_first.cwd = "/latest-value".into();
    loaded.push_message(latest_first);
    loaded.push_message(row("current", 6_000, false));
    let current = loaded.message("current").unwrap();
    let mut index = TimestampParentIndex::new(&loaded);
    let mut seen = HashSet::from(["current".to_owned()]);
    let selected = index.nearest(current, &seen).unwrap();
    assert_eq!(selected.uuid, "first");
    assert_eq!(selected.cwd, "/latest-value");
    seen.insert(selected.uuid.clone());
    assert_eq!(index.nearest(current, &seen).unwrap().uuid, "second");
    assert_eq!(
        index.parsed_timestamps, 4,
        "parse each last-UUID candidate once"
    );
    assert!(index
        .nearest(&row("beyond-gap", 6_001, false), &seen)
        .is_none());
    assert_eq!(
        index
            .nearest(&row("side", 6_000, true), &seen)
            .unwrap()
            .uuid,
        "sidechain"
    );
    let mut invalid = row("invalid", 6_000, false);
    invalid.timestamp = "not-a-date".into();
    assert!(index.nearest(&invalid, &seen).is_none());
}

#[test]
fn timestamp_index_parses_once_and_retires_visited_rows_once() {
    const COUNT: usize = 2_048;
    let mut loaded = LoadedTranscript::default();
    for n in 0..COUNT {
        let mut message = row(&format!("row-{n}"), 1_000_000 + n as i64, false);
        if n > 0 {
            message.parent_uuid = Some(format!("missing-{n}"));
        }
        loaded.push_message(message);
    }
    let mut index = TimestampParentIndex::new(&loaded);
    let mut seen = HashSet::new();
    let mut current = loaded.message(&format!("row-{}", COUNT - 1)).unwrap();
    for n in (1..COUNT).rev() {
        seen.insert(current.uuid.clone());
        current = index.nearest(current, &seen).unwrap();
        assert_eq!(current.uuid, format!("row-{}", n - 1));
    }
    assert_eq!(index.parsed_timestamps, COUNT);
    assert!(
        index.candidate_probes <= 2 * COUNT,
        "broken edges must not reparse or rescan the growing visited prefix"
    );
    let (chain, _) = build_conversation_chain(&loaded, "session");
    assert_eq!(chain.len(), COUNT);
    assert_eq!(chain.first().unwrap().uuid, "row-0");
    assert_eq!(chain.last().unwrap().uuid, format!("row-{}", COUNT - 1));
}

fn read_counts() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static COUNTS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn record_catalog_read(path: &Path) {
    *read_counts()
        .lock()
        .unwrap()
        .entry(path.to_path_buf())
        .or_default() += 1;
}

fn parse_counts() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static COUNTS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn record_catalog_parse(path: &Path) {
    *parse_counts()
        .lock()
        .unwrap()
        .entry(path.to_path_buf())
        .or_default() += 1;
}

#[derive(Clone, Copy)]
enum CatalogReadFault {
    FullReadOnly,
    PermissionDenied,
    UnsupportedPrefix,
}

fn read_faults() -> &'static Mutex<HashMap<PathBuf, CatalogReadFault>> {
    static FAULTS: OnceLock<Mutex<HashMap<PathBuf, CatalogReadFault>>> = OnceLock::new();
    FAULTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn catalog_read_fault(path: &Path, prefix: bool) -> Option<lingxi_core::host::FsError> {
    match read_faults().lock().unwrap().get(path) {
        Some(CatalogReadFault::PermissionDenied) => Some(
            lingxi_core::host::FsError::PermissionDenied(path.display().to_string()),
        ),
        Some(CatalogReadFault::FullReadOnly) if !prefix => Some(lingxi_core::host::FsError::Io(
            "transient complete read failure".into(),
        )),
        Some(CatalogReadFault::UnsupportedPrefix) if prefix => Some(
            lingxi_core::host::FsError::Io("prefix reads unsupported by test backend".into()),
        ),
        _ => None,
    }
}

struct ReadFaultGuard(PathBuf);

impl ReadFaultGuard {
    fn new(path: &Path, fault: CatalogReadFault) -> Self {
        read_faults().lock().unwrap().insert(path.to_owned(), fault);
        Self(path.to_owned())
    }
}

impl Drop for ReadFaultGuard {
    fn drop(&mut self) {
        read_faults().lock().unwrap().remove(&self.0);
    }
}

fn write_session(dir: &Path, cwd: &str) -> (Uuid, PathBuf) {
    let id = Uuid::new_v4();
    let mut message = row(&id.to_string(), 1_000_000, false);
    message.session_id = id.to_string();
    message.cwd = cwd.to_owned();
    let header = json!({"type":"fixed-session-mode", "sessionId":id, "sessionMode":"code", "instructionEpoch":0});
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{header}\n{}\n",
            String::from_utf8(bytes(&message)).unwrap()
        ),
    )
    .unwrap();
    (id, path)
}

#[tokio::test]
async fn catalog_cache_keeps_256_unchanged_rows_without_full_rereads() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.to_str().unwrap();
    let home = temp.path().join("home");
    let dir = home.join("projects").join(project_dir_name(cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let paths: Vec<_> = (0..256).map(|_| write_session(&dir, cwd).1).collect();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    let first = list_recent_sessions_in_scope_with_diagnostics(
        &home,
        cwd,
        SessionDiscoveryScope::Current,
        usize::MAX,
        Arc::clone(&fs),
    )
    .await
    .unwrap();
    assert_eq!(first.sessions.len(), 256);
    let counts: Vec<_> = paths
        .iter()
        .map(|path| read_counts().lock().unwrap()[path])
        .collect();
    assert!(counts.iter().all(|count| *count == 1));
    let second = list_recent_sessions_in_scope_with_diagnostics(
        &home,
        cwd,
        SessionDiscoveryScope::Current,
        usize::MAX,
        fs,
    )
    .await
    .unwrap();
    assert_eq!(first, second);
    for (path, count) in paths.iter().zip(counts) {
        assert_eq!(
            read_counts().lock().unwrap()[path],
            count,
            "discovery still runs but an unchanged transcript must not be fully reread"
        );
    }
}

#[tokio::test]
async fn catalog_cache_requires_current_read_and_preserves_full_read_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.to_str().unwrap();
    let home = temp.path().join("home");
    let dir = home.join("projects").join(project_dir_name(cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let (_, path) = write_session(&dir, cwd);
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_owned()));
    let list = || {
        list_recent_sessions_in_scope_with_diagnostics(
            &home,
            cwd,
            SessionDiscoveryScope::Current,
            usize::MAX,
            Arc::clone(&fs),
        )
    };
    let first = list().await.unwrap();
    assert_eq!(first.sessions.len(), 1);
    assert_eq!(read_counts().lock().unwrap()[&path], 1);
    let revision = std::fs::metadata(&path).unwrap();
    {
        // Inject denial at the filesystem-read boundary so this remains
        // deterministic for privileged test runners that can bypass chmod.
        let _denied = ReadFaultGuard::new(&path, CatalogReadFault::PermissionDenied);
        assert!(
            matches!(list().await, Err(LoaderError::Io { .. })),
            "a warm metadata row must not authorize a currently denied source"
        );
        assert_eq!(
            read_counts().lock().unwrap()[&path],
            2,
            "failed prefix validation must take the original full-read/error path"
        );
    }
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        revision.modified().unwrap()
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), revision.len());
    {
        let _unsupported = ReadFaultGuard::new(&path, CatalogReadFault::UnsupportedPrefix);
        assert_eq!(
            list().await.unwrap(),
            first,
            "a non-permission prefix failure must still allow a successful full read"
        );
        assert_eq!(read_counts().lock().unwrap()[&path], 3);
    }
    assert_eq!(list().await.unwrap(), first);
    assert_eq!(
        read_counts().lock().unwrap()[&path],
        3,
        "a successful current probe may reuse unchanged cached metadata again"
    );
}

#[test]
fn catalog_cache_replaces_revisions_and_enforces_owned_byte_budget() {
    let key = CatalogCacheKey {
        fs_identity: FileSystemCacheIdentity::new("test", PathBuf::from("/root"), 0),
        path: PathBuf::from("/session.jsonl"),
        current_is_sdk_entrypoint: false,
        target_cwds: vec!["/workspace".into()],
    };
    let entry = |revision, title_size| CatalogCacheEntry {
        file_len: revision,
        file_modified: SystemTime::UNIX_EPOCH,
        file_created: SystemTime::UNIX_EPOCH,
        scoped_cwd: None,
        skipped: false,
        last_access: 0,
        retained_bytes: 0,
        row: Some(SessionMetadata {
            uuid: Uuid::nil(),
            title: "x".repeat(title_size),
            modified: SystemTime::UNIX_EPOCH,
            created: SystemTime::UNIX_EPOCH,
            message_count: 1,
            mode: SessionMode::Code,
            path: PathBuf::from("/session.jsonl"),
            workspace_path: None,
            pr_number: None,
            custom_or_ai_title: None,
            resume_model: None,
            resume_model_profile: None,
        }),
    };
    let mut cache = CatalogCache::default();
    for revision in 0..512 {
        cache.insert(key.clone(), entry(revision, 64));
    }
    assert_eq!(cache.entries.len(), 1);
    assert_eq!(cache.recency.len(), 1);
    assert_eq!(cache.entries[&key].file_len, 511);
    let mut other = key.clone();
    other.path = PathBuf::from("/other.jsonl");
    cache.insert(other.clone(), entry(1, CATALOG_CACHE_BYTES / 2));
    cache.touch(&key);
    let mut third = key.clone();
    third.path = PathBuf::from("/third.jsonl");
    cache.insert(third.clone(), entry(1, CATALOG_CACHE_BYTES / 2));
    assert!(cache.entries.contains_key(&key));
    assert!(
        !cache.entries.contains_key(&other),
        "evict the least recently used row"
    );
    assert!(cache.retained_bytes <= CATALOG_CACHE_BYTES);
    cache.insert(key.clone(), entry(999, CATALOG_CACHE_BYTES));
    assert!(
        !cache.entries.contains_key(&key),
        "an oversize revision must replace and evict its old slot"
    );
    assert!(cache.retained_bytes <= CATALOG_CACHE_BYTES);
}

#[tokio::test]
async fn alias_index_retries_until_admission_and_keeps_all_destination_variants() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("source");
    std::fs::create_dir_all(&dir).unwrap();
    let destination = temp.path().join("C");
    std::fs::create_dir_all(&destination).unwrap();
    let wrong = temp.path().join("B");
    let (id, path) = write_session(&dir, temp.path().to_str().unwrap());
    let source = std::fs::read_to_string(&path).unwrap();
    let marker = json!({"type":"relocated", "sessionId":id, "relocatedCwd":destination});
    std::fs::write(&path, format!("{source}{marker}\n")).unwrap();
    let aliases = vec![
        (path.clone(), wrong),
        (path.clone(), destination.clone()),
        (path.clone(), destination.join(".")),
    ];
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    let mut candidates = Vec::new();
    collect_cwd_aliases(&aliases, &fs, &mut candidates, None).await;
    assert_eq!(candidates.len(), 1);
    let mut skipped = 0;
    let mut rows = enrich_candidates(
        candidates,
        &fs,
        false,
        &[],
        &[],
        std::slice::from_ref(&path),
        &mut skipped,
    )
    .await
    .unwrap();
    retain_current_cwd_alias_rows(&mut rows, &HashSet::new(), &aliases);
    assert_eq!(
        rows.len(),
        1,
        "retain any validated destination variant for the exact path"
    );
    let mut wrong_case_path = rows.clone();
    wrong_case_path[0].path = PathBuf::from(path.to_string_lossy().to_uppercase());
    retain_current_cwd_alias_rows(&mut wrong_case_path, &HashSet::new(), &aliases);
    assert!(
        wrong_case_path.is_empty(),
        "alias path keys must retain lexical case equality"
    );
    rows[0].workspace_path = Some(temp.path().join("D").to_string_lossy().into_owned());
    let original = rows.clone();
    retain_current_cwd_alias_rows(&mut rows, &HashSet::new(), &aliases);
    assert!(
        rows.is_empty(),
        "a later relocation must still revoke the stale alias"
    );
    let mut ordinary = original;
    retain_current_cwd_alias_rows(&mut ordinary, &HashSet::from([path]), &aliases);
    assert_eq!(
        ordinary.len(),
        1,
        "ordinary discovery keeps its existing precedence"
    );
}

#[tokio::test]
async fn successful_prefix_and_transient_full_read_failure_does_not_poison_revision() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.to_str().unwrap();
    let home = temp.path().join("home");
    let dir = home.join("projects").join(project_dir_name(cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let (_, path) = write_session(&dir, cwd);
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_owned()));
    let revision = std::fs::metadata(&path).unwrap();
    {
        let _fault = ReadFaultGuard::new(&path, CatalogReadFault::FullReadOnly);
        let rows = list_recent_sessions_in_scope_with_diagnostics(
            &home,
            cwd,
            SessionDiscoveryScope::Current,
            usize::MAX,
            fs.clone(),
        )
        .await;
        assert!(matches!(rows, Err(LoaderError::Io { .. })), "{rows:?}");
    }
    let restored = list_recent_sessions_in_scope_with_diagnostics(
        &home,
        cwd,
        SessionDiscoveryScope::Current,
        usize::MAX,
        fs,
    )
    .await
    .unwrap();
    assert_eq!(restored.sessions.len(), 1);
    assert_eq!(
        revision.modified().unwrap(),
        std::fs::metadata(&path).unwrap().modified().unwrap()
    );
    assert_eq!(revision.len(), std::fs::metadata(&path).unwrap().len());
}

fn alias_source_fixture() -> (
    tempfile::TempDir,
    Uuid,
    PathBuf,
    PathBuf,
    Arc<dyn FileSystem>,
) {
    let temp = tempfile::tempdir().unwrap();
    let source_dir = temp.path().join("source");
    let destination = temp.path().join("destination");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(&destination).unwrap();
    let (id, path) = write_session(&source_dir, temp.path().to_str().unwrap());
    let source = std::fs::read_to_string(&path).unwrap();
    let relocated = json!({"type":"relocated", "sessionId":id, "relocatedCwd":destination});
    std::fs::write(&path, format!("{source}{relocated}\n")).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_owned()));
    (temp, id, path, destination, fs)
}

#[tokio::test]
async fn unchanged_alias_admission_and_enrichment_share_one_full_read_and_parse() {
    let (_temp, id, path, destination, fs) = alias_source_fixture();
    let aliases = vec![(path.clone(), destination.clone())];
    let mut candidates = Vec::new();
    collect_cwd_aliases(&aliases, &fs, &mut candidates, None).await;
    assert_eq!(candidates.len(), 1);
    assert!(candidates[0].alias_capture.is_some());
    assert_eq!(read_counts().lock().unwrap()[&path], 1);
    assert_eq!(parse_counts().lock().unwrap()[&path], 1);
    let mut skipped = 0;
    let mut rows = enrich_candidates(
        candidates,
        &fs,
        false,
        &[],
        &[],
        std::slice::from_ref(&path),
        &mut skipped,
    )
    .await
    .unwrap();
    retain_current_cwd_alias_rows(&mut rows, &HashSet::new(), &aliases);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].uuid, id);
    assert_eq!(rows[0].workspace_path.as_deref(), destination.to_str());
    assert_eq!(rows[0].title, "hello");
    assert_eq!(rows[0].message_count, 1);
    assert_eq!(skipped, 0);
    assert_eq!(
        read_counts().lock().unwrap()[&path],
        1,
        "a stable alias needs one full read plus its late bounded access probe"
    );
    assert_eq!(
        parse_counts().lock().unwrap()[&path],
        1,
        "admission and row metadata must use the same routed snapshot"
    );
}

#[tokio::test]
async fn alias_capture_refreshes_late_relocation_before_final_membership_check() {
    let (temp, id, path, destination, fs) = alias_source_fixture();
    let aliases = vec![(path.clone(), destination)];
    let mut candidates = Vec::new();
    collect_cwd_aliases(&aliases, &fs, &mut candidates, None).await;
    let next_destination = temp.path().join("next");
    std::fs::create_dir_all(&next_destination).unwrap();
    let source = std::fs::read_to_string(&path).unwrap();
    let relocated = json!({"type":"relocated", "sessionId":id, "relocatedCwd":next_destination});
    std::fs::write(&path, format!("{source}{relocated}\n")).unwrap();
    let mut skipped = 0;
    let mut rows = enrich_candidates(
        candidates,
        &fs,
        false,
        &[],
        &[],
        std::slice::from_ref(&path),
        &mut skipped,
    )
    .await
    .unwrap();
    assert_eq!(rows[0].workspace_path.as_deref(), next_destination.to_str());
    retain_current_cwd_alias_rows(&mut rows, &HashSet::new(), &aliases);
    assert!(
        rows.is_empty(),
        "late /cd must not inherit the old alias destination"
    );
    assert_eq!(read_counts().lock().unwrap()[&path], 2);
    assert_eq!(parse_counts().lock().unwrap()[&path], 2);
}

#[tokio::test]
async fn alias_capture_refreshes_replacements_rewrites_and_truncation() {
    for mutation in ["replace", "rewrite", "truncate"] {
        let (temp, _id, path, destination, fs) = alias_source_fixture();
        let aliases = vec![(path.clone(), destination)];
        let mut candidates = Vec::new();
        collect_cwd_aliases(&aliases, &fs, &mut candidates, None).await;
        let before = std::fs::metadata(&path).unwrap();
        let source = std::fs::read_to_string(&path).unwrap();
        match mutation {
            "replace" => {
                let replacement = temp.path().join("replacement");
                std::fs::write(&replacement, &source).unwrap();
                filetime::set_file_mtime(
                    &replacement,
                    filetime::FileTime::from_system_time(before.modified().unwrap()),
                )
                .unwrap();
                std::fs::rename(&replacement, &path).unwrap();
            }
            "rewrite" => {
                // Length and mtime agree with the captured row; ctime and the
                // pinned inode revision must still force a fresh read.
                std::fs::write(&path, source.replace("hello", "world")).unwrap();
                filetime::set_file_mtime(
                    &path,
                    filetime::FileTime::from_system_time(before.modified().unwrap()),
                )
                .unwrap();
            }
            "truncate" => std::fs::write(&path, "").unwrap(),
            _ => unreachable!(),
        }
        let mut skipped = 0;
        let mut rows = enrich_candidates(
            candidates,
            &fs,
            false,
            &[],
            &[],
            std::slice::from_ref(&path),
            &mut skipped,
        )
        .await
        .unwrap();
        retain_current_cwd_alias_rows(&mut rows, &HashSet::new(), &aliases);
        if mutation == "truncate" {
            assert!(rows.is_empty());
        } else {
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].title,
                if mutation == "rewrite" {
                    "world"
                } else {
                    "hello"
                }
            );
        }
        assert_eq!(
            read_counts().lock().unwrap()[&path],
            2,
            "{mutation} must not reuse the old derived row"
        );
        assert_eq!(
            parse_counts().lock().unwrap()[&path],
            if mutation == "truncate" { 1 } else { 2 },
            "{mutation} must route a valid refreshed source or reject its missing fixed mode"
        );
    }
}

#[tokio::test]
async fn alias_capture_rechecks_access_and_keeps_unsupported_prefix_fallback() {
    for fault in [
        CatalogReadFault::PermissionDenied,
        CatalogReadFault::UnsupportedPrefix,
    ] {
        let (_temp, _id, path, destination, fs) = alias_source_fixture();
        let aliases = vec![(path.clone(), destination)];
        let mut candidates = Vec::new();
        collect_cwd_aliases(&aliases, &fs, &mut candidates, None).await;
        let _fault = ReadFaultGuard::new(&path, fault);
        let mut skipped = 0;
        let rows = enrich_candidates(
            candidates,
            &fs,
            false,
            &[],
            &[],
            std::slice::from_ref(&path),
            &mut skipped,
        )
        .await
        .unwrap();
        match fault {
            CatalogReadFault::FullReadOnly => {
                unreachable!("full-read-only failures use the separate revision recovery fixture")
            }
            CatalogReadFault::PermissionDenied => {
                assert!(
                    rows.is_empty(),
                    "a prepared row cannot authorize access revoked after admission"
                );
                assert_eq!(skipped, 1);
                assert_eq!(parse_counts().lock().unwrap()[&path], 1);
            }
            CatalogReadFault::UnsupportedPrefix => {
                assert_eq!(
                    rows.len(),
                    1,
                    "unsupported prefix access must use the existing full-read fallback"
                );
                assert_eq!(skipped, 0);
                assert_eq!(parse_counts().lock().unwrap()[&path], 2);
            }
        }
        assert_eq!(read_counts().lock().unwrap()[&path], 2);
    }
}

#[test]
fn captured_validation_reuses_a_shareable_routed_snapshot() {
    fn assert_shareable<T: Send + Sync + std::fmt::Debug>() {}
    assert_shareable::<LoadedTranscript>();
    let id = Uuid::new_v4();
    let path = PathBuf::from(format!("/projects/source/{id}.jsonl"));
    let mut message = row("message", 1_000_000, false);
    message.session_id = id.to_string();
    let header = json!({"type":"fixed-session-mode", "sessionId":id, "sessionMode":"code", "instructionEpoch":0});
    let source = format!(
        "{header}\n{}\n",
        String::from_utf8(bytes(&message)).unwrap()
    );
    let loaded = route_lines(&source);
    let allocation = loaded.messages_in_order.as_ptr();
    validate_captured_session_source(&path, id, &source, &loaded).unwrap();
    assert_eq!(loaded.messages_in_order.as_ptr(), allocation);
    assert!(matches!(
        validate_captured_session_source(&path, Uuid::new_v4(), &source, &loaded),
        Err(LoaderError::SessionNotFound { .. })
    ));
    let empty_source = format!("{header}\n");
    let empty = route_lines(&empty_source);
    assert!(matches!(
        validate_captured_session_source(&path, id, &empty_source, &empty),
        Err(LoaderError::EmptyDirectory)
    ));
    assert!(matches!(
        validate_captured_session_source(&path, id, "", &route_lines("")),
        Err(LoaderError::Io { .. })
    ));
    let draft_source = format!(
        "{header}\n{}\n",
        json!({
            "type":"custom-title", "sessionId":id, "customTitle":"Draft", "mobileEmptySession":1
        })
    );
    validate_captured_session_source(&path, id, &draft_source, &route_lines(&draft_source))
        .unwrap();
    let conflict = format!(
        "{source}{}\n",
        json!({"type":"fixed-session-mode", "sessionId":id, "sessionMode":"chat", "instructionEpoch":0})
    );
    assert!(
        validate_captured_session_source(&path, id, &conflict, &route_lines(&conflict)).is_err()
    );
    let boundary = json!({
        "type":"system", "uuid":"boundary", "parentUuid":null, "sessionId":id,
        "subtype":"compact_boundary", "compactMetadata":{
            "nativeExactPreserved":true,
            "preservedSegment":{"headUuid":"missing", "tailUuid":"missing", "anchorUuid":"message"},
            "preservedMessages":{"anchorUuid":"message", "uuids":["missing"], "allUuids":["missing"]}
        }
    });
    let invalid_preserved = format!("{source}{boundary}\n");
    assert!(matches!(
        validate_captured_session_source(
            &path,
            id,
            &invalid_preserved,
            &route_lines(&invalid_preserved)
        ),
        Err(LoaderError::Io { .. })
    ));
}

#[tokio::test]
async fn alias_variants_preserve_first_matching_admission_order_for_uuid_ties() {
    let (temp, _id, first_path, destination, fs) = alias_source_fixture();
    let second_dir = temp.path().join("second");
    std::fs::create_dir_all(&second_dir).unwrap();
    let second_path = second_dir.join(first_path.file_name().unwrap());
    let source = std::fs::read_to_string(&first_path).unwrap();
    std::fs::write(&second_path, source.replace("hello", "world")).unwrap();
    let modified = std::fs::metadata(&first_path).unwrap().modified().unwrap();
    filetime::set_file_mtime(&second_path, filetime::FileTime::from_system_time(modified)).unwrap();
    let aliases = vec![
        (first_path.clone(), temp.path().join("stale-destination")),
        (second_path.clone(), destination.clone()),
        (first_path.clone(), destination),
    ];
    let mut candidates = Vec::new();
    collect_cwd_aliases(&aliases, &fs, &mut candidates, None).await;
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| &candidate.path)
            .collect::<Vec<_>>(),
        [&second_path, &first_path]
    );
    let winner = deduplicate_catalog_candidates(candidates);
    assert_eq!(winner.len(), 1);
    assert_eq!(
        winner[0].path, second_path,
        "a stale earlier alias must not change the tie-winning source"
    );
    assert_eq!(read_counts().lock().unwrap()[&first_path], 1);
    assert_eq!(read_counts().lock().unwrap()[&second_path], 1);
}
