//! Shared notify + debouncer file-watch helper.
//!
//! NOTE: This file is a manually-kept copy of
//! `lingxi-code/platforms/posix/src/watch_helper.rs`. Keep the two in sync;
//! see M2-05 plan, "Deliberate divergences" §2 for rationale.
//!
//! Mirrors claude-code's chokidar 4 `awaitWriteFinish` semantics:
//! - `stabilityThreshold = 500ms` (events queued; emitted once writes settle)
//! - `pollInterval = 200ms` (debouncer wake-up rate)
//! - `.git` segment always excluded
//! - Editor swap files (`*.swp`, `~$*`, `4913`) always excluded
//!
//! See: claude-code `src/utils/settings/changeDetector.ts:103-141`,
//! `src/utils/skills/skillChangeDetector.ts:110-131`,
//! `src/utils/hooks/fileChangedWatcher.ts:69-77`.

use futures_core::stream::Stream;
use lingxi_core::host::{FileEvent, FileEventKind, FsError};
use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEvent, DebouncedEventKind, Debouncer};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Default stability threshold (chokidar 4 parity).
pub const DEFAULT_STABILITY_THRESHOLD_MS: u64 = 500;

/// Default poll interval (chokidar 4 parity).
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 200;

/// Watch `dir` recursively with chokidar-like debounce. Returns a stream of
/// [`FileEvent`]s with `.git/` and editor-swap paths filtered out.
///
/// The watcher is kept alive by the spawned task. When the consumer drops the
/// stream the outbound mpsc channel closes; the blocking task detects the
/// closed channel on its next wake — it polls `tx.is_closed()` on a bounded
/// `recv_timeout` cadence rather than parking forever on `recv()` — then drops
/// the `Debouncer` (RAII) and releases the OS handle. This guarantees teardown
/// even on a *quiet* directory that never emits another settled event (a plain
/// `recv()` would leave the thread + `notify` FDs parked indefinitely there).
///
/// `async` is required by the `FileSystem::watch` trait signature even
/// though no `.await` happens before the spawn — keep the signature stable
/// for downstream callers.
#[allow(clippy::unused_async)]
pub async fn watch_dir_with_debounce(
    dir: &str,
    stability_threshold_ms: u64,
    poll_interval_ms: u64,
) -> Result<Pin<Box<dyn Stream<Item = FileEvent> + Send>>, FsError> {
    let dir_path = std::path::PathBuf::from(dir);
    if !dir_path.exists() {
        return Err(FsError::Io(format!("watch target does not exist: {dir}")));
    }

    // mpsc bridge from the (blocking) notify callback into async land.
    let (tx, rx) = mpsc::channel::<FileEvent>(128);
    // Do not report a usable stream until the native watcher is armed. Without
    // this handshake callers can mutate the directory before
    // ReadDirectoryChangesW/FSEvents/inotify has registered and lose the first
    // event permanently.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

    // Spawn a blocking task to host the debouncer; notify's callback runs on
    // its own thread, so we can't keep the Debouncer on the async runtime
    // directly. The task exits when `tx` is dropped (consumer closed).
    let watch_root = dir_path.clone();
    tokio::task::spawn_blocking(move || {
        let mut ready_tx = Some(ready_tx);
        // Test-only liveness probe: increments on entry, decrements on the
        // closure's return (any exit path). Lets lifecycle tests assert the
        // blocking thread is actually released after a consumer drop, without
        // depending on FSEvents delivery timing.
        #[cfg(test)]
        let _probe = test_probe::Guard::new();

        let stability = Duration::from_millis(stability_threshold_ms);
        let poll = Duration::from_millis(poll_interval_ms);

        // notify-debouncer-mini 0.6 uses a single `timeout` (stability
        // window). We pick the larger of stability + poll to approximate
        // chokidar's behavior where pollInterval governs *re-checks* until
        // stability is reached.
        let tick = stability.max(poll);

        let (event_tx, event_rx) =
            std::sync::mpsc::channel::<notify_debouncer_mini::DebounceEventResult>();
        let mut debouncer: Debouncer<notify::RecommendedWatcher> =
            match new_debouncer(tick, event_tx) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("notify debouncer init failed: {e}");
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(format!("notify debouncer init failed: {e}")));
                    }
                    return;
                }
            };

        // Seed the seen-set from a pre-watch recursive scan (respecting the
        // same `should_skip` filter) so a file that appears AFTER the watch
        // starts classifies as Created — mirroring chokidar's `ignoreInitial`
        // initial scan, which records existing paths so later `add` events are
        // distinguishable from `change`. Seed BEFORE arming the watch so the
        // racy window biases toward Created (a path created after the scan but
        // before the watch arms is simply not observed; one created after the
        // watch arms is absent from `seen` → Created).
        let mut seen: HashSet<PathBuf> = HashSet::new();
        seed_seen_set(&watch_root, &mut seen);

        if let Err(e) = debouncer
            .watcher()
            .watch(&watch_root, RecursiveMode::Recursive)
        {
            tracing::error!("notify watch({:?}) failed: {e}", watch_root);
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Err(format!("notify watch({watch_root:?}) failed: {e}")));
            }
            return;
        }
        if let Some(tx) = ready_tx.take() {
            let _ = tx.send(Ok(()));
        }

        // Pump events until the async receiver closes. A quiet directory never
        // wakes `recv()`, so we use `recv_timeout(tick)` and re-check
        // `tx.is_closed()` on every wake: a consumer drop is detected within
        // one `tick` even with zero settled events (Timeout => re-check-or-loop,
        // Disconnected => the debouncer is gone, exit).
        loop {
            if tx.is_closed() {
                break;
            }
            let res = match event_rx.recv_timeout(tick) {
                Ok(res) => res,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let events = match res {
                Ok(ev) => ev,
                Err(err) => {
                    tracing::warn!("notify error: {err:?}");
                    continue;
                }
            };
            for ev in events {
                if should_skip(&ev.path) {
                    continue;
                }
                // Mirror chokidar 4 `awaitWriteFinish`: only emit a final
                // settled event. `AnyContinuous` indicates writes are still
                // arriving inside the stability window — suppress it so
                // rapid bursts collapse into one terminal event.
                if matches!(ev.kind, DebouncedEventKind::AnyContinuous) {
                    continue;
                }
                let kind = classify(&ev, &mut seen);
                if tx
                    .blocking_send(FileEvent {
                        path: ev.path.clone(),
                        kind,
                    })
                    .is_err()
                {
                    // Consumer dropped — exit cleanly.
                    return;
                }
            }
        }
        drop(debouncer); // explicit RAII release
    });

    match ready_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(message)) => return Err(FsError::Io(message)),
        Err(_) => {
            return Err(FsError::Io(
                "native file watcher exited before initialization completed".to_string(),
            ));
        }
    }

    Ok(Box::pin(ReceiverStream::new(rx)))
}

