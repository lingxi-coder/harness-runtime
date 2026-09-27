//! Verifies the FS watcher debounces rapid writes per chokidar's
//! awaitWriteFinish.stabilityThreshold (500ms / 200ms defaults).

use futures_util::StreamExt;
use platform_api::FileSystem;
use platform_posix::PosixFileSystem;
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[tokio::test]
async fn rapid_writes_collapse_to_single_event() {
    let dir = tempdir().unwrap();
    // Canonicalize to dereference /private/var → /var on macOS and prevent
    // path-mismatch surprises when notify reports the resolved absolute path.
    let dir_path = std::fs::canonicalize(dir.path()).unwrap();
    let fs = PosixFileSystem::new(dir_path.clone());

    let mut stream = fs.watch(dir_path.to_str().unwrap()).await.unwrap();

    // Settle the watcher. FSEvents (macOS) needs a beat to register its
    // CFRunLoop source before mutations are observed.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let target = dir_path.join("rapid.txt");

    // 3 writes within the 500ms stability window.
    let burst_start = Instant::now();
    for i in 0..3 {
        std::fs::write(&target, format!("v{i}")).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let burst_end = burst_start.elapsed();
    assert!(
        burst_end < Duration::from_millis(500),
        "burst overshot stability window: {burst_end:?}"
    );

    // notify-debouncer-mini's `batch_mode` (on by default) lets events be
    // delivered up to 2x the timeout after the last write. With 500ms
    // stability threshold that means the final `Any` event may arrive up to
    // 1s after the last write. We wait a generous 1500ms to ride out that
    // window, then drain everything available within an extra 1000ms.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Drain events that arrive within a short window. We can't use
    // `tokio::time::timeout` around the whole `while let Some` loop because
    // cancellation would drop already-pushed entries; instead we wrap each
    // `next().await` in a per-iteration timeout and break when it fires.
    let mut drain = Vec::new();
    // Loop until either the stream closes or the per-iteration timeout fires.
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(1000), stream.next()).await
    {
        if ev.path.file_name().and_then(|s| s.to_str()) == Some("rapid.txt") {
            drain.push(ev);
        }
    }

    assert_eq!(
        drain.len(),
        1,
        "expected exactly 1 debounced event, got {}: {drain:?}",
        drain.len()
    );
}
