//! Every vendored file must still be byte-identical to what `MANIFEST.json`
//! recorded, and every file in `vendor/` must be recorded.

use sha2::{Digest, Sha256};
use std::path::Path;

#[test]
fn vendored_files_match_the_manifest() {
    let vendor = Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(vendor.join("MANIFEST.json")).unwrap())
            .unwrap();
    let sources = manifest["sources"].as_array().unwrap();
    let mut recorded = Vec::new();
    for source in sources {
        let path = source["path"].as_str().unwrap();
        let bytes =
            std::fs::read(vendor.join(path)).unwrap_or_else(|error| panic!("{path}: {error}"));
        let digest = format!("{:x}", Sha256::digest(&bytes));
        assert_eq!(
            digest,
            source["sha256"].as_str().unwrap(),
            "{path} changed; re-vendor and update MANIFEST.json"
        );
        assert!(
            source["origin"]
                .as_str()
                .is_some_and(|origin| !origin.is_empty()),
            "{path}"
        );
        assert!(
            source["license"]
                .as_str()
                .is_some_and(|license| !license.is_empty()),
            "{path}"
        );
        recorded.push(path.to_string());
    }
    let mut on_disk = Vec::new();
    for directory in std::fs::read_dir(&vendor).unwrap() {
        let directory = directory.unwrap().path();
        if directory.is_dir() {
            for file in std::fs::read_dir(&directory).unwrap() {
                let file = file.unwrap().path();
                on_disk.push(
                    file.strip_prefix(&vendor)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    recorded.sort();
    on_disk.sort();
    assert_eq!(
        on_disk, recorded,
        "every vendored file is recorded exactly once"
    );
}

#[test]
fn upstream_runtime_is_still_the_pinned_codex_release() {
    let vendor = Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/codex/visualize.html");
    let html = std::fs::read_to_string(vendor).unwrap();
    assert!(html.starts_with("<!--__INLINE_VISUALIZATION_FRAGMENT__-->"));
    assert!(html.contains("https://unpkg.com/@floating-ui/core@1.7.3/"));
    assert!(html.contains("https://unpkg.com/@floating-ui/dom@1.7.4/"));
    assert!(html.contains("https://unpkg.com/lucide@1.17.0/"));
}