/// Path filter mirroring chokidar's `ignored` predicate in claude-code.
///
/// - Excludes any path whose components contain a `.git` segment.
/// - Excludes editor swap/lock files: `*.swp` (vim), `~$*` (Word/Excel),
///   `4913` (vim's open-test file).
fn should_skip(path: &Path) -> bool {
    // .git segment anywhere in the path
    if path.components().any(|c| c.as_os_str() == ".git") {
        return true;
    }
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        // vim swap file (case-insensitive: `.swp`, `.SWP`)
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("swp"))
        {
            return true;
        }
        if name.starts_with("~$") {
            return true;
        }
        if name == "4913" {
            return true;
        }
    }
    false
}

/// Recursively record every non-skipped file under `root` into `seen`,
/// pruning `should_skip` directories (notably `.git`). Best-effort: an
/// unreadable directory is silently skipped. Mirrors chokidar's initial scan
/// under `ignoreInitial` (the scan populates the known-paths set without
/// emitting `add` events for pre-existing files).
fn seed_seen_set(root: &Path, seen: &mut HashSet<PathBuf>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if should_skip(&path) {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(_) => {
                    seen.insert(path);
                }
                Err(_) => {}
            }
        }
    }
}

/// Classify a settled debounced event into a [`FileEventKind`], updating the
/// per-watcher `seen`-set.
///
/// notify-debouncer-mini 0.6 collapses Created/Modified/Removed into a single
/// [`DebouncedEventKind::Any`], so the kind cannot be read off the event. We
/// distinguish using the `seen`-set (seeded by [`seed_seen_set`]) plus a live
/// existence probe — reproducing chokidar's add/change/unlink distinction:
/// - exists & NOT previously seen  → [`FileEventKind::Created`] (`add`); record it.
/// - exists & previously seen      → [`FileEventKind::Modified`] (`change`).
/// - does not exist                → [`FileEventKind::Deleted`] (`unlink`); forget it.
///
/// `DebouncedEventKind` is `#[non_exhaustive]`; a non-`Any` variant (other than
/// `AnyContinuous`, already filtered upstream) falls through to `Modified`
/// (most conservative).
fn classify(ev: &DebouncedEvent, seen: &mut HashSet<PathBuf>) -> FileEventKind {
    if matches!(ev.kind, DebouncedEventKind::Any) {
        if ev.path.exists() {
            // `HashSet::insert` returns `true` when the path was NOT already
            // present — i.e. a brand-new file → Created; otherwise Modified.
            if seen.insert(ev.path.clone()) {
                FileEventKind::Created
            } else {
                FileEventKind::Modified
            }
        } else {
            seen.remove(&ev.path);
            FileEventKind::Deleted
        }
    } else {
        FileEventKind::Modified
    }
}

/// Test-only liveness probe for the blocking watch thread. Not compiled into
/// production builds. Increments a global counter on entry to the blocking
/// closure and decrements on exit, so lifecycle tests can observe that the
/// thread is actually released after a consumer drop.
#[cfg(test)]
pub(crate) mod test_probe {
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(crate) static LIVE: AtomicUsize = AtomicUsize::new(0);

    pub(crate) struct Guard;

