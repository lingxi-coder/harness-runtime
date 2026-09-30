//! `platform-android` (M8-P10) — the Android platform skeleton.
//!
//! [`AndroidPlatform`] implements the [`lingxi_core::host::Platform`] aggregate. The core
//! OS handles (filesystem/clock/process/sandbox/worktree) are currently reused
//! from `platform-posix-minimal` (portable Rust, valid on Android). The `http`
//! handle is the shared real client ([`http_client::ReqwestHttp`],
//! `reqwest` + `rustls-tls`), so a keyed conversation streams against the real
//! provider rather than the posix-minimal stub. The Android-specific device
//! capabilities (camera, voice, share) are injected as `Arc<dyn …>` trait
//! objects implemented natively in Kotlin via `UniFFI` (P12).
//!
//! M9 replaces the reused posix handles with scoped-storage-aware Android
//! impls. The crate is intentionally **not** `#[cfg(target_os = "android")]`-
//! gated: the skeleton is portable, so it compiles + is verified on the host
//! build and cross-compiles to `aarch64-linux-android` unchanged.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 2 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod mobile_linux;
pub use mobile_linux::{AndroidProotRuntime, AndroidProotRuntimeConfig};

use lingxi_core::host::{
    AndroidUiAutomation, AudioService, CalendarProvider, CameraControl, Clipboard, Clock,
    ContactsProvider, DeepLinkOpener, DeviceStatusProvider, FileSystem, HapticService,
    HttpTransport, LocationProvider, NotificationService, Platform, ProcessRunner, Sandbox,
    SandboxError, SecureStorage, SharingService, WorktreeManager,
};
use mobile_linux_api::{
    MobileLinuxRuntime, MobileLinuxRuntimeMode, MountPurpose, MountSpec, SandboxBackend,
    UnavailableMobileLinuxRuntime,
};
use platform_common::{GuestPathFileSystem, MobileLinuxProcessRunner, MobileLinuxSandbox};
use std::path::PathBuf;
use std::sync::Arc;

/// Construction inputs for [`AndroidPlatform`].
///
/// The native capabilities are supplied by the Kotlin layer (via `UniFFI` in
/// P12); `app_files_root` is the app-private files directory the filesystem is
/// confined to.
pub struct AndroidPlatformInputs {
    /// The app's writable private files-dir root.
    pub app_files_root: PathBuf,
    /// Native camera (Kotlin impl).
    pub camera: Arc<dyn CameraControl>,
    /// Unified app-scoped device audio service (Kotlin impl).
    pub audio: Arc<dyn AudioService>,
    /// Native one-shot location provider (Kotlin impl), when wired.
    pub location: Option<Arc<dyn LocationProvider>>,
    /// Native share sheet (Kotlin impl).
    pub share: Arc<dyn SharingService>,
    /// Native system notifications (Kotlin impl), when wired. `None` keeps the
    /// `notification` tool reporting "unavailable".
    pub notifications: Option<Arc<dyn NotificationService>>,
    /// Native system clipboard (Kotlin impl), when wired. `None` keeps the
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
    /// Native Android Keystore-backed secure store (Kotlin impl), when wired.
    /// `None` keeps the non-persisting development stub, which gates OAuth
    /// `/login` off (it cannot persist tokens). Inject a real store to enable
    /// subscription login.
    pub secure_storage: Option<Arc<dyn SecureStorage>>,
    /// Direct-build Android accessibility automation bridge. Play builds and
    /// headless engines pass `None`.
    pub android_ui_automation: Option<Arc<dyn AndroidUiAutomation>>,
    /// Android PRoot runtime used by the agent and terminal.
    pub mobile_linux: Arc<dyn MobileLinuxRuntime>,
    /// Host workspace root exposed to the mobile-linux guest. Defaults to
    /// `<app_files_root>/workspaces/default` when unset.
    pub mobile_linux_workspace_root: Option<PathBuf>,
    /// Stable workspace identifier for the guest path `/workspace/<id>`.
    pub mobile_linux_workspace_id: Option<String>,
    /// Managed rootfs directory reserved for the runtime implementation.
    pub mobile_linux_managed_root: Option<PathBuf>,
}

