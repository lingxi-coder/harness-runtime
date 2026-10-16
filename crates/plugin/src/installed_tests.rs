use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use serde_json::{json, Value};

use super::{load_normalized, path, InstalledRegistryTransaction};

fn obsolete_path(root: &Path) -> PathBuf {
    root.join("installed_plugins_v2.json")
}

fn current_doc(id: &str, install_path: &str, version: &str) -> Value {
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

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

#[test]
fn reads_current_v2_registry_without_rewriting_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let expected = current_doc("weather@acme", "cache/acme/weather/1.0.0", "1.0.0");
    write_json(&path(root), &expected);
    let original_bytes = fs::read(path(root)).unwrap();

    let loaded = load_normalized(root);

    assert_eq!(loaded, Some(expected));
    assert_eq!(fs::read(path(root)).unwrap(), original_bytes);
}

#[test]
fn obsolete_filename_is_ignored_and_left_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let obsolete = current_doc("weather@acme", "cache/acme/weather/1.0.0", "1.0.0");
    write_json(&obsolete_path(root), &obsolete);
    let original_bytes = fs::read(obsolete_path(root)).unwrap();

    assert_eq!(load_normalized(root), None);
    assert!(!path(root).exists());
    assert_eq!(fs::read(obsolete_path(root)).unwrap(), original_bytes);
}

#[test]
fn flat_v1_payload_is_not_migrated_or_rewritten() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let v1 = json!({
        "plugins": {
            "weather@acme": {
                "version": "1.0.0",
                "installedAt": "2026-08-30T00:00:00.000Z",
                "lastUpdated": "2026-08-31T00:00:00.000Z"
            }
        }
    });
    write_json(&path(root), &v1);
    let original_bytes = fs::read(path(root)).unwrap();

    assert_eq!(load_normalized(root), None);
    assert_eq!(fs::read(path(root)).unwrap(), original_bytes);
}

#[test]
fn nested_marketplace_payload_is_not_migrated_or_rewritten() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let nested = json!({
        "plugins": {
            "acme": {
                "weather": {
                    "version": "1.0.0",
                    "installPath": "cache/acme/weather/1.0.0",
                    "added": "2026-08-31T00:00:00.000Z"
                }
            }
        }
    });
    write_json(&path(root), &nested);
    let original_bytes = fs::read(path(root)).unwrap();

    assert_eq!(load_normalized(root), None);
    assert_eq!(fs::read(path(root)).unwrap(), original_bytes);
}

#[test]
fn concurrent_current_v2_reads_are_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let expected = current_doc("weather@acme", "cache/acme/weather/1.0.0", "1.0.0");
    write_json(&path(&root), &expected);

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

    assert_eq!(loaded, vec![Some(expected); 4]);
}

#[test]
fn transaction_rollback_restores_current_v2_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let original = current_doc("weather@acme", "cache/acme/weather/1.0.0", "1.0.0");
    write_json(&path(root), &original);
    let original_bytes = fs::read(path(root)).unwrap();

    let mut tx = InstalledRegistryTransaction::begin(root).unwrap();
    tx.document_mut()["plugins"]["calc@acme"] = json!([{
        "scope": "user",
        "installPath": "cache/acme/calc/2.0.0",
        "version": "2.0.0"
    }]);
    tx.persist().unwrap();
    assert_ne!(fs::read(path(root)).unwrap(), original_bytes);

    tx.restore_previous().unwrap();

    assert_eq!(fs::read(path(root)).unwrap(), original_bytes);
}
