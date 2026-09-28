//! Phase 1a — the annotation pipeline hands the agent a cropped JPEG BY PATH,
//! so the mobile Read tool must decode images. Without the `image-read`
//! feature it falls to the NUL scan and returns `format_binary`: the tool call
//! succeeds and the agent sees no picture.

use std::path::Path;

/// Regression guard: verifies the mobile profile independently declares `image-read`.
///
/// This test is immune to workspace feature unification. When CI runs
/// `cargo test --workspace`, harness-runtime::desktop requests tool-file with
/// `features = ["image-read"]` making the feature available globally,
/// which would hide the bug if we relied on build-time const checks alone.
/// Asserting the manifest itself is the only gate that actually fails when
/// the feature is stripped from harness-runtime::mobile's dependency.
#[test]
fn engine_mobile_manifest_declares_tool_file_image_read() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let manifest_path = Path::new(manifest_dir).join("Cargo.toml");
    let manifest_text = std::fs::read_to_string(&manifest_path).expect(
        "Failed to read engine-mobile Cargo.toml; test must run via `cargo test -p harness-runtime --features mobile`",
    );

    let manifest: toml::Value = toml::from_str(&manifest_text).expect("valid Cargo manifest");
    let mobile = manifest["features"]["mobile"]
        .as_array()
        .expect("mobile runtime profile");
    assert!(
        mobile
            .iter()
            .any(|feature| feature.as_str() == Some("tool-file/image-read")),
        "the mobile profile must enable image reading independently of desktop feature unification"
    );
}

/// Documentation test: verifies the build has the feature available.
///
/// This test documents the build-time state but does NOT protect against
/// regression; see `engine_mobile_manifest_declares_tool_file_image_read` for
/// the actual guard. Under workspace builds, harness-runtime::desktop's feature
/// declaration will make this pass even if harness-runtime::mobile strips its own.
#[test]
fn engine_mobile_builds_tool_file_with_image_read() {
    assert!(
        tool_file::IMAGE_READ_ENABLED,
        "engine-mobile depends on tool-file without the `image-read` feature, \
         so Read on an annotation .jpg returns the binary notice instead of an \
         image. Add features = [\"image-read\"] to the tool-file dependency."
    );
}
