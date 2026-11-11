//! Current native source replay contracts (Claude Code 2.1.321).

use lingxi_core::host::FileSystem;
use platform_posix::fs::PosixFileSystem;
use serde_json::Value;
use session::jsonl::{
    loader::list_recent_sessions_in_scope_with_worktrees, project_dir_name, read_session_aliases,
    record_session_alias, resolve_job_transcript_raw, JobTranscriptResolveOptions,
    SessionDiscoveryScope,
};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/session_discovery_2_1_321.json")).unwrap()
}

/// Native paths use Claude's namespace; the host intentionally uses its active
/// product namespace for managed worktree directories and their encoded buckets.
fn product_namespace(value: &str) -> String {
    value
        .replace(
            "/.claude/worktrees",
            &format!("/{}/worktrees", branding::DOT_DIR),
        )
        .replace(
            "-claude-worktrees-",
            &format!("{}-worktrees-", branding::DOT_DIR.replace('.', "-")),
        )
}

fn local_path(home: &Path, value: &str) -> PathBuf {
    if let Some(suffix) = value.strip_prefix("/oracle/.claude") {
        home.join(product_namespace(suffix.trim_start_matches('/')))
    } else if let Some(suffix) = value.strip_prefix("/linked/") {
        home.join("linked").join(product_namespace(suffix))
    } else if value == "/unread-other-worktree-alias" {
        home.join("external-aliases")
            .join("unread-other-worktree-alias")
    } else {
        PathBuf::from(product_namespace(value))
    }
}

fn local_content(home: &Path, value: &str) -> String {
    product_namespace(value)
        .replace("/oracle/.claude", &home.to_string_lossy())
        .replace(
            "/unread-other-worktree-alias",
            &local_path(home, "/unread-other-worktree-alias").to_string_lossy(),
        )
}

/// Every materialized fixture target must remain in this case's temp home.
fn seed_target(home: &Path, target: PathBuf) -> PathBuf {
    let relative = target.strip_prefix(home).unwrap_or_else(|_| {
        panic!(
            "fixture storage target escaped temp home: {}",
            target.display()
        )
    });
    assert!(
        relative
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir)),
        "fixture storage target traversed outside temp home: {}",
        target.display()
    );
    target
}

#[test]
fn native_321_project_slug_bytes_include_utf16_and_long_path_boundaries() {
    for case in fixture()["path_cases"].as_array().unwrap() {
        assert_eq!(
            project_dir_name(case["input"].as_str().unwrap()),
            case["expected"]["slug"].as_str().unwrap(),
            "{}",
            case["input"]
        );
    }
}

