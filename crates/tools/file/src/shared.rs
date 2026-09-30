//! Binary detection, BOM stripping, UTF-8 decoding.
//!
//! Used by every M4 file-touching tool. Pure functional; no I/O.

use std::path::{Path, PathBuf};

/// Search preparation failures that must be surfaced before a walk can expose
/// any result.  The search tools first resolve their root and install their
/// Read-deny overrides; this snapshot lets them confirm that neither changed
/// before (and while) the walk runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchResolutionError {
    /// The model-supplied search root no longer resolves to the approved path.
    SearchRootChanged,
    /// A path used by one of the active Read-deny rules no longer resolves to
    /// the path observed while the search was being prepared.
    ReadDenyPathChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathResolution {
    resolved: Option<PathBuf>,
    parent: Option<PathBuf>,
}

/// Resolution state captured after permission canonicalization and deny-glob
/// construction.  The approved root itself is used for the walk, while the
/// original spelling is retained for the TOCTOU re-check.
#[derive(Debug, Clone)]
pub(crate) struct SearchResolutionSnapshot {
    requested_root: PathBuf,
    approved_root: PathBuf,
    root_state: PathResolution,
    deny_paths: Vec<(PathBuf, PathResolution)>,
}

impl SearchResolutionSnapshot {
    /// Capture the root and the static path prefixes represented by deny globs.
    /// A glob's wildcard suffix cannot be canonicalized, so its longest static
    /// prefix is the path whose symlink chain can be checked deterministically.
    pub(crate) fn capture(
        requested_root: &Path,
        approved_root: &Path,
        deny_globs: &[String],
    ) -> Self {
        let deny_paths = deny_globs
            .iter()
            .filter_map(|glob| static_deny_path(approved_root, glob))
            .map(|path| {
                let state = path_resolution(&path);
                (path, state)
            })
            .collect();
        Self {
            requested_root: requested_root.to_path_buf(),
            approved_root: approved_root.to_path_buf(),
            root_state: path_resolution(requested_root),
            deny_paths,
        }
    }

    /// Re-check the original root and each deny-rule path.  A changed root is
    /// checked first because it determines the entire search tree; deny paths
    /// are checked next so no search result from a changed protected path is
    /// returned.
    pub(crate) fn verify(&self) -> Result<(), SearchResolutionError> {
        let current_root = path_resolution(&self.requested_root);
        if current_root != self.root_state
            || current_root.resolved.as_deref() != Some(self.approved_root.as_path())
        {
            return Err(SearchResolutionError::SearchRootChanged);
        }
        for (path, approved) in &self.deny_paths {
            if path_resolution(path) != *approved {
                return Err(SearchResolutionError::ReadDenyPathChanged);
            }
        }
        Ok(())
    }
}

fn path_resolution(path: &Path) -> PathResolution {
    let resolved = std::fs::canonicalize(path).ok();
    let parent = path
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok());
    PathResolution { resolved, parent }
}

/// Return the longest non-wildcard path represented by a deny glob.  Rooted
/// entries are relative to the canonical search root (the same anchor used by
/// `ignore::OverrideBuilder`); unrooted entries use that root as their stable
/// check point as well.  If a pattern begins with a wildcard there is no single
/// path to pin, and the search root re-check remains the applicable guard.
fn static_deny_path(search_root: &Path, glob: &str) -> Option<PathBuf> {
    let without_root = glob.strip_prefix('/').unwrap_or(glob);
    let wildcard = without_root.find(['*', '?', '[', '{']);
    let static_prefix = match wildcard {
        Some(index) => &without_root[..index],
        None => without_root,
    }
    .trim_end_matches('/');
    if static_prefix.is_empty() {
        return None;
    }
    let path = search_root.join(static_prefix);
    if path == search_root {
        None
    } else {
        Some(path)
    }
}

