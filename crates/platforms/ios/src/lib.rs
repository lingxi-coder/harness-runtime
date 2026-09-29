//! `platform-ios` (M8-P10) — the iOS platform skeleton.
//!
//! [`IosPlatform`] implements the [`platform_api::Platform`] aggregate. The core OS
//! handles (filesystem/clock/process/sandbox/worktree) are currently reused
//! from `platform-posix-minimal` — those impls are portable Rust (`std::fs`
//! over the App-Sandbox root, `std::time`, and `Unsupported` stubs) and valid
//! on iOS. The `http` handle is the shared real client
//! ([`http_client::ReqwestHttp`], `reqwest` + `rustls-tls`), so a
//! keyed conversation streams against the real provider rather than the
//! posix-minimal stub. The iOS-specific device capabilities (camera, voice,
//! share) are injected as `Arc<dyn …>` trait objects implemented natively in
//! Swift via `UniFFI` (P12).
//!
//! M9 replaces the reused posix handles with App-Sandbox-aware iOS impls. The
//! crate is intentionally **not** `#[cfg(target_os = "ios")]`-gated: the
//! skeleton is portable, so it compiles and is verified on the host build and
//! cross-compiles to `aarch64-apple-ios` unchanged.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 2 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

use mobile_linux_api::{
    MobileLinuxRuntime, MobileLinuxRuntimeMode, MountPurpose, MountSpec, SandboxBackend,
    UnavailableMobileLinuxRuntime,
};
use platform_api::{
    AudioService, CalendarProvider, CameraControl, Clipboard, Clock, ContactsProvider,
    DeepLinkOpener, DeviceStatusProvider, FileSystem, HapticService, HttpTransport,
    LocationProvider, NotificationService, Platform, ProcessRunner, Sandbox, SecureStorage,
    SharingService, WorktreeManager,
};
use platform_common::{GuestPathFileSystem, MobileLinuxProcessRunner, MobileLinuxSandbox};
use std::path::PathBuf;
use std::sync::Arc;

/// Construction inputs for [`IosPlatform`].
///
/// The native capabilities are supplied by the Swift layer (via `UniFFI` in P12);
/// `app_sandbox_root` is the container directory the filesystem is confined to.
pub struct IosPlatformInputs {
    /// The app's writable sandbox container root.
    pub app_sandbox_root: PathBuf,
    /// Native camera (Swift impl).
    pub camera: Arc<dyn CameraControl>,
    /// Unified app-scoped device audio service (Swift impl).
    pub audio: Arc<dyn AudioService>,
    /// Native share sheet (Swift impl).
    pub share: Arc<dyn SharingService>,
    /// Native system notifications (Swift impl), when wired. `None` keeps the
    /// `notification` tool reporting "unavailable".
    pub notifications: Option<Arc<dyn NotificationService>>,
    /// Native system clipboard (Swift impl), when wired. `None` keeps the
    /// `clipboard` tool reporting "unavailable".
    pub clipboard: Option<Arc<dyn Clipboard>>,
    /// Native device status/haptics/deep-link services, when wired.
    pub device_status: Option<Arc<dyn DeviceStatusProvider>>,
    pub haptics: Option<Arc<dyn HapticService>>,
    pub deep_link: Option<Arc<dyn DeepLinkOpener>>,
    /// Native read-only calendar provider, when wired.
    pub calendar: Option<Arc<dyn CalendarProvider>>,
    /// Native read-only contacts provider, when wired.
    pub contacts: Option<Arc<dyn ContactsProvider>>,
    /// Native iOS Keychain-backed secure store (Swift impl), when wired. `None`
    /// keeps the non-persisting development stub, which gates OAuth `/login` off
    /// (it cannot persist tokens). Inject a real store to enable subscription login.
    pub secure_storage: Option<Arc<dyn SecureStorage>>,
    /// Native one-shot location (Swift impl), when wired. `None` keeps the
    /// local-apps location bridge reporting "unavailable".
    pub location: Option<Arc<dyn LocationProvider>>,
    /// Mobile Linux runtime bridge (iSH path). `None` keeps the legacy
    /// unavailable shell behavior in place.
    pub mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
    /// Explicit host workspace root exposed to the guest when mobile-linux mode
    /// is selected. Legacy mode ignores it.
    pub workspace_host_path: Option<PathBuf>,
    /// Stable guest workspace id used to produce `/workspace/<id>`.
    pub stable_workspace_id: Option<String>,
}

