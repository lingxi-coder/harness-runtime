//! `tokio::fs`-backed [`FileSystem`] for Windows hosts.
//!
//! Implements the engine's sandboxed filesystem trait using the real OS
//! filesystem. Path containment is enforced via prefix-match against the
//! workspace root supplied at construction. [`FileSystem::watch`] is
//! backed by `notify` + `notify-debouncer-mini` (see `watch_helper`),
//! delivering chokidar-4-equivalent `awaitWriteFinish` semantics on
//! Windows (via `ReadDirectoryChangesW` selected by `RecommendedWatcher`).

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
pub struct WindowsFileSystem {
    workspace_root: PathBuf,
}

impl WindowsFileSystem {
    /// Build a new `WindowsFileSystem` rooted at `workspace_root`.
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

#[async_trait]
impl FileSystem for WindowsFileSystem {
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
            "windows",
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
            .map_err(|e| FsError::Io(e.to_string()))
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

    async fn append_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
    ) -> Result<(), FsError> {
        lingxi_core::host::rooted_fs::append_file(root, relative, content)
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
            use std::io::{Read, Seek, SeekFrom};
            let mut file = lingxi_core::host::rooted_fs::open_read_file_pinned(
                &root,
                &relative,
                expected.as_ref(),
            )?;
            file.seek(SeekFrom::Start(offset))
                .map_err(|error| FsError::Io(error.to_string()))?;
            let mut bytes = Vec::new();
            file.take(limit)
                .read_to_end(&mut bytes)
                .map_err(|error| FsError::Io(error.to_string()))?;
            Ok(bytes)
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
        let expected =
            expected.ok_or_else(|| FsError::Io("missing scheduler root identity".into()))?;
        lingxi_core::host::rooted_fs::lock_exclusive_pinned(
            root,
            relative,
            lingxi_core::host::rooted_fs::PRIVATE_DIR_MODE,
            lingxi_core::host::rooted_fs::PRIVATE_FILE_MODE,
            Some(expected),
        )
        .map(|guard| Box::new(guard) as Box<dyn FlockGuard>)
    }

    async fn flock_exclusive_rooted(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<Box<dyn FlockGuard>, FsError> {
        lingxi_core::host::rooted_fs::lock_exclusive(
            root,
            relative,
            lingxi_core::host::rooted_fs::PRIVATE_DIR_MODE,
            lingxi_core::host::rooted_fs::PRIVATE_FILE_MODE,
        )
        .map(|guard| Box::new(guard) as Box<dyn FlockGuard>)
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
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (target, link);
            Err(FsError::Io("symlink: unsupported on this OS".into()))
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
            Ok(Box::new(WindowsFlockGuard { _file: f, path }) as Box<dyn FlockGuard>)
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
/// lock — exactly the semantics [`FlockGuard`] documents. On Windows this
/// uses `LockFileEx` under the hood via `fs2::FileExt::lock_exclusive`.
struct WindowsFlockGuard {
    _file: std::fs::File,
    path: String,
}

impl FlockGuard for WindowsFlockGuard {
    fn path(&self) -> &str {
        &self.path
    }
}
