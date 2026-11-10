//! `tokio::fs`-backed [`FileSystem`] for desktop hosts.
//!
//! Implements the engine's sandboxed filesystem trait using the real OS
//! filesystem. Path containment is enforced via prefix-match against the
//! workspace root supplied at construction. [`FileSystem::watch`] is
//! backed by `notify` + `notify-debouncer-mini` (see `watch_helper`),
//! delivering chokidar-4-equivalent `awaitWriteFinish` semantics across
//! Linux (`inotify`), macOS (`FSEvents`), and Windows (`RDC`).

use async_trait::async_trait;
use fs2::FileExt;
use futures_core::stream::Stream;
use lingxi_core::host::{
    FileContent, FileEvent, FileSystem, FileSystemCacheIdentity, FlockGuard, FsError,
};
use std::path::{Path, PathBuf};
use std::pin::Pin;

/// Concrete [`FileSystem`] backed by `tokio::fs`.
///
/// Construction takes the workspace root used by
/// [`FileSystem::is_within_workspace`].
pub struct PosixFileSystem {
    workspace_root: PathBuf,
}

impl PosixFileSystem {
    /// Build a new `PosixFileSystem` rooted at `workspace_root`.
    #[must_use]
    pub fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }
}

async fn read_utf8_windowed(
    path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
) -> Result<FileContent, FsError> {
    if offset.is_none() && limit.is_none() {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        return Ok(lingxi_core::host::apply_line_window(content, None, None));
    }
    use tokio::io::AsyncBufReadExt;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| FsError::Io(e.to_string()))?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut line = String::new();
    let mut total_lines = 0u64;
    let skip = offset.unwrap_or(0);
    let mut taken = 0u64;
    let mut out = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        total_lines += 1;
        if total_lines <= skip {
            continue;
        }
        if let Some(lim) = limit {
            if taken >= lim {
                continue;
            }
        }
        let stripped = line.trim_end_matches(['\n', '\r']);
        if taken > 0 {
            out.push('\n');
        }
        out.push_str(stripped);
        taken += 1;
    }
    Ok(FileContent {
        content: out,
        total_lines,
        truncated: false,
    })
}

async fn read_utf8_prefix(path: &str, max_bytes: usize) -> Result<FileContent, FsError> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| FsError::Io(e.to_string()))?;
    let mut buf = vec![0u8; max_bytes];
    let n = file
        .read(&mut buf)
        .await
        .map_err(|e| FsError::Io(e.to_string()))?;
    lingxi_core::host::file_content_from_prefix_bytes(path, buf, n, max_bytes)
}

