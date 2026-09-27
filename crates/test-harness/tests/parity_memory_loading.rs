//! Parity fixture: LINGXI.md hierarchy walk + the loader's stat guard.
//!
//! Locks the shape of `lingxi_md::hierarchy::walk` + `lingxi_md::loader::load_file`
//! against claude-code. Each scenario builds a tempdir layout and asserts both
//! the walk order and which files the reader drops.
//!
//! ⚠️ CORRECTED. This header used to read "GAP 4: claude-code reads every file
//! whole (no size drop), so no file is ever skipped for size", and the harness
//! below hard-coded `skipped` to an empty vec while PANICKING on any load
//! error — so the fixture could not have expressed a skip even if a scenario
//! wanted one. Both the claim and the harness came from leaked TS
//! (`loader.ts:14-38`). The 2.1.220 binary skips anything non-regular or over
//! `ELu = 4194304` (`EG` @229022173, called from `Eds` @230805636).

use memory::lingxi_md::hierarchy::walk;
use memory::lingxi_md::loader::load_file;
use memory::MAX_MEMORY_FILE_SIZE;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct Fixture {
    scenarios: Vec<Scenario>,
}

#[derive(Deserialize)]
struct Scenario {
    name: String,
    /// Map of relative path → body. The literal "OVERSIZED:11mb" is replaced
    /// by 11 MB of `x` bytes at write time (keeps the fixture file small).
    layout: BTreeMap<String, String>,
    cwd: String,
    home: String,
    #[serde(default)]
    expected_order: Vec<String>,
    #[serde(default)]
    expected_skipped: Vec<String>,
}

fn materialize(layout: &BTreeMap<String, String>, root: &std::path::Path) {
    for (rel, body) in layout {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        if body == "OVERSIZED:11mb" {
            let bytes = vec![b'x'; MAX_MEMORY_FILE_SIZE + 1024 * 1024];
            fs::write(&full, &bytes).unwrap();
        } else {
            fs::write(&full, body.as_bytes()).unwrap();
        }
    }
}

#[test]
fn memory_loading_matches_claude_code() {
    let fx: Fixture = load_fixture("memory_loading");

    for sc in &fx.scenarios {
        let root = tempfile::TempDir::new().expect("tempdir");
        materialize(&sc.layout, root.path());
        let cwd = root.path().join(&sc.cwd);
        // The walk skips dirs that don't exist; if scenarios reference a
        // cwd that wasn't in the layout, create it now.
        fs::create_dir_all(&cwd).unwrap();
        let home = root.path().join(&sc.home);
        fs::create_dir_all(&home).unwrap();

        // Managed tier intentionally not exercised here (it lives at an absolute
        // system path); pass `None` for a hermetic walk.
        let h = walk(&cwd, &home, None);
        let mut loaded: Vec<PathBuf> = Vec::new();
        let mut skipped: Vec<PathBuf> = Vec::new();
        for entry in &h.entries {
            match load_file(&entry.path, None) {
                Ok(_) => loaded.push(entry.path.clone()),
                // The oracle's `null` return: not a regular file, or over
                // `ELu`. Recorded, not fatal — one skipped file must not stop
                // the rest of the hierarchy loading.
                Err(memory::lingxi_md::loader::LoaderError::FileTooLarge { .. }) => {
                    skipped.push(entry.path.clone());
                }
                // Anything else (ENOENT, perms, non-UTF-8) is a fixture bug:
                // the walk only yields paths it just saw on disk.
                Err(other) => panic!("scenario {}: unexpected error {other:?}", sc.name),
            }
        }

        // Compare on relative paths (strip the tempdir root prefix).
        let to_rel = |p: &PathBuf| -> String {
            p.strip_prefix(root.path())
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        };
        let got_order: Vec<String> = loaded.iter().map(to_rel).collect();
        let got_skipped: Vec<String> = skipped.iter().map(to_rel).collect();
        assert_eq!(
            got_order, sc.expected_order,
            "scenario {}: walk order must match claude-code",
            sc.name
        );
        assert_eq!(
            got_skipped, sc.expected_skipped,
            "scenario {}: skipped-due-to-size set must match",
            sc.name
        );
    }
}
