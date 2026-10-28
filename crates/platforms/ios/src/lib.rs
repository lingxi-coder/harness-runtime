//! iOS platform composition.
//!
//! Real filesystem I/O and the shared clock are reused from
//! `platform-posix-minimal`; its workspace root is a predicate, not a filesystem
//! confinement boundary. HTTP uses the real shared client. An injected iSH
//! runtime supplies process and sandbox services when mobile Linux is selected;
//! otherwise those services report unavailable. Native device and Keychain
//! capabilities are injected by the Swift host. The worktree implementation
//! remains unavailable on this path. Mount authorization is checked here and
//! by the native adapter before a guest filesystem is exposed.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 2 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

use lingxi_core::host::{
    AudioService, CalendarProvider, CameraControl, Clipboard, Clock, ContactsProvider,
    DeepLinkOpener, DeviceServices, DeviceStatusProvider, FileSystem, HapticService, HttpTransport,
    LocationProvider, NotificationService, Platform, ProcessRunner, Sandbox, SecureStorage,
    SharingService, WorktreeManager,
};
use mobile_linux_api::{
    MobileLinuxRuntime, MobileLinuxRuntimeMode, MountPurpose, MountSpec, SandboxBackend,
    UnavailableMobileLinuxRuntime,
};
use platform_common::{GuestPathFileSystem, MobileLinuxProcessRunner, MobileLinuxSandbox};
use std::path::PathBuf;
use std::sync::Arc;

/// Construction inputs for [`IosPlatform`].
///
/// The native capabilities are supplied by the Swift layer (via `UniFFI` in P12);
/// `app_sandbox_root` is the container directory used as the workspace root.
pub struct IosPlatformInputs {
    /// Authoritative Linux account and guest-path profile.
    pub brand_profile: mobile_linux_api::BrandRuntimeProfile,
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
    /// app location bridge reporting "unavailable".
    pub location: Option<Arc<dyn LocationProvider>>,
    /// Mobile Linux runtime bridge (iSH path). `None` keeps the legacy
    /// unavailable shell behavior in place.
    pub mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
    /// Explicit host workspace root exposed to the guest when mobile-linux mode
    /// is selected. Legacy mode ignores it.
    pub workspace_host_path: Option<PathBuf>,
    /// Stable workspace identifier appended under the selected brand workspace root.
    pub stable_workspace_id: Option<String>,
    /// Exact project-authorized host directories to expose beside the primary
    /// workspace mount. These are validated by the iOS FFI adapter and remain
    /// separate External mounts; callers cannot replace the primary workspace.
    pub additional_workspace_mounts: Vec<MountSpec>,
}