/// The iOS [`Platform`].
pub struct IosPlatform {
    fs: Arc<dyn FileSystem>,
    http: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
    process: Arc<dyn ProcessRunner>,
    sandbox: Arc<dyn Sandbox>,
    worktree: Arc<dyn WorktreeManager>,
    camera: Arc<dyn CameraControl>,
    audio: Arc<dyn AudioService>,
    share: Arc<dyn SharingService>,
    notifications: Option<Arc<dyn NotificationService>>,
    clipboard: Option<Arc<dyn Clipboard>>,
    device_status: Option<Arc<dyn DeviceStatusProvider>>,
    haptics: Option<Arc<dyn HapticService>>,
    deep_link: Option<Arc<dyn DeepLinkOpener>>,
    calendar: Option<Arc<dyn CalendarProvider>>,
    contacts: Option<Arc<dyn ContactsProvider>>,
    secure_storage: Option<Arc<dyn SecureStorage>>,
    location: Option<Arc<dyn LocationProvider>>,
    mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
}

impl IosPlatform {
    /// Assemble an [`IosPlatform`] from native inputs.
    #[must_use]
    pub fn new(inputs: IosPlatformInputs) -> Self {
        use platform_posix_minimal::{
            PosixClock, PosixFileSystem, PosixProcess, PosixSandbox, PosixWorktree,
        };
        let mobile_linux_selected = inputs
            .mobile_linux
            .as_ref()
            .is_some_and(|runtime| matches!(runtime.mode(), MobileLinuxRuntimeMode::MobileLinux));
        let workspace_mounts = build_mobile_linux_mounts(
            inputs.workspace_host_path.clone(),
            inputs.stable_workspace_id.clone(),
        );
        let runtime = inputs.mobile_linux.clone();
        let (process, sandbox, effective_runtime): (
            Arc<dyn ProcessRunner>,
            Arc<dyn Sandbox>,
            Option<Arc<dyn MobileLinuxRuntime>>,
        ) = if mobile_linux_selected {
            let runtime = runtime.expect("mobile-linux runtime must exist when selected");
            let (runtime, sandbox): (Arc<dyn MobileLinuxRuntime>, Arc<dyn Sandbox>) =
                match MobileLinuxSandbox::new(runtime.clone(), workspace_mounts) {
                    Ok(sandbox) => (runtime, Arc::new(sandbox)),
                    Err(error) => {
                        let unavailable: Arc<dyn MobileLinuxRuntime> =
                            Arc::new(UnavailableMobileLinuxRuntime::unavailable(
                                SandboxBackend::IosIsh,
                                MobileLinuxRuntimeMode::MobileLinux,
                                "ios",
                                "arm64",
                                format!("invalid mobile-linux workspace mount: {error}"),
                            ));
                        let sandbox = MobileLinuxSandbox::new(unavailable.clone(), Vec::new())
                            .expect("empty mobile-linux mount set must be valid");
                        (unavailable, Arc::new(sandbox))
                    }
                };
            let process =
                Arc::new(MobileLinuxProcessRunner::new(runtime.clone())) as Arc<dyn ProcessRunner>;
            (process, sandbox, Some(runtime))
        } else {
            (
                Arc::new(PosixProcess::new()) as Arc<dyn ProcessRunner>,
                Arc::new(PosixSandbox::new()) as Arc<dyn Sandbox>,
                runtime,
            )
        };
        // File tools speak guest paths on mobile-linux: the runtime's live
        // mount table maps them onto their host-backed twins, host paths keep
        // passing through unchanged, and unbacked (fakefs) guest space is
        // refused. Legacy mode keeps the bare host filesystem.
        let base_fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(inputs.app_sandbox_root));
        let fs: Arc<dyn FileSystem> = match (&effective_runtime, mobile_linux_selected) {
            (Some(runtime), true) => Arc::new(GuestPathFileSystem::new(base_fs, runtime.clone())),
            _ => base_fs,
        };
        Self {
            fs,
            http: Arc::new(http_client::ReqwestHttp::new()),
            clock: Arc::new(PosixClock::new()),
            process,
            sandbox,
            worktree: Arc::new(PosixWorktree::new()),
            camera: inputs.camera,
            audio: inputs.audio,
            share: inputs.share,
            notifications: inputs.notifications,
            clipboard: inputs.clipboard,
            device_status: inputs.device_status,
            haptics: inputs.haptics,
            deep_link: inputs.deep_link,
            calendar: inputs.calendar,
            contacts: inputs.contacts,
            secure_storage: inputs.secure_storage,
            location: inputs.location,
            mobile_linux: effective_runtime,
        }
    }
}

