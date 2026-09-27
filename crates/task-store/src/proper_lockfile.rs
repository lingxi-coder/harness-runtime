//! Cross-process advisory lock, a minimal port of the `proper-lockfile`
//! library that claude-code embeds and uses for its task store (`utils/tasks.ts`
//! `LOCK_OPTIONS`).
//!
//! ## Why not the in-tree `flock_exclusive` seam
//!
//! `traits`/`platforms-posix` already expose an `fs2` advisory `flock`, but
//! claude-code's task store serialises concurrent processes with
//! `proper-lockfile`, which is **`mkdir`-based**: the lock artifact is a
//! *directory* `<target>.lock`. Matching that on-disk mechanism (and its
//! staleness semantics) is what lets a LingXi process and a real claude-code
//! process interoperate on one tasks dir — an `flock` guard would exclude
//! neither.
//!
//! ## Byte-faithful with claude-code 2.1.207
//!
//! * getLockFile (`pIr(e,t){return t.lockfilePath||`${e}.lock`}`): the lock
//!   directory is exactly `<target>.lock`.
//! * `LOCK_OPTIONS` (`e6r={retries:{retries:30,minTimeout:5,maxTimeout:100},…}`)
//!   → [`RETRIES`] / [`MIN_TIMEOUT_MS`] / [`MAX_TIMEOUT_MS`], exponential
//!   backoff factor 2 (the `retry` library default).
//! * `stale` defaults to `1e4` ms (proper-lockfile default; `e6r` sets no
//!   `stale`) → [`STALE_MS`]; on `EEXIST` the holder's `mtime` is compared to
//!   `Date.now()` and a directory older than `stale` is stolen
//!   (`if(Date.now()-r.mtimeMs<W7u)return!1; … await e.unlink(t)`).
//!
//! ## Deliberate simplification
//!
//! proper-lockfile also spawns a background timer that refreshes the lock
//! `mtime` every `stale/2` ms and fires `onCompromised` if the refresh finds
//! the directory stolen. Every task-store mutation here completes in well under
//! that window (a `mkdir`/read/write/`rmdir` round-trip), so the refresh timer
//! is unobservable and is omitted; the lock is simply acquired, held for the
//! operation, and released.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// `LOCK_OPTIONS.retries.retries` (claude-code `e6r`): number of retries after
/// the initial attempt.
const RETRIES: u32 = 30;
/// `LOCK_OPTIONS.retries.minTimeout` in ms.
const MIN_TIMEOUT_MS: u64 = 5;
/// `LOCK_OPTIONS.retries.maxTimeout` in ms.
const MAX_TIMEOUT_MS: u64 = 100;
/// Exponential backoff factor — the `retry` library default (`factor:2`).
const FACTOR: u64 = 2;
/// proper-lockfile `stale` default (`1e4` ms): a lock directory whose `mtime`
/// is older than this is considered abandoned and may be stolen.
const STALE_MS: u64 = 10_000;

/// Held cross-process lock; releases (`rmdir`s the lock directory) on drop.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct LockGuard {
    lock_dir: PathBuf,
    released: bool,
}

