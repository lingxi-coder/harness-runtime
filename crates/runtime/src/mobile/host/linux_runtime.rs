use mobile_linux_api::{MobileLinuxCapability, MobileLinuxRuntimeMode, RootfsState, RootfsStatus};
use std::sync::Arc;
use tool_api::SessionCwd;

/// Lowered rootfs lifecycle state for the foreign host.
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobileLinuxRootfsStateDto {
    Missing,
    Installing,
    Ready,
    Corrupt,
    Repairing,
    Resetting,
    Unsupported,
    BlockedByLicense,
}

/// Combined runtime + rootfs status for Android/iOS settings UIs.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone)]
pub struct MobileLinuxStatusDto {
    /// Selected runtime mode.
    pub mode: String,
    /// Backend label for diagnostics / UI.
    pub backend: String,
    /// Whether the runtime can be used right now.
    pub available: bool,
    /// Human-readable availability / failure detail.
    pub reason: Option<String>,
    /// Rootfs lifecycle state.
    pub rootfs_state: MobileLinuxRootfsStateDto,
    /// Platform string (`android` / `ios`).
    pub platform: String,
    /// ABI / architecture string.
    pub abi: String,
    /// Active rootfs version, when present.
    pub version: Option<String>,
    /// Installed rootfs size in bytes, when known.
    pub installed_size_bytes: Option<u64>,
    /// Writable guest paths currently permitted.
    pub writable_guest_paths: Vec<String>,
    /// Capability flags.
    pub streaming_output: bool,
    pub background_processes: bool,
    pub pty: bool,
    pub bind_mounts: bool,
    pub rootfs_integrity: bool,
}

pub(super) fn lower_mobile_linux_state(state: RootfsState) -> MobileLinuxRootfsStateDto {
    match state {
        RootfsState::Missing => MobileLinuxRootfsStateDto::Missing,
        RootfsState::Installing => MobileLinuxRootfsStateDto::Installing,
        RootfsState::Ready => MobileLinuxRootfsStateDto::Ready,
        RootfsState::Corrupt => MobileLinuxRootfsStateDto::Corrupt,
        RootfsState::Repairing => MobileLinuxRootfsStateDto::Repairing,
        RootfsState::Resetting => MobileLinuxRootfsStateDto::Resetting,
        RootfsState::Unsupported => MobileLinuxRootfsStateDto::Unsupported,
        RootfsState::BlockedByLicense => MobileLinuxRootfsStateDto::BlockedByLicense,
    }
}

pub(super) fn lower_mobile_linux_mode(mode: MobileLinuxRuntimeMode) -> String {
    match mode {
        MobileLinuxRuntimeMode::Legacy => "legacy".to_string(),
        MobileLinuxRuntimeMode::MobileLinux => "mobile-linux".to_string(),
    }
}

pub(super) fn lower_mobile_linux_backend(backend: mobile_linux_api::SandboxBackend) -> String {
    match backend {
        mobile_linux_api::SandboxBackend::LinuxNamespaces => "linux-namespaces",
        mobile_linux_api::SandboxBackend::LinuxFirejail => "linux-firejail",
        mobile_linux_api::SandboxBackend::MacOsSandboxExec => "macos-sandbox-exec",
        mobile_linux_api::SandboxBackend::WindowsJobObject => "windows-job-object",
        mobile_linux_api::SandboxBackend::AndroidMinijail => "android-minijail",
        mobile_linux_api::SandboxBackend::AndroidProot => "android-proot",
        mobile_linux_api::SandboxBackend::IosIsh => "ios-ish",
        mobile_linux_api::SandboxBackend::None => "none",
    }
    .to_string()
}

pub(super) fn lower_mobile_linux_status(
    capability: MobileLinuxCapability,
    status: RootfsStatus,
) -> MobileLinuxStatusDto {
    MobileLinuxStatusDto {
        mode: lower_mobile_linux_mode(status.mode),
        backend: lower_mobile_linux_backend(status.backend),
        available: capability.available,
        reason: capability.reason.or(status.last_error),
        rootfs_state: lower_mobile_linux_state(status.state),
        platform: status.platform,
        abi: status.abi,
        version: status.version,
        installed_size_bytes: status.installed_size_bytes,
        writable_guest_paths: status.writable_guest_paths,
        streaming_output: capability.streaming_output,
        background_processes: capability.background_processes,
        pty: capability.pty,
        bind_mounts: capability.bind_mounts,
        rootfs_integrity: capability.rootfs_integrity,
    }
}

