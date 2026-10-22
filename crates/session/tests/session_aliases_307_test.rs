//! Native 2.1.307 directory-link bytes and filesystem behavior.

use serde_json::Value;
use session::jsonl::{
    project_dir_name, read_session_aliases, record_session_alias, SESSION_ALIASES_FILE,
};
use std::path::{Path, PathBuf};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/workspace_session_2_1_307.json")).unwrap()
}

fn local_path(home: &Path, value: &str) -> PathBuf {
    value.strip_prefix("/oracle/.claude").map_or_else(
        || PathBuf::from(value),
        |suffix| home.join(suffix.trim_start_matches('/')),
    )
}

fn local_content(home: &Path, value: &str) -> String {
    value.replace("/oracle/.claude", &home.to_string_lossy())
}

#[tokio::test]
async fn native_alias_read_fixture_preserves_exact_line_semantics() {
    for case in fixture()["alias_read_cases"].as_array().unwrap() {
        let input = &case["input"];
        if input["diskless"].as_bool() == Some(true) || input["storage_v5"].as_bool() == Some(true)
        {
            continue;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        if let Some(files) = input["initial_files"].as_array() {
            for file in files {
                let path = local_path(&home, file["path"].as_str().unwrap());
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, file["utf8"].as_str().unwrap()).unwrap();
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
async fn native_alias_write_fixture_preserves_direction_dedup_and_bytes() {
    for case in fixture()["alias_write_cases"].as_array().unwrap() {
        let input = &case["input"];
        if input.get("persistence").is_some()
            || input.get("diskless").is_some()
            || input.get("storage_v5").is_some()
            || input["failures"]
                .as_object()
                .is_some_and(|failures| failures.keys().any(|operation| operation != "realpath"))
            || input.get("realpath_result").is_some()
        {
            continue;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        if let Some(files) = input["initial_files"].as_array() {
            for file in files {
                let path = local_path(&home, file["path"].as_str().unwrap());
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, local_content(&home, file["utf8"].as_str().unwrap())).unwrap();
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
            let path = local_path(&home, file["path"].as_str().unwrap());
            let expected = local_content(&home, file["utf8"].as_str().unwrap());
            assert_eq!(
                std::fs::read(path).unwrap(),
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
async fn unreadable_alias_file_is_empty_for_read_but_stops_a_write() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let added = temp.path().join("missing-added");
    let project = home
        .join("projects")
        .join(project_dir_name(&added.to_string_lossy()));
    let alias_file = project.join(SESSION_ALIASES_FILE);
    std::fs::create_dir_all(&alias_file).unwrap();
    assert!(read_session_aliases(&project).await.is_empty());
    assert!(
        record_session_alias(&home, Path::new("/authoritative/source"), &added)
            .await
            .is_err()
    );
    assert!(alias_file.is_dir());
}

#[tokio::test]
async fn missing_decomposed_added_directory_retains_raw_bucket_bytes() {
    use unicode_normalization::UnicodeNormalization;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let added = temp.path().join("missing-cafe\u{301}");
    let raw = added.to_string_lossy();
    let normalized = raw.nfc().collect::<String>();
    assert_ne!(project_dir_name(&raw), project_dir_name(&normalized));
    assert!(record_session_alias(&home, Path::new("/source"), &added)
        .await
        .unwrap());
    assert_eq!(
        std::fs::read(
            home.join("projects")
                .join(project_dir_name(&raw))
                .join(SESSION_ALIASES_FILE)
        )
        .unwrap(),
        b"/source\n",
    );
    assert!(!home
        .join("projects")
        .join(project_dir_name(&normalized))
        .exists());
}

#[cfg(unix)]
#[tokio::test]
async fn canonical_target_and_private_modes_match_native() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let added = temp.path().join("cafe\u{301}");
    std::fs::create_dir(&added).unwrap();
    let link = temp.path().join("linked");
    symlink(&added, &link).unwrap();
    let source = Path::new("/authoritative/source-parent/");
    assert!(record_session_alias(&home, source, &link).await.unwrap());
    use unicode_normalization::UnicodeNormalization;
    let canonical = std::fs::canonicalize(&added)
        .unwrap()
        .to_string_lossy()
        .nfc()
        .collect::<String>();
    let project = home.join("projects").join(project_dir_name(&canonical));
    let path = project.join(SESSION_ALIASES_FILE);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"/authoritative/source-parent/\n"
    );
    assert_eq!(
        std::fs::metadata(&project).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!record_session_alias(&home, source, &added).await.unwrap());
}