impl LockGuard {
    fn release_inner(&mut self) {
        if !self.released {
            // proper-lockfile release = rmdir the lock directory.
            let _ = std::fs::remove_dir(&self.lock_dir);
            self.released = true;
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.release_inner();
    }
}

/// The lock directory path for `target` (`<target>.lock`, matching claude-code
/// `getLockFile`).
fn lock_dir_for(target: &Path) -> PathBuf {
    let mut os = target.as_os_str().to_os_string();
    os.push(".lock");
    PathBuf::from(os)
}

/// One `mkdir` attempt: `Ok(true)` acquired, `Ok(false)` already held
/// (`EEXIST`), `Err` for any other filesystem error.
fn try_acquire(lock_dir: &Path) -> io::Result<bool> {
    match std::fs::create_dir(lock_dir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether the existing lock directory is stale (`Date.now()-mtime >= stale`).
/// A missing directory or a future `mtime` is treated as *not* stale (the
/// caller retries `mkdir` regardless).
fn is_stale(lock_dir: &Path, stale: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(lock_dir) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    match SystemTime::now().duration_since(mtime) {
        Ok(age) => age >= stale,
        Err(_) => false,
    }
}

/// Backoff for retry `attempt` (0-indexed): `min(minTimeout*factor^attempt,
/// maxTimeout)` — the `retry` library `createTimeout` with `randomize:false`.
fn backoff_ms(attempt: u32) -> u64 {
    let scaled = MIN_TIMEOUT_MS.saturating_mul(FACTOR.saturating_pow(attempt));
    scaled.min(MAX_TIMEOUT_MS)
}

/// Acquire the cross-process lock for `target`, creating `<target>.lock` as an
/// atomic exclusive `mkdir`. Retries with exponential backoff up to
/// [`RETRIES`] times and steals a stale (> [`STALE_MS`]) holder, mirroring
/// claude-code's `LOCK_OPTIONS`.
///
/// The backoff sleeps are `.await`ed so the caller's async runtime is never
/// blocked. The lock directory's parent must already exist.
///
/// # Errors
/// Returns the underlying [`io::Error`] if `mkdir` fails for a reason other
/// than the lock being held, or [`io::ErrorKind::WouldBlock`] once the retry
/// budget is exhausted with the lock still held by a live holder.
pub async fn lock(target: &Path) -> io::Result<LockGuard> {
    lock_with_stale(target, Duration::from_millis(STALE_MS)).await
}

/// [`lock`] with an explicit stale window. Exists so tests can exercise the
/// stale-steal path without waiting out the 10s production threshold; the store
/// only ever calls [`lock`].
async fn lock_with_stale(target: &Path, stale: Duration) -> io::Result<LockGuard> {
    let lock_dir = lock_dir_for(target);
    let mut stole = false;
    for attempt in 0..=RETRIES {
        if try_acquire(&lock_dir)? {
            return Ok(LockGuard {
                lock_dir,
                released: false,
            });
        }
        // Held. Steal a stale holder once, then retry immediately.
        if !stole && is_stale(&lock_dir, stale) {
            stole = true;
            let _ = std::fs::remove_dir(&lock_dir);
            if try_acquire(&lock_dir)? {
                return Ok(LockGuard {
                    lock_dir,
                    released: false,
                });
            }
        }
        if attempt < RETRIES {
            tokio::time::sleep(Duration::from_millis(backoff_ms(attempt))).await;
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("lock held: {}", lock_dir.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_target(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "lingxi-lockfile-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("target")
    }

    #[test]
    fn lock_dir_suffix_matches_claude_code() {
        assert_eq!(
            lock_dir_for(Path::new("/x/1.json")),
            PathBuf::from("/x/1.json.lock")
        );
        assert_eq!(
            lock_dir_for(Path::new("/x/.lock")),
            PathBuf::from("/x/.lock.lock")
        );
    }

    #[test]
    fn backoff_is_exponential_capped_at_max() {
        assert_eq!(backoff_ms(0), 5);
        assert_eq!(backoff_ms(1), 10);
        assert_eq!(backoff_ms(2), 20);
        assert_eq!(backoff_ms(3), 40);
        assert_eq!(backoff_ms(4), 80);
        assert_eq!(backoff_ms(5), 100); // 160 -> capped
        assert_eq!(backoff_ms(30), 100); // no overflow, still capped
    }

    #[tokio::test]
    async fn acquire_creates_and_release_removes_lock_dir() {
        let target = temp_target("acq");
        let lock_dir = lock_dir_for(&target);
        {
            let _guard = lock(&target).await.unwrap();
            assert!(lock_dir.is_dir(), "lock dir must exist while held");
        }
        assert!(!lock_dir.exists(), "lock dir must be removed on drop");
        let _ = std::fs::remove_dir_all(target.parent().unwrap());
    }

    #[tokio::test]
    async fn held_lock_is_mutually_exclusive() {
        let target = temp_target("excl");
        let g1 = lock(&target).await.unwrap();
        // A raw single attempt must observe the directory as held (no double
        // acquire) — running the full lock() would burn the ~2.6s retry budget.
        assert!(
            !try_acquire(&lock_dir_for(&target)).unwrap(),
            "held lock must not be re-acquired"
        );
        drop(g1);
        // Now it is free again.
        assert!(try_acquire(&lock_dir_for(&target)).unwrap());
        let _ = std::fs::remove_dir_all(target.parent().unwrap());
    }

    #[tokio::test]
    async fn stale_lock_is_stolen() {
        let target = temp_target("stale");
        let lock_dir = lock_dir_for(&target);
        // Pre-create the lock dir (as a crashed holder would leave it), let it
        // age past a tiny stale window, then acquire: it must be stolen.
        std::fs::create_dir(&lock_dir).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let guard = lock_with_stale(&target, Duration::from_millis(1))
            .await
            .expect("stale holder must be stolen");
        assert!(lock_dir.is_dir());
        drop(guard);
        assert!(!lock_dir.exists());
        let _ = std::fs::remove_dir_all(target.parent().unwrap());
    }

    #[tokio::test]
    async fn fresh_lock_is_not_stolen() {
        let target = temp_target("fresh");
        let lock_dir = lock_dir_for(&target);
        std::fs::create_dir(&lock_dir).unwrap();
        // With the production stale window the just-created dir is fresh, so
        // acquisition must give up rather than steal a live holder.
        assert!(!is_stale(&lock_dir, Duration::from_millis(STALE_MS)));
        let _ = std::fs::remove_dir_all(target.parent().unwrap());
    }
}