pub(super) fn gate_mobile_shell_ctx(
    carrier: Option<tool_api::MobileShellToolCtx>,
    capability: Option<&MobileLinuxCapability>,
) -> Option<tool_api::MobileShellToolCtx> {
    let mut carrier = carrier?;
    if capability.is_some_and(|cap| {
        matches!(cap.mode, MobileLinuxRuntimeMode::MobileLinux) && !cap.available
    }) {
        carrier.enabled = false;
    }
    Some(carrier)
}

pub(super) fn gate_mobile_git_ctx(
    carrier: Option<tool_api::MobileGitToolCtx>,
    capability: Option<&MobileLinuxCapability>,
) -> Option<tool_api::MobileGitToolCtx> {
    let mut carrier = carrier?;
    if capability.is_some_and(|cap| {
        matches!(cap.mode, MobileLinuxRuntimeMode::MobileLinux) && !cap.available
    }) {
        carrier.enabled = false;
    }
    Some(carrier)
}

pub(super) fn build_mobile_runtime_environment(
    host_environment: Option<&platform_api::MobileHostEnvironment>,
    shell_ctx: Option<&tool_api::MobileShellToolCtx>,
    capability: Option<&MobileLinuxCapability>,
    session_cwd: &SessionCwd,
) -> Option<platform_api::MobileRuntimeEnvironment> {
    let host_environment = host_environment?.clone();
    let enabled_shell = shell_ctx.filter(|ctx| ctx.enabled);
    let tool_runtime = if capability
        .is_some_and(|cap| matches!(cap.mode, MobileLinuxRuntimeMode::MobileLinux) && cap.available)
        || enabled_shell.is_some_and(|ctx| ctx.force_platform_sandbox)
    {
        platform_api::MobileToolRuntime::MobileLinuxGuest
    } else if enabled_shell.is_some() {
        platform_api::MobileToolRuntime::AndroidLegacy
    } else {
        platform_api::MobileToolRuntime::Unavailable
    };
    let network_policy = match tool_runtime {
        platform_api::MobileToolRuntime::MobileLinuxGuest => {
            platform_api::MobileNetworkPolicy::PermissionMediated
        }
        platform_api::MobileToolRuntime::AndroidLegacy => {
            platform_api::MobileNetworkPolicy::DeniedByHost
        }
        platform_api::MobileToolRuntime::Unavailable => {
            platform_api::MobileNetworkPolicy::DeniedByHost
        }
    };
    let lifecycle_policy = match host_environment.launch_mode {
        platform_api::MobileLaunchMode::ScheduledHeadless => {
            platform_api::MobileLifecyclePolicy::ScheduledHeadlessBestEffort
        }
        platform_api::MobileLaunchMode::Interactive => match host_environment.host_os {
            platform_api::MobileHostOs::Ios => {
                platform_api::MobileLifecyclePolicy::IosFiniteBackgroundAssertion
            }
            platform_api::MobileHostOs::Android => {
                platform_api::MobileLifecyclePolicy::AndroidForegroundServiceBestEffort
            }
        },
        platform_api::MobileLaunchMode::Unknown => {
            platform_api::MobileLifecyclePolicy::UnknownBestEffort
        }
    };

    let guest_cwd = matches!(
        tool_runtime,
        platform_api::MobileToolRuntime::MobileLinuxGuest
    )
    .then(|| session_cwd.cwd().to_string_lossy().to_string());
    Some(platform_api::MobileRuntimeEnvironment::new(
        host_environment,
        tool_runtime,
        guest_cwd,
        enabled_shell.map(|ctx| ctx.shell_path.clone()),
        enabled_shell.map(|ctx| ctx.runtime_label.clone()),
        network_policy,
        lifecycle_policy,
    ))
}

pub(super) fn mobile_launch_is_interactive(
    host_environment: Option<&platform_api::MobileHostEnvironment>,
) -> bool {
    !host_environment.is_some_and(|environment| {
        matches!(
            environment.launch_mode,
            platform_api::MobileLaunchMode::ScheduledHeadless
        )
    })
}

