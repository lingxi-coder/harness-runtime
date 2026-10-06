//! Compatibility assembly for the product's Android Linux runtime.
//! Execution and rootfs lifecycle are owned by the standalone SDK.

use async_trait::async_trait;
use mobile_linux_api::{
    LinuxCommandRequest, LinuxCommandResult, LinuxProcessHandle, MobileLinuxCapability,
    MobileLinuxError, MobileLinuxEvent, MobileLinuxRuntime, MobileLinuxRuntimeMode,
    MobileLinuxTaskSnapshot, MountSpec, ProcessStreamSink, PtyOpenRequest, PtySessionHandle,
    PtySize, RawStdioOpenRequest, RawStdioReadResult, RawStdioSessionHandle, RootfsStatus,
    SandboxBackend,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Existing product configuration, preserved independently of SDK configuration.
#[derive(Debug, Clone)]
pub struct AndroidProotRuntimeConfig {
    /// App-private managed runtime directory.
    pub managed_root: PathBuf,
    /// Canonical application sandbox directory.
    pub app_sandbox_root: PathBuf,
    /// Android guest ABI.
    pub abi: String,
    /// Selected staged rootfs version.
    pub rootfs_version: String,
    /// Expected source archive digest.
    pub archive_sha256: Option<String>,
}

/// Product facade over the single SDK backend instance.
#[derive(Debug)]
pub struct AndroidProotRuntime {
    inner: mobile_linux_android::AndroidProotRuntime,
}

impl AndroidProotRuntime {
    /// Construct a runtime with LingXi's existing application layout.
    #[must_use]
    pub fn new(config: AndroidProotRuntimeConfig) -> Self {
        Self::new_with_native_library_dir(config, product_native_library_dir())
    }

    /// Supply the application's extracted native library directory explicitly.
    #[must_use]
    pub fn new_with_native_library_dir(
        config: AndroidProotRuntimeConfig,
        native_library_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            inner: mobile_linux_android::AndroidProotRuntime::new(
                mobile_linux_android::AndroidProotRuntimeConfig {
                    managed_root: config.managed_root,
                    app_sandbox_root: config.app_sandbox_root,
                    abi: config.abi,
                    rootfs_version: config.rootfs_version,
                    archive_sha256: config.archive_sha256,
                    native_library_dir,
                    isolated_build_profile: Some(product_build_profile()),
                },
            ),
        }
    }
}

fn product_build_profile() -> mobile_linux_android::IsolatedBuildProfile {
    mobile_linux_android::IsolatedBuildProfile {
        guest_root: local_app_contracts::guest_paths::LOCAL_APP_BUILD_ROOT.into(),
        project_directory: local_app_contracts::guest_paths::LOCAL_APP_BUILD_PROJECT_DIR.into(),
        dependency_store: local_app_contracts::guest_paths::LOCAL_APP_DEPENDENCY_STORE.into(),
        state_directory: ".lingxi-build-state".into(),
        host_apps_directory: "apps".into(),
        host_build_directory: "build".into(),
        host_workspace_directory: "workspace".into(),
        channels: vec!["store".into(), "full".into()],
    }
}

// Product library discovery stays in the product adapter. The SDK never scans
// another application's maps or guesses a library name.
fn product_native_library_dir() -> Option<PathBuf> {
    #[cfg(target_os = "android")]
    {
        let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
        maps.lines()
            .filter_map(|line| line.split_whitespace().last())
            .filter(|path| path.ends_with("/libandroid_aar.so"))
            .find_map(|path| {
                std::path::Path::new(path)
                    .parent()
                    .map(std::path::Path::to_path_buf)
            })
    }
    #[cfg(not(target_os = "android"))]
    {
        None
    }
}

#[async_trait]
impl MobileLinuxRuntime for AndroidProotRuntime {
    fn backend(&self) -> SandboxBackend {
        self.inner.backend()
    }

    fn mode(&self) -> MobileLinuxRuntimeMode {
        self.inner.mode()
    }

    async fn probe_capability(&self) -> MobileLinuxCapability {
        self.inner.probe_capability().await
    }

    async fn boot(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.inner.boot().await
    }

    async fn shutdown(&self) -> Result<(), MobileLinuxError> {
        self.inner.shutdown().await
    }

    async fn run(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        self.inner.run(request).await
    }

    async fn run_isolated(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        self.inner.run_isolated(request).await
    }

    async fn run_streaming(
        &self,
        request: LinuxCommandRequest,
        sink: Arc<dyn ProcessStreamSink>,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        self.inner.run_streaming(request, sink).await
    }

