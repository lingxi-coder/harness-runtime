//! [`FileSystem`] contract test suite.
//!
//! Run an impl through this to verify it honors the documented invariants:
//! write-then-read roundtrip, errors for missing files, and workspace
//! boundary enforcement. The suite is intentionally small in M1 — additional
//! cases (append, truncate, mtime, flock) come with the broader 13-trait
//! contract sweep in M2.

use platform_api::FileSystem;

/// Run the standard [`FileSystem`] contract against an impl.
///
/// # Panics
///
/// Panics on the first invariant violation; tests rely on this to fail.
pub async fn filesystem_contract_tests<F: FileSystem>(fs: &F) {
    test_write_then_read(fs).await;
    test_read_nonexistent_returns_error(fs).await;
    test_workspace_boundary_enforced(fs).await;
}

async fn test_write_then_read<F: FileSystem>(fs: &F) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = format!("/tmp/contract-{nanos}.txt");
    fs.write_file(&path, "hi")
        .await
        .expect("write should succeed");
    let content = fs
        .read_file(&path, None, None)
        .await
        .expect("read should succeed");
    assert_eq!(content.content, "hi");
    fs.delete_file(&path).await.ok();
}

async fn test_read_nonexistent_returns_error<F: FileSystem>(fs: &F) {
    let r = fs
        .read_file("/tmp/__never_exists_zzz_unlikely_name", None, None)
        .await;
    assert!(
        r.is_err(),
        "reading a nonexistent path must return an error"
    );
}

#[allow(clippy::unused_async)]
async fn test_workspace_boundary_enforced<F: FileSystem>(fs: &F) {
    assert!(
        !fs.is_within_workspace("/etc/passwd"),
        "/etc/passwd must not be reported as inside the workspace"
    );
}