/// The iOS [`Platform`].
pub struct IosPlatform {
    fs: Arc<dyn FileSystem>,
    http: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
    process: Arc<dyn ProcessRunner>,
    sandbox: Arc<dyn Sandbox>,
    worktree: Arc<dyn WorktreeManager>,
    device_services: DeviceServices,
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
        let primary_workspace_mounts = build_mobile_linux_mounts(
            inputs.workspace_host_path.clone(),
            inputs.stable_workspace_id.clone(),
            &inputs.brand_profile,
        );
        let workspace_mounts = append_additional_workspace_mounts(
            primary_workspace_mounts,
            inputs.additional_workspace_mounts,
            &inputs.brand_profile,
        );
        let runtime = inputs.mobile_linux.clone();
        let (process, sandbox, effective_runtime): (
            Arc<dyn ProcessRunner>,
            Arc<dyn Sandbox>,
            Option<Arc<dyn MobileLinuxRuntime>>,
        ) = if mobile_linux_selected {
            let runtime = runtime.expect("mobile-linux runtime must exist when selected");
            let (runtime, sandbox): (Arc<dyn MobileLinuxRuntime>, Arc<dyn Sandbox>) =
                match workspace_mounts.and_then(|mounts| {
                    MobileLinuxSandbox::new(runtime.clone(), mounts, inputs.brand_profile.clone())
                        .map_err(|error| format!("invalid mobile-linux workspace mounts: {error}"))
                }) {
                    Ok(sandbox) => (runtime, Arc::new(sandbox)),
                    Err(error) => {
                        let unavailable: Arc<dyn MobileLinuxRuntime> =
                            Arc::new(UnavailableMobileLinuxRuntime::unavailable(
                                SandboxBackend::IosIsh,
                                MobileLinuxRuntimeMode::MobileLinux,
                                "ios",
                                "arm64",
                                error,
                            ));
                        let sandbox = MobileLinuxSandbox::new(
                            unavailable.clone(),
                            Vec::new(),
                            inputs.brand_profile.clone(),
                        )
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
            device_services: DeviceServices {
                camera: Some(inputs.camera),
                audio: Some(inputs.audio),
                location: inputs.location,
                share: Some(inputs.share),
                notifications: inputs.notifications,
                clipboard: inputs.clipboard,
                device_status: inputs.device_status,
                haptics: inputs.haptics,
                deep_link: inputs.deep_link,
                calendar: inputs.calendar,
                contacts: inputs.contacts,
                computer_control: None,
                android_ui_automation: None,
                secure_storage: inputs.secure_storage,
                mobile_linux: effective_runtime,
            },
        }
    }
}

fn build_mobile_linux_mounts(
    workspace_host_path: Option<PathBuf>,
    stable_workspace_id: Option<String>,
    profile: &mobile_linux_api::BrandRuntimeProfile,
) -> Vec<MountSpec> {
    let guest_workspace_id = stable_workspace_id.unwrap_or_else(|| "default".to_string());
    workspace_host_path
        .map(|host_path| {
            vec![MountSpec {
                host_path,
                guest_path: mobile_linux_api::guest_paths::workspace_for_profile(
                    profile,
                    &guest_workspace_id,
                ),
                read_only: false,
                purpose: MountPurpose::Workspace,
            }]
        })
        .unwrap_or_default()
}

/// Append only exact External grants after validating that none can replace,
/// shadow, or overlap the single primary Workspace mount. The iOS FFI adapter
/// performs the native-home and protected-config checks before constructing
/// these values; this second boundary keeps accidental broader mount specs
/// from changing the platform's trust roots.
fn append_additional_workspace_mounts(
    mut primary: Vec<MountSpec>,
    additional: Vec<MountSpec>,
    profile: &mobile_linux_api::BrandRuntimeProfile,
) -> Result<Vec<MountSpec>, String> {
    if primary.is_empty() && !additional.is_empty() {
        return Err("additional project mounts require the primary workspace mount".into());
    }
    if primary
        .iter()
        .filter(|mount| matches!(mount.purpose, MountPurpose::Workspace))
        .count()
        != primary.len()
        || primary.len() > 1
    {
        return Err("expected at most one primary workspace mount".into());
    }

    let (primary_host, primary_guest, home_host) = if additional.is_empty() {
        (None, None, None)
    } else {
        let mount = primary.first().ok_or_else(|| {
            "additional project mounts require the primary workspace mount".to_string()
        })?;
        let expected_guest =
            mobile_linux_api::guest_paths::workspace_for_profile(profile, "projects");
        if mount.guest_path != expected_guest {
            return Err("primary workspace mount does not match the selected brand profile".into());
        }
        let canonical_primary = std::fs::canonicalize(&mount.host_path)
            .map_err(|error| format!("resolve primary workspace mount: {error}"))?;
        let home = canonical_primary
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or_else(|| "primary workspace mount has no user-home parent".to_string())?
            .to_path_buf();
        let expected_primary = home.join(&profile.brand_id).join("projects");
        if canonical_primary != expected_primary {
            return Err(
                "primary workspace host path does not match the selected brand home".into(),
            );
        }
        (Some(canonical_primary), Some(expected_guest), Some(home))
    };
    let guest_home = profile.guest_home();
    let config_guest = profile.config_guest_path();
    let apps_guest = profile.apps_guest_path();
    let primary_guest_path = primary_guest
        .as_deref()
        .map(|path| std::path::Path::new(path));

    for mount in additional {
        if mount.purpose != MountPurpose::External {
            return Err("additional project mounts must use External purpose".into());
        }
        if !mount.host_path.is_absolute()
            || !is_normalized_absolute_path(&mount.host_path)
            || !is_normalized_absolute_guest_path(&mount.guest_path)
            || mount.host_path.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
        {
            return Err("additional project mount paths must be absolute and normalized".into());
        }
        let guest = std::path::Path::new(&mount.guest_path);
        let relative = guest
            .strip_prefix(&guest_home)
            .map_err(|_| "additional project mount is outside the selected brand home")?;
        if relative.as_os_str().is_empty() {
            return Err("additional project mount may not expose the whole user home".into());
        }
        let is_local_app_source = guest.starts_with(&apps_guest) && guest != apps_guest;
        if guest == apps_guest || apps_guest.starts_with(guest) {
            return Err(
                "additional project mount may not expose the Local App root or its parent".into(),
            );
        }
        if paths_overlap(&mount.guest_path, &config_guest.to_string_lossy()) && !is_local_app_source
        {
            return Err("additional project mount overlaps the protected brand config tree".into());
        }
        if primary_guest_path
            .is_some_and(|primary| paths_overlap(&mount.guest_path, &primary.to_string_lossy()))
        {
            return Err("additional project mount overlaps the primary workspace".into());
        }
        let expected_host = canonical_directory_without_symlinks(
            home_host
                .as_deref()
                .expect("home is resolved for additional mounts"),
            relative,
        )?;
        if mount.host_path != expected_host {
            return Err(
                "additional project host and guest paths do not map to the selected brand home"
                    .into(),
            );
        }
        if primary_host.as_ref().is_some_and(|primary| {
            paths_overlap(
                &primary.to_string_lossy(),
                &mount.host_path.to_string_lossy(),
            )
        }) {
            return Err("additional project mount overlaps the primary workspace".into());
        }
        if primary
            .iter()
            .filter(|existing| matches!(existing.purpose, MountPurpose::External))
            .any(|existing| {
                paths_overlap(&existing.guest_path, &mount.guest_path)
                    || paths_overlap(
                        &existing.host_path.to_string_lossy(),
                        &mount.host_path.to_string_lossy(),
                    )
            })
        {
            return Err("additional project mounts overlap each other".into());
        }
        primary.push(mount);
    }
    Ok(primary)
}

fn is_normalized_absolute_path(path: &std::path::Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
}

fn is_normalized_absolute_guest_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() > 1
        && !path.ends_with('/')
        && !path.contains('\0')
        && path
            .split('/')
            .skip(1)
            .all(|component| !component.is_empty() && component != "." && component != "..")
        && is_normalized_absolute_path(std::path::Path::new(path))
}

