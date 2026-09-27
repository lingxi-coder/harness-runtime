use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use serde_json::{json, Value};

use super::{load_normalized, path, InstalledRegistryTransaction};

fn legacy_path(root: &Path) -> PathBuf {
    root.join("installed_plugins_v2.json")
}

fn canonical_doc(id: &str, install_path: &str, version: &str) -> Value {
    json!({
        "version": 2,
        "plugins": {
            id: [{
                "scope": "user",
                "installPath": install_path,
                "version": version,
                "installedAt": "2026-08-31T00:00:00.000Z",
                "lastUpdated": "2026-08-31T00:00:00.000Z"
            }]
        }
    })
}

fn nested_legacy_doc() -> Value {
    json!({
        "plugins": {
            "acme": {
                "weather": {
                    "version": "1.0.0",
                    "installPath": "cache/acme/weather/1.0.0",
                    "added": "2026-08-31T00:00:00.000Z"
                }
            }
        }
    })
}

fn normalized_nested_legacy_doc() -> Value {
    canonical_doc("weather@acme", "cache/acme/weather/1.0.0", "1.0.0")
}

fn flat_v1_doc() -> Value {
    json!({
        "plugins": {
            "weather@acme": {
                "version": "1.0.0",
                "installedAt": "2026-08-30T00:00:00.000Z",
                "lastUpdated": "2026-08-31T00:00:00.000Z",
                "gitCommitSha": "0123456789abcdef"
            }
        }
    })
}

fn normalized_flat_v1_doc(root: &Path) -> Value {
    let install_path = root
        .join("cache/acme/weather/1.0.0")
        .to_string_lossy()
        .into_owned();
    json!({
        "version": 2,
        "plugins": {
            "weather@acme": [{
                "scope": "user",
                "installPath": install_path,
                "version": "1.0.0",
                "installedAt": "2026-08-30T00:00:00.000Z",
                "lastUpdated": "2026-08-31T00:00:00.000Z",
                "gitCommitSha": "0123456789abcdef"
            }]
        }
    })
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_string(value).unwrap()).unwrap();
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn migrates_legacy_filename_when_current_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let expected = canonical_doc("weather@acme", "cache/acme/weather/1.0.0", "1.0.0");
    write_json(&legacy_path(root), &expected);

    let loaded = load_normalized(root);

    assert_eq!(loaded, Some(expected.clone()));
    assert_eq!(read_json(&path(root)), expected);
    assert!(!legacy_path(root).exists());
}

#[test]
fn rename_collision_keeps_the_current_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let current = canonical_doc("weather@acme", "cache/acme/weather/1.0.0", "1.0.0");
    let legacy = canonical_doc("calc@acme", "cache/acme/calc/2.0.0", "2.0.0");
    write_json(&path(root), &current);
    write_json(&legacy_path(root), &legacy);

    let loaded = load_normalized(root);

    assert_eq!(loaded, Some(current.clone()));
    assert_eq!(read_json(&path(root)), current);
    assert_eq!(read_json(&legacy_path(root)), legacy);
}

#[test]
fn flat_v1_current_registry_wins_over_legacy_filename_collision() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let expected = normalized_flat_v1_doc(root);
    let legacy = canonical_doc("calc@acme", "cache/acme/calc/2.0.0", "2.0.0");
    write_json(&path(root), &flat_v1_doc());
    write_json(&legacy_path(root), &legacy);

    let loaded = load_normalized(root);

    assert_eq!(loaded, Some(expected.clone()));
    assert_eq!(read_json(&path(root)), expected);
    assert_eq!(read_json(&legacy_path(root)), legacy);
}

#[test]
fn normalizes_nested_legacy_shape_and_persists_canonical_v2() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let expected = normalized_nested_legacy_doc();
    write_json(&path(root), &nested_legacy_doc());

    let loaded = load_normalized(root);

    assert_eq!(loaded, Some(expected.clone()));
    assert_eq!(read_json(&path(root)), expected);
}

#[test]
fn normalizes_real_flat_v1_shape_without_dropping_the_plugin() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let expected = normalized_flat_v1_doc(root);
    write_json(&path(root), &flat_v1_doc());

    let loaded = load_normalized(root);

    assert_eq!(loaded, Some(expected.clone()));
    assert_eq!(read_json(&path(root)), expected);
}

#[test]
fn malformed_current_recovers_from_the_legacy_filename() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let expected = canonical_doc("weather@acme", "cache/acme/weather/1.0.0", "1.0.0");
    fs::write(path(root), "{not json").unwrap();
    write_json(&legacy_path(root), &expected);

    let loaded = load_normalized(root);

    assert_eq!(loaded, Some(expected.clone()));
    assert_eq!(read_json(&path(root)), expected);
    assert!(legacy_path(root).exists());
}

#[test]
fn persistence_failure_still_returns_the_legacy_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let expected = canonical_doc("weather@acme", "cache/acme/weather/1.0.0", "1.0.0");
    fs::create_dir(path(root)).unwrap();
    write_json(&legacy_path(root), &expected);

    let loaded = load_normalized(root);

    assert_eq!(loaded, Some(expected));
    assert!(path(root).is_dir());
    assert!(legacy_path(root).exists());
}

#[test]
fn concurrent_rerun_is_idempotent_under_v2_filename_migration() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let expected = canonical_doc("weather@acme", "cache/acme/weather/1.0.0", "1.0.0");
    write_json(&legacy_path(&root), &expected);

    let threads: Vec<_> = (0..4)
        .map(|_| {
            let root = root.clone();
            thread::spawn(move || load_normalized(&root))
        })
        .collect();

    let loaded: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();

    assert_eq!(loaded, vec![Some(expected.clone()); 4]);
    assert_eq!(read_json(&path(&root)), expected);
    assert!(!legacy_path(&root).exists());
}

#[test]
fn transaction_restore_previous_restores_legacy_only_state() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let legacy = nested_legacy_doc();

    write_json(&legacy_path(root), &legacy);

    let mut tx = InstalledRegistryTransaction::begin(root).unwrap();
    tx.document_mut()["plugins"]["calc@acme"] = json!([{
        "scope": "user",
        "installPath": "cache/acme/calc/2.0.0",
        "version": "2.0.0"
    }]);
    tx.persist().unwrap();

    assert!(path(root).exists());
    assert!(!legacy_path(root).exists());

    tx.restore_previous().unwrap();

    assert!(!path(root).exists());
    assert_eq!(read_json(&legacy_path(root)), legacy);
}