fn build_mobile_linux_mounts(
    workspace_host_path: Option<PathBuf>,
    stable_workspace_id: Option<String>,
) -> Vec<MountSpec> {
    let guest_workspace_id = stable_workspace_id.unwrap_or_else(|| "default".to_string());
    workspace_host_path
        .map(|host_path| {
            vec![MountSpec {
                host_path,
                guest_path: mobile_linux_api::guest_paths::workspace(&guest_workspace_id),
                read_only: false,
                purpose: MountPurpose::Workspace,
            }]
        })
        .unwrap_or_default()
}

impl Platform for IosPlatform {
    fn filesystem(&self) -> Arc<dyn FileSystem> {
        self.fs.clone()
    }
    fn http(&self) -> Arc<dyn HttpTransport> {
        self.http.clone()
    }
    fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }
    fn process(&self) -> Arc<dyn ProcessRunner> {
        self.process.clone()
    }
    fn sandbox(&self) -> Arc<dyn Sandbox> {
        self.sandbox.clone()
    }
    fn worktree(&self) -> Arc<dyn WorktreeManager> {
        self.worktree.clone()
    }
    fn camera(&self) -> Option<Arc<dyn CameraControl>> {
        Some(self.camera.clone())
    }
    fn audio_service(&self) -> Option<Arc<dyn AudioService>> {
        Some(self.audio.clone())
    }
    fn share(&self) -> Option<Arc<dyn SharingService>> {
        Some(self.share.clone())
    }
    fn location(&self) -> Option<Arc<dyn LocationProvider>> {
        self.location.clone()
    }
    fn notifications(&self) -> Option<Arc<dyn NotificationService>> {
        self.notifications.clone()
    }
    fn clipboard(&self) -> Option<Arc<dyn Clipboard>> {
        self.clipboard.clone()
    }
    fn device_status(&self) -> Option<Arc<dyn DeviceStatusProvider>> {
        self.device_status.clone()
    }
    fn haptics(&self) -> Option<Arc<dyn HapticService>> {
        self.haptics.clone()
    }
    fn deep_link(&self) -> Option<Arc<dyn DeepLinkOpener>> {
        self.deep_link.clone()
    }
    fn calendar(&self) -> Option<Arc<dyn CalendarProvider>> {
        self.calendar.clone()
    }
    fn contacts(&self) -> Option<Arc<dyn ContactsProvider>> {
        self.contacts.clone()
    }
    fn secure_storage(&self) -> Option<Arc<dyn SecureStorage>> {
        self.secure_storage.clone()
    }
    fn mobile_linux(&self) -> Option<Arc<dyn MobileLinuxRuntime>> {
        self.mobile_linux.clone()
    }
    // computer_control() defaults to None — screen automation is not an iOS
    // capability in M8.
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use mobile_linux_api::UnavailableMobileLinuxRuntime;
    use platform_api::{
        CameraError, CapturePhotoOpts, CapturedImage, FsError, ShareError, SharePayload,
        ShareResult,
    };

    struct StubCamera;
    #[async_trait]
    impl CameraControl for StubCamera {
        async fn capture_photo(
            &self,
            _opts: CapturePhotoOpts,
        ) -> Result<CapturedImage, CameraError> {
            Err(CameraError::DeviceUnavailable)
        }
        async fn pick_from_library(&self) -> Result<CapturedImage, CameraError> {
            Err(CameraError::DeviceUnavailable)
        }
    }

    struct StubAudio;
    #[async_trait]
    impl AudioService for StubAudio {
        fn capabilities(&self) -> platform_api::AudioCapabilitySnapshot {
            platform_api::AudioCapabilitySnapshot {
                service_epoch: 0,
                support_revision: 0,
                supported_operations: Vec::new(),
                readiness: Vec::new(),
                max_payload_bytes: 0,
            }
        }
        async fn execute(
            &self,
            _context: platform_api::AudioOperationContext,
            _operation: platform_api::AudioOperation,
        ) -> Result<platform_api::AudioOperationSuccess, platform_api::AudioError> {
            Err(platform_api::AudioError::new(
                platform_api::AudioErrorKind::Unavailable,
                "audio service not wired",
            ))
        }
        async fn cancel(
            &self,
            _identity: platform_api::AudioOperationId,
        ) -> Result<(), platform_api::AudioError> {
            Ok(())
        }
    }

    struct StubShare;
    #[async_trait]
    impl SharingService for StubShare {
        async fn share(&self, _payload: SharePayload) -> Result<ShareResult, ShareError> {
            Err(ShareError::Unsupported)
        }
    }

    fn inputs(
        root: &std::path::Path,
        mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
    ) -> IosPlatformInputs {
        IosPlatformInputs {
            app_sandbox_root: root.to_path_buf(),
            camera: Arc::new(StubCamera),
            audio: Arc::new(StubAudio),
            share: Arc::new(StubShare),
            notifications: None,
            clipboard: None,
            device_status: None,
            haptics: None,
            deep_link: None,
            calendar: None,
            contacts: None,
            secure_storage: None,
            location: None,
            mobile_linux,
            workspace_host_path: Some(root.join("workspace")),
            stable_workspace_id: Some("abc".to_string()),
        }
    }

    /// The assembly seam this pins: selecting mobile-linux mode must wrap the
    /// filesystem in the guest-path translation layer. The unavailable stub
    /// runtime keeps an empty mount table, so guest space is fenced with
    /// `PermissionDenied` — an unwrapped host filesystem would report a plain
    /// I/O error for the same missing path.
    #[tokio::test]
    async fn mobile_linux_mode_wraps_the_filesystem_in_guest_translation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(UnavailableMobileLinuxRuntime::unavailable(
                SandboxBackend::IosIsh,
                MobileLinuxRuntimeMode::MobileLinux,
                "ios",
                "arm64",
                "test stub",
            ));
        let platform = IosPlatform::new(inputs(temp.path(), Some(runtime)));
        let fenced = platform
            .filesystem()
            .read_file("/tmp/does-not-exist", None, None)
            .await;
        assert!(
            matches!(fenced, Err(FsError::PermissionDenied(_))),
            "guest scratch must be fenced through the wrapper: {fenced:?}"
        );

        let legacy = IosPlatform::new(inputs(temp.path(), None));
        let passthrough = legacy
            .filesystem()
            .read_file("/tmp/does-not-exist", None, None)
            .await;
        assert!(
            !matches!(passthrough, Err(FsError::PermissionDenied(_))),
            "legacy mode must keep the bare host filesystem: {passthrough:?}"
        );
    }
}
