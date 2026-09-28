//! Guest-path translation for file tools over a mobile-linux runtime.
//!
//! On mobile-linux platforms the model lives in TWO coordinate systems: file
//! tools (Read/Write/Edit) speak host paths while the shell speaks guest
//! paths (`/workspace/<id>`, `/root`). Every writable guest area is a
//! host-backed bind mount, so the split is purely a missing translation
//! table — [`GuestPathFileSystem`] closes it by rewriting guest paths onto
//! their host twins before delegating to the real host filesystem. I/O never
//! enters the emulated kernel; performance is identical to the inner
//! filesystem.
//!
//! Resolution rules, in order:
//! 1. A path under a live mount's `guest_path` is rebased onto that mount's
//!    `host_path` (longest guest prefix wins; writes to a `read_only` mount
//!    are refused).
//! 2. A path inside guest space but NOT under any mount is refused outright:
//!    that region lives in the emulated filesystem (iSH fakefs), whose
//!    `meta.db` inode bookkeeping a direct host write would silently corrupt.
//!    This fence is the one hard rejection of the design.
//! 3. Anything else is passed through unchanged — host paths keep working
//!    exactly as before (compatibility during the host→guest presentation
//!    migration).
//!
//! The mount table is read live from [`MobileLinuxRuntime::current_mounts`]
//! on every call, so external mounts added via `configure_mounts` translate
//! without rebuilding the filesystem.

use async_trait::async_trait;
use futures::Stream;
use platform_api::mobile_linux::guest_paths;
use platform_api::{FileContent, FileEvent, FileSystem, FlockGuard, FsError, MobileLinuxRuntime};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

/// [`FileSystem`] decorator that makes guest paths first-class for file
/// tools. See the module docs for the resolution rules.
pub struct GuestPathFileSystem {
    inner: Arc<dyn FileSystem>,
    runtime: Arc<dyn MobileLinuxRuntime>,
}

impl GuestPathFileSystem {
    /// Wrap `inner` so guest paths translate against `runtime`'s live mount
    /// table.
    #[must_use]
    pub fn new(inner: Arc<dyn FileSystem>, runtime: Arc<dyn MobileLinuxRuntime>) -> Self {
        Self { inner, runtime }
    }

    /// Resolve `path` to the host path the inner filesystem should receive.
    /// `write` marks mutating operations, which a `read_only` mount refuses.
    fn resolve(&self, path: &str, write: bool) -> Result<String, FsError> {
        match self.resolve_translation(path, write)? {
            Some(host) => Ok(host),
            None => Ok(path.to_string()),
        }
    }

    /// Core translation: `Ok(Some(host))` for a mounted guest path,
    /// `Ok(None)` for a host path (passthrough), `Err` for fenced guest
    /// space / refused writes.
    fn resolve_translation(&self, path: &str, write: bool) -> Result<Option<String>, FsError> {
        let mounts = self.runtime.current_mounts();
        mobile_linux_core::guest_path::resolve_guest_path(
            path,
            &mounts,
            &guest_paths::writable_roots(),
            write,
        )
        .map(|host| host.map(|value| value.to_string_lossy().into_owned()))
        .map_err(|error| match error {
            mobile_linux_core::guest_path::GuestPathError::ReadOnly { guest_path } => {
                FsError::PermissionDenied(format!(
                    "guest path is on a read-only mount ({guest_path}): {path}"
                ))
            }
            mobile_linux_core::guest_path::GuestPathError::NotHostBacked => {
                FsError::PermissionDenied(format!(
                    "guest path is not host-backed (emulated-filesystem area); file tools can only \
                     reach bind-mounted guest paths — use the shell for: {path}"
                ))
            }
        })
    }

    fn resolve_root(&self, root: &Path, write: bool) -> Result<PathBuf, FsError> {
        self.resolve(&root.to_string_lossy(), write)
            .map(PathBuf::from)
    }
}