    async fn spawn_background(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxProcessHandle, MobileLinuxError> {
        self.inner.spawn_background(request).await
    }

    async fn kill(&self, handle: &LinuxProcessHandle) -> Result<(), MobileLinuxError> {
        self.inner.kill(handle).await
    }

    async fn open_pty(
        &self,
        request: PtyOpenRequest,
    ) -> Result<PtySessionHandle, MobileLinuxError> {
        self.inner.open_pty(request).await
    }

    async fn write_pty(
        &self,
        handle: &PtySessionHandle,
        input: Vec<u8>,
    ) -> Result<(), MobileLinuxError> {
        self.inner.write_pty(handle, input).await
    }

    async fn resize_pty(
        &self,
        handle: &PtySessionHandle,
        size: PtySize,
    ) -> Result<(), MobileLinuxError> {
        self.inner.resize_pty(handle, size).await
    }

    async fn close_pty(&self, handle: &PtySessionHandle) -> Result<(), MobileLinuxError> {
        self.inner.close_pty(handle).await
    }

    async fn open_raw_stdio(
        &self,
        request: RawStdioOpenRequest,
    ) -> Result<RawStdioSessionHandle, MobileLinuxError> {
        self.inner.open_raw_stdio(request).await
    }

    async fn write_raw_stdio(
        &self,
        handle: &RawStdioSessionHandle,
        input: Vec<u8>,
    ) -> Result<(), MobileLinuxError> {
        self.inner.write_raw_stdio(handle, input).await
    }

    async fn read_raw_stdio(
        &self,
        handle: &RawStdioSessionHandle,
        max_bytes: usize,
    ) -> Result<RawStdioReadResult, MobileLinuxError> {
        self.inner.read_raw_stdio(handle, max_bytes).await
    }

    async fn close_raw_stdio(
        &self,
        handle: &RawStdioSessionHandle,
    ) -> Result<(), MobileLinuxError> {
        self.inner.close_raw_stdio(handle).await
    }

    async fn rootfs_status(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.inner.rootfs_status().await
    }

    async fn verify_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.inner.verify_rootfs().await
    }

    async fn repair_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.inner.repair_rootfs().await
    }

    async fn reset_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.inner.reset_rootfs().await
    }

    async fn configure_mounts(&self, mounts: Vec<MountSpec>) -> Result<(), MobileLinuxError> {
        self.inner.configure_mounts(mounts).await
    }

    fn current_mounts(&self) -> Vec<MountSpec> {
        self.inner.current_mounts()
    }

    async fn read_events(
        &self,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<MobileLinuxEvent>, MobileLinuxError> {
        self.inner.read_events(after_sequence, limit).await
    }

    async fn list_tasks(&self) -> Result<Vec<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        self.inner.list_tasks().await
    }

    async fn task_status(
        &self,
        task_id: &str,
    ) -> Result<Option<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        self.inner.task_status(task_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::RootfsState;

    /// Byte-pins the atlas atoms. The Swift twin (`LXISHGuestPaths`) pins the
    /// SAME literals — drift on either side fails one of the twins.
    #[test]
    fn atlas_atoms_are_pinned() {
        use local_app_contracts::guest_paths as product;
        use mobile_linux_api::guest_paths as sdk;
        assert_eq!(sdk::HOME, "/root");
        assert_eq!(sdk::SCRATCH, &["/tmp", "/var/tmp"]);
        assert_eq!(sdk::WORKSPACE_ROOT, "/workspace");
        assert_eq!(product::LOCAL_APP_BUILD_ROOT, "/var/lingxi/local-app-build");
        assert_eq!(
            product::LOCAL_APP_DEPENDENCY_STORE,
            "/var/lingxi/local-app-dependency-store"
        );
        assert_eq!(product::LOCAL_APP_BUILD_PROJECT_DIR, "project");
        assert_eq!(sdk::workspace("abc-123"), "/workspace/abc-123");
        assert_eq!(
            product::local_app_build_project("abc-123", "store"),
            "/var/lingxi/local-app-build/abc-123/store/project"
        );
        assert_eq!(sdk::writable_roots(), ["/root", "/tmp", "/var/tmp", "/workspace"]);
    }

    #[test]
    fn product_profile_keeps_existing_paths() {
        let profile = product_build_profile();
        assert_eq!(profile.guest_root, "/var/lingxi/local-app-build");
        assert_eq!(profile.state_directory, ".lingxi-build-state");
        assert_eq!(profile.channels, ["store", "full"]);
    }

    #[tokio::test]
    async fn product_facade_uses_sdk_runtime_and_preserves_missing_rootfs() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = AndroidProotRuntime::new(AndroidProotRuntimeConfig {
            managed_root: directory.path().join("runtime"),
            app_sandbox_root: directory.path().to_path_buf(),
            abi: "arm64-v8a".into(),
            rootfs_version: "fixture-v1".into(),
            archive_sha256: None,
        });
        assert_eq!(runtime.backend(), SandboxBackend::AndroidProot);
        assert_eq!(
            runtime.rootfs_status().await.unwrap().state,
            RootfsState::Missing
        );
    }
}
