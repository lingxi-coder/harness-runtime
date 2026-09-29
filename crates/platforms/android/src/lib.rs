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

pub mod capabilities;
pub mod config;
pub mod mobile_linux;
pub mod policy;
pub mod process;
pub mod receipt;
pub mod sandbox;

pub use capabilities::{AndroidSandboxCapabilities, CapabilityCache};
pub use config::AndroidShellConfig;
pub use mobile_linux::{AndroidProotRuntime, AndroidProotRuntimeConfig};
pub use policy::{
    build_shell_env, plan_from_policy, AndroidSandboxPlan, ExecTarget, NetProfile, ProcessCleanup,
    Rlimit, RlimitResource, SeccompRef,
};
pub use process::AndroidMinijailProcessRunner;
pub use receipt::AndroidSandboxReceipt;
pub use sandbox::AndroidMinijailSandbox;

use lingxi_core::host::{
    AndroidUiAutomation, AudioService, CalendarProvider, CameraControl, Clipboard, Clock,
    ContactsProvider, DeepLinkOpener, DeviceStatusProvider, FileSystem, HapticService,
    HttpTransport, LocationProvider, MobileLinuxRuntime, MobileLinuxRuntimeMode, MountPurpose,
    MountSpec, NotificationService, Platform, ProcessRunner, Sandbox, SandboxBackend, SandboxError,
    SecureStorage, SharingService, UnavailableMobileLinuxRuntime, WorktreeManager,
};
use platform_common::{MobileLinuxProcessRunner, MobileLinuxSandbox};
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
    /// Mobile Linux runtime bridge (Android PRoot path). `None` keeps the
    /// legacy shell runner in place.
    pub mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
    /// Host workspace root exposed to the mobile-linux guest. Defaults to
    /// `<app_files_root>/workspaces/default` when unset.
    pub mobile_linux_workspace_root: Option<PathBuf>,
    /// Stable workspace identifier for the guest path `/workspace/<id>`.
    pub mobile_linux_workspace_id: Option<String>,
    /// Managed rootfs directory reserved for the runtime implementation.
    pub mobile_linux_managed_root: Option<PathBuf>,
    /// Android shell/sandbox configuration (spec r3). `None` keeps shell
    /// support fully absent (posix-minimal stubs stay wired).
    pub shell: Option<AndroidShellConfig>,
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
    mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
    mobile_linux_mode: MobileLinuxRuntimeMode,
    /// The shared capability cache when shell support is wired (`None` for the
    /// posix-minimal-stub configuration). Held so the eager probe (engine-mobile)
    /// and the runner can read/populate the SAME instance the sandbox reads.
    shell_caps: Option<std::sync::Arc<crate::capabilities::CapabilityCache>>,
}

impl AndroidPlatform {
    /// Assemble an [`AndroidPlatform`] from native inputs.
    #[must_use]
    pub fn new(inputs: AndroidPlatformInputs) -> Self {
        Self::new_with_mode(inputs, MobileLinuxRuntimeMode::Legacy)
    }