    impl Guard {
        pub(crate) fn new() -> Self {
            LIVE.fetch_add(1, Ordering::SeqCst);
            Guard
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            LIVE.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    fn any_event(path: &str) -> DebouncedEvent {
        DebouncedEvent::new(PathBuf::from(path), DebouncedEventKind::Any)
    }

    #[test]
    fn should_skip_dot_git_root() {
        assert!(should_skip(&PathBuf::from("/repo/.git/HEAD")));
        assert!(should_skip(&PathBuf::from(".git/config")));
        assert!(!should_skip(&PathBuf::from("/repo/src/lib.rs")));
    }

    #[test]
    fn should_skip_editor_swap_files() {
        assert!(should_skip(&PathBuf::from("/repo/foo.txt.swp")));
        assert!(should_skip(&PathBuf::from("/repo/~$report.docx")));
        assert!(should_skip(&PathBuf::from("/repo/4913")));
        assert!(!should_skip(&PathBuf::from("/repo/swp_file.txt")));
    }

    #[test]
    fn seed_seen_set_records_files_and_prunes_dot_git() {
        // A real dir tree: one tracked file, a nested tracked file, and a
        // `.git/` segment + a swap file that must be pruned by `should_skip`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".env"), "A=1").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/cfg.toml"), "x=1").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: x").unwrap();
        std::fs::write(root.join("draft.txt.swp"), "vim").unwrap();

        let mut seen: HashSet<PathBuf> = HashSet::new();
        seed_seen_set(root, &mut seen);

        assert!(seen.contains(&root.join(".env")), "top-level file recorded");
        assert!(
            seen.contains(&root.join("sub/cfg.toml")),
            "nested file recorded"
        );
        assert!(
            !seen.contains(&root.join(".git/HEAD")),
            ".git segment pruned"
        );
        assert!(
            !seen.contains(&root.join("draft.txt.swp")),
            "swap file skipped"
        );
    }

    #[test]
    fn classify_first_sighting_is_created_then_modified() {
        // A physically-present file that is NOT in the seen-set classifies as
        // Created on first sight (and is recorded), then Modified thereafter —
        // the parity fix for a file that appears after the watch starts.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("new.txt");
        std::fs::write(&p, "hi").unwrap();
        let ps = p.to_string_lossy().into_owned();

        let mut seen: HashSet<PathBuf> = HashSet::new();
        assert_eq!(
            classify(&any_event(&ps), &mut seen),
            FileEventKind::Created,
            "unseen existing file → Created (add)"
        );
        assert!(seen.contains(&p), "Created insertion recorded");
        assert_eq!(
            classify(&any_event(&ps), &mut seen),
            FileEventKind::Modified,
            "second event on the now-seen file → Modified (change)"
        );
    }

    #[test]
    fn classify_seen_file_is_modified_and_missing_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("known.txt");
        std::fs::write(&p, "hi").unwrap();
        let ps = p.to_string_lossy().into_owned();

        // Pre-seed as an existing file (chokidar's initial scan) → Modified.
        let mut seen: HashSet<PathBuf> = HashSet::new();
        seen.insert(p.clone());
        assert_eq!(
            classify(&any_event(&ps), &mut seen),
            FileEventKind::Modified,
            "pre-seeded existing file → Modified (change), never Created"
        );

        // Now delete it: the classify existence probe → Deleted, and the path
        // is forgotten so a later re-create classifies as Created again.
        std::fs::remove_file(&p).unwrap();
        assert_eq!(
            classify(&any_event(&ps), &mut seen),
            FileEventKind::Deleted,
            "missing path → Deleted (unlink)"
        );
        assert!(!seen.contains(&p), "Deleted removal forgets the path");

        std::fs::write(&p, "again").unwrap();
        assert_eq!(
            classify(&any_event(&ps), &mut seen),
            FileEventKind::Created,
            "re-created after unlink → Created again"
        );
    }

    #[tokio::test]
    async fn blocking_task_exits_after_consumer_drop_on_quiet_dir() {
        // The leak regression: on a directory that never emits a settled event,
        // dropping the stream must still release the blocking watch thread. We
        // observe the thread via the test-only liveness probe rather than any
        // FSEvents delivery, so this is deterministic (no fseventsd timing).
        let dir = tempfile::tempdir().unwrap();
        let baseline = test_probe::LIVE.load(Ordering::SeqCst);

        // Small tick (stability=80ms, poll=40ms) so the cancel is detected fast.
        let stream = watch_dir_with_debounce(dir.path().to_str().unwrap(), 80, 40)
            .await
            .unwrap();

        // Wait for the blocking closure to actually start (probe > baseline).
        let mut started = false;
        for _ in 0..200 {
            if test_probe::LIVE.load(Ordering::SeqCst) > baseline {
                started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(started, "blocking watch thread should have started");

        // Drop the stream with ZERO events delivered — the quiet-dir case.
        drop(stream);

        // The thread must exit within a bounded number of ticks (tick=80ms);
        // poll the probe back to baseline. Generous ceiling to avoid CI flake.
        let mut released = false;
        for _ in 0..400 {
            if test_probe::LIVE.load(Ordering::SeqCst) <= baseline {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            released,
            "blocking watch thread must be released after the consumer drops \
             the stream, even on a quiet directory (no FD/thread leak)"
        );
    }
}
