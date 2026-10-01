//! Sandboxed filesystem abstraction. Implementations live in platform crates
//! and enforce workspace-root containment (see spec §6 / D17).

use async_trait::async_trait;
use futures_core::stream::Stream;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::pin::Pin;

// The error, guard and append vocabulary the rooted primitives share with this
// trait lives in `rooted-fs`, so the Local App service can use those primitives
// without depending on this crate; re-exported so every existing path resolves.
pub use rooted_fs::{FileAppendError, FileAppendStage, FlockGuard, FsError};

/// Stable namespace for caches whose entries also include per-file OS
/// metadata. `generation` must change whenever a backend can change read
/// results without changing that metadata. Real disk backends use generation
/// zero; virtual or decorated backends should return `None` unless they can
/// provide such a generation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileSystemCacheIdentity {
    backend: &'static str,
    namespace: PathBuf,
    generation: u64,
}

impl FileSystemCacheIdentity {
    /// Construct a stable cache namespace for one filesystem backend.
    #[must_use]
    pub fn new(backend: &'static str, namespace: PathBuf, generation: u64) -> Self {
        Self {
            backend,
            namespace,
            generation,
        }
    }
}

/// Sandboxed read/write access to the workspace.
///
/// Engine and tool code receive an `Arc<dyn FileSystem>` rather than calling
/// `std::fs` directly so the host can sandbox, virtualize, or audit access.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Stable cache identity for metadata-keyed read caches.
    ///
    /// The default disables cross-call caching. Virtual/decorated backends
    /// should opt in only when their generation invalidates every content
    /// change that underlying OS file metadata cannot observe.
    fn cache_identity(&self) -> Option<FileSystemCacheIdentity> {
        None
    }

    /// Read a UTF-8 file, optionally constrained to a line-indexed `offset`/`limit`
    /// window. Returns [`FsError::BinaryFile`] for non-text files.
    async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, FsError>;

    /// Read at most `max_bytes` from the start of a UTF-8 file.
    ///
    /// Used by session catalog / lite metadata so listing hundreds of JSONL
    /// transcripts does not slurp each file in full. The default implementation
    /// reads the whole file via [`read_file`](Self::read_file) and truncates;
    /// platform filesystems override this with a real prefix read.
    async fn read_file_prefix(&self, path: &str, max_bytes: usize) -> Result<FileContent, FsError> {
        let mut file = self.read_file(path, None, None).await?;
        if file.content.len() > max_bytes {
            let mut end = max_bytes;
            while end > 0 && !file.content.is_char_boundary(end) {
                end -= 1;
            }
            file.content.truncate(end);
            file.truncated = true;
        }
        Ok(file)
    }

    /// Write `content` to `path`, creating or truncating as needed.
    async fn write_file(&self, path: &str, content: &str) -> Result<(), FsError>;

    /// True iff `path` resolves inside the sandboxed workspace root.
    fn is_within_workspace(&self, path: &str) -> bool;

    /// Watch `dir` for changes and emit a stream of [`FileEvent`]s.
    ///
    /// Used by the team-memory watcher (spec §6.4); the platform is
    /// responsible for debouncing and resolving symlinks.
    async fn watch(
        &self,
        dir: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = FileEvent> + Send>>, FsError>;

    /// Append with open/write distinction for a retrying task-output queue.
    /// Compatibility adapters conservatively classify errors as writes; native
    /// implementations override this to prove that open failures consumed nothing.
    async fn append_file_rooted_staged(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
        expected: Option<&crate::host::rooted_fs::RootIdentity>,
    ) -> Result<(), FileAppendError> {
        self.append_file_rooted_no_follow_pinned(root, relative, content, expected)
            .await
            .map_err(|error| FileAppendError {
                stage: FileAppendStage::Write,
                error,
            })
    }

    /// Append `content` to `path`, creating the file if it does not exist.
    ///
    /// Used by session storage to grow append-only JSONL transcripts without
    /// rewriting prior bytes (spec §22).
    async fn append_file(&self, path: &str, content: &str) -> Result<(), FsError>;

    /// Exclusively create a brand-new empty file at `path`, failing if anything
    /// already exists there (including a symlink).
    ///
    /// Mirrors claude-code's task-output init (`diskOutput.ts` `initTaskOutput`),
    /// which opens with `O_CREAT | O_EXCL | O_NOFOLLOW`:
    /// - `O_EXCL` makes the create idempotent-safe — a second create for the
    ///   same path returns [`FsError::AlreadyExists`] rather than truncating
    ///   bytes a concurrent writer already appended (the TOCTOU double-allocate
    ///   race).
    /// - `O_NOFOLLOW` refuses to follow a pre-planted symlink, closing the
    ///   symlink-follow write vector from inside a sandbox.
    ///
    /// The default implementation creates the file via
    /// [`write_file`](Self::write_file) (no exclusivity / symlink protection),
    /// preserving the pre-hardening behavior for platforms and in-memory mocks
    /// that do not need the atomic open. The hardened POSIX platform overrides
    /// it with a real `O_EXCL | O_NOFOLLOW` open so the exclusive-create
    /// guarantee (and the [`FsError::AlreadyExists`] collision) is genuine; a
    /// mock that needs to exercise the collision path overrides this method to
    /// fail on an already-present path.
    async fn create_new_file(&self, path: &str) -> Result<(), FsError> {
        self.write_file(path, "").await
    }

    /// Append `content` to `path` WITHOUT following a final-component symlink,
    /// creating the file if it does not exist.
    ///
    /// Mirrors claude-code's task-output append (`diskOutput.ts`), which opens
    /// with `O_WRONLY | O_APPEND | O_CREAT | O_NOFOLLOW` so a symlink planted at
    /// the spool path from inside a sandbox cannot redirect the write to an
    /// arbitrary host file.
    ///
    /// The default implementation delegates to [`append_file`](Self::append_file)
    /// (no extra symlink protection); the hardened POSIX platform overrides it
    /// with a real `O_NOFOLLOW` open.
    async fn append_file_no_follow(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.append_file(path, content).await
    }

    /// Exclusively create a file addressed relative to `root`, without
    /// following directory or final-component symlinks on hardened
    /// implementations.
    async fn create_new_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<(), FsError> {
        let path = crate::host::rooted_fs::checked_join(root, relative)?;
        self.create_new_file(&path.to_string_lossy()).await
    }

    /// Capture a stable root-directory identity when the platform supports
    /// handle-relative filesystem operations. Virtual filesystems return
    /// `None` and retain their existing path-based behavior.
    async fn root_identity_no_follow(
        &self,
        _root: &Path,
    ) -> Result<Option<crate::host::rooted_fs::RootIdentity>, FsError> {
        Ok(None)
    }

    /// Pinned counterpart to [`Self::create_new_file_rooted_no_follow`].
    async fn create_new_file_rooted_no_follow_pinned(
        &self,
        root: &Path,
        relative: &Path,
        _expected: Option<&crate::host::rooted_fs::RootIdentity>,
    ) -> Result<(), FsError> {
        self.create_new_file_rooted_no_follow(root, relative).await
    }

    /// Append to a file addressed relative to `root`, without following
    /// directory or final-component symlinks on hardened implementations.
    async fn append_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
    ) -> Result<(), FsError> {
        let path = crate::host::rooted_fs::checked_join(root, relative)?;
        self.append_file_no_follow(&path.to_string_lossy(), content)
            .await
    }

    /// Pinned counterpart to [`Self::append_file_rooted_no_follow`].
    async fn append_file_rooted_no_follow_pinned(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
        _expected: Option<&crate::host::rooted_fs::RootIdentity>,
    ) -> Result<(), FsError> {
        self.append_file_rooted_no_follow(root, relative, content)
            .await
    }

    /// Append `content` to `path`, creating the file with the given unix
    /// permission `mode` if it does not yet exist.
    ///
    /// Mirrors claude-code's session-transcript `appendToFile`
    /// (`sessionStorage.ts:634`), which appends with `{ mode: 0o600 }` so the
    /// `<uuid>.jsonl` transcript (prompt + tool content) is owner-only, never
    /// group/other-readable. `mode` is ignored when the file already exists
    /// (only the create applies it) and on non-unix platforms.
    ///
    /// The default implementation delegates to [`append_file`](Self::append_file)
    /// (no mode control); the POSIX platform overrides it with an
    /// `OpenOptions::mode(mode)` create.
    async fn append_file_with_mode(
        &self,
        path: &str,
        content: &str,
        mode: u32,
    ) -> Result<(), FsError> {
        let _ = mode;
        self.append_file(path, content).await
    }

    /// Read a UTF-8 file addressed relative to a trusted `root`, without
    /// following symlinks below that root on hardened platform implementations.
    ///
    /// The default keeps in-memory/mock implementations source-compatible by
    /// validating the relative path and delegating to [`read_file`](Self::read_file).
    async fn read_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<FileContent, FsError> {
        let path = crate::host::rooted_fs::checked_join(root, relative)?;
        self.read_file(&path.to_string_lossy(), None, None).await
    }

    /// Read a line-indexed window of a file addressed relative to `root`,
    /// without following symlinks on hardened implementations.
    async fn read_file_rooted_no_follow_window(
        &self,
        root: &Path,
        relative: &Path,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        // Keep the default's existing `read_file` offset/limit contract for
        // virtual filesystems and legacy mocks. Hardened implementations
        // override this with a rooted open followed by their native windowing
        // semantics, so no pathname is reopened after validation.
        let path = crate::host::rooted_fs::checked_join(root, relative)?;
        self.read_file(&path.to_string_lossy(), offset, limit).await
    }

    /// Pinned counterpart to [`Self::read_file_rooted_no_follow_window`].
    async fn read_file_rooted_no_follow_window_pinned(
        &self,
        root: &Path,
        relative: &Path,
        offset: Option<u64>,
        limit: Option<u64>,
        _expected: Option<&crate::host::rooted_fs::RootIdentity>,
    ) -> Result<FileContent, FsError> {
        self.read_file_rooted_no_follow_window(root, relative, offset, limit)
            .await
    }

    /// Read a bounded byte window for task-output deltas. Physical platforms
    /// override this with seek/read; virtual filesystems preserve their backend.
    async fn read_file_rooted_byte_window_pinned(
        &self,
        root: &Path,
        relative: &Path,
        expected: Option<&crate::host::rooted_fs::RootIdentity>,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<u8>, FsError> {
        let content = self
            .read_file_rooted_no_follow_window_pinned(root, relative, None, None, expected)
            .await?
            .content;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(content.len());
        let end = start
            .saturating_add(usize::try_from(limit).unwrap_or(usize::MAX))
            .min(content.len());
        Ok(content.as_bytes()[start..end].to_vec())
    }

    /// Atomically replace a file addressed relative to a trusted `root`,
    /// refusing symlink traversal below the root on hardened platforms.
    ///
    /// The default delegates to [`write_file`](Self::write_file) after lexical
    /// validation so virtual/mock filesystems retain their existing semantics.
    async fn write_file_rooted_atomic(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
    ) -> Result<(), FsError> {
        let path = crate::host::rooted_fs::checked_join(root, relative)?;
        self.write_file(&path.to_string_lossy(), content).await
    }

    /// Lock a file addressed relative to a trusted `root`. Hardened platform
    /// implementations create missing private parents and open every component
    /// without following symlinks.
    async fn flock_exclusive_rooted(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<Box<dyn FlockGuard>, FsError> {
        let path = crate::host::rooted_fs::checked_join(root, relative)?;
        self.flock_exclusive(&path.to_string_lossy()).await
    }

    /// Delete a file addressed relative to a trusted root without following
    /// symlinked parent components on hardened platforms.
    async fn delete_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<(), FsError> {
        let path = crate::host::rooted_fs::checked_join(root, relative)?;
        self.delete_file(&path.to_string_lossy()).await
    }

    /// Truncate `path` to exactly `len` bytes.
    ///
    /// Used by the crash-safe JSONL reader to drop a torn tail after a power
    /// loss (spec §22.4 / B5).
    async fn truncate(&self, path: &str, len: u64) -> Result<(), FsError>;

    /// Return the last-modified time of `path`.
    ///
    /// Used by `verify_file_state` to detect Read↔Edit races (spec §23.2 / C5).
    async fn file_mtime(&self, path: &str) -> Result<std::time::SystemTime, FsError>;

    /// Return the size in bytes of `path`.
    ///
    /// Used to enforce size budgets and to validate JSONL recovery offsets.
    async fn file_size(&self, path: &str) -> Result<u64, FsError>;

    /// Delete the regular file at `path`.
    ///
    /// Used by session storage and file-state eviction.
    async fn delete_file(&self, path: &str) -> Result<(), FsError>;

    /// Create a symbolic link `link` pointing at `target`.
    ///
    /// Used to materialize project-level shortcuts (e.g. `last-session ->`).
    async fn symlink(&self, target: &str, link: &str) -> Result<(), FsError>;

    /// Acquire an OS-level advisory exclusive lock on `path`. The returned
    /// [`FlockGuard`] releases the lock when dropped.
    ///
    /// Mobile platforms (Android/iOS) may stub this with app-internal locking;
    /// the trait method is required so engine code can express the intent.
    async fn flock_exclusive(&self, path: &str) -> Result<Box<dyn FlockGuard>, FsError>;

    /// `fsync` the file at `path`, flushing OS buffers to durable storage.
    ///
    /// Engine code calls this after every important write so a power loss
    /// cannot leave a partially-flushed transcript visible (spec §22.3 / B5).
    async fn fsync(&self, path: &str) -> Result<(), FsError>;

    /// Translate a MODEL-SUPPLIED path into the host path tools should
    /// operate on, when this filesystem exposes a separate model-visible
    /// coordinate space (mobile-linux guest paths).
    ///
    /// - `Ok(None)` — the path needs no translation; use it as-is. This is
    ///   the default for every filesystem whose model-visible space IS the
    ///   host space.
    /// - `Ok(Some(host))` — `path` was a guest path on a host-backed mount;
    ///   operate on `host` instead.
    /// - `Err(_)` — `path` is inside the model-visible space but must not be
    ///   touched from the host (e.g. iSH fakefs regions, read-only mounts
    ///   for `write == true`). Tools surface the error to the model.
    ///
    /// File tools call this BEFORE canonicalization/containment so a guest
    /// path validates as its host twin. `write` marks mutating operations.
    fn translate_model_path(&self, path: &str, write: bool) -> Result<Option<String>, FsError> {
        let _ = (path, write);
        Ok(None)
    }
}

/// Apply the line-indexed `offset`/`limit` window used by [`FileSystem::read_file`].
///
/// Unwindowed reads return `content` as-is (no line-split / join). Windowed
/// reads skip `offset` lines then keep at most `limit` lines, joined by `\n`,
/// matching the historical `str::lines()` contract (trailing newline dropped).
#[must_use]
pub fn apply_line_window(content: String, offset: Option<u64>, limit: Option<u64>) -> FileContent {
    let total_lines = content.lines().count() as u64;
    if offset.is_none() && limit.is_none() {
        return FileContent {
            content,
            total_lines,
            truncated: false,
        };
    }
    let skip = offset.unwrap_or(0);
    let mut taken = 0u64;
    let mut out = String::new();
    for (index, line) in content.lines().enumerate() {
        if (index as u64) < skip {
            continue;
        }
        if let Some(lim) = limit {
            if taken >= lim {
                break;
            }
        }
        if taken > 0 {
            out.push('\n');
        }
        out.push_str(line);
        taken += 1;
    }
    FileContent {
        content: out,
        total_lines,
        truncated: false,
    }
}

/// Decode a UTF-8 prefix read of `n` bytes (possibly mid-character at the tail).
///
/// Incomplete trailing bytes are dropped so the result is always valid UTF-8.
/// `truncated` is true when the read filled the requested `max_bytes` budget.
pub fn file_content_from_prefix_bytes(
    path: &str,
    mut buf: Vec<u8>,
    n: usize,
    max_bytes: usize,
) -> Result<FileContent, FsError> {
    buf.truncate(n);
    let mut incomplete_trailing_utf8 = false;
    if let Err(error) = std::str::from_utf8(&buf) {
        if error.error_len().is_some() {
            return Err(FsError::BinaryFile(path.to_string()));
        }
        incomplete_trailing_utf8 = true;
        buf.truncate(error.valid_up_to());
    }
    let content = String::from_utf8(buf).map_err(|_| FsError::BinaryFile(path.to_string()))?;
    let total_lines = content.lines().count() as u64;
    Ok(FileContent {
        content,
        total_lines,
        truncated: n == max_bytes || incomplete_trailing_utf8,
    })
}

/// Result of [`FileSystem::read_file`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContent {
    /// The file body (possibly truncated; see `truncated`).
    pub content: String,
    /// Total line count of the underlying file before any truncation.
    pub total_lines: u64,
    /// True when `content` is a prefix of the full file.
    pub truncated: bool,
}

