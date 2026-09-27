//! The accepted-divergence register, checked against the tree it describes.
//!
//! The register itself explains why it exists; this file is the half that makes
//! it more than prose. Every active entry must still point at something real:
//! a `present` anchor fails when the code it names is renamed or deleted, and an
//! `absent` anchor fails when the thing we said we deliberately do not have
//! turns up. Without the second kind an entry can be false for weeks and read
//! exactly like an entry that is true, which is what happened to the plugin
//! function-hook row.

use std::path::{Path, PathBuf};

use serde_json::Value;

const REGISTER: &str = include_str!("../src/parity/fixtures/accepted_divergences.json");

fn engine_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("test-harness sits under the workspace crates directory")
        .to_path_buf()
}

fn register() -> Value {
    serde_json::from_str(REGISTER).expect("accepted_divergences.json parses")
}

fn entries() -> Vec<Value> {
    register()["divergences"]
        .as_array()
        .expect("`divergences` is an array")
        .clone()
}

fn field<'a>(entry: &'a Value, name: &str) -> &'a str {
    entry[name]
        .as_str()
        .unwrap_or_else(|| panic!("{}: `{name}` must be a string", entry["id"]))
}

/// Count occurrences of `text` under `path`, which may be a file or a directory.
/// Source extensions only: a divergence is about what the code does, and the
/// register's own prose naturally mentions every string it forbids.
fn occurrences(path: &Path, text: &str) -> usize {
    if path.is_file() {
        return std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
            .matches(text)
            .count();
    }
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = std::fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("read directory {}: {error}", dir.display()));
        for entry in read {
            let child = entry.expect("read source directory entry").path();
            if child.is_dir() {
                if child.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(child);
                continue;
            }
            let is_source = child
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e, "rs" | "json" | "toml"));
            // The register and this checker both spell every forbidden string.
            let is_self = child.ends_with("accepted_divergences.json")
                || child.ends_with("parity_accepted_divergences.rs");
            if is_source && !is_self {
                total += std::fs::read_to_string(&child)
                    .unwrap_or_else(|error| panic!("read {}: {error}", child.display()))
                    .matches(text)
                    .count();
            }
        }
    }
    total
}

#[test]
fn every_entry_is_identified_reasoned_and_dated() {
    let mut ids: Vec<String> = Vec::new();
    for entry in entries() {
        let id = field(&entry, "id").to_string();
        assert!(!id.is_empty(), "an entry has an empty id");
        assert!(!ids.contains(&id), "duplicate divergence id: {id}");
        for required in ["finding", "reason", "decided_on", "decided_by", "kind"] {
            assert!(
                !field(&entry, required).trim().is_empty(),
                "{id}: `{required}` must not be empty — an unreasoned exemption is \
                 indistinguishable from an oversight"
            );
        }
        assert!(
            matches!(field(&entry, "decided_by"), "user" | "oracle"),
            "{id}: `decided_by` says who may revisit this; it must be `user` or `oracle`"
        );
        ids.push(id);
    }
    assert!(!ids.is_empty(), "the register must not be empty");
}

#[test]
fn every_active_entry_still_points_at_the_tree() {
    let root = engine_root();
    let mut checked = 0_usize;
    for entry in entries() {
        let id = field(&entry, "id");
        if field(&entry, "status") != "active" {
            continue;
        }
        let anchors = entry["anchors"]
            .as_array()
            .unwrap_or_else(|| panic!("{id}: `anchors` must be an array"));
        assert!(
            !anchors.is_empty(),
            "{id}: an active entry needs at least one anchor, or nothing can tell \
             whether it is still true"
        );
        for anchor in anchors {
            let kind = field(anchor, "kind");
            let rel = field(anchor, "path");
            let text = field(anchor, "text");
            let path = root.join(rel);
            assert!(path.exists(), "{id}: anchor path no longer exists: {rel}");
            let hits = occurrences(&path, text);
            match kind {
                "present" => assert!(
                    hits >= 1,
                    "{id}: `present` anchor is gone — {rel} no longer contains {text:?}. \
                     Either the code moved (repoint the anchor) or the divergence ended \
                     (retire the entry)."
                ),
                "absent" => assert_eq!(
                    hits, 0,
                    "{id}: `absent` anchor fired — {rel} now contains {text:?}. \
                     This entry says the port deliberately does NOT have that; if it \
                     was built, retire the entry instead of leaving it to rot."
                ),
                other => panic!("{id}: unknown anchor kind {other:?}"),
            }
            checked += 1;
        }
    }
    assert!(
        checked >= 10,
        "only {checked} anchors were checked; the register lost its coverage"
    );
}

#[test]
fn a_retired_entry_says_when_and_why() {
    for entry in entries() {
        let id = field(&entry, "id");
        if field(&entry, "status") != "retired" {
            continue;
        }
        for required in ["retired_on", "retired_because"] {
            assert!(
                !field(&entry, required).trim().is_empty(),
                "{id}: a retired entry must record `{required}` — a divergence that \
                 silently disappears reads the same as one nobody checked"
            );
        }
    }
}