async fn acquire_rooted_lock(
    root: &Path,
    relative: &Path,
    expected: Option<lingxi_core::host::rooted_fs::RootIdentity>,
) -> Result<Box<dyn FlockGuard>, FsError> {
    use lingxi_core::host::rooted_fs::{self, RootedFileLockAttempt};
    let root = root.to_owned();
    let relative = relative.to_owned();
    let mut request = tokio::task::spawn_blocking(move || {
        rooted_fs::prepare_lock_exclusive_pinned(
            &root,
            &relative,
            rooted_fs::PRIVATE_DIR_MODE,
            rooted_fs::PRIVATE_FILE_MODE,
            expected.as_ref(),
        )
    })
    .await
    .map_err(|error| FsError::Io(error.to_string()))??;
    loop {
        // Move the complete capability through the worker. If this future is
        // cancelled during an attempt, its discarded result drops the pending
        // handle or the acquired RAII guard and releases the lock.
        match tokio::task::spawn_blocking(move || request.try_acquire())
            .await
            .map_err(|error| FsError::Io(error.to_string()))??
        {
            RootedFileLockAttempt::Acquired(guard) => return Ok(Box::new(guard)),
            RootedFileLockAttempt::Contended(pending) => request = pending,
        }
        // Contention has no deadline and never reopens the lock pathname.
        // Timer waiting keeps both the async runtime and blocking pool free.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[async_trait]
impl FileSystem for PosixFileSystem {
    async fn ensure_directory_rooted_identity_pinned(
        &self,
        root: &Path,
        relative: &Path,
        expected: Option<&lingxi_core::host::rooted_fs::RootIdentity>,
    ) -> Result<Option<lingxi_core::host::rooted_fs::RootIdentity>, FsError> {
        let expected =
            expected.ok_or_else(|| FsError::Io("missing scheduler root identity".into()))?;
        lingxi_core::host::rooted_fs::ensure_directory_identity_pinned(root, relative, expected)
            .map(Some)
    }
    async fn root_identity_no_follow(
        &self,
        root: &Path,
    ) -> Result<Option<lingxi_core::host::rooted_fs::RootIdentity>, FsError> {
        let root = root.to_owned();
        tokio::task::spawn_blocking(move || {
            lingxi_core::host::rooted_fs::root_identity(&root).map(Some)
        })
        .await
        .map_err(|error| FsError::Io(error.to_string()))?
    }

    fn cache_identity(&self) -> Option<FileSystemCacheIdentity> {
        Some(FileSystemCacheIdentity::new(
            "posix",
            self.workspace_root.clone(),
            0,
        ))
    }

    async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        read_utf8_windowed(path, offset, limit).await
    }

    async fn read_file_prefix(&self, path: &str, max_bytes: usize) -> Result<FileContent, FsError> {
        read_utf8_prefix(path, max_bytes).await
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        tokio::fs::write(path, content)
            .await
            .map_err(|e| FsError::Io(e.to_string()))
    }

    async fn append_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        f.write_all(content.as_bytes())
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        // `tokio::fs::File` buffers through the blocking pool and does NOT
        // flush on drop — without this an immediately-following read (e.g. a
        // second append computing its offset, or the caller reading the file
        // back) can race the not-yet-committed bytes. Flush makes append-then-
        // read deterministic. Durability only; the written bytes are unchanged.
        f.flush().await.map_err(|e| FsError::Io(e.to_string()))
    }

    async fn create_new_file(&self, path: &str) -> Result<(), FsError> {
        // SECURITY: exclusive create with O_NOFOLLOW, byte-for-byte the
        // claude-code `initTaskOutput` open (`diskOutput.ts`):
        //   O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW
        // - O_EXCL: fail with EEXIST if the path is already occupied — the
        //   second-allocate truncate race is impossible (we never clobber bytes
        //   a worker already appended).
        // - O_NOFOLLOW: refuse a pre-planted symlink at the final component, so
        //   a sandboxed attacker cannot redirect the create to a host file.
        use std::os::unix::fs::OpenOptionsExt;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true) // O_CREAT | O_EXCL
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    FsError::AlreadyExists(path.to_string())
                } else {
                    FsError::Io(e.to_string())
                }
            })?;
        drop(f);
        Ok(())
    }

    async fn create_new_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<(), FsError> {
        lingxi_core::host::rooted_fs::create_new_file(root, relative)
    }

    async fn create_new_file_rooted_no_follow_pinned(
        &self,
        root: &Path,
        relative: &Path,
        expected: Option<&lingxi_core::host::rooted_fs::RootIdentity>,
    ) -> Result<(), FsError> {
        lingxi_core::host::rooted_fs::create_new_file_pinned(root, relative, expected)
    }

    async fn write_new_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
    ) -> Result<(), FsError> {
        use std::io::Write;
        let mut file =
            lingxi_core::host::rooted_fs::open_create_new_file_pinned(root, relative, None)?;
        file.write_all(content.as_bytes())
            .map_err(|error| FsError::Io(error.to_string()))?;
        file.flush().map_err(|error| FsError::Io(error.to_string()))
    }

    async fn validate_file_rooted_single_link(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<(), FsError> {
        lingxi_core::host::rooted_fs::validate_file_single_link(root, relative)
    }

    async fn read_file_rooted_single_link_checked(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<lingxi_core::host::rooted_fs::CheckedFileSnapshot, FsError> {
        lingxi_core::host::rooted_fs::read_file_single_link_checked(root, relative)
    }

    async fn append_or_sync_file_rooted_durable_checked(
        &self,
        root: &Path,
        relative: &Path,
        expected: &lingxi_core::host::rooted_fs::CheckedFileSnapshot,
        append: Option<&str>,
    ) -> Result<(), FsError> {
        lingxi_core::host::rooted_fs::append_or_sync_file_durable_checked(
            root, relative, expected, append,
        )
    }

    async fn append_file_no_follow(&self, path: &str, content: &str) -> Result<(), FsError> {
        // SECURITY: append with O_NOFOLLOW, byte-for-byte the claude-code
        // task-output append open (`diskOutput.ts`):
        //   O_WRONLY | O_APPEND | O_CREAT | O_NOFOLLOW
        // O_NOFOLLOW refuses to follow a symlink planted at the spool path, so a
        // worker append cannot be redirected to an arbitrary host file.
        // (`tokio::fs::OpenOptions::custom_flags` is inherent — no trait import.)
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        f.write_all(content.as_bytes())
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        // Same flush rationale as `append_file`: make append-then-read
        // deterministic across the blocking pool.
        f.flush().await.map_err(|e| FsError::Io(e.to_string()))
    }

    async fn append_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
    ) -> Result<(), FsError> {
        lingxi_core::host::rooted_fs::append_file(root, relative, content)
    }

    async fn read_file_rooted_journal_pinned(
        &self,
        root: &Path,
        relative: &Path,
        expected_root: Option<&lingxi_core::host::rooted_fs::RootIdentity>,
    ) -> Result<lingxi_core::host::rooted_fs::JournalRead, FsError> {
        let root = root.to_owned();
        let relative = relative.to_owned();
        let expected_root = expected_root.copied();
        tokio::task::spawn_blocking(move || {
            lingxi_core::host::rooted_fs::read_journal_pinned(
                &root,
                &relative,
                expected_root.as_ref(),
            )
        })
        .await
        .map_err(|error| FsError::Io(error.to_string()))?
    }

    async fn append_file_rooted_journal_durable(
        &self,
        root: &Path,
        relative: &Path,
        expected: &lingxi_core::host::rooted_fs::JournalCursor,
        content: &str,
    ) -> Result<
        lingxi_core::host::rooted_fs::JournalCursor,
        lingxi_core::host::rooted_fs::JournalWriteError,
    > {
        let root = root.to_owned();
        let relative = relative.to_owned();
        let expected = *expected;
        let content = content.to_owned();
        tokio::task::spawn_blocking(move || {
            lingxi_core::host::rooted_fs::append_journal_durable(
                &root, &relative, &expected, &content,
            )
        })
        .await
        .map_err(|error| {
            lingxi_core::host::rooted_fs::JournalWriteError::uncertain(FsError::Io(
                error.to_string(),
            ))
        })?
    }

    async fn truncate_file_rooted_journal_durable(
        &self,
        root: &Path,
        relative: &Path,
        expected: &lingxi_core::host::rooted_fs::JournalCursor,
        length: u64,
    ) -> Result<
        lingxi_core::host::rooted_fs::JournalCursor,
        lingxi_core::host::rooted_fs::JournalWriteError,
    > {
        let root = root.to_owned();
        let relative = relative.to_owned();
        let expected = *expected;
        tokio::task::spawn_blocking(move || {
            lingxi_core::host::rooted_fs::truncate_journal_durable(
                &root, &relative, &expected, length,
            )
        })
        .await
        .map_err(|error| {
            lingxi_core::host::rooted_fs::JournalWriteError::uncertain(FsError::Io(
                error.to_string(),
            ))
        })?
    }

    async fn sync_file_rooted_journal_durable(
        &self,
        root: &Path,
        relative: &Path,
        expected: &lingxi_core::host::rooted_fs::JournalCursor,
    ) -> Result<(), FsError> {
        let root = root.to_owned();
        let relative = relative.to_owned();
        let expected = *expected;
        tokio::task::spawn_blocking(move || {
            lingxi_core::host::rooted_fs::sync_journal_durable(&root, &relative, &expected)
        })
        .await
        .map_err(|error| FsError::Io(error.to_string()))?
    }

    async fn append_file_rooted_durable(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
    ) -> Result<(), FsError> {
        lingxi_core::host::rooted_fs::append_file_durable(root, relative, content)
    }

    async fn sync_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<(), FsError> {
        lingxi_core::host::rooted_fs::sync_file_no_follow(root, relative)
    }

    async fn truncate_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
        length: u64,
    ) -> Result<(), FsError> {
        lingxi_core::host::rooted_fs::truncate_file_pinned(root, relative, length, None)
    }

    async fn append_file_rooted_no_follow_pinned(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
        expected: Option<&lingxi_core::host::rooted_fs::RootIdentity>,
    ) -> Result<(), FsError> {
        lingxi_core::host::rooted_fs::append_file_pinned(root, relative, content, expected)
    }

    async fn append_file_rooted_staged(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
        expected: Option<&lingxi_core::host::rooted_fs::RootIdentity>,
    ) -> Result<(), lingxi_core::host::filesystem::FileAppendError> {
        lingxi_core::host::rooted_fs::append_file_staged(root, relative, content, expected)
    }

    async fn append_file_with_mode(
        &self,
        path: &str,
        content: &str,
        mode: u32,
    ) -> Result<(), FsError> {
        // claude-code `appendToFile` (`sessionStorage.ts:634`):
        // `fsAppendFile(path, data, { mode: 0o600 })` — the create applies `mode`
        // (ignored when the file already exists), so the session transcript is
        // owner-only. `.mode()` sets the create permission bits (pre-umask).
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(mode)
            .open(path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        f.write_all(content.as_bytes())
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        f.flush().await.map_err(|e| FsError::Io(e.to_string()))
    }

    async fn read_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<FileContent, FsError> {
        let content = lingxi_core::host::rooted_fs::read_to_string(root, relative)?;
        Ok(FileContent {
            total_lines: content.lines().count() as u64,
            content,
            truncated: false,
        })
    }

    async fn read_file_rooted_no_follow_window(
        &self,
        root: &Path,
        relative: &Path,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        let content = lingxi_core::host::rooted_fs::read_to_string(root, relative)?;
        Ok(lingxi_core::host::apply_line_window(content, offset, limit))
    }

    async fn read_file_rooted_no_follow_window_pinned(
        &self,
        root: &Path,
        relative: &Path,
        offset: Option<u64>,
        limit: Option<u64>,
        expected: Option<&lingxi_core::host::rooted_fs::RootIdentity>,
    ) -> Result<FileContent, FsError> {
        let content =
            lingxi_core::host::rooted_fs::read_to_string_pinned(root, relative, expected)?;
        Ok(lingxi_core::host::apply_line_window(content, offset, limit))
    }

    async fn read_file_rooted_byte_window_pinned(
        &self,
        root: &Path,
        relative: &Path,
        expected: Option<&lingxi_core::host::rooted_fs::RootIdentity>,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<u8>, FsError> {
        let root = root.to_owned();
        let relative = relative.to_owned();
        let expected = expected.copied();
        tokio::task::spawn_blocking(move || {
            lingxi_core::host::rooted_fs::read_byte_window_pinned(
                &root,
                &relative,
                expected.as_ref(),
                offset,
                limit,
            )
        })
        .await
        .map_err(|error| FsError::Io(error.to_string()))?
    }

    async fn read_file_rooted_text_window_pinned(
        &self,
        root: &Path,
        relative: &Path,
        expected: Option<&lingxi_core::host::rooted_fs::RootIdentity>,
        offset: Option<u64>,
        limit: Option<u64>,
        tail_bytes: u64,
    ) -> Result<lingxi_core::host::rooted_fs::TextWindow, FsError> {
        let root = root.to_owned();
        let relative = relative.to_owned();
        let expected = expected.copied();
        tokio::task::spawn_blocking(move || {
            lingxi_core::host::rooted_fs::read_text_window_pinned(
                &root,
                &relative,
                expected.as_ref(),
                offset,
                limit,
                tail_bytes,
            )
        })
        .await
        .map_err(|error| FsError::Io(error.to_string()))?
    }

    async fn write_file_rooted_atomic(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
    ) -> Result<(), FsError> {
        lingxi_core::host::rooted_fs::atomic_write(
            root,
            relative,
            content.as_bytes(),
            lingxi_core::host::AtomicWriteOptions::default(),
        )
    }

    async fn write_file_rooted_atomic_pinned(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
        expected: Option<&lingxi_core::host::rooted_fs::RootIdentity>,
    ) -> Result<(), FsError> {
        let expected = expected
            .copied()
            .ok_or_else(|| FsError::Io("missing scheduler root identity".into()))?;
        let root = root.to_owned();
        let relative = relative.to_owned();
        let content = content.to_owned();
        tokio::task::spawn_blocking(move || {
            lingxi_core::host::rooted_fs::atomic_write_pinned(
                &root,
                &relative,
                content.as_bytes(),
                lingxi_core::host::AtomicWriteOptions::default(),
                &expected,
            )
        })
        .await
        .map_err(|error| FsError::Io(error.to_string()))?
    }

    async fn flock_exclusive_rooted_pinned(
        &self,
        root: &Path,
        relative: &Path,
        expected: Option<&lingxi_core::host::rooted_fs::RootIdentity>,
    ) -> Result<Box<dyn FlockGuard>, FsError> {
        let expected = expected
            .copied()
            .ok_or_else(|| FsError::Io("missing scheduler root identity".into()))?;
        acquire_rooted_lock(root, relative, Some(expected)).await
    }

    async fn flock_exclusive_rooted(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<Box<dyn FlockGuard>, FsError> {
        acquire_rooted_lock(root, relative, None).await
    }

    async fn delete_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<(), FsError> {
        lingxi_core::host::rooted_fs::remove_file(root, relative)
    }

    async fn truncate(&self, path: &str, len: u64) -> Result<(), FsError> {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| FsError::Io(e.to_string()))?;
        f.set_len(len).map_err(|e| FsError::Io(e.to_string()))
    }

    async fn file_mtime(&self, path: &str) -> Result<std::time::SystemTime, FsError> {
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        meta.modified().map_err(|e| FsError::Io(e.to_string()))
    }

    async fn file_size(&self, path: &str) -> Result<u64, FsError> {
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;
        Ok(meta.len())
    }

    async fn delete_file(&self, path: &str) -> Result<(), FsError> {
        tokio::fs::remove_file(path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))
    }

    async fn symlink(&self, target: &str, link: &str) -> Result<(), FsError> {
        #[cfg(unix)]
        {
            tokio::fs::symlink(target, link)
                .await
                .map_err(|e| FsError::Io(e.to_string()))
        }
        #[cfg(windows)]
        {
            tokio::fs::symlink_file(target, link)
                .await
                .map_err(|e| FsError::Io(e.to_string()))
        }
    }

    async fn flock_exclusive(&self, path: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        let path = path.to_owned();
        match tokio::task::spawn_blocking(move || {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| FsError::Io(e.to_string()))?;
            f.lock_exclusive().map_err(|e| FsError::Io(e.to_string()))?;
            Ok(Box::new(PosixFlockGuard { _file: f, path }) as Box<dyn FlockGuard>)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => Err(FsError::Io(error.to_string())),
        }
    }

    async fn fsync(&self, path: &str) -> Result<(), FsError> {
        let path = path.to_owned();
        match tokio::task::spawn_blocking(move || {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .open(path)
                .map_err(|e| FsError::Io(e.to_string()))?;
            f.sync_all().map_err(|e| FsError::Io(e.to_string()))
        })
        .await
        {
            Ok(result) => result,
            Err(error) => Err(FsError::Io(error.to_string())),
        }
    }

    fn is_within_workspace(&self, path: &str) -> bool {
        std::path::Path::new(path).starts_with(&self.workspace_root)
    }

    async fn watch(
        &self,
        dir: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = FileEvent> + Send>>, FsError> {
        crate::watch_helper::watch_dir_with_debounce(
            dir,
            crate::watch_helper::DEFAULT_STABILITY_THRESHOLD_MS,
            crate::watch_helper::DEFAULT_POLL_INTERVAL_MS,
        )
        .await
    }
}

/// Guard holding the `fs2` advisory lock. Dropping the file releases the
/// lock — exactly the semantics [`FlockGuard`] documents.
struct PosixFlockGuard {
    _file: std::fs::File,
    path: String,
}

impl FlockGuard for PosixFlockGuard {
    fn path(&self) -> &str {
        &self.path
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[tokio::test(flavor = "current_thread")]
    async fn rooted_lock_contention_yields_to_the_holder_release_timer() {
        use lingxi_core::host::FileSystem;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_owned();
        let relative = std::path::Path::new("timer.lock");
        let fs = std::sync::Arc::new(super::PosixFileSystem::new(root.clone()));
        let pin = fs.root_identity_no_follow(&root).await.unwrap();
        for pinned in [false, true] {
            let holder = fs.flock_exclusive_rooted(&root, relative).await.unwrap();
            let contender_fs = fs.clone();
            let contender_root = root.clone();
            let contender = tokio::spawn(async move {
                if pinned {
                    contender_fs
                        .flock_exclusive_rooted_pinned(&contender_root, relative, pin.as_ref())
                        .await
                } else {
                    contender_fs
                        .flock_exclusive_rooted(&contender_root, relative)
                        .await
                }
            });
            let release = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                drop(holder);
            });
            let guard = tokio::time::timeout(std::time::Duration::from_secs(5), contender)
                .await
                .expect("the holder timer must run on a current-thread runtime")
                .unwrap()
                .unwrap();
            release.await.unwrap();
            guard.validate_current().unwrap();
            drop(guard);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_rooted_lock_owner_releases_its_guard() {
        use lingxi_core::host::FileSystem;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_owned();
        let relative = std::path::Path::new("cancel.lock");
        let fs = std::sync::Arc::new(super::PosixFileSystem::new(root.clone()));
        let pin = fs.root_identity_no_follow(&root).await.unwrap();
        let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel();
        let owner_fs = fs.clone();
        let owner_root = root.clone();
        let owner = tokio::spawn(async move {
            let _guard = owner_fs
                .flock_exclusive_rooted_pinned(&owner_root, relative, pin.as_ref())
                .await
                .unwrap();
            acquired_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        acquired_rx.await.unwrap();
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());
        let guard = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fs.flock_exclusive_rooted(&root, relative),
        )
        .await
        .expect("cancelled owner releases its advisory lock")
        .unwrap();
        drop(guard);
    }

    #[test]
    fn cancelling_contended_rooted_lock_keeps_the_only_blocking_worker_free() {
        use lingxi_core::host::FileSystem;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap()
            .block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path().to_owned();
                let relative = std::path::Path::new("pending.lock");
                let fs = std::sync::Arc::new(super::PosixFileSystem::new(root.clone()));
                let holder = fs.flock_exclusive_rooted(&root, relative).await.unwrap();
                let contender_fs = fs.clone();
                let contender_root = root.clone();
                let contender = tokio::spawn(async move {
                    contender_fs
                        .flock_exclusive_rooted(&contender_root, relative)
                        .await
                });
                // Give the contender an opportunity to attempt the held lock.
                // This is scheduling coordination, not a duration assertion.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                contender.abort();
                assert!(matches!(contender.await, Err(error) if error.is_cancelled()));
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    tokio::task::spawn_blocking(|| ()),
                )
                .await
                .expect("contention must not occupy the blocking worker")
                .unwrap();
                drop(holder);
                let guard = fs.flock_exclusive_rooted(&root, relative).await.unwrap();
                drop(guard);
            });
    }

    #[tokio::test]
    async fn rooted_text_projection_streams_tail_and_validates_skipped_bytes() {
        use lingxi_core::host::FileSystem;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let relative = std::path::Path::new("spool.output");
        let path = root.join(relative);
        std::fs::write(&path, "head\r\n🙂中\ntail\n").unwrap();
        let fs = super::PosixFileSystem::new(root.to_owned());
        let pin = fs.root_identity_no_follow(root).await.unwrap();
        let tail = fs
            .read_file_rooted_text_window_pinned(root, relative, pin.as_ref(), None, None, 10)
            .await
            .unwrap();
        assert_eq!(tail.content, "中\ntail\n");
        assert_eq!(tail.omitted_bytes, 10);
        assert_eq!(tail.total_lines, 3);
        let line = fs
            .read_file_rooted_text_window_pinned(root, relative, pin.as_ref(), Some(1), Some(1), 1)
            .await
            .unwrap();
        assert_eq!(line.content, "🙂中");
        assert_eq!(line.total_lines, 3);
        assert_eq!(line.omitted_bytes, 0);
        assert_eq!(
            fs.read_file_rooted_byte_window_pinned(root, relative, pin.as_ref(), 6, 4,)
                .await
                .unwrap(),
            "🙂".as_bytes()
        );
        std::fs::write(&path, b"first\n\xff\n").unwrap();
        assert!(matches!(
            fs.read_file_rooted_text_window_pinned(
                root,
                relative,
                pin.as_ref(),
                Some(0),
                Some(1),
                1,
            )
            .await,
            Err(lingxi_core::host::FsError::Io(_))
        ));
        std::fs::remove_file(&path).unwrap();
        let outside = directory.path().join("target");
        std::fs::write(&outside, "hidden").unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(matches!(
            fs.read_file_rooted_text_window_pinned(root, relative, pin.as_ref(), None, None, 3,)
                .await,
            Err(lingxi_core::host::FsError::OutsideWorkspace(_))
        ));
    }

    use super::*;
    use lingxi_core::host::FileSystem;

    fn fs_at(root: &std::path::Path) -> PosixFileSystem {
        PosixFileSystem::new(root.to_path_buf())
    }

    #[tokio::test]
    async fn staged_task_append_reports_open_failure_without_writing_symlink_target() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "unchanged").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join("output")).unwrap();
        let error = fs_at(dir.path())
            .append_file_rooted_staged(dir.path(), Path::new("output"), "payload", None)
            .await
            .unwrap_err();
        assert_eq!(
            error.stage,
            lingxi_core::host::filesystem::FileAppendStage::Open
        );
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "unchanged");
    }

    #[test]
    fn cache_identity_is_stable_across_instances_for_the_same_root() {
        let dir = tempfile::tempdir().unwrap();
        let first = fs_at(dir.path());
        let second = fs_at(dir.path());
        assert_eq!(first.cache_identity(), second.cache_identity());
    }

    // ---- T4: exclusive-create (O_CREAT | O_EXCL | O_NOFOLLOW) --------------

    #[tokio::test]
    async fn create_new_file_creates_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let fs = fs_at(dir.path());
        let p = dir.path().join("spool.txt");
        let ps = p.to_str().unwrap();

        fs.create_new_file(ps).await.expect("first create succeeds");
        assert!(p.exists(), "file materialized");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "", "created empty");
    }

    #[tokio::test]
    async fn second_create_new_file_errors_and_does_not_truncate() {
        // The double-allocate truncate race: a second exclusive create of the
        // same path must FAIL (AlreadyExists), not silently clobber bytes a
        // worker already appended.
        let dir = tempfile::tempdir().unwrap();
        let fs = fs_at(dir.path());
        let p = dir.path().join("spool.txt");
        let ps = p.to_str().unwrap();

        fs.create_new_file(ps).await.unwrap();
        // Simulate a worker appending output between the two allocations.
        fs.append_file_no_follow(ps, "worker output\n")
            .await
            .unwrap();

        let err = fs
            .create_new_file(ps)
            .await
            .expect_err("a second exclusive create must error");
        assert!(
            matches!(err, FsError::AlreadyExists(_)),
            "second create returns AlreadyExists, not a silent truncate; got {err:?}"
        );

        // The worker's output survived — the failed create touched no bytes.
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "worker output\n",
            "the existing output was NOT truncated by the refused create"
        );
    }

    // ---- T18: O_NOFOLLOW refuses a pre-planted symlink --------------------

    #[tokio::test]
    async fn create_new_file_refuses_a_preplanted_symlink() {
        // An attacker plants a symlink at the spool path pointing at a host
        // file. O_NOFOLLOW must refuse to open it (create_new also implies
        // O_EXCL → the existing symlink is an AlreadyExists collision), so the
        // host file is never created/written through the link.
        let dir = tempfile::tempdir().unwrap();
        let fs = fs_at(dir.path());

        let target = dir.path().join("victim-host-file");
        let link = dir.path().join("spool.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = fs
            .create_new_file(link.to_str().unwrap())
            .await
            .expect_err("create_new_file must refuse a symlink at the path");
        assert!(
            matches!(err, FsError::AlreadyExists(_) | FsError::Io(_)),
            "symlink create is refused (EEXIST/ELOOP), got {err:?}"
        );
        // The symlink target was never materialized through the link.
        assert!(
            !target.exists(),
            "the host file behind the symlink was NOT created"
        );
    }

    #[tokio::test]
    async fn append_no_follow_refuses_a_symlinked_spool_path() {
        // O_NOFOLLOW on the append open: a worker append cannot be redirected
        // through a symlink planted at the spool path.
        let dir = tempfile::tempdir().unwrap();
        let fs = fs_at(dir.path());

        let target = dir.path().join("victim-host-file");
        std::fs::write(&target, "original host content").unwrap();
        let link = dir.path().join("spool.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = fs
            .append_file_no_follow(link.to_str().unwrap(), "redirected!\n")
            .await
            .expect_err("append_file_no_follow must refuse to follow the symlink");
        assert!(matches!(err, FsError::Io(_)), "got {err:?}");
        // The host file behind the symlink is untouched (no append landed).
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "original host content",
            "the symlink target was NOT written through"
        );
    }
}