pub(super) async fn mobile_typescript_lsp_ready(
    runtime: &Arc<dyn mobile_linux_api::MobileLinuxRuntime>,
    capability: Option<&mobile_linux_api::MobileLinuxCapability>,
    host_environment: Option<&platform_api::MobileHostEnvironment>,
) -> bool {
    if !capability.is_some_and(|value| value.available)
        || host_environment.is_some_and(|environment| {
            matches!(environment.host_os, platform_api::MobileHostOs::Ios)
                && matches!(
                    environment.execution_target,
                    platform_api::MobileExecutionTarget::Simulator
                )
        })
    {
        return false;
    }
    let Ok(mut status) = runtime.rootfs_status().await else {
        return false;
    };
    if matches!(status.state, mobile_linux_api::RootfsState::Missing) {
        let Ok(booted) = runtime.boot().await else {
            return false;
        };
        status = booted;
    }
    if !matches!(status.state, mobile_linux_api::RootfsState::Ready) {
        return false;
    }
    let Some(active_root) = status.active_root else {
        return false;
    };
    let relative = std::path::Path::new("opt/lingxi/toolchains/typescript/7.0.2");
    let toolchain_root = match runtime.backend() {
        mobile_linux_api::SandboxBackend::IosIsh => active_root.join("data").join(relative),
        _ => active_root.join(relative),
    };
    if !toolchain_root.join("tsc").is_file() {
        return false;
    }
    std::fs::read_to_string(toolchain_root.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|metadata| {
            metadata.get("version").and_then(serde_json::Value::as_str) == Some("7.0.2")
                && metadata
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|name| name.starts_with("@typescript/typescript-linux-"))
        })
}

pub(super) fn model_visible_mobile_cwd(
    path: &std::path::Path,
    mounts: &[mobile_linux_api::MountSpec],
    has_mobile_linux_guest: bool,
) -> Option<String> {
    if !has_mobile_linux_guest {
        return None;
    }
    mobile_linux_api::map_host_path_to_guest(path, mounts).or_else(|| {
        path.to_str()
            .and_then(platform_api::mobile_runtime_environment::normalize_mobile_guest_cwd)
    })
}

pub(super) fn subagent_env_platform_name(rust_os: &str) -> &str {
    match rust_os {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

pub(super) fn build_mobile_subagent_env_renderer(
    probe_cwd: std::path::PathBuf,
    mobile_runtime_environment: Option<&platform_api::MobileRuntimeEnvironment>,
    mobile_workspace_cwd_provider: agent::handle::MobileWorkspaceCwdProvider,
) -> agent::handle::SubagentEnvRenderer {
    if mobile_runtime_environment.is_none() {
        return Arc::new(orchestrator::prompt::subagent_env::boot_renderer(probe_cwd));
    }

    let is_git_repo = orchestrator::prompt::git_status::probe(&probe_cwd).is_some();
    let platform = subagent_env_platform_name(std::env::consts::OS).to_string();
    let shell = orchestrator::prompt::env_meta::detect_shell();
    let os_version = orchestrator::prompt::env_meta::os_version_string();
    let default_visible_cwd = mobile_workspace_cwd_provider(None)
        .or_else(|| {
            mobile_runtime_environment
                .and_then(|environment| environment.guest_cwd().map(ToOwned::to_owned))
        })
        .or_else(|| {
            probe_cwd
                .to_str()
                .and_then(platform_api::mobile_runtime_environment::normalize_mobile_guest_cwd)
        })
        .unwrap_or_else(|| mobile_linux_api::guest_paths::WORKSPACE_ROOT.to_string());

    Arc::new(
        move |model_id: &str, cwd_override: Option<&std::path::Path>| {
            let visible_cwd = mobile_workspace_cwd_provider(cwd_override)
                .or_else(|| {
                    cwd_override.and_then(|path| {
                        path.to_str().and_then(
                            platform_api::mobile_runtime_environment::normalize_mobile_guest_cwd,
                        )
                    })
                })
                .unwrap_or_else(|| default_visible_cwd.clone());
            orchestrator::prompt::subagent_env::subagent_env_block(
                model_id,
                std::path::Path::new(&visible_cwd),
                is_git_repo,
                &platform,
                &shell,
                &os_version,
                &[],
                cwd_override.is_some(),
            )
        },
    )
}
