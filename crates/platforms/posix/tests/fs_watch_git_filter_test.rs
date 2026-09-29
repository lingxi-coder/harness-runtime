//! Verifies the FS watcher excludes `.git/**` paths (parity with claude-code
//! `changeDetector.ts:118` which calls .split(sep).some(d => d === '.git')).

use futures_util::StreamExt;
use lingxi_core::host::FileSystem;
use platform_posix::PosixFileSystem;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::test]
async fn watch_excludes_dot_git_directory() {
    let dir = tempdir().unwrap();
    // Canonicalize to dereference /private/var → /var on macOS so notify's
    // reported paths align with our filter checks.
    let dir_path = std::fs::canonicalize(dir.path()).unwrap();

    // Create .git/ subdir BEFORE starting watcher
    let git_dir = dir_path.join(".git");
    std::fs::create_dir(&git_dir).unwrap();

    let fs = PosixFileSystem::new(dir_path.clone());
    let mut stream = fs.watch(dir_path.to_str().unwrap()).await.unwrap();

    // Settle the watcher. FSEvents (macOS) needs a beat to register its
    // CFRunLoop source before mutations are observed.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Write to .git/foo — must NOT appear in stream
    std::fs::write(git_dir.join("foo"), b"x").unwrap();

    // Write to top-level bar.txt — MUST appear after stability window
    std::fs::write(dir_path.join("bar.txt"), b"y").unwrap();

    // Wait for the debouncer's stability window (500ms) plus slack
    // (batch_mode can extend up to 2x). Then per-iter timeout drains events
    // without throwing them away on cancel.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let mut collect = Vec::new();
    // Loop until either the stream closes or the per-iteration timeout fires.
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(1000), stream.next()).await
    {
        collect.push(ev);
    }

    let saw_git = collect
        .iter()
        .any(|e| e.path.components().any(|c| c.as_os_str() == ".git"));
    assert!(!saw_git, "watcher leaked .git event: {collect:?}");

    let saw_bar = collect
        .iter()
        .any(|e| e.path.file_name().and_then(|s| s.to_str()) == Some("bar.txt"));
    assert!(saw_bar, "watcher missed bar.txt event: {collect:?}");
}