#[cfg(test)]
mod search_test_hook {
    use once_cell::sync::Lazy;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static HOOKS: Lazy<Mutex<Vec<(PathBuf, Box<dyn FnOnce() + Send + 'static>)>>> =
        Lazy::new(|| Mutex::new(Vec::new()));
    static CANDIDATE_HOOKS: Lazy<Mutex<Vec<(PathBuf, Box<dyn FnOnce() + Send + 'static>)>>> =
        Lazy::new(|| Mutex::new(Vec::new()));

    pub(crate) fn install(path: &Path, hook: impl FnOnce() + Send + 'static) {
        let mut hooks = HOOKS.lock().expect("search hook mutex poisoned");
        hooks.retain(|(registered_path, _)| registered_path != path);
        hooks.push((path.to_path_buf(), Box::new(hook)));
    }

    pub(crate) fn run(path: &Path) {
        let mut hooks = HOOKS.lock().expect("search hook mutex poisoned");
        let hook = hooks
            .iter()
            .position(|(registered_path, _)| registered_path == path)
            .map(|index| hooks.remove(index).1);
        drop(hooks);
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(crate) fn install_candidate(path: &Path, hook: impl FnOnce() + Send + 'static) {
        let mut hooks = CANDIDATE_HOOKS
            .lock()
            .expect("search candidate hook mutex poisoned");
        hooks.retain(|(registered_path, _)| registered_path != path);
        hooks.push((path.to_path_buf(), Box::new(hook)));
    }

    pub(crate) fn run_candidate(path: &Path) {
        let mut hooks = CANDIDATE_HOOKS
            .lock()
            .expect("search candidate hook mutex poisoned");
        let hook = hooks
            .iter()
            .position(|(registered_path, _)| registered_path == path)
            .map(|index| hooks.remove(index).1);
        drop(hooks);
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Deterministic test seam for the gap between search preparation and the
/// first filesystem walk. Production builds compile this to a no-op.
#[cfg(test)]
pub(crate) fn run_search_preparation_hook(path: &Path) {
    search_test_hook::run(path);
}

#[cfg(not(test))]
pub(crate) fn run_search_preparation_hook(_path: &Path) {}

#[cfg(test)]
pub(crate) fn install_search_preparation_hook(path: &Path, hook: impl FnOnce() + Send + 'static) {
    search_test_hook::install(path, hook);
}

/// Deterministic test seam for the gap after a walk entry is classified as a
/// regular file and before its stable rooted handle is opened.
#[cfg(test)]
pub(crate) fn run_search_candidate_hook(path: &Path) {
    search_test_hook::run_candidate(path);
}

#[cfg(not(test))]
pub(crate) fn run_search_candidate_hook(_path: &Path) {}

#[cfg(test)]
pub(crate) fn install_search_candidate_hook(path: &Path, hook: impl FnOnce() + Send + 'static) {
    search_test_hook::install_candidate(path, hook);
}

/// Open a walked candidate through a rooted, no-follow handle chain and verify
/// that its resolved path is still exactly the path yielded under the search
/// root. Any candidate drift fails the whole search closed, discarding partial
/// matches rather than exposing data from a swapped leaf or ancestor.
pub(crate) fn open_rooted_search_file(
    search_root: &Path,
    candidate: &Path,
) -> Result<std::fs::File, SearchResolutionError> {
    if !candidate.starts_with(search_root) {
        return Err(SearchResolutionError::SearchRootChanged);
    }
    // A direct-file Grep has no relative components below its search root.
    // Pin its parent directory and open the approved leaf without following it.
    if candidate == search_root {
        let parent = candidate
            .parent()
            .ok_or(SearchResolutionError::SearchRootChanged)?;
        let filename = candidate
            .file_name()
            .ok_or(SearchResolutionError::SearchRootChanged)?;
        return lingxi_core::host::rooted_fs::open_file_after_permission(
            parent,
            Path::new(filename),
            candidate,
            candidate,
        )
        .map_err(|_| SearchResolutionError::SearchRootChanged);
    }
    let relative = candidate
        .strip_prefix(search_root)
        .map_err(|_| SearchResolutionError::SearchRootChanged)?;
    lingxi_core::host::rooted_fs::open_file_after_permission(
        search_root,
        relative,
        candidate,
        candidate,
    )
    .map_err(|_| SearchResolutionError::SearchRootChanged)
}

/// Map an approved canonical path back to a canonical trusted root and a
/// root-relative path. Rooted I/O uses this pair to open fixed directory
/// handles instead of reopening the canonical pathname. The longest matching
/// root wins when a session has nested trusted directories.
pub(crate) fn rooted_location(
    approved: &Path,
    trusted_dirs: &[PathBuf],
) -> Option<(PathBuf, PathBuf)> {
    trusted_dirs
        .iter()
        .filter_map(|root| {
            let root = std::fs::canonicalize(root).ok()?;
            if !approved.starts_with(&root) {
                return None;
            }
            let relative = approved.strip_prefix(&root).ok()?.to_path_buf();
            Some((root, relative))
        })
        .max_by_key(|(root, _)| root.components().count())
}

/// Window size for binary detection — spec §7 lock.
///
/// Matches claude-code's `src/tools/FileReadTool/utils.ts` first-pass
/// NUL-byte scan. A file whose first 8 KB contain any `0x00` byte is
/// classified as binary and rejected.
pub const NUL_SCAN_WINDOW: usize = 8 * 1024;

/// True iff `prefix` (capped at [`NUL_SCAN_WINDOW`]) contains a NUL byte.
#[must_use]
pub fn looks_binary(prefix: &[u8]) -> bool {
    let window = &prefix[..prefix.len().min(NUL_SCAN_WINDOW)];
    window.iter().any(|b| *b == 0)
}

/// Return `bytes` with the UTF-8 BOM (`EF BB BF`) stripped if present.
#[must_use]
pub fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        &bytes[3..]
    } else {
        bytes
    }
}

/// Decode `bytes` as UTF-8, stripping the BOM if present. Rejects non-UTF-8
/// (no fallback to latin-1 / cp1252 in M4-01 per spec §7 "Default encoding:
/// UTF-8 (BOM-aware); reject non-UTF-8").
///
/// # Errors
/// Returns `Utf8Error` if the bytes are not valid UTF-8.
pub fn decode_utf8_strict(bytes: &[u8]) -> Result<String, std::str::Utf8Error> {
    let trimmed = strip_utf8_bom(bytes);
    std::str::from_utf8(trimmed).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nul_scan_window_is_8kb() {
        assert_eq!(NUL_SCAN_WINDOW, 8 * 1024);
    }

    #[test]
    fn looks_binary_finds_nul_in_window() {
        let mut buf = vec![b'A'; NUL_SCAN_WINDOW];
        buf[100] = 0;
        assert!(looks_binary(&buf));
    }

    #[test]
    fn looks_binary_ignores_nul_outside_window() {
        // Deviation from plan: plan wrote `vec![b'A'; NUL_SCAN_WINDOW + 10]`
        // and then `buf[9000] = 0`, but 9000 > 8202 → OOB panic. Resize
        // buffer so the NUL at offset 9000 is in-bounds but past the
        // 8192-byte scan window.
        let mut buf = vec![b'A'; NUL_SCAN_WINDOW + 1000];
        // NUL at position 9000 (past the 8192-byte window) → not binary
        buf[9000] = 0;
        assert!(!looks_binary(&buf));
    }

    #[test]
    fn looks_binary_empty_input_is_text() {
        assert!(!looks_binary(b""));
    }

    #[test]
    fn looks_binary_pure_text_is_text() {
        assert!(!looks_binary(b"hello world\nline 2\n"));
    }

    #[test]
    fn strip_utf8_bom_removes_bom() {
        let with_bom = b"\xEF\xBB\xBFhello";
        assert_eq!(strip_utf8_bom(with_bom), b"hello");
    }

    #[test]
    fn strip_utf8_bom_preserves_non_bom() {
        let no_bom = b"hello";
        assert_eq!(strip_utf8_bom(no_bom), b"hello");
    }

    #[test]
    fn decode_utf8_strict_handles_bom() {
        let s = decode_utf8_strict(b"\xEF\xBB\xBFhello \xE4\xB8\x96\xE7\x95\x8C").unwrap();
        assert_eq!(s, "hello 世界");
    }

    #[test]
    fn decode_utf8_strict_rejects_invalid() {
        // 0xFF is never a valid UTF-8 starter
        assert!(decode_utf8_strict(&[0xFF, 0xFE, b'a']).is_err());
    }
}

#[cfg(test)]
pub(crate) struct TaskOutputTestRegistry(pub std::path::PathBuf);

#[cfg(test)]
#[async_trait::async_trait]
impl lingxi_core::host::task_registry::TaskRegistryHandle for TaskOutputTestRegistry {
    async fn task_output_directory(&self) -> Option<String> {
        Some(self.0.to_string_lossy().into_owned())
    }
    async fn create(
        &self,
        _: lingxi_core::host::task_registry::TaskCreateInput,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn get(
        &self,
        _: &str,
    ) -> Result<
        Option<lingxi_core::host::task_registry::TaskRecord>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Ok(None)
    }
    async fn list(
        &self,
        _: lingxi_core::host::task_registry::TaskListFilter,
    ) -> Result<
        Vec<lingxi_core::host::task_registry::TaskRecord>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Ok(vec![])
    }
    async fn update(
        &self,
        _: &str,
        _: lingxi_core::host::task_registry::TaskUpdatePatch,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn set_status(
        &self,
        _: &str,
        _: &str,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn kill(
        &self,
        _: &str,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn output(
        &self,
        _: &str,
        _: Option<u64>,
    ) -> Result<
        lingxi_core::host::task_registry::TaskOutputChunk,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
}