/// The Android [`Platform`].
pub struct AndroidPlatform {
    fs: Arc<dyn FileSystem>,
    http: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
    process: Arc<dyn ProcessRunner>,
    sandbox: Arc<dyn Sandbox>,
    worktree: Arc<dyn WorktreeManager>,
    camera: Arc<dyn CameraControl>,
    audio: Arc<dyn AudioService>,
    location: Option<Arc<dyn LocationProvider>>,
    share: Arc<dyn SharingService>,
    notifications: Option<Arc<dyn NotificationService>>,
    clipboard: Option<Arc<dyn Clipboard>>,
    device_status: Option<Arc<dyn DeviceStatusProvider>>,
    haptics: Option<Arc<dyn HapticService>>,
    deep_link: Option<Arc<dyn DeepLinkOpener>>,
    calendar: Option<Arc<dyn CalendarProvider>>,
    contacts: Option<Arc<dyn ContactsProvider>>,
    secure_storage: Option<Arc<dyn SecureStorage>>,
    android_ui_automation: Option<Arc<dyn AndroidUiAutomation>>,
    mobile_linux: Arc<dyn MobileLinuxRuntime>,
}

impl AndroidPlatform {
    /// Assemble the Android platform with one PRoot execution backend.
    #[must_use]
    pub fn new(inputs: AndroidPlatformInputs) -> Self {
        use platform_posix_minimal::{PosixClock, PosixFileSystem, PosixWorktree};

        let workspace_root = inputs
            .mobile_linux_workspace_root
            .clone()
            .unwrap_or_else(|| inputs.app_files_root.join("workspaces").join("default"));
        let workspace_id = inputs
            .mobile_linux_workspace_id
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let mounts = default_mobile_linux_mounts(&workspace_root, &workspace_id);
        let canonical_workspace_root = std::fs::canonicalize(&workspace_root).ok();
        let guest_workspace_path = &mounts[0].guest_path;
        let mount_is_configured = inputs.mobile_linux.current_mounts().iter().any(|mount| {
            Some(&mount.host_path) == canonical_workspace_root.as_ref()
                && mount.guest_path == *guest_workspace_path
                && !mount.read_only
        });
        let sandbox_result = if mount_is_configured
            && workspace_root_is_allowed(
                &workspace_root,
                inputs.mobile_linux_managed_root.as_deref(),
            ) {
            MobileLinuxSandbox::new(inputs.mobile_linux.clone(), mounts)
        } else {
            Err(SandboxError::Unavailable(
                "invalid mobile-linux workspace mount configuration".to_string(),
            ))
        };
        let (runtime, sandbox): (Arc<dyn MobileLinuxRuntime>, Arc<dyn Sandbox>) =
            match sandbox_result {
                Ok(sandbox) => (inputs.mobile_linux, Arc::new(sandbox)),
                Err(error) => {
                    let runtime: Arc<dyn MobileLinuxRuntime> =
                        Arc::new(UnavailableMobileLinuxRuntime::unavailable(
                            SandboxBackend::AndroidProot,
                            MobileLinuxRuntimeMode::MobileLinux,
                            "android",
                            "unknown",
                            format!("mobile-linux shell unavailable: {error}"),
                        ));
                    let sandbox = MobileLinuxSandbox::new(runtime.clone(), Vec::new())
                        .expect("empty mobile-linux mount set must be valid");
                    (runtime, Arc::new(sandbox))
                }
            };
        let base_fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(inputs.app_files_root));
        let fs: Arc<dyn FileSystem> = Arc::new(GuestPathFileSystem::new(base_fs, runtime.clone()));
        let process: Arc<dyn ProcessRunner> =
            Arc::new(MobileLinuxProcessRunner::new(runtime.clone()));
        Self {
            fs,
            http: Arc::new(http_client::ReqwestHttp::new_with_detailed_connection_errors()),
            clock: Arc::new(PosixClock::new()),
            process,
            sandbox,
            worktree: Arc::new(PosixWorktree::new()),
            camera: inputs.camera,
            audio: inputs.audio,
            location: inputs.location,
            share: inputs.share,
            notifications: inputs.notifications,
            clipboard: inputs.clipboard,
            device_status: inputs.device_status,
            haptics: inputs.haptics,
            deep_link: inputs.deep_link,
            calendar: inputs.calendar,
            contacts: inputs.contacts,
            secure_storage: inputs.secure_storage,
            android_ui_automation: inputs.android_ui_automation,
            mobile_linux: runtime,
        }
    }
}