/// One file-system change event emitted by [`FileSystem::watch`].
#[derive(Debug, Clone)]
pub struct FileEvent {
    /// Path that changed.
    pub path: std::path::PathBuf,
    /// What kind of change.
    pub kind: FileEventKind,
}

/// Kind of file-system change reported by [`FileEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventKind {
    /// New file created.
    Created,
    /// Existing file modified.
    Modified,
    /// File deleted.
    Deleted,
}

#[cfg(test)]
mod apply_line_window_tests {
    use super::{apply_line_window, file_content_from_prefix_bytes, FsError};

    #[test]
    fn unwindowed_preserves_trailing_newline() {
        let got = apply_line_window("a\nb\n".into(), None, None);
        assert_eq!(got.content, "a\nb\n");
        assert_eq!(got.total_lines, 2);
        assert!(!got.truncated);
    }

    #[test]
    fn offset_and_limit_are_line_indexed() {
        let got = apply_line_window("a\nb\nc\nd".into(), Some(1), Some(2));
        assert_eq!(got.content, "b\nc");
        assert_eq!(got.total_lines, 4);
    }

    #[test]
    fn prefix_read_trims_only_incomplete_trailing_utf8() {
        let bytes = "hi🙂".as_bytes();
        let got = file_content_from_prefix_bytes(
            "x",
            bytes[..bytes.len() - 1].to_vec(),
            bytes.len() - 1,
            bytes.len(),
        )
        .expect("prefix with incomplete tail stays readable");
        assert_eq!(got.content, "hi");
        assert!(got.truncated);
    }

    #[test]
    fn prefix_read_rejects_internal_invalid_utf8() {
        let err = file_content_from_prefix_bytes("x", vec![b'h', 0xff, b'i'], 3, 3)
            .expect_err("internal invalid byte must be binary");
        assert!(matches!(err, FsError::BinaryFile(path) if path == "x"));
    }
}