    /// Assemble an [`AndroidPlatform`] while explicitly selecting the shared
    /// mobile Linux runtime mode. The legacy constructor remains unchanged for
    /// current production callers.
    #[must_use]
    pub fn new_with_mode(
        inputs: AndroidPlatformInputs,
        mobile_linux_mode: MobileLinuxRuntimeMode,
    ) -> Self {
        use platform_posix_minimal::{
            PosixClock, PosixFileSystem, PosixProcess, PosixSandbox, PosixWorktree,
        };
        let default_workspace_root = inputs.app_files_root.join("workspaces").join("default");
        let workspace_root = inputs
            .mobile_linux_workspace_root
            .clone()
            .or_else(|| {
                inputs
                    .shell
                    .as_ref()
                    .map(|cfg| cfg.shell_workspace_root.clone())
            })
            .unwrap_or(default_workspace_root);
        let workspace_id = inputs
            .mobile_linux_workspace_id
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let workspace_valid =
            workspace_root_is_allowed(&workspace_root, inputs.mobile_linux_managed_root.as_deref());
        let mobile_linux_runtime = match (mobile_linux_mode, inputs.mobile_linux.clone()) {
            (MobileLinuxRuntimeMode::MobileLinux, Some(runtime)) => Some(runtime),
            (MobileLinuxRuntimeMode::MobileLinux, None) => {
                Some(Arc::new(UnavailableMobileLinuxRuntime::unavailable(
                    SandboxBackend::AndroidProot,
                    MobileLinuxRuntimeMode::MobileLinux,
                    "android",
                    "unknown",
                    "mobile-linux mode selected without a wired Android runtime",
                )) as Arc<dyn MobileLinuxRuntime>)
            }
            (MobileLinuxRuntimeMode::Legacy, runtime) => runtime,
        };
        let unavailable_mobile_linux_shell = |reason: String| {
            let runtime: Arc<dyn MobileLinuxRuntime> =
                Arc::new(UnavailableMobileLinuxRuntime::unavailable(
                    SandboxBackend::AndroidProot,
                    MobileLinuxRuntimeMode::MobileLinux,
                    "android",
                    "unknown",
                    reason,
                ));
            let sandbox_result = if workspace_valid {
                MobileLinuxSandbox::new(
                    runtime.clone(),
                    default_mobile_linux_mounts(&workspace_root, &workspace_id),
                )
            } else {
                Err(SandboxError::Unavailable(
                    "invalid mobile-linux workspace mount configuration".to_string(),
                ))
            };
            let sandbox: Arc<dyn Sandbox> = match sandbox_result {
                Ok(sandbox) => Arc::new(sandbox),
                Err(error) => {
                    let runtime: Arc<dyn MobileLinuxRuntime> =
                        Arc::new(UnavailableMobileLinuxRuntime::unavailable(
                            SandboxBackend::AndroidProot,
                            MobileLinuxRuntimeMode::MobileLinux,
                            "android",
                            "unknown",
                            format!("mobile-linux shell unavailable: {error}"),
                        ));
                    Arc::new(
                        MobileLinuxSandbox::new(runtime, Vec::new())
                            .expect("empty mobile-linux mount set must be valid"),
                    )
                }
            };
            let process: Arc<dyn ProcessRunner> = Arc::new(MobileLinuxProcessRunner::new(runtime));
            (process, sandbox)
        };
        let (process, sandbox, shell_caps, effective_mobile_linux_runtime) = match (
            mobile_linux_mode,
            mobile_linux_runtime.clone(),
            inputs.shell,
        ) {
            (_, _, Some(shell_cfg)) => {
                let caps = Arc::new(crate::capabilities::CapabilityCache::new());
                let process: Arc<dyn ProcessRunner> = Arc::new(
                    crate::process::AndroidMinijailProcessRunner::new(caps.clone()),
                );
                let sandbox: Arc<dyn Sandbox> = Arc::new(
                    crate::sandbox::AndroidMinijailSandbox::new(shell_cfg, caps.clone()),
                );
                (process, sandbox, Some(caps), mobile_linux_runtime)
            }
            (MobileLinuxRuntimeMode::MobileLinux, _, None) => {
                let (process, sandbox) = unavailable_mobile_linux_shell(
                    "agent shell is disabled in mobile-linux mode because Android PRoot does not enforce host Minijail policies; direct PTY/rootfs APIs remain available"
                        .to_string(),
                );
                (process, sandbox, None, mobile_linux_runtime)
            }
            (_, _, None) => (
                Arc::new(PosixProcess::new()) as Arc<dyn ProcessRunner>,
                Arc::new(PosixSandbox::new()) as Arc<dyn Sandbox>,
                None,
                mobile_linux_runtime,
            ),
        };
        Self {
            fs: Arc::new(PosixFileSystem::new(inputs.app_files_root)),
            // Android surfaces transport failures directly in the conversation
            // UI. Retain reqwest's nested DNS/TCP/TLS causes so the Kotlin layer
            // can distinguish an unresolvable host from an invalid key/model.
            // Desktop continues to use `ReqwestHttp::new()` and keeps its exact
            // legacy error text.
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
            mobile_linux: effective_mobile_linux_runtime,
            mobile_linux_mode,
            shell_caps,
        }
    }

    /// The shared shell capability cache, when shell support is wired.
    ///
    /// Returns `Some(Arc<CapabilityCache>)` when the platform was constructed
    /// with an [`AndroidShellConfig`], `None` for the posix-minimal-stub
    /// configuration. The eager probe (engine-mobile) uses this to populate the
    /// cache before tool registration; the runner uses it to gate per-plan
    /// admission (e.g., `DenyNet` requires `seccomp_filter + net_deny_verified`).
    #[must_use]
    pub fn shell_capability_cache(
        &self,
    ) -> Option<std::sync::Arc<crate::capabilities::CapabilityCache>> {
        self.shell_caps.clone()
    }