fn default_mobile_linux_mounts(
    workspace_root: &std::path::Path,
    workspace_id: &str,
) -> Vec<MountSpec> {
    let workspace_id = sanitize_workspace_id(workspace_id);
    vec![MountSpec {
        host_path: workspace_root.to_path_buf(),
        guest_path: format!("/workspace/{workspace_id}"),
        read_only: false,
        purpose: MountPurpose::Workspace,
    }]
}

fn sanitize_workspace_id(input: &str) -> String {
    let filtered: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if filtered.is_empty() {
        "default".to_string()
    } else {
        filtered
    }
}

fn workspace_root_is_allowed(
    workspace_root: &std::path::Path,
    managed_root: Option<&std::path::Path>,
) -> bool {
    let text = workspace_root.to_string_lossy().to_ascii_lowercase();
    if text.contains("/.lingxi")
        || text.contains("keystore")
        || text.contains("credential")
        || text.contains("secret")
        || text.contains("token")
    {
        return false;
    }
    if let Some(managed_root) = managed_root {
        if workspace_root.starts_with(managed_root) || managed_root.starts_with(workspace_root) {
            return false;
        }
    }
    true
}

impl Platform for AndroidPlatform {
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
    fn location(&self) -> Option<Arc<dyn LocationProvider>> {
        self.location.clone()
    }
    fn share(&self) -> Option<Arc<dyn SharingService>> {
        Some(self.share.clone())
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
    fn android_ui_automation(&self) -> Option<Arc<dyn AndroidUiAutomation>> {
        self.android_ui_automation.clone()
    }
    fn mobile_linux(&self) -> Option<Arc<dyn MobileLinuxRuntime>> {
        Some(self.mobile_linux.clone())
    }
    // computer_control() defaults to None.
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::host::{
        CameraControl, CameraError, CapturePhotoOpts, CapturedImage, LocationError, LocationFix,
        Platform, ShareError, SharePayload, ShareResult, SharingService,
    };

    struct NoCam;
    #[async_trait]
    impl CameraControl for NoCam {
        async fn capture_photo(&self, _: CapturePhotoOpts) -> Result<CapturedImage, CameraError> {
            Err(CameraError::DeviceUnavailable)
        }
        async fn pick_from_library(&self) -> Result<CapturedImage, CameraError> {
            Err(CameraError::DeviceUnavailable)
        }
    }
    struct NoAudio;
    #[async_trait]
    impl AudioService for NoAudio {
        fn capabilities(&self) -> lingxi_core::host::AudioCapabilitySnapshot {
            lingxi_core::host::AudioCapabilitySnapshot {
                service_epoch: 0,
                support_revision: 0,
                supported_operations: Vec::new(),
                readiness: Vec::new(),
                max_payload_bytes: 0,
            }
        }
        async fn execute(
            &self,
            _context: lingxi_core::host::AudioOperationContext,
            _operation: lingxi_core::host::AudioOperation,
        ) -> Result<lingxi_core::host::AudioOperationSuccess, lingxi_core::host::AudioError>
        {
            Err(lingxi_core::host::AudioError::new(
                lingxi_core::host::AudioErrorKind::Unavailable,
                "audio service not wired",
            ))
        }
        async fn cancel(
            &self,
            _identity: lingxi_core::host::AudioOperationId,
        ) -> Result<(), lingxi_core::host::AudioError> {
            Ok(())
        }
    }
    struct NoLocation;
    #[async_trait]
    impl LocationProvider for NoLocation {
        async fn current_location(&self) -> Result<LocationFix, LocationError> {
            Err(LocationError::Unavailable)
        }
    }
    struct NoShare;
    #[async_trait]
    impl SharingService for NoShare {
        async fn share(&self, _: SharePayload) -> Result<ShareResult, ShareError> {
            Err(ShareError::Unsupported)
        }
    }

    fn inputs() -> AndroidPlatformInputs {
        AndroidPlatformInputs {
            app_files_root: std::env::temp_dir(),
            camera: Arc::new(NoCam),
            audio: Arc::new(NoAudio),
            location: None,
            share: Arc::new(NoShare),
            notifications: None,
            clipboard: None,
            device_status: None,
            haptics: None,
            deep_link: None,
            calendar: None,
            contacts: None,
            secure_storage: None,
            android_ui_automation: None,
            mobile_linux: Arc::new(UnavailableMobileLinuxRuntime::unavailable(
                SandboxBackend::AndroidProot,
                MobileLinuxRuntimeMode::MobileLinux,
                "android",
                "arm64-v8a",
                "rootfs not installed",
            )),
            mobile_linux_workspace_root: Some(std::env::temp_dir()),
            mobile_linux_workspace_id: None,
            mobile_linux_managed_root: None,
        }
    }

    #[test]
    fn agent_process_and_sandbox_use_proot_even_when_unavailable() {
        let platform = AndroidPlatform::new(inputs());
        assert_eq!(platform.sandbox().backend(), SandboxBackend::AndroidProot);
        let runtime = platform.mobile_linux().expect("PRoot runtime");
        assert_eq!(runtime.backend(), SandboxBackend::AndroidProot);
        assert_eq!(runtime.mode(), MobileLinuxRuntimeMode::MobileLinux);
    }

    #[test]
    fn location_provider_is_exposed_only_when_injected() {
        assert!(AndroidPlatform::new(inputs()).location().is_none());

        let mut with_location = inputs();
        with_location.location = Some(Arc::new(NoLocation));
        assert!(AndroidPlatform::new(with_location).location().is_some());
    }

    #[tokio::test]
    async fn invalid_workspace_mount_blocks_agent_execution() {
        let mut android_inputs = inputs();
        android_inputs.mobile_linux_workspace_root = Some(std::env::temp_dir().join(".lingxi"));
        let platform = AndroidPlatform::new(android_inputs);
        assert_eq!(platform.sandbox().backend(), SandboxBackend::AndroidProot);
        let capability = platform
            .mobile_linux()
            .expect("fail-closed runtime")
            .probe_capability()
            .await;
        assert!(!capability.available);
    }

    #[tokio::test]
    async fn configured_proot_mount_is_shared_by_agent_shell_and_file_tools() {
        use lingxi_core::host::{ProcessCommand, SandboxPolicy};
        use mobile_linux_api::NetworkPolicy;

        let temp = tempfile::tempdir().expect("app sandbox");
        let workspace = temp.path().join("workspaces/default");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let workspace = std::fs::canonicalize(workspace).expect("canonical workspace");
        let managed_root = temp.path().join("mobile-linux/android-proot");
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(AndroidProotRuntime::new(AndroidProotRuntimeConfig {
                managed_root: managed_root.clone(),
                app_sandbox_root: temp.path().to_path_buf(),
                abi: "arm64-v8a".into(),
                rootfs_version: "v1".into(),
                archive_sha256: None,
            }));
        runtime
            .configure_mounts(default_mobile_linux_mounts(&workspace, "default"))
            .await
            .expect("configure workspace mount");
        let mut android_inputs = inputs();
        android_inputs.app_files_root = temp.path().to_path_buf();
        android_inputs.mobile_linux = runtime;
        android_inputs.mobile_linux_workspace_root = Some(workspace.clone());
        android_inputs.mobile_linux_managed_root = Some(managed_root);
        let platform = AndroidPlatform::new(android_inputs);

        platform
            .filesystem()
            .write_file("/workspace/default/probe.txt", "from guest path")
            .await
            .expect("guest file path resolves into workspace");
        assert_eq!(
            std::fs::read_to_string(workspace.join("probe.txt")).expect("host twin"),
            "from guest path"
        );
        let prepared = platform
            .sandbox()
            .prepare(
                ProcessCommand {
                    command: "/bin/sh".into(),
                    args: vec!["-c".into(), "pwd".into()],
                    cwd: Some(workspace),
                    env: Default::default(),
                    timeout: None,
                    stdin: None,
                },
                &SandboxPolicy {
                    network: NetworkPolicy::Allowed,
                    writable_paths: vec![],
                    denied_paths: vec![],
                    allow_subprocess: true,
                    limits: Default::default(),
                },
            )
            .expect("PRoot sandbox plan");
        assert!(matches!(
            prepared.tag(),
            lingxi_core::host::sandbox::SandboxedTag::Wrapped {
                backend: SandboxBackend::AndroidProot
            }
        ));
        assert!(platform.process().run(&prepared).await.is_err());
    }
}