#[tokio::test]
async fn native_321_alias_read_bytes_preserve_cr_spaces_nul_and_first_seen_order() {
    for case in fixture()["alias_read_cases"].as_array().unwrap() {
        let input = &case["input"];
        if input["diskless"].as_bool() == Some(true)
            || input["storage_v5"].as_bool() == Some(true)
            || input.get("failures").is_some()
        {
            continue;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        if let Some(files) = input["initial_files"].as_array() {
            for file in files {
                let target = seed_target(&home, local_path(&home, file["path"].as_str().unwrap()));
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::write(target, file["utf8"].as_str().unwrap()).unwrap();
            }
        }
        let project = home
            .join("projects")
            .join(project_dir_name(input["project_path"].as_str().unwrap()));
        let actual: Vec<_> = read_session_aliases(&project)
            .await
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let expected: Vec<_> = case["expected"]["aliases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect();
        assert_eq!(actual, expected, "{}", case["name"]);
    }
}

#[tokio::test]
async fn native_321_alias_appends_match_exact_bytes_without_repairing_lf() {
    for case in fixture()["alias_write_cases"].as_array().unwrap() {
        let input = &case["input"];
        if input.get("persistence").is_some()
            || input.get("diskless").is_some()
            || input.get("storage_v5").is_some()
            || input.get("realpath_result").is_some()
            || input["failures"]
                .as_object()
                .is_some_and(|failures| failures.keys().any(|operation| operation != "realpath"))
        {
            continue;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        if let Some(files) = input["initial_files"].as_array() {
            for file in files {
                let target = seed_target(&home, local_path(&home, file["path"].as_str().unwrap()));
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::write(target, local_content(&home, file["utf8"].as_str().unwrap()))
                    .unwrap();
            }
        }
        let source = input["source_project_dir"].as_str().map_or_else(
            || {
                let cwd = input["pinned_project_path"]
                    .as_str()
                    .unwrap_or(input["current_dir"].as_str().unwrap());
                home.join("projects").join(project_dir_name(cwd))
            },
            |source| local_path(&home, source),
        );
        for _ in 0..input["repeat"].as_u64().unwrap_or(1) {
            record_session_alias(
                &home,
                &source,
                Path::new(input["added_dir"].as_str().unwrap()),
            )
            .await
            .unwrap();
        }
        for file in case["expected"]["files"].as_array().unwrap() {
            let target = seed_target(&home, local_path(&home, file["path"].as_str().unwrap()));
            let expected = local_content(&home, file["utf8"].as_str().unwrap());
            assert_eq!(
                std::fs::read(target).unwrap(),
                expected.as_bytes(),
                "{}",
                case["name"]
            );
        }
        if case["expected"]["files"].as_array().unwrap().is_empty() {
            assert!(!home.exists(), "{} must not create storage", case["name"]);
        }
    }
}

#[tokio::test]
async fn native_321_raw_uuid_lookup_matches_disk_selection_and_presence() {
    let fixture = fixture();
    let mut exercised = 0;
    let case_sensitive = {
        let probe = tempfile::tempdir().unwrap();
        std::fs::write(probe.path().join("case-sensitive-probe"), b"probe").unwrap();
        !probe.path().join("CASE-SENSITIVE-PROBE").exists()
    };
    for case in fixture["uuid_resolution_cases"].as_array().unwrap() {
        let input = &case["input"];
        let files = input["transcripts"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        // Actual Git enumeration and injected OS failures belong to the
        // controlled native replay; these cases exercise the real disk adapter.
        if input.get("realpath_result").is_some()
            || input.get("project_directory_error").is_some()
            || files.iter().any(|file| {
                ["lstat_error", "open_error", "stream_error"]
                    .iter()
                    .any(|key| file.get(key).is_some())
            })
            || case["expected"]["resolution"]["via"] == "worktreeProjectDir"
            // The VM inventory distinguishes case even on a macOS volume
            // which resolves uppercase queries to an existing lowercase file.
            || (!case_sensitive
                && case["name"] == "uppercase_UUID_is_valid_but_filename_lookup_preserves_case")
        {
            continue;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        for file in files {
            let target = seed_target(&home, local_path(&home, file["path"].as_str().unwrap()));
            if file["kind"] == "directory" {
                std::fs::create_dir_all(target).unwrap();
            } else {
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::write(target, file["utf8"].as_str().unwrap_or("")).unwrap();
            }
        }
        let actual = resolve_job_transcript_raw(
            &home,
            input["current_dir"].as_str().unwrap(),
            input["session_id"].as_str().unwrap(),
            &JobTranscriptResolveOptions {
                link_scan_path: input["link_scan_path"]
                    .as_str()
                    .map(|path| local_path(&home, path)),
                cross_worktree: input["cross_worktree"].as_bool().unwrap_or(true),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            actual.path,
            local_path(
                &home,
                case["expected"]["resolution"]["path"].as_str().unwrap()
            ),
            "{}",
            case["name"]
        );
        assert_eq!(
            actual.has_messages,
            case["expected"]["resolution"]["hasMessages"]
                .as_bool()
                .unwrap(),
            "{}",
            case["name"]
        );
        exercised += 1;
    }
    assert!(
        exercised >= 15,
        "disk selection coverage unexpectedly shrank"
    );

    // Keep uppercase filename coverage on every volume: a file actually named
    // with that UUID resolves and the returned path preserves the query bytes.
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = "/workspace/A";
    let id = "ABCDEFAB-0000-4000-8000-000000000001";
    let transcript = seed_target(
        &home,
        home.join("projects")
            .join(project_dir_name(cwd))
            .join(format!("{id}.jsonl")),
    );
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
    let selected = resolve_job_transcript_raw(
        &home,
        cwd,
        id,
        &JobTranscriptResolveOptions {
            link_scan_path: None,
            cross_worktree: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(selected.path, transcript);
    assert!(selected.has_messages);
}

#[tokio::test]
async fn native_321_catalog_keeps_alias_dedup_limits_and_repository_union() {
    let fixture = fixture();
    let mut exercised = 0;
    for case in fixture["discovery_cases"].as_array().unwrap() {
        let input = &case["input"];
        if input.get("diskless").is_some()
            || input.get("storage_v5").is_some()
            || input.get("foreign_session_ids").is_some()
            || input.get("resolved_project_dir").is_some()
            || input.get("failures").is_some()
        {
            continue;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = product_namespace(input["current_dir"].as_str().unwrap());
        if let Some(files) = input["initial_files"].as_array() {
            for file in files {
                let target = seed_target(&home, local_path(&home, file["path"].as_str().unwrap()));
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::write(target, local_content(&home, file["utf8"].as_str().unwrap()))
                    .unwrap();
            }
        }
        if let Some(directories) = input["project_directories"].as_array() {
            for directory in directories {
                if directory["is_directory"].as_bool() == Some(true) {
                    std::fs::create_dir_all(seed_target(
                        &home,
                        home.join("projects")
                            .join(product_namespace(directory["name"].as_str().unwrap())),
                    ))
                    .unwrap();
                }
            }
        }
        for (project, rows) in input["project_rows"].as_object().unwrap() {
            for row in rows.as_array().unwrap() {
                let id = row["session_id"].as_str().unwrap();
                let target = seed_target(
                    &home,
                    local_path(&home, project).join(format!("{id}.jsonl")),
                );
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                let created_ms = row["created_ms"].as_i64().unwrap();
                let timestamp = chrono::DateTime::from_timestamp_millis(created_ms)
                    .unwrap()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                let modified_ms = row["modified_ms"].as_i64().unwrap();
                let activity_timestamp =
                    chrono::DateTime::from_timestamp_millis(created_ms.max(modified_ms))
                        .unwrap()
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                let header = serde_json::json!({"type":"fixed-session-mode", "sessionId":id, "sessionMode":"code", "instructionEpoch":0});
                let message = serde_json::json!({"type":"user", "uuid":id, "sessionId":id, "parentUuid":null,
                    "timestamp":timestamp, "message":{"role":"user", "content":"native replay"},
                    "cwd":cwd, "isSidechain":false});
                // Native's lightweight inventory supplies creation and physical
                // activity independently. Represent both before the real loader
                // enriches the transcript and clamps activity to file mtime.
                let assistant = serde_json::json!({"type":"assistant", "uuid":uuid::Uuid::new_v4(),
                    "sessionId":id, "parentUuid":id, "timestamp":activity_timestamp,
                    "message":{"role":"assistant", "content":[{"type":"text", "text":"native replay"}]},
                    "cwd":cwd, "isSidechain":false});
                std::fs::write(&target, format!("{header}\n{message}\n{assistant}\n")).unwrap();
                filetime::set_file_mtime(
                    &target,
                    filetime::FileTime::from_unix_time(
                        0,
                        row["modified_ms"].as_u64().unwrap() as u32 * 1_000_000,
                    ),
                )
                .unwrap();
            }
        }
        let worktrees: Vec<_> = input["repository_paths"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|path| product_namespace(path.as_str().unwrap()))
            .collect();
        let first_equal_mtime_copy = if case["name"]
            == "repository_multiple_worktrees_use_enumeration_order_for_equal_modified_copy"
        {
            // The native VM supplies [worktree, root] as its directory
            // inventory. APFS may enumerate those same directories in the
            // opposite order. Retain the native first-encounter assertion,
            // using the actual disk inventory for this adapter case only.
            let id = case["expected"]["sessions"][0]["session_id"]
                .as_str()
                .unwrap();
            let mut inventory = tokio::fs::read_dir(home.join("projects")).await.unwrap();
            let mut copies = Vec::new();
            while let Some(entry) = inventory.next_entry().await.unwrap() {
                if entry.file_type().await.unwrap().is_dir() {
                    let copy = entry.path().join(format!("{id}.jsonl"));
                    if copy.is_file() {
                        copies.push(copy);
                    }
                }
            }
            assert_eq!(copies.len(), 2, "the tie needs both physical copies");
            assert_eq!(
                std::fs::metadata(&copies[0]).unwrap().modified().unwrap(),
                std::fs::metadata(&copies[1]).unwrap().modified().unwrap(),
                "the duplicate's physical modification times must be equal"
            );
            Some(copies.remove(0))
        } else {
            None
        };
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
        let actual = list_recent_sessions_in_scope_with_worktrees(
            &home,
            &cwd,
            if input["selector"] == "current" {
                SessionDiscoveryScope::Current
            } else {
                SessionDiscoveryScope::Repository
            },
            input["max_count"]
                .as_u64()
                .map_or(usize::MAX, |n| n as usize),
            fs,
            &worktrees,
        )
        .await
        .unwrap();
        let selected: Vec<_> = actual.sessions.into_iter().map(|row| row.path).collect();
        let mut expected: Vec<_> = case["expected"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| local_path(&home, row["full_path"].as_str().unwrap()))
            .collect();
        if let Some(first_copy) = first_equal_mtime_copy {
            expected[0] = first_copy;
        }
        assert_eq!(selected, expected, "{}", case["name"]);
        exercised += 1;
    }
    assert!(exercised >= 15, "catalog coverage unexpectedly shrank");
}