fn canonical_directory_without_symlinks(
    root: &std::path::Path,
    relative: &std::path::Path,
) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(root)
        .map_err(|error| format!("resolve selected user home: {error}"))?;
    let mut path = root.clone();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err("additional project mount contains a non-normal guest path".into());
        };
        path.push(component);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("resolve additional project mount: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(
                "additional project mount must be a real directory without symlinks".into(),
            );
        }
    }
    let canonical = std::fs::canonicalize(&path)
        .map_err(|error| format!("resolve canonical additional project mount: {error}"))?;
    if !canonical.starts_with(&root) {
        return Err("additional project mount escaped the selected user home".into());
    }
    Ok(canonical)
}

fn paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
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
    fn device_services(&self) -> Option<&DeviceServices> {
        Some(&self.device_services)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::host::{
        CameraError, CapturePhotoOpts, CapturedImage, FsError, ProcessCommand, SandboxPolicy,
        ShareError, SharePayload, ShareResult,
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
            brand_profile: mobile_linux_api::BrandRuntimeProfile::default(),
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
            additional_workspace_mounts: Vec::new(),
        }
    }

    #[test]
    fn device_services_preserve_injected_handles_and_unavailable_options() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut inputs = inputs(temp.path(), None);
        inputs.secure_storage = Some(Arc::new(lingxi_core::host::InMemorySecureStorage::new()));
        let camera = inputs.camera.clone();
        let audio = inputs.audio.clone();
        let share = inputs.share.clone();
        let secure_storage = inputs.secure_storage.clone().unwrap();
        let platform = IosPlatform::new(inputs);
        let platform: &dyn Platform = &platform;

        assert!(Arc::ptr_eq(&camera, &platform.camera().unwrap()));
        assert!(Arc::ptr_eq(&audio, &platform.audio_service().unwrap()));
        assert!(Arc::ptr_eq(&share, &platform.share().unwrap()));
        assert!(Arc::ptr_eq(
            &secure_storage,
            &platform.secure_storage().unwrap()
        ));
        assert!(platform.device_services().is_some());
        assert!(platform.mobile_linux().is_none());
        assert!(platform.location().is_none());
        assert!(platform.notifications().is_none());
        assert!(platform.clipboard().is_none());
        assert!(platform.device_status().is_none());
        assert!(platform.haptics().is_none());
        assert!(platform.deep_link().is_none());
        assert!(platform.calendar().is_none());
        assert!(platform.contacts().is_none());
        assert!(platform.computer_control().is_none());
        assert!(platform.android_ui_automation().is_none());
    }

    #[test]
    fn workspace_mount_uses_selected_brand_profile() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace_host_path = temp.path().join("persistent/atlas_guest/atlas/projects");
        let profile = mobile_linux_api::BrandRuntimeProfile::new("atlas", "atlas_guest", ".atlas");
        let mounts = build_mobile_linux_mounts(
            Some(workspace_host_path.clone()),
            Some("projects".into()),
            &profile,
        );

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].host_path, workspace_host_path);
        assert_eq!(mounts[0].guest_path, "/home/atlas_guest/atlas/projects");
    }

    fn mount_fixture(
        root: &std::path::Path,
        profile: &mobile_linux_api::BrandRuntimeProfile,
    ) -> (PathBuf, PathBuf) {
        let home = root.join("persistent").join(&profile.linux_user);
        let primary_host = home.join(&profile.brand_id).join("projects");
        let app_host = home
            .join(&profile.config_dir_name)
            .join(".apps")
            .join("calorie-a1b2c3d4");
        std::fs::create_dir_all(&primary_host).expect("create primary workspace");
        std::fs::create_dir_all(&app_host).expect("create Local App source");
        (primary_host, app_host)
    }

    fn external_mount(host_path: PathBuf, guest_path: impl Into<String>) -> MountSpec {
        MountSpec {
            host_path,
            guest_path: guest_path.into(),
            read_only: false,
            purpose: MountPurpose::External,
        }
    }

    #[test]
    fn exact_external_project_mount_preserves_one_workspace_and_brand_mapping() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile = mobile_linux_api::BrandRuntimeProfile::new("atlas", "atlas_guest", ".atlas");
        let (primary_host, app_host) = mount_fixture(temp.path(), &profile);
        let app_host = app_host.canonicalize().expect("canonical app source");
        let primary = build_mobile_linux_mounts(
            Some(primary_host.clone()),
            Some("projects".into()),
            &profile,
        );
        let app_guest = "/home/atlas_guest/.atlas/.apps/calorie-a1b2c3d4";
        let mounts = append_additional_workspace_mounts(
            primary,
            vec![external_mount(app_host.clone(), app_guest)],
            &profile,
        )
        .unwrap();

        assert_eq!(
            mounts
                .iter()
                .filter(|mount| matches!(mount.purpose, MountPurpose::Workspace))
                .count(),
            1
        );
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].host_path, primary_host);
        assert_eq!(mounts[1].host_path, app_host);
        assert_eq!(mounts[1].guest_path, app_guest);
        assert_eq!(mounts[1].purpose, MountPurpose::External);

        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(UnavailableMobileLinuxRuntime::unavailable(
                SandboxBackend::IosIsh,
                MobileLinuxRuntimeMode::MobileLinux,
                "ios",
                "arm64",
                "test stub",
            ));
        let sandbox =
            MobileLinuxSandbox::new(runtime, mounts.clone(), profile.clone()).expect("sandbox");
        let prepared = sandbox
            .prepare(
                ProcessCommand {
                    command: "/bin/sh".into(),
                    args: vec!["-lc".into(), "pwd".into()],
                    cwd: Some(app_host.clone()),
                    env: Default::default(),
                    timeout: None,
                    stdin: None,
                },
                &SandboxPolicy {
                    network: mobile_linux_api::NetworkPolicy::Disabled,
                    writable_paths: vec![app_host.clone()],
                    denied_paths: Vec::new(),
                    allow_subprocess: true,
                    limits: mobile_linux_api::ResourceLimits::default(),
                },
            )
            .expect("prepare with exact Project mounts");
        let plan = prepared
            .backend_plan()
            .expect("mobile sandbox plan")
            .downcast::<mobile_linux_api::MobileLinuxSandboxPlan>()
            .expect("mobile Linux sandbox plan type");
        assert_eq!(plan.mounts.len(), 2);
        for mount in &mounts {
            let mut canonical_mount = mount.clone();
            canonical_mount.host_path = mount.host_path.canonicalize().expect("canonical mount");
            assert!(plan.mounts.contains(&canonical_mount));
        }
        assert_eq!(plan.request.mounts, plan.mounts);
        assert_eq!(plan.request.cwd.as_deref(), Some(app_guest));

        let wrong_profile =
            mobile_linux_api::BrandRuntimeProfile::new("other", "other_guest", ".other");
        assert!(append_additional_workspace_mounts(
            build_mobile_linux_mounts(
                Some(mounts[0].host_path.clone()),
                Some("projects".into()),
                &wrong_profile,
            ),
            vec![external_mount(app_host, app_guest)],
            &wrong_profile,
        )
        .is_err());
    }

    #[test]
    fn additional_mounts_reject_unmapped_private_and_symlink_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile = mobile_linux_api::BrandRuntimeProfile::new("atlas", "atlas_guest", ".atlas");
        let (primary_host, app_host) = mount_fixture(temp.path(), &profile);
        let home = temp.path().join("persistent/atlas_guest");
        let primary = build_mobile_linux_mounts(
            Some(primary_host.clone()),
            Some("projects".into()),
            &profile,
        );
        let guest_home = "/home/atlas_guest";
        let apps_guest = "/home/atlas_guest/.atlas/.apps";
        let cases = [
            external_mount(temp.path().join("outside"), "/tmp/outside"),
            external_mount(home.clone(), guest_home),
            external_mount(home.join(".atlas/.apps"), apps_guest),
            external_mount(
                home.join(".atlas/private"),
                "/home/atlas_guest/.atlas/private",
            ),
            external_mount(
                primary_host.join("subdir"),
                "/home/atlas_guest/atlas/projects/subdir",
            ),
            external_mount(
                app_host.join(".."),
                "/home/atlas_guest/.atlas/.apps/../calorie-a1b2c3d4",
            ),
        ];
        for invalid in cases {
            assert!(
                append_additional_workspace_mounts(primary.clone(), vec![invalid], &profile)
                    .is_err(),
                "invalid mount should be rejected"
            );
        }

        #[cfg(unix)]
        {
            let alias = home.join(".atlas/.apps/alias");
            std::os::unix::fs::symlink(&app_host, &alias).expect("create app alias symlink");
            assert!(append_additional_workspace_mounts(
                primary,
                vec![external_mount(alias, format!("{apps_guest}/alias"))],
                &profile,
            )
            .is_err());
        }
    }

    #[test]
    fn additional_mounts_reject_duplicate_and_nested_sources() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile = mobile_linux_api::BrandRuntimeProfile::new("atlas", "atlas_guest", ".atlas");
        let (primary_host, app_host) = mount_fixture(temp.path(), &profile);
        let parent = app_host.parent().unwrap().join("team");
        let nested = parent.join("nested");
        std::fs::create_dir_all(&nested).expect("create nested project sources");
        let parent = parent.canonicalize().expect("canonical parent source");
        let nested = nested.canonicalize().expect("canonical nested source");
        let primary =
            build_mobile_linux_mounts(Some(primary_host), Some("projects".into()), &profile);
        let parent_guest = "/home/atlas_guest/.atlas/.apps/team";
        let nested_guest = "/home/atlas_guest/.atlas/.apps/team/nested";
        let app = external_mount(parent.clone(), parent_guest);
        assert!(append_additional_workspace_mounts(
            primary.clone(),
            vec![app.clone(), app],
            &profile,
        )
        .is_err());
        assert!(append_additional_workspace_mounts(
            primary,
            vec![
                external_mount(parent, parent_guest),
                external_mount(nested, nested_guest)
            ],
            &profile,
        )
        .is_err());
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
