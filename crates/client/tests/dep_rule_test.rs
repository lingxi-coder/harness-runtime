#![cfg(feature = "adapter")]

//! F1-16 — CI dep-rule assertion.
//!
//! The client crate must not depend on app or example leaves. Module-level
//! protocol and presentation boundaries are checked in `module_boundaries_test`.
//! This test also runs the authoritative workspace dependency gate.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// Workspace root above `crates/client`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("client lives under workspace crates/")
        .to_path_buf()
}

/// `cargo metadata --no-deps` for the workspace, parsed to JSON. Mirrors the
/// invocation in `scripts/check-deps.sh` (offline first; the metadata call does
/// not need a full resolve, only the direct workspace edges).
fn cargo_metadata() -> Value {
    let root = workspace_root();
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version=1", "--no-deps", "--offline"])
        .current_dir(&root)
        .output()
        .expect("spawn `cargo metadata`");

    let raw = if out.status.success() {
        out.stdout
    } else {
        // Fall back to a networked metadata call only if the offline lockfile
        // is incomplete — same fallback `check-deps.sh` uses.
        let out = Command::new(env!("CARGO"))
            .args(["metadata", "--format-version=1", "--no-deps"])
            .current_dir(&root)
            .output()
            .expect("spawn networked `cargo metadata`");
        assert!(
            out.status.success(),
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    };

    serde_json::from_slice(&raw).expect("cargo metadata emits valid JSON")
}

/// Classify a workspace dependency by name exactly as `check_deps.py::classify`
/// does: take its package's `manifest_path` relative to `workspace_root` and
/// check whether the first path segment is `apps`.
fn is_app_crate(meta: &Value, name: &str) -> bool {
    let root = meta["workspace_root"]
        .as_str()
        .expect("workspace_root is a string");
    for pkg in meta["packages"].as_array().expect("packages array") {
        if pkg["name"].as_str() == Some(name) {
            let manifest = pkg["manifest_path"]
                .as_str()
                .expect("manifest_path is a string");
            let rel = Path::new(manifest)
                .strip_prefix(root)
                .expect("manifest path is under workspace root");
            let rel = rel.strip_prefix("crates").unwrap_or(rel);
            return rel
                .components()
                .next()
                .is_some_and(|c| c.as_os_str() == "apps");
        }
    }
    panic!("workspace package `{name}` not found in cargo metadata");
}

/// The set of in-workspace, non-dev dependency names declared by `crate_name`.
/// Mirrors `check_deps.py::ws_deps` (drop self, drop dev edges, keep only deps
/// whose name is a workspace member).
fn workspace_deps(meta: &Value, crate_name: &str) -> Vec<String> {
    let names: std::collections::HashSet<&str> = meta["packages"]
        .as_array()
        .expect("packages array")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();

    for pkg in meta["packages"].as_array().expect("packages array") {
        if pkg["name"].as_str() != Some(crate_name) {
            continue;
        }
        let mut deps: Vec<String> = pkg["dependencies"]
            .as_array()
            .expect("dependencies array")
            .iter()
            .filter(|d| d["kind"].as_str() != Some("dev"))
            .filter_map(|d| d["name"].as_str())
            .filter(|n| *n != crate_name && names.contains(n))
            .map(str::to_owned)
            .collect();
        deps.sort();
        deps.dedup();
        return deps;
    }
    panic!("workspace package `{crate_name}` not found in cargo metadata");
}

/// Assert a crate declares NO `apps/*` (leaf) dependency. The reusable core of
/// the two per-crate tests below.
fn assert_no_app_edge(meta: &Value, crate_name: &str) {
    let offenders: Vec<String> = workspace_deps(meta, crate_name)
        .into_iter()
        .filter(|dep| is_app_crate(meta, dep))
        .collect();
    assert!(
        offenders.is_empty(),
        "`{crate_name}` (engine-tier) must not depend on any apps/* leaf \
         (check_deps.py:99); offending edges: {offenders:?}"
    );
}

#[test]
fn client_has_no_app_edge() {
    let meta = cargo_metadata();
    // Sanity: the crate exists and is itself engine-tier (NOT an app).
    assert!(
        !is_app_crate(&meta, "client"),
        "`client` must be an engine-tier (root-level) crate, not apps/*"
    );
    assert_no_app_edge(&meta, "client");
}

#[test]
fn check_deps_sh_green() {
    let root = workspace_root();
    let script = root.join("scripts").join("check-deps.sh");
    assert!(
        script.is_file(),
        "the authoritative dep gate `{}` must exist",
        script.display()
    );

    let out = Command::new("bash")
        .arg(&script)
        .current_dir(&root)
        .output()
        .expect("spawn scripts/check-deps.sh");

    assert!(
        out.status.success(),
        "scripts/check-deps.sh must exit 0 (no §8.1 dependency violations).\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