#[async_trait]
impl FileSystem for GuestPathFileSystem {
    async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        let path = self.resolve(path, false)?;
        self.inner.read_file(&path, offset, limit).await
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        let path = self.resolve(path, true)?;
        self.inner.write_file(&path, content).await
    }

    fn is_within_workspace(&self, path: &str) -> bool {
        match self.resolve(path, false) {
            Ok(path) => self.inner.is_within_workspace(&path),
            Err(_) => false,
        }
    }

    async fn watch(
        &self,
        dir: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = FileEvent> + Send>>, FsError> {
        let dir = self.resolve(dir, false)?;
        self.inner.watch(&dir).await
    }

    async fn append_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        let path = self.resolve(path, true)?;
        self.inner.append_file(&path, content).await
    }

    async fn create_new_file(&self, path: &str) -> Result<(), FsError> {
        let path = self.resolve(path, true)?;
        self.inner.create_new_file(&path).await
    }

    async fn append_file_no_follow(&self, path: &str, content: &str) -> Result<(), FsError> {
        let path = self.resolve(path, true)?;
        self.inner.append_file_no_follow(&path, content).await
    }

    async fn append_file_rooted_staged(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
        expected: Option<&platform_api::rooted_fs::RootIdentity>,
    ) -> Result<(), platform_api::filesystem::FileAppendError> {
        let root = self.resolve_root(root, true).map_err(|error| {
            platform_api::filesystem::FileAppendError {
                stage: platform_api::filesystem::FileAppendStage::Open,
                error,
            }
        })?;
        self.inner
            .append_file_rooted_staged(&root, relative, content, expected)
            .await
    }

    async fn append_file_with_mode(
        &self,
        path: &str,
        content: &str,
        mode: u32,
    ) -> Result<(), FsError> {
        let path = self.resolve(path, true)?;
        self.inner.append_file_with_mode(&path, content, mode).await
    }

    async fn read_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<FileContent, FsError> {
        let root = self.resolve_root(root, false)?;
        self.inner.read_file_rooted_no_follow(&root, relative).await
    }

    async fn write_file_rooted_atomic(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
    ) -> Result<(), FsError> {
        let root = self.resolve_root(root, true)?;
        self.inner
            .write_file_rooted_atomic(&root, relative, content)
            .await
    }

    async fn flock_exclusive_rooted(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<Box<dyn FlockGuard>, FsError> {
        let root = self.resolve_root(root, true)?;
        self.inner.flock_exclusive_rooted(&root, relative).await
    }

    async fn delete_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<(), FsError> {
        let root = self.resolve_root(root, true)?;
        self.inner
            .delete_file_rooted_no_follow(&root, relative)
            .await
    }

    async fn truncate(&self, path: &str, len: u64) -> Result<(), FsError> {
        let path = self.resolve(path, true)?;
        self.inner.truncate(&path, len).await
    }

    async fn file_mtime(&self, path: &str) -> Result<std::time::SystemTime, FsError> {
        let path = self.resolve(path, false)?;
        self.inner.file_mtime(&path).await
    }

    async fn file_size(&self, path: &str) -> Result<u64, FsError> {
        let path = self.resolve(path, false)?;
        self.inner.file_size(&path).await
    }

    async fn delete_file(&self, path: &str) -> Result<(), FsError> {
        let path = self.resolve(path, true)?;
        self.inner.delete_file(&path).await
    }

    async fn symlink(&self, target: &str, link: &str) -> Result<(), FsError> {
        // The link is the entry being created; the target is only read.
        let target = self.resolve(target, false)?;
        let link = self.resolve(link, true)?;
        self.inner.symlink(&target, &link).await
    }

    async fn flock_exclusive(&self, path: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        // Lock files are created on demand, so this is a mutating open.
        let path = self.resolve(path, true)?;
        self.inner.flock_exclusive(&path).await
    }

    async fn fsync(&self, path: &str) -> Result<(), FsError> {
        let path = self.resolve(path, false)?;
        self.inner.fsync(&path).await
    }

    /// The seam the file TOOLS consult before canonicalization: they run on
    /// raw `tokio::fs`, not this trait's I/O methods, so translation must be
    /// answered as a question rather than applied implicitly.
    fn translate_model_path(&self, path: &str, write: bool) -> Result<Option<String>, FsError> {
        self.resolve_translation(path, write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::{
        LinuxCommandRequest, LinuxCommandResult, LinuxProcessHandle, MobileLinuxCapability,
        MobileLinuxError, MobileLinuxRuntimeMode, MountPurpose, MountSpec, PtyOpenRequest,
        PtySessionHandle, RootfsStatus, SandboxBackend,
    };
    use std::sync::Mutex;

    /// Runtime stub that exists only to serve a mount table.
    struct MountsOnlyRuntime {
        mounts: Vec<MountSpec>,
    }

    #[async_trait]
    impl MobileLinuxRuntime for MountsOnlyRuntime {
        fn backend(&self) -> SandboxBackend {
            SandboxBackend::IosIsh
        }
        fn mode(&self) -> MobileLinuxRuntimeMode {
            MobileLinuxRuntimeMode::MobileLinux
        }
        async fn probe_capability(&self) -> MobileLinuxCapability {
            unimplemented!("not used by GuestPathFileSystem")
        }
        async fn boot(&self) -> Result<RootfsStatus, MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        async fn shutdown(&self) -> Result<(), MobileLinuxError> {
            Ok(())
        }
        async fn run(
            &self,
            _request: LinuxCommandRequest,
        ) -> Result<LinuxCommandResult, MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        async fn spawn_background(
            &self,
            _request: LinuxCommandRequest,
        ) -> Result<LinuxProcessHandle, MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        async fn kill(&self, _handle: &LinuxProcessHandle) -> Result<(), MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        async fn open_pty(
            &self,
            _request: PtyOpenRequest,
        ) -> Result<PtySessionHandle, MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        async fn write_pty(
            &self,
            _handle: &PtySessionHandle,
            _input: Vec<u8>,
        ) -> Result<(), MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        async fn resize_pty(
            &self,
            _handle: &PtySessionHandle,
            _size: platform_api::PtySize,
        ) -> Result<(), MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        async fn close_pty(&self, _handle: &PtySessionHandle) -> Result<(), MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        async fn rootfs_status(&self) -> Result<RootfsStatus, MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        async fn verify_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        async fn repair_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        async fn reset_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        async fn configure_mounts(&self, _mounts: Vec<MountSpec>) -> Result<(), MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }
        fn current_mounts(&self) -> Vec<MountSpec> {
            self.mounts.clone()
        }
    }

    /// Inner filesystem that records the paths it is handed.
    #[derive(Default)]
    struct RecordingFs {
        calls: Mutex<Vec<(String, String)>>,
    }

    impl RecordingFs {
        fn record(&self, op: &str, path: &str) {
            self.calls
                .lock()
                .expect("calls mutex")
                .push((op.to_string(), path.to_string()));
        }

        fn last(&self) -> (String, String) {
            self.calls
                .lock()
                .expect("calls mutex")
                .last()
                .cloned()
                .expect("at least one recorded call")
        }
    }

    #[async_trait]
    impl FileSystem for RecordingFs {
        async fn read_file(
            &self,
            path: &str,
            _offset: Option<u64>,
            _limit: Option<u64>,
        ) -> Result<FileContent, FsError> {
            self.record("read", path);
            Ok(FileContent {
                content: String::new(),
                total_lines: 0,
                truncated: false,
            })
        }
        async fn write_file(&self, path: &str, _content: &str) -> Result<(), FsError> {
            self.record("write", path);
            Ok(())
        }
        fn is_within_workspace(&self, path: &str) -> bool {
            path.starts_with("/host/workspace")
        }
        async fn watch(
            &self,
            _dir: &str,
        ) -> Result<Pin<Box<dyn Stream<Item = FileEvent> + Send>>, FsError> {
            Err(FsError::Io("watch unused".to_string()))
        }
        async fn append_file(&self, path: &str, _content: &str) -> Result<(), FsError> {
            self.record("append", path);
            Ok(())
        }
        async fn truncate(&self, path: &str, _len: u64) -> Result<(), FsError> {
            self.record("truncate", path);
            Ok(())
        }
        async fn file_mtime(&self, path: &str) -> Result<std::time::SystemTime, FsError> {
            self.record("mtime", path);
            Ok(std::time::SystemTime::UNIX_EPOCH)
        }
        async fn file_size(&self, path: &str) -> Result<u64, FsError> {
            self.record("size", path);
            Ok(0)
        }
        async fn delete_file(&self, path: &str) -> Result<(), FsError> {
            self.record("delete", path);
            Ok(())
        }
        async fn symlink(&self, target: &str, link: &str) -> Result<(), FsError> {
            self.record("symlink-target", target);
            self.record("symlink-link", link);
            Ok(())
        }
        async fn flock_exclusive(&self, _path: &str) -> Result<Box<dyn FlockGuard>, FsError> {
            Err(FsError::Io("flock unused".to_string()))
        }
        async fn fsync(&self, path: &str) -> Result<(), FsError> {
            self.record("fsync", path);
            Ok(())
        }
    }

    fn workspace_mount() -> MountSpec {
        MountSpec {
            host_path: PathBuf::from("/host/workspace/abc"),
            guest_path: "/workspace/abc".to_string(),
            read_only: false,
            purpose: MountPurpose::Workspace,
        }
    }

    fn home_mount() -> MountSpec {
        MountSpec {
            host_path: PathBuf::from("/host/persistent/root"),
            guest_path: "/root".to_string(),
            read_only: false,
            purpose: MountPurpose::External,
        }
    }

    fn fs_with(mounts: Vec<MountSpec>) -> (Arc<RecordingFs>, GuestPathFileSystem) {
        let inner = Arc::new(RecordingFs::default());
        let fs = GuestPathFileSystem::new(inner.clone(), Arc::new(MountsOnlyRuntime { mounts }));
        (inner, fs)
    }

    #[tokio::test]
    async fn guest_paths_rebase_onto_their_mounts_host_twin() {
        let (inner, fs) = fs_with(vec![workspace_mount(), home_mount()]);

        fs.read_file("/workspace/abc/src/main.rs", None, None)
            .await
            .expect("workspace read");
        assert_eq!(
            inner.last(),
            (
                "read".to_string(),
                "/host/workspace/abc/src/main.rs".to_string()
            )
        );

        fs.write_file("/root/.bashrc", "x")
            .await
            .expect("home write");
        assert_eq!(
            inner.last(),
            (
                "write".to_string(),
                "/host/persistent/root/.bashrc".to_string()
            )
        );

        // The mount root itself maps to the host root.
        fs.file_size("/workspace/abc").await.expect("mount root");
        assert_eq!(
            inner.last(),
            ("size".to_string(), "/host/workspace/abc".to_string())
        );
    }

    #[tokio::test]
    async fn longest_guest_prefix_wins() {
        let nested = MountSpec {
            host_path: PathBuf::from("/host/external"),
            guest_path: "/workspace/abc/ext".to_string(),
            read_only: false,
            purpose: MountPurpose::External,
        };
        let (inner, fs) = fs_with(vec![workspace_mount(), nested]);

        fs.read_file("/workspace/abc/ext/data.txt", None, None)
            .await
            .expect("nested read");
        assert_eq!(
            inner.last(),
            ("read".to_string(), "/host/external/data.txt".to_string())
        );
    }

    #[tokio::test]
    async fn read_only_mounts_refuse_writes_but_serve_reads() {
        let ro = MountSpec {
            host_path: PathBuf::from("/host/shared"),
            guest_path: "/workspace/abc".to_string(),
            read_only: true,
            purpose: MountPurpose::Workspace,
        };
        let (inner, fs) = fs_with(vec![ro]);

        fs.read_file("/workspace/abc/f", None, None)
            .await
            .expect("read-only read");
        assert_eq!(
            inner.last(),
            ("read".to_string(), "/host/shared/f".to_string())
        );

        let denied = fs.write_file("/workspace/abc/f", "x").await;
        assert!(
            matches!(denied, Err(FsError::PermissionDenied(_))),
            "write to a read-only mount must be refused: {denied:?}"
        );
    }

    #[tokio::test]
    async fn unbacked_guest_space_is_fenced() {
        let (_, fs) = fs_with(vec![workspace_mount()]);
        for path in [
            "/tmp/scratch.txt",
            "/var/tmp/scratch.txt",
            "/root/unbacked",           // atlas root without a live mount
            "/workspace/other-id/file", // guest workspace space, no mount
            "/workspace/abc/../abc/f",  // dirty guest path must not leak
        ] {
            let result = fs.read_file(path, None, None).await;
            assert!(
                matches!(result, Err(FsError::PermissionDenied(_))),
                "expected fence for {path}: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn host_paths_pass_through_unchanged() {
        let (inner, fs) = fs_with(vec![workspace_mount()]);
        // /var/... is guest scratch's parent but NOT guest space itself.
        for path in [
            "/host/other/file.txt",
            "/var/mobile/Containers/x",
            "relative.txt",
        ] {
            fs.read_file(path, None, None).await.expect("passthrough");
            assert_eq!(inner.last(), ("read".to_string(), path.to_string()));
        }
    }

    #[tokio::test]
    async fn symlink_translates_both_endpoints() {
        let (inner, fs) = fs_with(vec![workspace_mount()]);
        fs.symlink("/workspace/abc/target", "/workspace/abc/link")
            .await
            .expect("symlink");
        let calls = inner.calls.lock().expect("calls mutex").clone();
        assert_eq!(
            calls,
            vec![
                (
                    "symlink-target".to_string(),
                    "/host/workspace/abc/target".to_string()
                ),
                (
                    "symlink-link".to_string(),
                    "/host/workspace/abc/link".to_string()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn workspace_membership_follows_translation_and_fence() {
        let (_, fs) = fs_with(vec![workspace_mount()]);
        // Translated: /workspace/abc → /host/workspace/abc, inside the
        // recording fs's workspace.
        assert!(fs.is_within_workspace("/workspace/abc/file"));
        // Fenced guest space is never "within the workspace".
        assert!(!fs.is_within_workspace("/tmp/file"));
        // Host paths keep the inner answer.
        assert!(!fs.is_within_workspace("/elsewhere/file"));
    }
}
