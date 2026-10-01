//! The filesystem error vocabulary shared by the rooted primitives and the
//! host `FileSystem` trait.

use thiserror::Error;

/// Which phase of an append failed. An open failure retains queued output;
/// a write failure may have consumed bytes and cannot safely replay the batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAppendStage {
    /// No payload write was attempted.
    Open,
    /// Payload write was attempted and may have partially succeeded.
    Write,
}

/// Append failure with its observable consumption phase.
#[derive(Debug)]
pub struct FileAppendError {
    /// Failed phase.
    pub stage: FileAppendStage,
    /// Underlying filesystem failure.
    pub error: FsError,
}

/// Guard for an OS advisory file lock acquired via the host
/// `FileSystem::flock_exclusive`. Releasing the lock happens in `Drop`.
pub trait FlockGuard: Send + Sync {
    /// The path the guard locks. Implementations may use this for diagnostics.
    fn path(&self) -> &str;
}

/// Failure modes for the host `FileSystem` calls and the rooted primitives.
#[derive(Debug, Clone, Error)]
pub enum FsError {
    /// Requested path does not exist.
    #[error("file not found: {0}")]
    NotFound(String),
    /// Caller lacks permission on the underlying OS or sandbox.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// Path resolves outside the workspace root.
    #[error("path outside workspace: {0}")]
    OutsideWorkspace(String),
    /// A file (or symlink) already exists where an exclusive create was
    /// requested. Surfaced by `FileSystem::create_new_file` when the path is
    /// occupied — the `O_EXCL` collision that guards the spool double-allocate
    /// race.
    #[error("file already exists: {0}")]
    AlreadyExists(String),
    /// File contents are not valid UTF-8 / look like binary data.
    #[error("file is binary: {0}")]
    BinaryFile(String),
    /// File or read window exceeds the configured size limit.
    #[error("size exceeds limit: {actual} > {limit}")]
    TooLarge {
        /// Actual size encountered, in bytes.
        actual: u64,
        /// Configured maximum, in bytes.
        limit: u64,
    },
    /// Catch-all for underlying I/O failures.
    #[error("io error: {0}")]
    Io(String),
}
