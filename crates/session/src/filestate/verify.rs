//! Read↔Edit verification (spec §23.2 / C5).
//!
//! Before allowing an Edit to mutate a file, the engine consults the
//! [`FileStateCache`] to ensure no external change has happened between the
//! most recent Read and the Edit. Partial-view reads short-circuit to a
//! distinct outcome so the caller can require an exact-match against the
//! partial window rather than the whole file.

use super::cache::{FileState, FileStateCache};
use sha2::{Digest, Sha256};
use std::time::SystemTime;

/// Outcome of a [`verify_file_state`] check.
#[derive(Debug, Clone)]
pub enum FileStateVerification {
    /// Cache holds a fresh full-file read; Edit may proceed.
    Valid,
    /// No cached entry for this path — caller must Read first.
    NotInCache,
    /// Cached read was a partial window — caller must do a full Read.
    PartialView,
    /// On-disk `mtime` is newer than the cached read.
    ModifiedSinceRead {
        /// Cached read's timestamp.
        cached_at: SystemTime,
        /// On-disk last-modified time observed now.
        disk_mtime: SystemTime,
    },
    /// Disk hash differs from the cached content's hash (when supplied).
    ContentMismatch,
}

/// Compare `cache`'s entry for `path` against the current on-disk state. The
/// `current_disk_hash` argument is optional so callers can skip a re-read
/// when they have already hashed the file.
#[must_use]
pub fn verify_file_state(
    cache: &FileStateCache,
    path: &str,
    current_disk_mtime: SystemTime,
    current_disk_hash: Option<[u8; 32]>,
) -> FileStateVerification {
    let Some(cached) = cache.get(path) else {
        return FileStateVerification::NotInCache;
    };
    if cached.is_partial_view {
        return FileStateVerification::PartialView;
    }
    if current_disk_mtime > cached.timestamp {
        return FileStateVerification::ModifiedSinceRead {
            cached_at: cached.timestamp,
            disk_mtime: current_disk_mtime,
        };
    }
    if let Some(disk_hash) = current_disk_hash {
        let cached_hash = sha256_of(&cached);
        if disk_hash != cached_hash {
            return FileStateVerification::ContentMismatch;
        }
    }
    FileStateVerification::Valid
}

fn sha256_of(state: &FileState) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(state.content.as_bytes());
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_in_cache_returned_when_missing() {
        let c = FileStateCache::new(10, 1024);
        let v = verify_file_state(&c, "missing.txt", SystemTime::now(), None);
        assert!(matches!(v, FileStateVerification::NotInCache));
    }

    #[test]
    fn detects_modification_via_mtime() {
        let c = FileStateCache::new(10, 1024);
        let old = SystemTime::UNIX_EPOCH;
        c.set(
            "a.txt",
            FileState {
                content: "hi".into(),
                timestamp: old,
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let v = verify_file_state(&c, "a.txt", old + std::time::Duration::from_secs(1), None);
        assert!(matches!(v, FileStateVerification::ModifiedSinceRead { .. }));
    }
}
