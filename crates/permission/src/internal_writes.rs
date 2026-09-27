//! Process-local hints used to distinguish our own atomic settings writes from
//! external editor/policy changes observed by the settings watcher.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

fn marks() -> &'static Mutex<HashMap<PathBuf, Instant>> {
    static MARKS: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
    MARKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Mark `path` immediately before an internal settings replacement.
pub fn mark_internal_write(path: &Path) {
    let now = Instant::now();
    let mut guard = marks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|_, marked| now.duration_since(*marked) <= Duration::from_secs(30));
    guard.insert(key(path), now);
}

/// Consume a matching recent internal-write marker. Returning `true` tells the
/// watcher to suppress exactly one debounced target-file event.
#[must_use]
pub fn consume_internal_write(path: &Path, window: Duration) -> bool {
    let now = Instant::now();
    let mut guard = marks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = key(path);
    let recent = guard
        .get(&path)
        .is_some_and(|marked| now.duration_since(*marked) <= window);
    guard.remove(&path);
    recent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_is_path_normalized_recent_and_single_use() {
        let path = std::env::temp_dir().join(format!(
            "lingxi-internal-write-{}-settings.json",
            std::process::id()
        ));
        mark_internal_write(&path);
        assert!(consume_internal_write(&path, Duration::from_secs(5)));
        assert!(!consume_internal_write(&path, Duration::from_secs(5)));
    }

    #[test]
    fn expired_marker_does_not_suppress() {
        let path = std::env::temp_dir().join(format!(
            "lingxi-expired-write-{}-settings.json",
            std::process::id()
        ));
        mark_internal_write(&path);
        assert!(!consume_internal_write(&path, Duration::ZERO));
    }
}
