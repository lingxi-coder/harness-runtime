//! Smoke test: create + modify + delete a file inside the watched dir, verify
//! at least one event arrives. Detailed parity tests live in posix/.

use futures_util::StreamExt;
use platform_api::FileSystem;
use platform_windows::WindowsFileSystem;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::test]
async fn create_modify_delete_emits_events() {
    let dir = tempdir().unwrap();
    // Canonicalize so notify's reported path matches what we compare against.
    let dir_path = std::fs::canonicalize(dir.path()).unwrap();
    let fs = WindowsFileSystem::new(dir_path.clone());
    let mut stream = fs.watch(dir_path.to_str().unwrap()).await.unwrap();

    // Settle the watcher. ReadDirectoryChangesW (Windows) / FSEvents (macOS) /
    // inotify (Linux) all need a beat to register before mutations are
    // observed reliably.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let target = dir_path.join("hello.txt");
    std::fs::write(&target, b"hi").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&target, b"hi there").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::remove_file(&target).unwrap();

    // Wait for debouncer to flush (stability 500ms + batch slack up to 2x).
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let mut events = Vec::new();
    // Loop until either the stream closes or the per-iteration timeout fires.
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(1000), stream.next()).await
    {
        if ev.path.file_name().and_then(|s| s.to_str()) == Some("hello.txt") {
            events.push(ev);
        }
    }

    assert!(!events.is_empty(), "no FS events arrived");
}
