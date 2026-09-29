//! `tokio::fs`-backed [`FileSystem`] for desktop hosts.
//!
//! Implements the engine's sandboxed filesystem trait using the real OS
//! filesystem. Path containment is enforced via prefix-match against the
//! workspace root supplied at construction; this is intentionally minimal —
//! a hardened POSIX platform (Plan 17+) layers symlink-canonicalisation and
//! sandbox-exec on top.

use async_trait::async_trait;
use fs2::FileExt;
use futures_core::stream::Stream;
use futures_util::stream::empty;
use lingxi_core::host::{
    FileContent, FileEvent, FileSystem, FileSystemCacheIdentity, FlockGuard, FsError,
};
use std::path::{Path, PathBuf};
use std::pin::Pin;

/// Concrete [`FileSystem`] backed by `tokio::fs`.
///
/// Construction takes the workspace root used by
/// [`FileSystem::is_within_workspace`]; pass the directory the CLI was
/// launched in (`std::env::current_dir()`) for the M1 demo.
pub struct PosixFileSystem {
    workspace_root: PathBuf,
}

impl PosixFileSystem {
    /// Build a new filesystem rooted at `workspace_root`.
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
impl FileSystem for PosixFileSystem {
    async fn root_identity_no_follow(
        &self,
        root: &Path,
    ) -> Result<Option<lingxi_core::host::rooted_fs::RootIdentity>, FsError> {
        lingxi_core::host::rooted_fs::root_identity(root).map(Some)
    }

    fn cache_identity(&self) -> Option<FileSystemCacheIdentity> {
        Some(FileSystemCacheIdentity::new(
            "posix-minimal",
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
        #[cfg(unix)]
        let opts = {
            use std::os::unix::fs::OpenOptionsExt;
            let mut o = std::fs::OpenOptions::new();
            o.write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW); // O_CREAT | O_EXCL
            o
        };
        // Non-unix has no O_NOFOLLOW; `create_new` still gives the O_EXCL
        // exclusive-create / AlreadyExists collision guarantee.
        #[cfg(not(unix))]
        let opts = {
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create_new(true);
            o
        };
        let f = opts.open(path).map_err(|e| {
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

    async fn append_file_no_follow(&self, path: &str, content: &str) -> Result<(), FsError> {
        // SECURITY: append with O_NOFOLLOW, byte-for-byte the claude-code
        // task-output append open (`diskOutput.ts`):
        //   O_WRONLY | O_APPEND | O_CREAT | O_NOFOLLOW
        // O_NOFOLLOW refuses to follow a symlink planted at the spool path, so a
        // worker append cannot be redirected to an arbitrary host file.
        // (`tokio::fs::OpenOptions::custom_flags` is inherent — no trait import.)
        use tokio::io::AsyncWriteExt;
        let mut opts = tokio::fs::OpenOptions::new();
        opts.append(true).create(true);
        // Non-unix has no O_NOFOLLOW; fall back to a plain create+append.
        #[cfg(unix)]
        opts.custom_flags(libc::O_NOFOLLOW);
        let mut f = opts
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
    }

    async fn flock_exclusive(&self, path: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| FsError::Io(e.to_string()))?;
        f.lock_exclusive().map_err(|e| FsError::Io(e.to_string()))?;
        Ok(Box::new(PosixFlockGuard {
            _file: f,
            path: path.to_string(),
        }))
    }

    async fn fsync(&self, path: &str) -> Result<(), FsError> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|e| FsError::Io(e.to_string()))?;
        f.sync_all().map_err(|e| FsError::Io(e.to_string()))
    }

    fn is_within_workspace(&self, path: &str) -> bool {
        std::path::Path::new(path).starts_with(&self.workspace_root)
    }

    async fn watch(
        &self,
        _dir: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = FileEvent> + Send>>, FsError> {
        // posix-minimal: emit an empty stream. Real `notify`-backed watcher
        // lands in `platforms/posix` (Plan 17+).
        Ok(Box::pin(empty()))
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
    use super::*;
    use lingxi_core::host::FileSystem;

    fn fs_at(root: &std::path::Path) -> PosixFileSystem {
        PosixFileSystem::new(root.to_path_buf())
    }

    // ---- exclusive-create (O_CREAT | O_EXCL [| O_NOFOLLOW]) ----------------

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
        // worker already appended. This is the regression posix-minimal had
        // while it inherited the `write_file(path, "")` trait default.
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

    // ---- O_NOFOLLOW refuses a pre-planted symlink (unix only) -------------

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    // ---- append_file flush determinism ------------------------------------

    #[tokio::test]
    async fn append_file_flushes_so_immediate_read_back_sees_the_bytes() {
        // `append_file` must flush the tokio blocking-pool buffer so an
        // immediately-following read observes the appended bytes (the spool's
        // append-then-read offset computation depends on this).
        let dir = tempfile::tempdir().unwrap();
        let fs = fs_at(dir.path());
        let p = dir.path().join("spool.txt");
        let ps = p.to_str().unwrap();

        fs.append_file(ps, "first\n").await.unwrap();
        fs.append_file(ps, "second\n").await.unwrap();

        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "first\nsecond\n",
            "both appends are visible immediately after the calls return"
        );
    }
}