    /// Selected mobile Linux runtime mode.
    #[must_use]
    pub fn mobile_linux_mode(&self) -> MobileLinuxRuntimeMode {
        self.mobile_linux_mode
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
        self.mobile_linux.clone()
    }
    // computer_control() defaults to None.
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::host::{
        CameraControl, CameraError, CapturePhotoOpts, CapturedImage, LocationError, LocationFix,
        Platform, SandboxBackend, ShareError, SharePayload, ShareResult, SharingService,
        UnavailableMobileLinuxRuntime,
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

    fn inputs(shell: Option<AndroidShellConfig>) -> AndroidPlatformInputs {
        AndroidPlatformInputs {
            app_files_root: std::env::temp_dir(),
            camera: std::sync::Arc::new(NoCam),
            audio: std::sync::Arc::new(NoAudio),
            location: None,
            share: std::sync::Arc::new(NoShare),
            notifications: None,
            clipboard: None,
            device_status: None,
            haptics: None,
            deep_link: None,
            calendar: None,
            contacts: None,
            secure_storage: None,
            android_ui_automation: None,
            mobile_linux: None,
            mobile_linux_workspace_root: None,
            mobile_linux_workspace_id: None,
            mobile_linux_managed_root: None,
            shell,
        }
    }

    fn shell_cfg() -> AndroidShellConfig {
        AndroidShellConfig {
            native_library_dir: std::env::temp_dir(),
            shell_workspace_root: std::env::temp_dir(),
            app_cache_root: std::env::temp_dir(),
            package_name: "com.example".into(),
            package_version_code: 1,
            app_writable_roots: vec![],
            enable_shell: true,
            secrets_in_keystore: true,
            shell_data_exposure_accepted: false,
            bundled_mksh_path: None,
            bundled_mksh_hash: None,
            bundled_applet_dir: None,
        }
    }

    #[test]
    fn shell_config_wires_android_sandbox_and_runner() {
        let p = AndroidPlatform::new(inputs(Some(shell_cfg())));
        assert_eq!(p.sandbox().backend(), SandboxBackend::AndroidMinijail);
        assert!(
            !p.process().is_available(),
            "fresh (unprobed) cache reads conservative-unavailable until the eager probe"
        );
        assert!(
            !p.sandbox().is_available(),
            "fresh cache reads conservative-unavailable until the eager probe (P2)"
        );
    }

    #[test]
    fn no_shell_config_keeps_posix_minimal_stubs() {
        let p = AndroidPlatform::new(inputs(None));
        assert_eq!(p.sandbox().backend(), SandboxBackend::None);
    }

    #[test]
    fn location_provider_is_exposed_only_when_injected() {
        let without_location = AndroidPlatform::new(inputs(None));
        assert!(without_location.location().is_none());

        let mut with_location = inputs(None);
        with_location.location = Some(Arc::new(NoLocation));
        let with_location = AndroidPlatform::new(with_location);
        assert!(with_location.location().is_some());
    }

    #[test]
    fn shell_platform_exposes_shared_capability_cache() {
        let p = AndroidPlatform::new(inputs(Some(shell_cfg())));
        let cache = p
            .shell_capability_cache()
            .expect("shell platform exposes its cache");
        // Same instance the sandbox reads: setting it flips is_available().
        cache.set(crate::capabilities::AndroidSandboxCapabilities {
            probed: true,
            minijail_smoke: true,
            no_new_privs: true,
            ..crate::capabilities::AndroidSandboxCapabilities::default()
        });
        assert!(p.sandbox().is_available(), "sandbox reads the shared cache");
    }

    #[test]
    fn no_shell_platform_has_no_cache() {
        let p = AndroidPlatform::new(inputs(None));
        assert!(p.shell_capability_cache().is_none());
    }

    #[test]
    fn explicit_mobile_linux_mode_and_runtime_are_retained() {
        let runtime = Arc::new(UnavailableMobileLinuxRuntime::blocked(
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
            "license blocked",
        ));
        let mut android_inputs = inputs(Some(shell_cfg()));
        android_inputs.mobile_linux = Some(runtime.clone());

        let platform =
            AndroidPlatform::new_with_mode(android_inputs, MobileLinuxRuntimeMode::MobileLinux);

        assert_eq!(
            platform.mobile_linux_mode(),
            MobileLinuxRuntimeMode::MobileLinux
        );
        let rt = platform.mobile_linux().expect("runtime should be injected");
        assert_eq!(rt.backend(), SandboxBackend::AndroidProot);
        assert_eq!(rt.mode(), MobileLinuxRuntimeMode::MobileLinux);
    }
}
