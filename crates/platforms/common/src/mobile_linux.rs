//! Shared rootfs manifest and verification helpers for the mobile Linux phase-1 path.

#![allow(missing_docs)]

use async_trait::async_trait;
use platform_api::{
    BackendPlanHandle, LinuxCommandRequest, LinuxProcessHandle, MobileLinuxError,
    MobileLinuxRuntime, MobileLinuxRuntimeMode, MobileLinuxSandboxPlan, MobileLinuxTaskStatus,
    MountSpec, ProcessCommand, ProcessError, ProcessHandle, ProcessOutput, ProcessRunner,
    ProcessStreamSink, RootfsState, RootfsStatus, Sandbox, SandboxBackend, SandboxCapability,
    SandboxError, SandboxFeatures, SandboxPolicy, SandboxedCommand, SandboxedTag,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;

// Derived from the single guest-path atlas — see
// `platform_api::mobile_linux::guest_paths` for why this is no longer a local
// literal (four Rust + seven Swift sites used to restate it independently).
const ALLOWED_WRITABLE_GUEST_PATHS: &[&str] = &[
    platform_api::mobile_linux::guest_paths::HOME,
    platform_api::mobile_linux::guest_paths::SCRATCH[0],
    platform_api::mobile_linux::guest_paths::SCRATCH[1],
    platform_api::mobile_linux::guest_paths::WORKSPACE_ROOT,
];
const RESETTABLE_GUEST_PATHS: &[&str] = platform_api::mobile_linux::guest_paths::SCRATCH;
const REQUIRED_ROOTFS_PACKAGES: &[&str] = &[
    "busybox",
    "git",
    "openssh-client",
    "python3",
    "ca-certificates",
];

fn to_sandbox_error(error: MobileLinuxError) -> SandboxError {
    match error {
        MobileLinuxError::Unsupported => SandboxError::Unsupported,
        MobileLinuxError::Unavailable(message)
        | MobileLinuxError::LicenseBlocked(message)
        | MobileLinuxError::Integrity(message)
        | MobileLinuxError::InvalidRequest(message)
        | MobileLinuxError::Io(message) => SandboxError::Unavailable(message),
        MobileLinuxError::NetworkPolicyUnavailable(message) => {
            SandboxError::Unavailable(format!("network_policy_unavailable: {message}"))
        }
        MobileLinuxError::ResourceLimitExceeded(message) => {
            SandboxError::Unavailable(format!("resource_limit_exceeded: {message}"))
        }
        MobileLinuxError::Timeout => {
            SandboxError::Io("unexpected timeout during sandbox prepare".to_string())
        }
    }
}

fn to_process_error(error: MobileLinuxError) -> ProcessError {
    match error {
        MobileLinuxError::Unsupported => ProcessError::Unsupported,
        MobileLinuxError::Unavailable(message)
        | MobileLinuxError::LicenseBlocked(message)
        | MobileLinuxError::Integrity(message)
        | MobileLinuxError::InvalidRequest(message)
        | MobileLinuxError::Io(message) => ProcessError::Io(message),
        MobileLinuxError::NetworkPolicyUnavailable(message) => {
            ProcessError::PolicyUnsupported(format!("network_policy_unavailable: {message}"))
        }
        MobileLinuxError::ResourceLimitExceeded(message) => {
            ProcessError::SandboxEnforcementFailed(format!("resource_limit_exceeded: {message}"))
        }
        MobileLinuxError::Timeout => ProcessError::Timeout,
    }
}

fn normalize_mounts(mounts: &[MountSpec]) -> Result<Vec<MountSpec>, MobileLinuxError> {
    let mut normalized: Vec<MountSpec> = Vec::with_capacity(mounts.len());
    for mount in mounts {
        if !mount.host_path.is_absolute() {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "mount host path must be absolute: {}",
                mount.host_path.display()
            )));
        }
        let host_text = mount.host_path.to_string_lossy();
        if host_text.contains('\0') {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "mount host path contains NUL: {}",
                mount.host_path.display()
            )));
        }
        let metadata = fs::symlink_metadata(&mount.host_path).map_err(|error| {
            MobileLinuxError::InvalidRequest(format!(
                "mount host path must be an existing real directory: {} ({error})",
                mount.host_path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "mount host path must be a real directory, not a symlink or file: {}",
                mount.host_path.display()
            )));
        }
        let canonical_host_path = fs::canonicalize(&mount.host_path).map_err(|error| {
            MobileLinuxError::InvalidRequest(format!(
                "failed to canonicalize mount host path {}: {error}",
                mount.host_path.display()
            ))
        })?;
        let canonical_host_text = canonical_host_path.to_string_lossy();
        validate_guest_path(&mount.guest_path)
            .map_err(|err| MobileLinuxError::InvalidRequest(err.to_string()))?;
        if path_looks_sensitive(&host_text)
            || path_looks_sensitive(&canonical_host_text)
            || path_looks_sensitive(&mount.guest_path)
        {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "sensitive host or guest mount path is not allowed: {} -> {}",
                mount.host_path.display(),
                mount.guest_path
            )));
        }
        for existing in &normalized {
            if canonical_host_path == existing.host_path
                || canonical_host_path.starts_with(&existing.host_path)
                || existing.host_path.starts_with(&canonical_host_path)
            {
                return Err(MobileLinuxError::InvalidRequest(format!(
                    "overlapping host mount sources are not allowed: {} vs {}",
                    canonical_host_path.display(),
                    existing.host_path.display()
                )));
            }
            if path_is_within_guest_path(&mount.guest_path, &existing.guest_path)
                || path_is_within_guest_path(&existing.guest_path, &mount.guest_path)
            {
                return Err(MobileLinuxError::InvalidRequest(format!(
                    "overlapping guest mount targets are not allowed: {} vs {}",
                    mount.guest_path, existing.guest_path
                )));
            }
        }
        let mut normalized_mount = mount.clone();
        normalized_mount.host_path = canonical_host_path;
        normalized.push(normalized_mount);
    }
    normalized.sort_by(|left, right| {
        right
            .host_path
            .components()
            .count()
            .cmp(&left.host_path.components().count())
    });
    Ok(normalized)
}

fn validate_linux_request(request: &LinuxCommandRequest) -> Result<(), MobileLinuxError> {
    if request.command.trim().is_empty() {
        return Err(MobileLinuxError::InvalidRequest(
            "mobile-linux command must not be empty".to_string(),
        ));
    }
    if request.command.starts_with('/') {
        validate_guest_path(&request.command)
            .map_err(|err| MobileLinuxError::InvalidRequest(err.to_string()))?;
    }
    if let Some(cwd) = &request.cwd {
        validate_guest_path(cwd)
            .map_err(|err| MobileLinuxError::InvalidRequest(err.to_string()))?;
    }
    if let Some(timeout_ms) = request.timeout_ms {
        if timeout_ms == 0 {
            return Err(MobileLinuxError::InvalidRequest(
                "timeout_ms must be greater than zero".to_string(),
            ));
        }
    }
    for mount in &request.mounts {
        if !mount.host_path.is_absolute() {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "mount host path must be absolute: {}",
                mount.host_path.display()
            )));
        }
        validate_guest_path(&mount.guest_path)
            .map_err(|err| MobileLinuxError::InvalidRequest(err.to_string()))?;
    }
    for key in request.env.keys() {
        if key.is_empty() || key.contains('=') || key.as_bytes().contains(&0) {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "invalid environment variable name: {key}"
            )));
        }
    }
    if request
        .env
        .values()
        .any(|value| value.as_bytes().contains(&0))
    {
        return Err(MobileLinuxError::InvalidRequest(
            "environment variable values may not contain NUL".to_string(),
        ));
    }
    Ok(())
}

fn build_linux_request(
    cmd: &ProcessCommand,
    policy: &SandboxPolicy,
    mounts: &[MountSpec],
) -> Result<LinuxCommandRequest, MobileLinuxError> {
    if cmd.command.trim().is_empty() {
        return Err(MobileLinuxError::InvalidRequest(
            "mobile-linux command must not be empty".to_string(),
        ));
    }

    for path in &policy.writable_paths {
        if !path.is_absolute() {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "writable path must be absolute: {}",
                path.display()
            )));
        }
        let (mount, guest_path) = map_host_path_to_guest(path, mounts)?;
        if mount.read_only {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "writable host path is covered by read-only mount: {}",
                path.display()
            )));
        }
        if !ALLOWED_WRITABLE_GUEST_PATHS
            .iter()
            .any(|allowed| path_is_within_guest_path(&guest_path, allowed))
        {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "guest writable path is outside the allowed mobile-linux prefixes: {guest_path}"
            )));
        }
    }

    for path in &policy.denied_paths {
        if !path.is_absolute() {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "denied path must be absolute: {}",
                path.display()
            )));
        }
        if map_host_path_to_guest(path, mounts).is_ok() {
            return Err(MobileLinuxError::Unsupported);
        }
    }

    let request = LinuxCommandRequest {
        command: map_command_or_cwd_to_guest(&cmd.command, mounts)?,
        args: cmd.args.clone(),
        cwd: cmd
            .cwd
            .as_ref()
            .map(|cwd| map_cwd_to_guest(cwd, mounts))
            .transpose()?,
        env: cmd
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>(),
        stdin: cmd.stdin.clone(),
        timeout_ms: cmd
            .timeout
            .map(|timeout| u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX))
            .filter(|timeout_ms| *timeout_ms > 0),
        network: policy.network,
        resource_limits: policy.limits,
        mounts: mounts.to_vec(),
    };
    validate_linux_request(&request)?;
    Ok(request)
}

fn map_command_or_cwd_to_guest(
    value: &str,
    mounts: &[MountSpec],
) -> Result<String, MobileLinuxError> {
    if value.starts_with('/') {
        match map_host_path_to_guest(Path::new(value), mounts) {
            Ok((_, guest)) => Ok(guest),
            Err(_) => {
                validate_guest_path(value)
                    .map_err(|err| MobileLinuxError::InvalidRequest(err.to_string()))?;
                Ok(value.to_string())
            }
        }
    } else {
        Ok(value.to_string())
    }
}

fn map_cwd_to_guest(value: &Path, mounts: &[MountSpec]) -> Result<String, MobileLinuxError> {
    match map_host_path_to_guest(value, mounts) {
        Ok((_, guest)) => Ok(guest),
        Err(_) => {
            let value = value.to_string_lossy().into_owned();
            validate_guest_path(&value)
                .map_err(|err| MobileLinuxError::InvalidRequest(err.to_string()))?;
            if ALLOWED_WRITABLE_GUEST_PATHS
                .iter()
                .any(|allowed| path_is_within_guest_path(&value, allowed))
            {
                Ok(value)
            } else {
                Err(MobileLinuxError::InvalidRequest(format!(
                    "cwd is not covered by a mobile-linux mount and is not an allowed guest path: {value}"
                )))
            }
        }
    }
}

fn map_host_path_to_guest<'a>(
    host_path: &Path,
    mounts: &'a [MountSpec],
) -> Result<(&'a MountSpec, String), MobileLinuxError> {
    let normalized_host_path = canonicalize_host_path_allow_missing(host_path)?;
    let mount = mounts
        .iter()
        .find(|mount| {
            normalized_host_path == mount.host_path
                || normalized_host_path.starts_with(&mount.host_path)
        })
        .ok_or_else(|| {
            MobileLinuxError::InvalidRequest(format!(
                "path is not covered by any mobile-linux mount: {}",
                host_path.display()
            ))
        })?;
    let relative = normalized_host_path
        .strip_prefix(&mount.host_path)
        .map_err(|_| {
            MobileLinuxError::InvalidRequest(format!(
                "path escapes mount root: {}",
                host_path.display()
            ))
        })?;
    let mut guest = PathBuf::from(&mount.guest_path);
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => guest.push(segment),
            _ => {
                return Err(MobileLinuxError::InvalidRequest(format!(
                    "unsupported host path component for {}",
                    host_path.display()
                )))
            }
        }
    }
    let guest = guest.to_string_lossy().into_owned();
    validate_guest_path(&guest).map_err(|err| MobileLinuxError::InvalidRequest(err.to_string()))?;
    Ok((mount, guest))
}

fn canonicalize_host_path_allow_missing(path: &Path) -> Result<PathBuf, MobileLinuxError> {
    if !path.is_absolute() {
        return Err(MobileLinuxError::InvalidRequest(format!(
            "host path must be absolute: {}",
            path.display()
        )));
    }
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| {
                    MobileLinuxError::InvalidRequest(format!(
                        "host path has no existing ancestor: {}",
                        path.display()
                    ))
                })?;
                missing.push(name.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| {
                        MobileLinuxError::InvalidRequest(format!(
                            "host path has no existing ancestor: {}",
                            path.display()
                        ))
                    })?
                    .to_path_buf();
            }
            Err(error) => {
                return Err(MobileLinuxError::InvalidRequest(format!(
                    "failed to inspect host path {}: {error}",
                    path.display()
                )));
            }
        }
    }
    let mut canonical = fs::canonicalize(&existing).map_err(|error| {
        MobileLinuxError::InvalidRequest(format!(
            "failed to canonicalize host path {}: {error}",
            existing.display()
        ))
    })?;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn path_looks_sensitive(path: &str) -> bool {
    let lowered = path.to_ascii_lowercase();
    [
        "keychain",
        "keystore",
        "secret",
        "credential",
        "token",
        "api_key",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

#[derive(Clone)]
pub struct MobileLinuxSandbox {
    runtime: Arc<dyn MobileLinuxRuntime>,
    backend: SandboxBackend,
    mode: MobileLinuxRuntimeMode,
    mounts: Vec<MountSpec>,
}

impl std::fmt::Debug for MobileLinuxSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobileLinuxSandbox")
            .field("backend", &self.backend)
            .field("mode", &self.mode)
            .field("mount_count", &self.mounts.len())
            .finish()
    }
}

impl MobileLinuxSandbox {
    pub fn new(
        runtime: Arc<dyn MobileLinuxRuntime>,
        mounts: Vec<MountSpec>,
    ) -> Result<Self, SandboxError> {
        let backend = runtime.backend();
        if !matches!(
            backend,
            SandboxBackend::AndroidProot | SandboxBackend::IosIsh
        ) {
            return Err(SandboxError::Unavailable(format!(
                "mobile-linux sandbox requires AndroidProot or IosIsh backend, got {backend:?}"
            )));
        }
        let normalized_mounts = normalize_mounts(&mounts).map_err(to_sandbox_error)?;
        Ok(Self {
            mode: runtime.mode(),
            runtime,
            backend,
            mounts: normalized_mounts,
        })
    }
}

#[async_trait]
impl Sandbox for MobileLinuxSandbox {
    fn is_available(&self) -> bool {
        // This synchronous trait method reports that the adapter is installed.
        // Runtime readiness is intentionally reported by
        // `MobileLinuxRuntime::probe_capability`; every operation still
        // fail-closes when that capability is unavailable or license-blocked.
        true
    }

    fn backend(&self) -> SandboxBackend {
        self.backend
    }

    fn prepare(
        &self,
        cmd: ProcessCommand,
        policy: &SandboxPolicy,
    ) -> Result<SandboxedCommand, SandboxError> {
        let request = build_linux_request(&cmd, policy, &self.mounts).map_err(to_sandbox_error)?;
        let plan = MobileLinuxSandboxPlan {
            backend: self.backend,
            mode: self.mode,
            request,
            mounts: self.mounts.clone(),
            limits: policy.limits,
            allow_subprocess: policy.allow_subprocess,
        };
        Ok(SandboxedCommand::__new_sandboxed_with_plan(
            cmd,
            SandboxedTag::Wrapped {
                backend: self.backend,
            },
            BackendPlanHandle::new(plan),
        ))
    }

    fn bypass_with_audit(&self, cmd: ProcessCommand, reason: &str) -> SandboxedCommand {
        SandboxedCommand::__new_sandboxed(
            cmd,
            SandboxedTag::BypassAuditedWithReason {
                reason: reason.to_string(),
            },
        )
    }

    async fn probe_capability(&self) -> SandboxCapability {
        let capability = self.runtime.probe_capability().await;
        SandboxCapability {
            available: capability.available,
            reason: capability.reason,
            features: SandboxFeatures {
                network_isolation: false,
                fs_readonly: false,
                fs_readwrite_paths: capability.bind_mounts,
                process_limit: false,
                no_new_privileges: false,
            },
        }
    }
}

pub struct MobileLinuxProcessRunner {
    runtime: Arc<dyn MobileLinuxRuntime>,
    backend: SandboxBackend,
    background_handles: Arc<Mutex<HashMap<String, LinuxProcessHandle>>>,
}

impl MobileLinuxProcessRunner {
    #[must_use]
    pub fn new(runtime: Arc<dyn MobileLinuxRuntime>) -> Self {
        Self {
            backend: runtime.backend(),
            runtime,
            background_handles: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn spawn_background_reaper(&self, task_id: String) {
        let runtime = self.runtime.clone();
        let handles = self.background_handles.clone();
        tokio::spawn(async move {
            let mut consecutive_missing = 0_u8;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                if !handles
                    .lock()
                    .expect("mobile-linux background handle map poisoned")
                    .contains_key(&task_id)
                {
                    break;
                }
                match runtime.task_status(&task_id).await {
                    Ok(Some(snapshot)) if task_status_is_terminal(snapshot.status) => {
                        handles
                            .lock()
                            .expect("mobile-linux background handle map poisoned")
                            .remove(&task_id);
                        break;
                    }
                    Ok(Some(_)) => consecutive_missing = 0,
                    Ok(None) => {
                        consecutive_missing += 1;
                        if consecutive_missing >= 4 {
                            handles
                                .lock()
                                .expect("mobile-linux background handle map poisoned")
                                .remove(&task_id);
                            break;
                        }
                    }
                    Err(_) => {
                        // Preserve ownership on transient status failures so a
                        // later explicit kill can still terminate the task.
                        consecutive_missing = 0;
                    }
                }
            }
        });
    }

    fn admitted_plan(
        &self,
        cmd: &SandboxedCommand,
    ) -> Result<Arc<MobileLinuxSandboxPlan>, ProcessError> {
        match cmd.tag() {
            SandboxedTag::BypassAuditedWithReason { reason } => {
                return Err(ProcessError::MalformedSandboxPlan(format!(
                    "bypass-audited commands cannot execute via mobile-linux runner: {reason}"
                )));
            }
            SandboxedTag::Wrapped { backend } if *backend != self.backend => {
                return Err(ProcessError::MalformedSandboxPlan(format!(
                    "foreign sandbox backend {backend:?} routed to mobile-linux runner"
                )));
            }
            SandboxedTag::Wrapped { .. } => {}
        }
        let handle = cmd.backend_plan().ok_or_else(|| {
            ProcessError::MalformedSandboxPlan(
                "mobile-linux plan missing from SandboxedCommand".to_string(),
            )
        })?;
        let plan = handle.downcast::<MobileLinuxSandboxPlan>().ok_or_else(|| {
            ProcessError::MalformedSandboxPlan(
                "backend plan failed to downcast to MobileLinuxSandboxPlan".to_string(),
            )
        })?;
        if plan.backend != self.backend {
            return Err(ProcessError::MalformedSandboxPlan(format!(
                "mobile-linux plan backend mismatch: expected {:?}, got {:?}",
                self.backend, plan.backend
            )));
        }
        validate_linux_request(&plan.request).map_err(to_process_error)?;
        Ok(plan)
    }
}

#[async_trait]
impl ProcessRunner for MobileLinuxProcessRunner {
    async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
        let plan = self.admitted_plan(cmd)?;
        let result = self
            .runtime
            .run(plan.request.clone())
            .await
            .map_err(to_process_error)?;
        Ok(result.into())
    }

    async fn run_streaming(
        &self,
        cmd: &SandboxedCommand,
        sink: Arc<dyn ProcessStreamSink>,
    ) -> Result<ProcessOutput, ProcessError> {
        let plan = self.admitted_plan(cmd)?;
        let result = self
            .runtime
            .run_streaming(plan.request.clone(), sink)
            .await
            .map_err(to_process_error)?;
        Ok(result.into())
    }

    async fn spawn_background(
        &self,
        cmd: &SandboxedCommand,
    ) -> Result<ProcessHandle, ProcessError> {
        let plan = self.admitted_plan(cmd)?;
        let handle = self
            .runtime
            .spawn_background(plan.request.clone())
            .await
            .map_err(to_process_error)?;
        self.background_handles
            .lock()
            .expect("mobile-linux background handle map poisoned")
            .insert(handle.id.clone(), handle.clone());
        self.spawn_background_reaper(handle.id.clone());
        Ok(ProcessHandle {
            task_id: handle.id,
            pid: 0,
        })
    }

    async fn kill(&self, handle: &ProcessHandle) -> Result<(), ProcessError> {
        let Some(linux_handle) = self
            .background_handles
            .lock()
            .expect("mobile-linux background handle map poisoned")
            .get(&handle.task_id)
            .cloned()
        else {
            return Err(ProcessError::Io(format!(
                "unknown background process handle: {}",
                handle.task_id
            )));
        };
        self.runtime
            .kill(&linux_handle)
            .await
            .map_err(to_process_error)?;
        self.background_handles
            .lock()
            .expect("mobile-linux background handle map poisoned")
            .remove(&handle.task_id);
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

fn task_status_is_terminal(status: MobileLinuxTaskStatus) -> bool {
    matches!(
        status,
        MobileLinuxTaskStatus::Completed
            | MobileLinuxTaskStatus::Failed
            | MobileLinuxTaskStatus::Cancelled
            | MobileLinuxTaskStatus::TimedOut
    )
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootfsManifest {
    pub schema_version: u32,
    pub runtime: String,
    pub platform: String,
    pub abi: String,
    pub rootfs_version: String,
    pub archive: RootfsArchive,
    pub packages: Vec<RootfsPackage>,
    pub executable_allowlist: Vec<RootfsManifestEntry>,
    pub immutable_files: Vec<RootfsImmutableEntry>,
    pub writable_paths: Vec<String>,
}

impl RootfsManifest {
    pub fn validate(&self) -> Result<(), RootfsManifestError> {
        if self.schema_version != 2 {
            return Err(RootfsManifestError::InvalidSchemaVersion);
        }
        if self.runtime.trim().is_empty()
            || self.platform.trim().is_empty()
            || self.abi.trim().is_empty()
            || self.rootfs_version.trim().is_empty()
            || self.archive.filename.trim().is_empty()
        {
            return Err(RootfsManifestError::MissingRequiredField);
        }
        let runtime_matches_platform = matches!(
            (self.runtime.as_str(), self.platform.as_str()),
            ("android-proot", "android") | ("ios-ish", "ios")
        );
        let abi_is_supported = matches!(self.abi.as_str(), "arm64" | "x86_64")
            && !(self.platform == "ios" && self.abi != "arm64");
        if !runtime_matches_platform || !abi_is_supported {
            return Err(RootfsManifestError::UnsupportedTarget {
                runtime: self.runtime.clone(),
                platform: self.platform.clone(),
                abi: self.abi.clone(),
            });
        }
        if !is_single_safe_component(&self.rootfs_version)
            || !is_single_safe_component(&self.archive.filename)
        {
            return Err(RootfsManifestError::UnsafeArtifactName);
        }
        if self.packages.is_empty()
            || self.executable_allowlist.is_empty()
            || self.immutable_files.is_empty()
        {
            return Err(RootfsManifestError::MissingRequiredField);
        }
        if self.archive.size_bytes == 0 || !is_lowercase_sha256(&self.archive.sha256) {
            return Err(RootfsManifestError::InvalidSha256(
                self.archive.sha256.clone(),
            ));
        }

        let mut package_names = BTreeSet::new();
        for package in &self.packages {
            if package.name.trim().is_empty()
                || package.version.trim().is_empty()
                || package.license.trim().is_empty()
                || package.architecture.trim().is_empty()
                || package.origin.trim().is_empty()
                || !package_names.insert(package.name.as_str())
            {
                return Err(RootfsManifestError::InvalidPackage(package.name.clone()));
            }
        }
        for required in REQUIRED_ROOTFS_PACKAGES {
            if !package_names.contains(required) {
                return Err(RootfsManifestError::MissingRequiredPackage(
                    (*required).to_string(),
                ));
            }
        }

        let mut seen = BTreeSet::new();
        for entry in &self.executable_allowlist {
            validate_guest_path(&entry.path)?;
            if !seen.insert(entry.path.clone()) {
                return Err(RootfsManifestError::DuplicatePath(entry.path.clone()));
            }
            if !is_lowercase_sha256(&entry.sha256) {
                return Err(RootfsManifestError::InvalidSha256(entry.sha256.clone()));
            }
        }

        let mut writable_paths = BTreeSet::new();
        for path in &self.writable_paths {
            validate_guest_path(path)?;
            if !writable_paths.insert(path.as_str()) {
                return Err(RootfsManifestError::DuplicatePath(path.clone()));
            }
            if !ALLOWED_WRITABLE_GUEST_PATHS.contains(&path.as_str()) {
                return Err(RootfsManifestError::WritablePathNotAllowed(path.clone()));
            }
        }
        for required in ALLOWED_WRITABLE_GUEST_PATHS {
            if !writable_paths.contains(required) {
                return Err(RootfsManifestError::MissingWritablePath(
                    (*required).to_string(),
                ));
            }
        }

        self.validate_integrity_inventory()
    }

    fn validate_integrity_inventory(&self) -> Result<(), RootfsManifestError> {
        let mut immutable_by_path = BTreeMap::new();
        for entry in &self.immutable_files {
            validate_guest_path(&entry.path)?;
            if immutable_by_path
                .insert(entry.path.as_str(), entry)
                .is_some()
            {
                return Err(RootfsManifestError::DuplicatePath(entry.path.clone()));
            }
            if !is_lowercase_sha256(&entry.sha256) {
                return Err(RootfsManifestError::InvalidSha256(entry.sha256.clone()));
            }
            if ALLOWED_WRITABLE_GUEST_PATHS
                .iter()
                .any(|writable| path_is_within_guest_path(&entry.path, writable))
            {
                return Err(RootfsManifestError::ImmutableFileInWritablePath(
                    entry.path.clone(),
                ));
            }
        }

        for entry in &self.executable_allowlist {
            if ALLOWED_WRITABLE_GUEST_PATHS
                .iter()
                .any(|writable| path_is_within_guest_path(&entry.path, writable))
            {
                return Err(RootfsManifestError::ExecutableInWritablePath(
                    entry.path.clone(),
                ));
            }
            let Some(immutable) = immutable_by_path.get(entry.path.as_str()) else {
                return Err(RootfsManifestError::ExecutableInventoryMismatch(
                    entry.path.clone(),
                ));
            };
            if immutable.kind != RootfsImmutableKind::RegularFile
                || immutable.sha256 != entry.sha256
                || entry
                    .size_bytes
                    .is_some_and(|size| size != immutable.size_bytes)
            {
                return Err(RootfsManifestError::ExecutableInventoryMismatch(
                    entry.path.clone(),
                ));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootfsArchive {
    pub filename: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootfsPackage {
    pub name: String,
    pub version: String,
    pub license: String,
    pub architecture: String,
    pub origin: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootfsManifestEntry {
    pub path: String,
    pub sha256: String,
    pub kind: RootfsEntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootfsEntryKind {
    Elf,
    SharedLibrary,
    Interpreter,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootfsImmutableEntry {
    pub path: String,
    pub sha256: String,
    pub kind: RootfsImmutableKind,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootfsImmutableKind {
    RegularFile,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootfsVerificationReport {
    pub ok: bool,
    pub verified_files: usize,
    pub verified_bytes: u64,
    pub issues: Vec<RootfsVerificationIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootfsVerificationIssue {
    pub path: String,
    pub reason: String,
}

pub struct RootfsStore {
    managed_root: PathBuf,
    backend: SandboxBackend,
    mode: MobileLinuxRuntimeMode,
    platform: String,
    abi: String,
}

impl RootfsStore {
    #[must_use]
    pub fn new(
        managed_root: PathBuf,
        backend: SandboxBackend,
        mode: MobileLinuxRuntimeMode,
        platform: impl Into<String>,
        abi: impl Into<String>,
    ) -> Self {
        Self {
            managed_root,
            backend,
            mode,
            platform: platform.into(),
            abi: abi.into(),
        }
    }

    #[must_use]
    pub fn managed_root(&self) -> &Path {
        &self.managed_root
    }

    #[must_use]
    pub fn active_root(&self) -> PathBuf {
        self.managed_root.join("active")
    }

    #[must_use]
    pub fn staged_root(&self, version: &str) -> PathBuf {
        self.managed_root.join("staged").join(version)
    }

    pub fn verify_archive(
        &self,
        archive_path: &Path,
        manifest: &RootfsManifest,
    ) -> Result<(), RootfsStoreError> {
        self.validate_manifest_target(manifest)?;
        let metadata = fs::symlink_metadata(archive_path).map_err(|err| RootfsStoreError::Io {
            path: archive_path.to_path_buf(),
            message: err.to_string(),
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(RootfsStoreError::UnsafeManagedPath(
                archive_path.to_path_buf(),
            ));
        }
        if metadata.len() != manifest.archive.size_bytes {
            return Err(RootfsStoreError::ArchiveSizeMismatch {
                expected: manifest.archive.size_bytes,
                actual: metadata.len(),
            });
        }
        let actual = sha256_file(archive_path)?;
        if actual != manifest.archive.sha256 {
            return Err(RootfsStoreError::ArchiveHashMismatch {
                expected: manifest.archive.sha256.clone(),
                actual,
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn status(&self, manifest: &RootfsManifest) -> RootfsStatus {
        let active_root = self.active_root();
        let mut last_error = self
            .validate_manifest_target(manifest)
            .err()
            .map(|error| error.to_string());
        let staged_root = if last_error.is_none() {
            self.staged_root(&manifest.rootfs_version)
        } else {
            self.managed_root.join("staged").join("invalid-manifest")
        };
        let active_present = path_present(&active_root);
        let staged_present = path_present(&staged_root);
        let state = if last_error.is_some() {
            RootfsState::Corrupt
        } else if active_present {
            match self.verify_active_root(manifest) {
                Ok(report) if report.ok => RootfsState::Ready,
                Ok(report) => {
                    last_error = Some(verification_failure_message(&report));
                    RootfsState::Corrupt
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                    RootfsState::Corrupt
                }
            }
        } else if staged_present {
            RootfsState::Installing
        } else {
            RootfsState::Missing
        };
        RootfsStatus {
            state,
            backend: self.backend,
            mode: self.mode,
            platform: self.platform.clone(),
            abi: self.abi.clone(),
            version: Some(manifest.rootfs_version.clone()),
            managed_root: Some(self.managed_root.clone()),
            active_root: active_present.then_some(active_root),
            staged_root: staged_present.then_some(staged_root),
            archive_sha256: Some(manifest.archive.sha256.clone()),
            installed_size_bytes: dir_size(self.active_root()).ok(),
            writable_guest_paths: manifest.writable_paths.clone(),
            last_error,
        }
    }

    pub fn verify_active_root(
        &self,
        manifest: &RootfsManifest,
    ) -> Result<RootfsVerificationReport, RootfsStoreError> {
        self.verify_root_at(&self.active_root(), manifest)
    }

    fn verify_root_at(
        &self,
        root: &Path,
        manifest: &RootfsManifest,
    ) -> Result<RootfsVerificationReport, RootfsStoreError> {
        self.validate_manifest_target(manifest)?;
        let active_root = root.to_path_buf();
        let active_metadata = match fs::symlink_metadata(&active_root) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RootfsVerificationReport {
                    ok: false,
                    verified_files: 0,
                    verified_bytes: 0,
                    issues: vec![RootfsVerificationIssue {
                        path: "/".to_string(),
                        reason: "active rootfs is missing".to_string(),
                    }],
                });
            }
            Err(err) => {
                return Err(RootfsStoreError::Io {
                    path: active_root.clone(),
                    message: err.to_string(),
                });
            }
        };
        if active_metadata.file_type().is_symlink() || !active_metadata.is_dir() {
            return Ok(RootfsVerificationReport {
                ok: false,
                verified_files: 0,
                verified_bytes: 0,
                issues: vec![RootfsVerificationIssue {
                    path: "/".to_string(),
                    reason: "active rootfs must be a real directory, not a symlink".to_string(),
                }],
            });
        }

        let mut issues = Vec::new();
        let mut verified_files = 0usize;
        let mut verified_bytes = 0u64;
        for writable_path in &manifest.writable_paths {
            if let Some(reason) = verify_writable_root(&active_root, writable_path) {
                issues.push(RootfsVerificationIssue {
                    path: writable_path.clone(),
                    reason,
                });
            }
        }
        for entry in &manifest.immutable_files {
            match verify_immutable_entry(&active_root, entry)? {
                ImmutableEntryVerification::Verified(bytes) => {
                    verified_files += 1;
                    verified_bytes += bytes;
                }
                ImmutableEntryVerification::Failed(reason) => {
                    issues.push(RootfsVerificationIssue {
                        path: entry.path.clone(),
                        reason,
                    });
                }
            }
        }

        let expected_paths = manifest
            .immutable_files
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<BTreeSet<_>>();
        for actual_path in collect_immutable_rootfs_paths(&active_root, &manifest.writable_paths)? {
            if !expected_paths.contains(actual_path.as_str()) {
                issues.push(RootfsVerificationIssue {
                    path: actual_path,
                    reason: "file is absent from immutable inventory".to_string(),
                });
            }
        }

        Ok(RootfsVerificationReport {
            ok: issues.is_empty(),
            verified_files,
            verified_bytes,
            issues,
        })
    }

    pub fn stage_prepared_rootfs(
        &self,
        prepared_root: &Path,
        manifest: &RootfsManifest,
    ) -> Result<PathBuf, RootfsStoreError> {
        self.validate_manifest_target(manifest)?;
        self.ensure_managed_root()?;
        let prepared_metadata =
            fs::symlink_metadata(prepared_root).map_err(|err| RootfsStoreError::Io {
                path: prepared_root.to_path_buf(),
                message: err.to_string(),
            })?;
        if prepared_metadata.file_type().is_symlink() || !prepared_metadata.is_dir() {
            return Err(RootfsStoreError::UnsafeManagedPath(
                prepared_root.to_path_buf(),
            ));
        }
        let report = self.verify_root_at(prepared_root, manifest)?;
        if !report.ok {
            return Err(RootfsStoreError::Integrity(
                "prepared rootfs failed manifest verification".to_string(),
            ));
        }

        let staged_parent = self.managed_root.join("staged");
        fs::create_dir_all(&staged_parent).map_err(|err| RootfsStoreError::Io {
            path: staged_parent.clone(),
            message: err.to_string(),
        })?;
        ensure_real_directory(&staged_parent)?;
        let staged = self.staged_root(&manifest.rootfs_version);
        if fs::symlink_metadata(&staged).is_ok() {
            remove_path(staged.clone())?;
        }
        fs::rename(prepared_root, &staged).map_err(|err| RootfsStoreError::Io {
            path: staged.clone(),
            message: err.to_string(),
        })?;
        Ok(staged)
    }

    pub fn activate_staged_rootfs(
        &self,
        manifest: &RootfsManifest,
    ) -> Result<RootfsStatus, RootfsStoreError> {
        self.validate_manifest_target(manifest)?;
        self.ensure_managed_root()?;
        self.recover_interrupted_activation(manifest)?;

        let staged = self.staged_root(&manifest.rootfs_version);
        let report = self.verify_root_at(&staged, manifest)?;
        if !report.ok {
            return Err(RootfsStoreError::Integrity(
                "staged rootfs failed manifest verification".to_string(),
            ));
        }

        let active = self.active_root();
        let next = self.managed_root.join("active.next");
        let rollback = self.managed_root.join("active.rollback");
        if fs::symlink_metadata(&next).is_ok() {
            remove_path(next.clone())?;
        }
        fs::rename(&staged, &next).map_err(|err| RootfsStoreError::Io {
            path: next.clone(),
            message: err.to_string(),
        })?;
        if fs::symlink_metadata(&active).is_ok() {
            fs::rename(&active, &rollback).map_err(|err| RootfsStoreError::Io {
                path: rollback.clone(),
                message: err.to_string(),
            })?;
        }
        if let Err(err) = fs::rename(&next, &active) {
            if fs::symlink_metadata(&rollback).is_ok() && fs::symlink_metadata(&active).is_err() {
                let _ = fs::rename(&rollback, &active);
            }
            return Err(RootfsStoreError::Io {
                path: active,
                message: err.to_string(),
            });
        }
        if fs::symlink_metadata(&rollback).is_ok() {
            remove_path(rollback)?;
        }
        Ok(self.status(manifest))
    }

    pub fn recover_interrupted_activation(
        &self,
        manifest: &RootfsManifest,
    ) -> Result<RootfsStatus, RootfsStoreError> {
        self.validate_manifest_target(manifest)?;
        self.ensure_managed_root()?;
        let active = self.active_root();
        let next = self.managed_root.join("active.next");
        let rollback = self.managed_root.join("active.rollback");

        if fs::symlink_metadata(&active).is_err() {
            if fs::symlink_metadata(&next).is_ok() && self.verify_root_at(&next, manifest)?.ok {
                fs::rename(&next, &active).map_err(|err| RootfsStoreError::Io {
                    path: active.clone(),
                    message: err.to_string(),
                })?;
            } else if fs::symlink_metadata(&rollback).is_ok()
                && self.verify_root_at(&rollback, manifest)?.ok
            {
                fs::rename(&rollback, &active).map_err(|err| RootfsStoreError::Io {
                    path: active.clone(),
                    message: err.to_string(),
                })?;
            }
        }

        if fs::symlink_metadata(&active).is_ok() {
            if self.verify_root_at(&active, manifest)?.ok {
                for stale in [&next, &rollback] {
                    if fs::symlink_metadata(stale).is_ok() {
                        remove_path(stale.clone())?;
                    }
                }
            } else if fs::symlink_metadata(&rollback).is_ok()
                && self.verify_root_at(&rollback, manifest)?.ok
            {
                remove_path(active.clone())?;
                fs::rename(&rollback, &active).map_err(|err| RootfsStoreError::Io {
                    path: active.clone(),
                    message: err.to_string(),
                })?;
            }
        }
        Ok(self.status(manifest))
    }

    fn ensure_managed_root(&self) -> Result<(), RootfsStoreError> {
        fs::create_dir_all(&self.managed_root).map_err(|err| RootfsStoreError::Io {
            path: self.managed_root.clone(),
            message: err.to_string(),
        })?;
        ensure_real_directory(&self.managed_root)
    }

    fn validate_manifest_target(&self, manifest: &RootfsManifest) -> Result<(), RootfsStoreError> {
        manifest.validate()?;
        let expected_runtime = match self.backend {
            SandboxBackend::AndroidProot => "android-proot",
            SandboxBackend::IosIsh => "ios-ish",
            _ => {
                return Err(RootfsStoreError::TargetMismatch {
                    store: format!("{:?}/{}/{}", self.backend, self.platform, self.abi),
                    manifest: format!(
                        "{}/{}/{}",
                        manifest.runtime, manifest.platform, manifest.abi
                    ),
                });
            }
        };
        if manifest.runtime != expected_runtime
            || manifest.platform != self.platform
            || !abis_match(&manifest.abi, &self.abi)
        {
            return Err(RootfsStoreError::TargetMismatch {
                store: format!("{expected_runtime}/{}/{}", self.platform, self.abi),
                manifest: format!(
                    "{}/{}/{}",
                    manifest.runtime, manifest.platform, manifest.abi
                ),
            });
        }
        Ok(())
    }

    pub fn authorize_manifest_file(
        &self,
        guest_path: &str,
        manifest: &RootfsManifest,
    ) -> Result<PathBuf, RootfsStoreError> {
        self.validate_manifest_target(manifest)?;
        validate_guest_path(guest_path)?;
        if ALLOWED_WRITABLE_GUEST_PATHS
            .iter()
            .any(|writable| path_is_within_guest_path(guest_path, writable))
        {
            return Err(RootfsStoreError::ExecutionDenied {
                path: guest_path.to_string(),
                reason: "execution and dynamic loading from writable paths are forbidden"
                    .to_string(),
            });
        }
        let Some(entry) = manifest
            .executable_allowlist
            .iter()
            .find(|entry| entry.path == guest_path)
        else {
            return Err(RootfsStoreError::ExecutionDenied {
                path: guest_path.to_string(),
                reason: "path is absent from the rootfs manifest allowlist".to_string(),
            });
        };
        let host_path = secure_guest_host_path_with_real_parents(&self.active_root(), guest_path)
            .map_err(|error| RootfsStoreError::ExecutionDenied {
            path: guest_path.to_string(),
            reason: error.to_string(),
        })?;
        let metadata = fs::symlink_metadata(&host_path).map_err(|err| RootfsStoreError::Io {
            path: host_path.clone(),
            message: err.to_string(),
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(RootfsStoreError::ExecutionDenied {
                path: guest_path.to_string(),
                reason: "allowlisted path is not a regular non-symlink file".to_string(),
            });
        }
        if entry
            .size_bytes
            .is_some_and(|expected| expected != metadata.len())
        {
            return Err(RootfsStoreError::ExecutionDenied {
                path: guest_path.to_string(),
                reason: "allowlisted file size no longer matches".to_string(),
            });
        }
        let actual = sha256_file(&host_path)?;
        if !actual.eq_ignore_ascii_case(&entry.sha256) {
            return Err(RootfsStoreError::ExecutionDenied {
                path: guest_path.to_string(),
                reason: "allowlisted file hash no longer matches".to_string(),
            });
        }
        Ok(host_path)
    }

    pub fn reset_writable_state(&self, manifest: &RootfsManifest) -> Result<(), RootfsStoreError> {
        self.validate_manifest_target(manifest)?;
        let active_root = self.active_root();
        let active_metadata =
            fs::symlink_metadata(&active_root).map_err(|err| RootfsStoreError::Io {
                path: active_root.clone(),
                message: err.to_string(),
            })?;
        if active_metadata.file_type().is_symlink() || !active_metadata.is_dir() {
            return Err(RootfsStoreError::UnsafeResetPath(active_root));
        }
        for guest_path in &manifest.writable_paths {
            // `/root` can contain user SSH material and `/workspace` contains
            // projects. Rootfs reset may clear only runtime scratch paths.
            if !RESETTABLE_GUEST_PATHS.contains(&guest_path.as_str()) {
                continue;
            }
            let target = secure_guest_host_path_with_real_parents(&active_root, guest_path)
                .map_err(|error| match error {
                    RootfsStoreError::UnsafeManagedPath(path) => {
                        RootfsStoreError::UnsafeResetPath(path)
                    }
                    other => other,
                })?;
            if target == active_root {
                return Err(RootfsStoreError::UnsafeResetPath(target));
            }
            let metadata = match fs::symlink_metadata(&target) {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(RootfsStoreError::Io {
                        path: target.clone(),
                        message: err.to_string(),
                    });
                }
            };
            if metadata.file_type().is_symlink() {
                fs::remove_file(&target).map_err(|err| RootfsStoreError::Io {
                    path: target.clone(),
                    message: err.to_string(),
                })?;
                continue;
            }
            if metadata.is_dir() {
                for entry in fs::read_dir(&target).map_err(|err| RootfsStoreError::Io {
                    path: target.clone(),
                    message: err.to_string(),
                })? {
                    let entry = entry.map_err(|err| RootfsStoreError::Io {
                        path: target.clone(),
                        message: err.to_string(),
                    })?;
                    remove_path(entry.path())?;
                }
            } else {
                fs::remove_file(&target).map_err(|err| RootfsStoreError::Io {
                    path: target.clone(),
                    message: err.to_string(),
                })?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum RootfsManifestError {
    #[error("rootfs manifest schema_version must be exactly 2")]
    InvalidSchemaVersion,
    #[error("rootfs manifest is missing a required field")]
    MissingRequiredField,
    #[error("rootfs version and archive filename must be single safe path components")]
    UnsafeArtifactName,
    #[error("invalid or duplicate rootfs package: {0}")]
    InvalidPackage(String),
    #[error("required fixed-toolset package is missing: {0}")]
    MissingRequiredPackage(String),
    #[error("unsupported mobile Linux target: runtime={runtime}, platform={platform}, abi={abi}")]
    UnsupportedTarget {
        runtime: String,
        platform: String,
        abi: String,
    },
    #[error("invalid sha256: {0}")]
    InvalidSha256(String),
    #[error("duplicate manifest path: {0}")]
    DuplicatePath(String),
    #[error("invalid guest path: {0}")]
    InvalidGuestPath(String),
    #[error("writable guest path is not allowed: {0}")]
    WritablePathNotAllowed(String),
    #[error("required writable guest path is missing: {0}")]
    MissingWritablePath(String),
    #[error("verified executable or dynamic library lives in a writable guest path: {0}")]
    ExecutableInWritablePath(String),
    #[error("immutable rootfs file lives in a writable guest path: {0}")]
    ImmutableFileInWritablePath(String),
    #[error("executable allowlist entry does not match immutable inventory: {0}")]
    ExecutableInventoryMismatch(String),
}

#[derive(Debug, Error)]
pub enum RootfsStoreError {
    #[error(transparent)]
    Manifest(#[from] RootfsManifestError),
    #[error("io error for {path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("archive size mismatch: expected {expected}, got {actual}")]
    ArchiveSizeMismatch { expected: u64, actual: u64 },
    #[error("archive hash mismatch: expected {expected}, got {actual}")]
    ArchiveHashMismatch { expected: String, actual: String },
    #[error("rootfs manifest target {manifest} does not match store target {store}")]
    TargetMismatch { store: String, manifest: String },
    #[error("unsafe reset path: {0}")]
    UnsafeResetPath(PathBuf),
    #[error("unsafe managed path or symlink component: {0}")]
    UnsafeManagedPath(PathBuf),
    #[error("rootfs integrity failure: {0}")]
    Integrity(String),
    #[error("execution denied for {path}: {reason}")]
    ExecutionDenied { path: String, reason: String },
}

fn validate_guest_path(path: &str) -> Result<(), RootfsManifestError> {
    if !path.starts_with('/')
        || path == "/"
        || path.ends_with('/')
        || path.contains("//")
        || path.as_bytes().contains(&0)
    {
        return Err(RootfsManifestError::InvalidGuestPath(path.to_string()));
    }
    let parsed = Path::new(path);
    if parsed
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(RootfsManifestError::InvalidGuestPath(path.to_string()));
    }
    Ok(())
}

fn is_single_safe_component(value: &str) -> bool {
    let mut components = Path::new(value).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn path_is_within_guest_path(path: &str, candidate_parent: &str) -> bool {
    let Ok(path) = normalized_guest_path(path) else {
        return false;
    };
    let Ok(parent) = normalized_guest_path(candidate_parent) else {
        return false;
    };
    path == parent || path.starts_with(&(parent.clone() + "/"))
}

fn normalized_guest_path(path: &str) -> Result<String, RootfsManifestError> {
    validate_guest_path(path)?;
    let mut normalized = PathBuf::from("/");
    for component in Path::new(path).components() {
        match component {
            Component::RootDir => {}
            Component::Normal(segment) => normalized.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(RootfsManifestError::InvalidGuestPath(path.to_string()))
            }
        }
    }
    Ok(normalized.to_string_lossy().into_owned())
}

fn secure_guest_host_path(active_root: &Path, guest_path: &str) -> Result<PathBuf, PathBuf> {
    let normalized = normalized_guest_path(guest_path).map_err(|_| active_root.to_path_buf())?;
    let mut host_path = active_root.to_path_buf();
    for component in Path::new(&normalized).components() {
        if let Component::Normal(segment) = component {
            host_path.push(segment);
        }
    }
    Ok(host_path)
}

fn secure_guest_host_path_with_real_parents(
    active_root: &Path,
    guest_path: &str,
) -> Result<PathBuf, RootfsStoreError> {
    let host_path = secure_guest_host_path(active_root, guest_path)
        .map_err(RootfsStoreError::UnsafeManagedPath)?;
    ensure_real_directory(active_root)?;

    let relative = host_path
        .strip_prefix(active_root)
        .map_err(|_| RootfsStoreError::UnsafeManagedPath(host_path.clone()))?;
    let mut parent = active_root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(segment) = component else {
            return Err(RootfsStoreError::UnsafeManagedPath(host_path));
        };
        if components.peek().is_none() {
            break;
        }
        parent.push(segment);
        match fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(RootfsStoreError::UnsafeManagedPath(parent));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(err) => {
                return Err(RootfsStoreError::Io {
                    path: parent,
                    message: err.to_string(),
                });
            }
        }
    }
    Ok(host_path)
}

fn ensure_real_directory(path: &Path) -> Result<(), RootfsStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|err| RootfsStoreError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RootfsStoreError::UnsafeManagedPath(path.to_path_buf()));
    }
    Ok(())
}

fn path_present(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn abis_match(manifest_abi: &str, store_abi: &str) -> bool {
    fn normalize(abi: &str) -> Option<&'static str> {
        match abi {
            "arm64" | "arm64-v8a" | "aarch64" => Some("arm64"),
            "x86_64" => Some("x86_64"),
            _ => None,
        }
    }
    normalize(manifest_abi)
        .zip(normalize(store_abi))
        .is_some_and(|(manifest, store)| manifest == store)
}

fn verification_failure_message(report: &RootfsVerificationReport) -> String {
    let mut reasons = report
        .issues
        .iter()
        .take(3)
        .map(|issue| format!("{}: {}", issue.path, issue.reason))
        .collect::<Vec<_>>();
    if report.issues.len() > reasons.len() {
        reasons.push(format!(
            "{} additional issue(s)",
            report.issues.len() - reasons.len()
        ));
    }
    format!("rootfs verification failed: {}", reasons.join("; "))
}

enum ImmutableEntryVerification {
    Verified(u64),
    Failed(String),
}

fn verify_writable_root(root: &Path, guest_path: &str) -> Option<String> {
    let host_path = match secure_guest_host_path_with_real_parents(root, guest_path) {
        Ok(path) => path,
        Err(RootfsStoreError::UnsafeManagedPath(path)) => {
            return Some(format!(
                "writable root has a symlinked or invalid ancestor ({})",
                path.display()
            ));
        }
        Err(error) => return Some(error.to_string()),
    };
    match fs::symlink_metadata(&host_path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => None,
        Ok(_) => Some("writable root must be a real directory, not a symlink or file".to_string()),
        Err(error) => Some(format!("writable root is missing: {error}")),
    }
}

fn verify_immutable_entry(
    root: &Path,
    entry: &RootfsImmutableEntry,
) -> Result<ImmutableEntryVerification, RootfsStoreError> {
    let host_path = match secure_guest_host_path_with_real_parents(root, &entry.path) {
        Ok(path) => path,
        Err(RootfsStoreError::UnsafeManagedPath(path)) => {
            return Ok(ImmutableEntryVerification::Failed(format!(
                "symlinked or invalid ancestor blocks verification ({})",
                path.display()
            )));
        }
        Err(error) => return Ok(ImmutableEntryVerification::Failed(error.to_string())),
    };
    let metadata = match fs::symlink_metadata(&host_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Ok(ImmutableEntryVerification::Failed(format!(
                "missing: {error}"
            )));
        }
    };
    match entry.kind {
        RootfsImmutableKind::RegularFile => verify_immutable_regular(&host_path, &metadata, entry),
        RootfsImmutableKind::Symlink => verify_immutable_symlink(&host_path, &metadata, entry),
    }
}

fn verify_immutable_regular(
    host_path: &Path,
    metadata: &fs::Metadata,
    entry: &RootfsImmutableEntry,
) -> Result<ImmutableEntryVerification, RootfsStoreError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(ImmutableEntryVerification::Failed(
            "immutable regular-file path is a symlink or has the wrong file type".to_string(),
        ));
    }
    if metadata.len() != entry.size_bytes {
        return Ok(ImmutableEntryVerification::Failed(format!(
            "size mismatch (expected {}, got {})",
            entry.size_bytes,
            metadata.len()
        )));
    }
    let actual = sha256_file(host_path)?;
    if actual != entry.sha256 {
        return Ok(ImmutableEntryVerification::Failed(format!(
            "hash mismatch (expected {}, got {})",
            entry.sha256, actual
        )));
    }
    Ok(ImmutableEntryVerification::Verified(entry.size_bytes))
}

fn verify_immutable_symlink(
    host_path: &Path,
    metadata: &fs::Metadata,
    entry: &RootfsImmutableEntry,
) -> Result<ImmutableEntryVerification, RootfsStoreError> {
    if !metadata.file_type().is_symlink() {
        return Ok(ImmutableEntryVerification::Failed(
            "immutable symlink path has the wrong file type".to_string(),
        ));
    }
    let target = fs::read_link(host_path).map_err(|err| RootfsStoreError::Io {
        path: host_path.to_path_buf(),
        message: err.to_string(),
    })?;
    if !symlink_target_stays_within_root(&entry.path, &target) {
        return Ok(ImmutableEntryVerification::Failed(format!(
            "symlink target escapes the rootfs: {}",
            target.display()
        )));
    }
    let target_bytes = target.as_os_str().as_encoded_bytes();
    if target_bytes.len() as u64 != entry.size_bytes {
        return Ok(ImmutableEntryVerification::Failed(format!(
            "symlink target size mismatch (expected {}, got {})",
            entry.size_bytes,
            target_bytes.len()
        )));
    }
    let actual = sha256_bytes(target_bytes);
    if actual != entry.sha256 {
        return Ok(ImmutableEntryVerification::Failed(format!(
            "hash mismatch (expected {}, got {})",
            entry.sha256, actual
        )));
    }
    Ok(ImmutableEntryVerification::Verified(entry.size_bytes))
}

fn symlink_target_stays_within_root(guest_path: &str, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut depth = Path::new(guest_path)
        .parent()
        .map_or(0, |parent| parent.components().count().saturating_sub(1));
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

fn collect_immutable_rootfs_paths(
    root: &Path,
    writable_paths: &[String],
) -> Result<Vec<String>, RootfsStoreError> {
    fn visit(
        root: &Path,
        directory: &Path,
        writable_paths: &[String],
        paths: &mut Vec<String>,
    ) -> Result<(), RootfsStoreError> {
        for entry in fs::read_dir(directory).map_err(|err| RootfsStoreError::Io {
            path: directory.to_path_buf(),
            message: err.to_string(),
        })? {
            let entry = entry.map_err(|err| RootfsStoreError::Io {
                path: directory.to_path_buf(),
                message: err.to_string(),
            })?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| RootfsStoreError::UnsafeManagedPath(path.clone()))?;
            let guest_path = format!(
                "/{}",
                relative
                    .components()
                    .filter_map(|component| match component {
                        Component::Normal(segment) => Some(segment.to_string_lossy()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("/")
            );
            if writable_paths
                .iter()
                .any(|writable| path_is_within_guest_path(&guest_path, writable))
            {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(|err| RootfsStoreError::Io {
                path: path.clone(),
                message: err.to_string(),
            })?;
            if metadata.is_dir() {
                visit(root, &path, writable_paths, paths)?;
            } else {
                paths.push(guest_path);
            }
        }
        Ok(())
    }

    let mut paths = Vec::new();
    visit(root, root, writable_paths, &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn sha256_bytes(value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value);
    format!("{:x}", hasher.finalize())
}

fn sha256_file(path: &Path) -> Result<String, RootfsStoreError> {
    let mut file = fs::File::open(path).map_err(|err| RootfsStoreError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 8192];
    loop {
        let read = file.read(&mut buf).map_err(|err| RootfsStoreError::Io {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn dir_size(path: PathBuf) -> Result<u64, RootfsStoreError> {
    if !path.exists() {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(&path).map_err(|err| RootfsStoreError::Io {
        path: path.clone(),
        message: err.to_string(),
    })?;
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return fs::metadata(&path)
            .map(|metadata| metadata.len())
            .map_err(|err| RootfsStoreError::Io {
                path,
                message: err.to_string(),
            });
    }
    let mut total = 0;
    for entry in fs::read_dir(&path).map_err(|err| RootfsStoreError::Io {
        path: path.clone(),
        message: err.to_string(),
    })? {
        let entry = entry.map_err(|err| RootfsStoreError::Io {
            path: path.clone(),
            message: err.to_string(),
        })?;
        total += dir_size(entry.path())?;
    }
    Ok(total)
}

fn remove_path(path: PathBuf) -> Result<(), RootfsStoreError> {
    let metadata = fs::symlink_metadata(&path).map_err(|err| RootfsStoreError::Io {
        path: path.clone(),
        message: err.to_string(),
    })?;
    if metadata.file_type().is_symlink() {
        fs::remove_file(&path).map_err(|err| RootfsStoreError::Io {
            path,
            message: err.to_string(),
        })
    } else if metadata.is_dir() {
        for entry in fs::read_dir(&path).map_err(|err| RootfsStoreError::Io {
            path: path.clone(),
            message: err.to_string(),
        })? {
            let entry = entry.map_err(|err| RootfsStoreError::Io {
                path: path.clone(),
                message: err.to_string(),
            })?;
            remove_path(entry.path())?;
        }
        fs::remove_dir(&path).map_err(|err| RootfsStoreError::Io {
            path,
            message: err.to_string(),
        })
    } else {
        fs::remove_file(&path).map_err(|err| RootfsStoreError::Io {
            path,
            message: err.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::{
        LinuxCommandResult, LinuxEnforcementReceipt, MobileLinuxCapability,
        MobileLinuxTaskSnapshot, MobileLinuxTaskStatus, PtyOpenRequest, PtySessionHandle,
        ResourceLimits,
    };

    fn manifest() -> RootfsManifest {
        RootfsManifest {
            schema_version: 2,
            runtime: "android-proot".to_string(),
            platform: "android".to_string(),
            abi: "arm64".to_string(),
            rootfs_version: "1.0.0".to_string(),
            archive: RootfsArchive {
                filename: "rootfs.tar.zst".to_string(),
                sha256: "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
                    .to_string(),
                size_bytes: 1,
            },
            packages: REQUIRED_ROOTFS_PACKAGES
                .iter()
                .map(|name| RootfsPackage {
                    name: (*name).to_string(),
                    version: "1.0-r0".to_string(),
                    license: "MIT".to_string(),
                    architecture: "arm64".to_string(),
                    origin: (*name).to_string(),
                })
                .collect(),
            executable_allowlist: vec![RootfsManifestEntry {
                path: "/usr/bin/python3".to_string(),
                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
                kind: RootfsEntryKind::Interpreter,
                size_bytes: Some(0),
            }],
            immutable_files: vec![RootfsImmutableEntry {
                path: "/usr/bin/python3".to_string(),
                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
                kind: RootfsImmutableKind::RegularFile,
                size_bytes: 0,
            }],
            writable_paths: ALLOWED_WRITABLE_GUEST_PATHS
                .iter()
                .map(|path| (*path).to_string())
                .collect(),
        }
    }

    fn write_valid_root(path: &Path) {
        let executable = path.join("usr/bin/python3");
        fs::create_dir_all(executable.parent().expect("parent")).expect("rootfs dirs");
        fs::write(executable, b"").expect("rootfs executable");
        for writable in ALLOWED_WRITABLE_GUEST_PATHS {
            fs::create_dir_all(path.join(writable.trim_start_matches('/')))
                .expect("writable rootfs dir");
        }
    }

    #[test]
    fn manifest_rejects_duplicate_paths() {
        let mut manifest = manifest();
        manifest.executable_allowlist.push(RootfsManifestEntry {
            path: "/usr/bin/python3".to_string(),
            sha256: manifest.executable_allowlist[0].sha256.clone(),
            kind: RootfsEntryKind::SharedLibrary,
            size_bytes: Some(0),
        });
        let err = manifest.validate().expect_err("duplicate path");
        assert!(matches!(err, RootfsManifestError::DuplicatePath(_)));
    }

    #[test]
    fn manifest_rejects_unknown_schema_versions() {
        let mut manifest = manifest();
        manifest.schema_version = 3;
        assert!(matches!(
            manifest.validate(),
            Err(RootfsManifestError::InvalidSchemaVersion)
        ));

        manifest.schema_version = 1;
        assert!(matches!(
            manifest.validate(),
            Err(RootfsManifestError::InvalidSchemaVersion)
        ));
    }

    #[test]
    fn manifest_requires_the_fixed_toolset_packages() {
        let mut manifest = manifest();
        manifest.packages.retain(|package| package.name != "git");
        assert!(matches!(
            manifest.validate(),
            Err(RootfsManifestError::MissingRequiredPackage(package)) if package == "git"
        ));
    }

    #[test]
    fn manifest_deserialization_rejects_unknown_fields() {
        let mut value = serde_json::to_value(manifest()).expect("serialize manifest");
        value
            .as_object_mut()
            .expect("manifest object")
            .insert("future_policy".to_string(), serde_json::json!(true));
        assert!(serde_json::from_value::<RootfsManifest>(value).is_err());
    }

    #[test]
    fn manifest_rejects_unapproved_writable_paths() {
        let mut manifest = manifest();
        manifest.writable_paths = vec!["/usr/local".to_string()];
        let err = manifest
            .validate()
            .expect_err("writable path must be rejected");
        assert!(matches!(
            err,
            RootfsManifestError::WritablePathNotAllowed(path) if path == "/usr/local"
        ));
    }

    #[test]
    fn manifest_requires_the_complete_fixed_writable_path_set() {
        let mut manifest = manifest();
        manifest.writable_paths.retain(|path| path != "/workspace");
        let err = manifest
            .validate()
            .expect_err("missing fixed writable path must be rejected");
        assert!(matches!(
            err,
            RootfsManifestError::MissingWritablePath(path) if path == "/workspace"
        ));
    }

    #[test]
    fn manifest_rejects_non_lowercase_hashes() {
        let mut manifest = manifest();
        manifest.archive.sha256 =
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string();
        assert!(matches!(
            manifest.validate(),
            Err(RootfsManifestError::InvalidSha256(_))
        ));
    }

    #[test]
    fn manifest_rejects_executables_in_writable_paths() {
        let mut manifest = manifest();
        manifest.executable_allowlist[0].path = "/workspace/bin/python3".to_string();
        let err = manifest
            .validate()
            .expect_err("writable executable must be rejected");
        assert!(matches!(
            err,
            RootfsManifestError::ExecutableInWritablePath(path)
                if path == "/workspace/bin/python3"
        ));
    }

    #[test]
    fn manifest_rejects_path_like_versions_and_archive_names() {
        let mut invalid_version = manifest();
        invalid_version.rootfs_version = "../outside".to_string();
        assert!(matches!(
            invalid_version.validate(),
            Err(RootfsManifestError::UnsafeArtifactName)
        ));

        let mut invalid_archive = manifest();
        invalid_archive.archive.filename = "nested/rootfs.tar.zst".to_string();
        assert!(matches!(
            invalid_archive.validate(),
            Err(RootfsManifestError::UnsafeArtifactName)
        ));
    }

    #[test]
    fn verify_archive_hash_and_size() {
        let temp = tempfile::tempdir().expect("tempdir");
        let archive = temp.path().join("rootfs.tar.zst");
        fs::write(&archive, b"x").expect("write archive");
        let store = RootfsStore::new(
            temp.path().join("managed"),
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );
        store
            .verify_archive(&archive, &manifest())
            .expect("archive ok");
    }

    #[test]
    fn store_rejects_manifest_for_a_different_runtime_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let archive = temp.path().join("rootfs.tar.zst");
        fs::write(&archive, b"x").expect("write archive");
        let store = RootfsStore::new(
            temp.path().join("managed"),
            SandboxBackend::IosIsh,
            MobileLinuxRuntimeMode::MobileLinux,
            "ios",
            "arm64",
        );

        let err = store
            .verify_archive(&archive, &manifest())
            .expect_err("Android manifest must not be accepted by the iOS store");
        assert!(err.to_string().contains("does not match"));
    }

    #[cfg(unix)]
    #[test]
    fn verify_archive_rejects_symlinked_assets() {
        use std::os::unix::fs as unix_fs;

        let temp = tempfile::tempdir().expect("tempdir");
        let real_archive = temp.path().join("real-rootfs.tar.zst");
        let archive_link = temp.path().join("rootfs.tar.zst");
        fs::write(&real_archive, b"x").expect("write archive");
        unix_fs::symlink(&real_archive, &archive_link).expect("archive symlink");
        let store = RootfsStore::new(
            temp.path().join("managed"),
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );

        assert!(matches!(
            store.verify_archive(&archive_link, &manifest()),
            Err(RootfsStoreError::UnsafeManagedPath(path)) if path == archive_link
        ));
    }

    #[test]
    fn verify_active_root_reports_missing_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = RootfsStore::new(
            temp.path().join("managed"),
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );
        let report = store
            .verify_active_root(&manifest())
            .expect("verify report");
        assert!(!report.ok);
        assert_eq!(report.issues.len(), 1);
    }

    #[test]
    fn status_is_ready_only_after_integrity_verification() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = RootfsStore::new(
            temp.path().join("managed"),
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );
        fs::create_dir_all(store.active_root()).expect("active root");
        let corrupt = store.status(&manifest());
        assert_eq!(corrupt.state, RootfsState::Corrupt);
        assert!(
            corrupt.last_error.is_some(),
            "corrupt status must surface the integrity failure"
        );

        let executable = store.active_root().join("usr/bin/python3");
        write_valid_root(&store.active_root());
        assert_eq!(store.status(&manifest()).state, RootfsState::Ready);
        assert_eq!(
            store
                .authorize_manifest_file("/usr/bin/python3", &manifest())
                .expect("allowlisted executable"),
            executable
        );
    }

    #[test]
    fn verify_active_root_detects_non_executable_tampering_and_unlisted_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = RootfsStore::new(
            temp.path().join("managed"),
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );
        write_valid_root(&store.active_root());
        let stdlib = store.active_root().join("usr/lib/python3.12/site.py");
        fs::create_dir_all(stdlib.parent().expect("stdlib parent")).expect("stdlib dirs");
        fs::write(&stdlib, b"trusted").expect("stdlib fixture");

        let mut expected = manifest();
        expected.immutable_files.push(RootfsImmutableEntry {
            path: "/usr/lib/python3.12/site.py".to_string(),
            sha256: "a9a089195c68d2adeee23beaa2c3a93b1d4cdf09046e7a9e520b3b166dff3e6a".to_string(),
            kind: RootfsImmutableKind::RegularFile,
            size_bytes: 7,
        });
        assert!(
            store
                .verify_active_root(&expected)
                .expect("trusted root")
                .ok
        );

        fs::write(&stdlib, b"hostile").expect("tamper stdlib");
        let tampered = store
            .verify_active_root(&expected)
            .expect("tampered report");
        assert!(!tampered.ok);
        assert!(tampered
            .issues
            .iter()
            .any(|issue| issue.path == "/usr/lib/python3.12/site.py"
                && issue.reason.contains("hash mismatch")));

        fs::write(&stdlib, b"trusted").expect("restore stdlib");
        fs::write(
            store.active_root().join("usr/lib/python3.12/injected.py"),
            b"payload",
        )
        .expect("unlisted stdlib file");
        let unlisted = store
            .verify_active_root(&expected)
            .expect("unlisted report");
        assert!(!unlisted.ok);
        assert!(unlisted
            .issues
            .iter()
            .any(|issue| issue.path == "/usr/lib/python3.12/injected.py"
                && issue.reason.contains("absent from immutable inventory")));
    }

    #[test]
    fn staged_activation_and_interruption_recovery_are_bounded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let managed = temp.path().join("managed");
        let store = RootfsStore::new(
            managed.clone(),
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );
        let prepared = temp.path().join("prepared");
        write_valid_root(&prepared);
        store
            .stage_prepared_rootfs(&prepared, &manifest())
            .expect("stage");
        let status = store.activate_staged_rootfs(&manifest()).expect("activate");
        assert_eq!(status.state, RootfsState::Ready);

        fs::rename(store.active_root(), managed.join("active.rollback"))
            .expect("simulate interrupted swap");
        let recovered = store
            .recover_interrupted_activation(&manifest())
            .expect("recover");
        assert_eq!(recovered.state, RootfsState::Ready);
        assert!(store.active_root().is_dir());
        assert!(!managed.join("active.rollback").exists());
    }

    #[cfg(unix)]
    #[test]
    fn staging_rejects_a_symlinked_staged_directory() {
        use std::os::unix::fs as unix_fs;

        let temp = tempfile::tempdir().expect("tempdir");
        let managed = temp.path().join("managed");
        let outside = temp.path().join("outside");
        let prepared = temp.path().join("prepared");
        fs::create_dir_all(&managed).expect("managed");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("keep.txt"), b"keep").expect("outside data");
        unix_fs::symlink(&outside, managed.join("staged")).expect("staged symlink");
        write_valid_root(&prepared);
        let store = RootfsStore::new(
            managed,
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );

        assert!(matches!(
            store.stage_prepared_rootfs(&prepared, &manifest()),
            Err(RootfsStoreError::UnsafeManagedPath(_))
        ));
        assert_eq!(
            fs::read(outside.join("keep.txt")).expect("outside preserved"),
            b"keep"
        );
    }

    #[test]
    fn recovery_never_promotes_an_invalid_rollback() {
        let temp = tempfile::tempdir().expect("tempdir");
        let managed = temp.path().join("managed");
        fs::create_dir_all(managed.join("active.rollback")).expect("invalid rollback");
        let store = RootfsStore::new(
            managed,
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );

        let status = store
            .recover_interrupted_activation(&manifest())
            .expect("recovery status");
        assert_eq!(status.state, RootfsState::Missing);
        assert!(!path_present(&store.active_root()));
    }

    #[test]
    fn reset_clears_only_scratch_and_preserves_projects_and_keys() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = RootfsStore::new(
            temp.path().join("managed"),
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );
        write_valid_root(&store.active_root());
        fs::create_dir_all(store.active_root().join("tmp")).expect("tmp");
        fs::create_dir_all(store.active_root().join("root/.ssh")).expect("ssh");
        fs::create_dir_all(store.active_root().join("workspace/project")).expect("workspace");
        fs::write(store.active_root().join("tmp/delete.txt"), b"temp").expect("temp data");
        fs::write(store.active_root().join("root/.ssh/id_ed25519"), b"key").expect("key");
        fs::write(
            store.active_root().join("workspace/project/keep.txt"),
            b"project",
        )
        .expect("project data");

        store
            .reset_writable_state(&manifest())
            .expect("scratch reset");
        assert!(!store.active_root().join("tmp/delete.txt").exists());
        assert!(store.active_root().join("root/.ssh/id_ed25519").exists());
        assert!(store
            .active_root()
            .join("workspace/project/keep.txt")
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn verify_active_root_rejects_symlinked_executables() {
        use std::os::unix::fs as unix_fs;

        let temp = tempfile::tempdir().expect("tempdir");
        let store = RootfsStore::new(
            temp.path().join("managed"),
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );
        let active = store.active_root().join("usr/bin");
        fs::create_dir_all(&active).expect("active tree");
        let outside = temp.path().join("outside-python3");
        fs::write(&outside, b"payload").expect("outside file");
        unix_fs::symlink(&outside, active.join("python3")).expect("symlink");

        let mut manifest = manifest();
        manifest.executable_allowlist[0].size_bytes = Some(7);
        manifest.executable_allowlist[0].sha256 =
            "239f59ed55e737c77147cf55ad0c1b03b8c4fa0193f6c82b53f6ba356b4a8044".to_string();
        manifest.immutable_files[0].size_bytes = 7;
        manifest.immutable_files[0].sha256 =
            "239f59ed55e737c77147cf55ad0c1b03b8c4fa0193f6c82b53f6ba356b4a8044".to_string();
        let report = store.verify_active_root(&manifest).expect("report");
        assert!(!report.ok);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.reason.contains("symlink")));
    }

    #[cfg(unix)]
    #[test]
    fn verify_and_authorize_reject_symlinked_parent_directories() {
        use std::os::unix::fs as unix_fs;

        let temp = tempfile::tempdir().expect("tempdir");
        let store = RootfsStore::new(
            temp.path().join("managed"),
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );
        let active_usr = store.active_root().join("usr");
        let outside_bin = temp.path().join("outside-bin");
        fs::create_dir_all(&active_usr).expect("active tree");
        fs::create_dir_all(&outside_bin).expect("outside bin");
        fs::write(outside_bin.join("python3"), b"").expect("outside executable");
        unix_fs::symlink(&outside_bin, active_usr.join("bin")).expect("parent symlink");

        let report = store.verify_active_root(&manifest()).expect("report");
        assert!(!report.ok);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.reason.contains("symlink")));
        assert!(matches!(
            store.authorize_manifest_file("/usr/bin/python3", &manifest()),
            Err(RootfsStoreError::ExecutionDenied { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn reset_rejects_symlinked_active_root() {
        use std::os::unix::fs as unix_fs;

        let temp = tempfile::tempdir().expect("tempdir");
        let managed = temp.path().join("managed");
        let outside = temp.path().join("outside");
        fs::create_dir_all(outside.join("root")).expect("outside");
        fs::write(outside.join("root/keep.txt"), b"keep").expect("outside data");
        fs::create_dir_all(&managed).expect("managed");
        unix_fs::symlink(&outside, managed.join("active")).expect("active symlink");
        let store = RootfsStore::new(
            managed,
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );

        assert!(matches!(
            store.reset_writable_state(&manifest()),
            Err(RootfsStoreError::UnsafeResetPath(_))
        ));
        assert_eq!(
            fs::read(outside.join("root/keep.txt")).expect("outside preserved"),
            b"keep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reset_rejects_symlinked_parent_directories() {
        use std::os::unix::fs as unix_fs;

        let temp = tempfile::tempdir().expect("tempdir");
        let store = RootfsStore::new(
            temp.path().join("managed"),
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
        );
        let outside_var = temp.path().join("outside-var");
        fs::create_dir_all(store.active_root()).expect("active root");
        fs::create_dir_all(outside_var.join("tmp")).expect("outside tmp");
        fs::write(outside_var.join("tmp/keep.txt"), b"keep").expect("outside data");
        unix_fs::symlink(&outside_var, store.active_root().join("var")).expect("parent symlink");

        assert!(matches!(
            store.reset_writable_state(&manifest()),
            Err(RootfsStoreError::UnsafeResetPath(_)) | Err(RootfsStoreError::UnsafeManagedPath(_))
        ));
        assert_eq!(
            fs::read(outside_var.join("tmp/keep.txt")).expect("outside preserved"),
            b"keep"
        );
    }

    #[derive(Default)]
    struct RecordingSink {
        stdout: Mutex<Vec<String>>,
        stderr: Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl ProcessStreamSink for RecordingSink {
        async fn stdout_line(&self, line: String) -> Result<(), ProcessError> {
            self.stdout.lock().expect("stdout mutex").push(line);
            Ok(())
        }

        async fn stderr_chunk(&self, chunk: Vec<u8>) -> Result<(), ProcessError> {
            self.stderr.lock().expect("stderr mutex").push(chunk);
            Ok(())
        }
    }

    struct MockRuntime {
        backend: SandboxBackend,
        mode: MobileLinuxRuntimeMode,
        capability: MobileLinuxCapability,
        last_run: Mutex<Vec<LinuxCommandRequest>>,
        last_background: Mutex<Vec<LinuxCommandRequest>>,
        killed: Mutex<Vec<String>>,
        kill_error: Mutex<Option<MobileLinuxError>>,
        task_statuses: Mutex<HashMap<String, MobileLinuxTaskStatus>>,
    }

    impl MockRuntime {
        fn new(backend: SandboxBackend) -> Self {
            Self {
                backend,
                mode: MobileLinuxRuntimeMode::MobileLinux,
                capability: MobileLinuxCapability {
                    available: true,
                    backend,
                    mode: MobileLinuxRuntimeMode::MobileLinux,
                    reason: None,
                    streaming_output: true,
                    background_processes: true,
                    pty: true,
                    bind_mounts: true,
                    rootfs_integrity: false,
                },
                last_run: Mutex::new(Vec::new()),
                last_background: Mutex::new(Vec::new()),
                killed: Mutex::new(Vec::new()),
                kill_error: Mutex::new(None),
                task_statuses: Mutex::new(HashMap::new()),
            }
        }

        fn set_task_status(&self, task_id: &str, status: MobileLinuxTaskStatus) {
            self.task_statuses
                .lock()
                .expect("task status mutex")
                .insert(task_id.to_string(), status);
        }
    }

    #[async_trait]
    impl MobileLinuxRuntime for MockRuntime {
        fn backend(&self) -> SandboxBackend {
            self.backend
        }

        fn mode(&self) -> MobileLinuxRuntimeMode {
            self.mode
        }

        async fn probe_capability(&self) -> MobileLinuxCapability {
            self.capability.clone()
        }

        async fn boot(&self) -> Result<RootfsStatus, MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }

        async fn shutdown(&self) -> Result<(), MobileLinuxError> {
            Ok(())
        }

        async fn run(
            &self,
            request: LinuxCommandRequest,
        ) -> Result<LinuxCommandResult, MobileLinuxError> {
            self.last_run.lock().expect("run mutex").push(request);
            Ok(LinuxCommandResult {
                stdout: "line-1\nline-2".to_string(),
                stderr: "err".to_string(),
                exit_code: 7,
                timed_out: false,
                cancelled: false,
                enforcement: LinuxEnforcementReceipt::default(),
            })
        }

        async fn run_streaming(
            &self,
            request: LinuxCommandRequest,
            sink: Arc<dyn ProcessStreamSink>,
        ) -> Result<LinuxCommandResult, MobileLinuxError> {
            self.last_run.lock().expect("run mutex").push(request);
            sink.stdout_line("stream-1".to_string())
                .await
                .expect("stdout");
            sink.stderr_chunk(b"stream-err".to_vec())
                .await
                .expect("stderr");
            Ok(LinuxCommandResult {
                stdout: "stream-1".to_string(),
                stderr: "stream-err".to_string(),
                exit_code: 0,
                timed_out: false,
                cancelled: false,
                enforcement: LinuxEnforcementReceipt::default(),
            })
        }

        async fn spawn_background(
            &self,
            request: LinuxCommandRequest,
        ) -> Result<LinuxProcessHandle, MobileLinuxError> {
            self.last_background
                .lock()
                .expect("background mutex")
                .push(request);
            self.set_task_status("bg-task", MobileLinuxTaskStatus::Backgrounded);
            Ok(LinuxProcessHandle {
                id: "bg-task".to_string(),
                enforcement: LinuxEnforcementReceipt::default(),
            })
        }

        async fn kill(&self, handle: &LinuxProcessHandle) -> Result<(), MobileLinuxError> {
            self.killed
                .lock()
                .expect("kill mutex")
                .push(handle.id.clone());
            if let Some(error) = self.kill_error.lock().expect("kill error mutex").take() {
                return Err(error);
            }
            Ok(())
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
            Ok(())
        }

        async fn task_status(
            &self,
            task_id: &str,
        ) -> Result<Option<MobileLinuxTaskSnapshot>, MobileLinuxError> {
            Ok(self
                .task_statuses
                .lock()
                .expect("task status mutex")
                .get(task_id)
                .copied()
                .map(|status| MobileLinuxTaskSnapshot {
                    task_id: task_id.to_string(),
                    status,
                    command: "test".to_string(),
                    started_at_ms: Some(1),
                    finished_at_ms: matches!(
                        status,
                        MobileLinuxTaskStatus::Completed
                            | MobileLinuxTaskStatus::Failed
                            | MobileLinuxTaskStatus::Cancelled
                            | MobileLinuxTaskStatus::TimedOut
                    )
                    .then_some(2),
                    exit_code: None,
                    detail: None,
                }))
        }
    }

    fn sample_mounts(temp: &tempfile::TempDir) -> Vec<MountSpec> {
        vec![
            MountSpec {
                host_path: temp.path().join("workspace"),
                guest_path: "/workspace/project".to_string(),
                read_only: false,
                purpose: platform_api::MountPurpose::Workspace,
            },
            MountSpec {
                host_path: temp.path().join("readonly"),
                guest_path: "/root/readonly".to_string(),
                read_only: true,
                purpose: platform_api::MountPurpose::Shared,
            },
        ]
    }

    fn sample_command(temp: &tempfile::TempDir) -> ProcessCommand {
        ProcessCommand {
            command: "/bin/sh".to_string(),
            args: vec!["-lc".to_string(), "pwd".to_string()],
            cwd: Some(temp.path().join("workspace")),
            env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
            timeout: Some(std::time::Duration::from_secs(3)),
            stdin: Some("echo hi".to_string()),
        }
    }

    fn sample_policy(temp: &tempfile::TempDir) -> SandboxPolicy {
        SandboxPolicy {
            network: platform_api::NetworkPolicy::Disabled,
            writable_paths: vec![temp.path().join("workspace")],
            denied_paths: vec![],
            allow_subprocess: true,
            limits: ResourceLimits::default(),
        }
    }

    #[test]
    fn mobile_linux_sandbox_maps_host_paths_into_guest_request() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("workspace")).expect("workspace");
        fs::create_dir_all(temp.path().join("readonly")).expect("readonly");
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let sandbox = MobileLinuxSandbox::new(runtime, sample_mounts(&temp)).expect("sandbox");

        let cmd = sample_command(&temp);
        let prepared = sandbox
            .prepare(cmd, &sample_policy(&temp))
            .expect("prepare should succeed");
        let plan = prepared
            .backend_plan()
            .expect("backend plan")
            .downcast::<MobileLinuxSandboxPlan>()
            .expect("plan type");

        assert_eq!(plan.request.command, "/bin/sh");
        assert_eq!(plan.request.cwd.as_deref(), Some("/workspace/project"));
        assert_eq!(plan.request.network, platform_api::NetworkPolicy::Disabled);
        assert_eq!(plan.request.mounts.len(), 2);
    }

    #[tokio::test]
    async fn mobile_linux_sandbox_probe_does_not_overclaim_enforcement() {
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let sandbox = MobileLinuxSandbox::new(runtime, Vec::new()).expect("sandbox");

        let capability = sandbox.probe_capability().await;
        assert!(capability.available);
        assert!(!capability.features.network_isolation);
        assert!(!capability.features.fs_readonly);
        assert!(!capability.features.no_new_privileges);
        assert!(capability.features.fs_readwrite_paths);
    }

    #[test]
    fn mobile_linux_sandbox_rejects_denied_paths_inside_mounts() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("workspace/private")).expect("workspace");
        fs::create_dir_all(temp.path().join("readonly")).expect("readonly");
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let sandbox = MobileLinuxSandbox::new(runtime, sample_mounts(&temp)).expect("sandbox");
        let mut policy = sample_policy(&temp);
        policy.denied_paths = vec![temp.path().join("workspace/private")];

        let error = sandbox
            .prepare(sample_command(&temp), &policy)
            .expect_err("deny path inside mounted tree is unsupported");
        assert!(matches!(error, SandboxError::Unsupported));
    }

    #[test]
    fn mobile_linux_sandbox_rejects_readonly_writable_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("workspace")).expect("workspace");
        fs::create_dir_all(temp.path().join("readonly")).expect("readonly");
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let sandbox = MobileLinuxSandbox::new(runtime, sample_mounts(&temp)).expect("sandbox");
        let mut policy = sample_policy(&temp);
        policy.writable_paths = vec![temp.path().join("readonly")];

        let error = sandbox
            .prepare(sample_command(&temp), &policy)
            .expect_err("readonly mounted path cannot be writable");
        assert!(matches!(error, SandboxError::Unavailable(_)));
    }

    #[test]
    fn mobile_linux_sandbox_rejects_unmounted_host_cwd() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("workspace")).expect("workspace");
        fs::create_dir_all(temp.path().join("readonly")).expect("readonly");
        fs::create_dir_all(temp.path().join("outside")).expect("outside");
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let sandbox = MobileLinuxSandbox::new(runtime, sample_mounts(&temp)).expect("sandbox");
        let mut cmd = sample_command(&temp);
        cmd.cwd = Some(temp.path().join("outside"));

        let error = sandbox
            .prepare(cmd, &sample_policy(&temp))
            .expect_err("unmounted host cwd must be rejected");
        assert!(matches!(error, SandboxError::Unavailable(_)));
    }

    #[test]
    fn mobile_linux_sandbox_rejects_sensitive_or_overlapping_mounts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));

        let sensitive = vec![MountSpec {
            host_path: temp.path().join("secrets"),
            guest_path: "/workspace/secrets".to_string(),
            read_only: true,
            purpose: platform_api::MountPurpose::Shared,
        }];
        let error = MobileLinuxSandbox::new(runtime.clone(), sensitive)
            .expect_err("sensitive path must be rejected");
        assert!(matches!(error, SandboxError::Unavailable(_)));

        fs::create_dir_all(temp.path().join("workspace")).expect("workspace");
        fs::create_dir_all(temp.path().join("workspace-sub")).expect("workspace-sub");
        let overlapping = vec![
            MountSpec {
                host_path: temp.path().join("workspace"),
                guest_path: "/workspace/project".to_string(),
                read_only: false,
                purpose: platform_api::MountPurpose::Workspace,
            },
            MountSpec {
                host_path: temp.path().join("workspace-sub"),
                guest_path: "/workspace/project/sub".to_string(),
                read_only: false,
                purpose: platform_api::MountPurpose::Shared,
            },
        ];
        let error = MobileLinuxSandbox::new(runtime.clone(), overlapping)
            .expect_err("overlapping guest targets must be rejected");
        assert!(matches!(error, SandboxError::Unavailable(_)));

        fs::create_dir_all(temp.path().join("external")).expect("external");
        let external_rw = vec![MountSpec {
            host_path: temp.path().join("external"),
            guest_path: "/workspace/external".to_string(),
            read_only: false,
            purpose: platform_api::MountPurpose::External,
        }];
        MobileLinuxSandbox::new(runtime, external_rw)
            .expect("explicit host-approved external write mount should be accepted");
    }

    #[test]
    fn mobile_linux_sandbox_rejects_overlapping_host_mount_aliases() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let host = temp.path().join("workspace");
        fs::create_dir_all(host.join("nested")).expect("host fixture");

        let aliases = vec![
            MountSpec {
                host_path: host.clone(),
                guest_path: "/root/readonly".to_string(),
                read_only: true,
                purpose: platform_api::MountPurpose::Shared,
            },
            MountSpec {
                host_path: host.clone(),
                guest_path: "/workspace/writable-alias".to_string(),
                read_only: false,
                purpose: platform_api::MountPurpose::Workspace,
            },
        ];
        assert!(matches!(
            MobileLinuxSandbox::new(runtime.clone(), aliases),
            Err(SandboxError::Unavailable(_))
        ));

        let nested_alias = vec![
            MountSpec {
                host_path: host.clone(),
                guest_path: "/workspace/project".to_string(),
                read_only: false,
                purpose: platform_api::MountPurpose::Workspace,
            },
            MountSpec {
                host_path: host.join("nested"),
                guest_path: "/root/nested".to_string(),
                read_only: true,
                purpose: platform_api::MountPurpose::Shared,
            },
        ];
        assert!(matches!(
            MobileLinuxSandbox::new(runtime, nested_alias),
            Err(SandboxError::Unavailable(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn mobile_linux_sandbox_rejects_symlink_mount_roots() {
        use std::os::unix::fs as unix_fs;

        let temp = tempfile::tempdir().expect("tempdir");
        let outside = temp.path().join("outside");
        let linked = temp.path().join("workspace");
        fs::create_dir_all(&outside).expect("outside");
        unix_fs::symlink(&outside, &linked).expect("workspace symlink");
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let mounts = vec![MountSpec {
            host_path: linked,
            guest_path: "/workspace/project".to_string(),
            read_only: false,
            purpose: platform_api::MountPurpose::Workspace,
        }];

        let error = MobileLinuxSandbox::new(runtime, mounts)
            .expect_err("symlinked mount root must be rejected");
        assert!(matches!(error, SandboxError::Unavailable(_)));
    }

    #[tokio::test]
    async fn mobile_linux_process_runner_maps_run_and_streaming() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("workspace")).expect("workspace");
        fs::create_dir_all(temp.path().join("readonly")).expect("readonly");
        let runtime_impl = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let runtime: Arc<dyn MobileLinuxRuntime> = runtime_impl.clone();
        let sandbox =
            MobileLinuxSandbox::new(runtime.clone(), sample_mounts(&temp)).expect("sandbox");
        let prepared = sandbox
            .prepare(sample_command(&temp), &sample_policy(&temp))
            .expect("prepare");
        let runner = MobileLinuxProcessRunner::new(runtime);

        let output = runner.run(&prepared).await.expect("run");
        assert_eq!(output.exit_code, 7);
        assert_eq!(runtime_impl.last_run.lock().expect("run mutex").len(), 1);

        let sink = Arc::new(RecordingSink::default());
        let output = runner
            .run_streaming(&prepared, sink.clone())
            .await
            .expect("streaming");
        assert_eq!(output.stdout, "stream-1");
        assert_eq!(
            sink.stdout.lock().expect("stdout").as_slice(),
            &["stream-1".to_string()]
        );
        assert_eq!(
            sink.stderr.lock().expect("stderr").as_slice(),
            &[b"stream-err".to_vec()]
        );
    }

    #[tokio::test]
    async fn mobile_linux_process_runner_maps_background_and_kill() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("workspace")).expect("workspace");
        fs::create_dir_all(temp.path().join("readonly")).expect("readonly");
        let runtime_impl = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let runtime: Arc<dyn MobileLinuxRuntime> = runtime_impl.clone();
        let sandbox =
            MobileLinuxSandbox::new(runtime.clone(), sample_mounts(&temp)).expect("sandbox");
        let prepared = sandbox
            .prepare(sample_command(&temp), &sample_policy(&temp))
            .expect("prepare");
        let runner = MobileLinuxProcessRunner::new(runtime);

        let handle = runner
            .spawn_background(&prepared)
            .await
            .expect("spawn background");
        assert_eq!(handle.task_id, "bg-task");
        runner.kill(&handle).await.expect("kill");
        assert_eq!(
            runtime_impl.killed.lock().expect("kill mutex").as_slice(),
            &["bg-task".to_string()]
        );
    }

    #[tokio::test]
    async fn mobile_linux_process_runner_rejects_unknown_background_handles() {
        let runtime_impl = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let runtime: Arc<dyn MobileLinuxRuntime> = runtime_impl.clone();
        let runner = MobileLinuxProcessRunner::new(runtime);
        let foreign = ProcessHandle {
            task_id: "foreign-task".to_string(),
            pid: 0,
        };

        let error = runner
            .kill(&foreign)
            .await
            .expect_err("unknown handles must fail closed");
        assert!(error
            .to_string()
            .contains("unknown background process handle"));
        assert!(runtime_impl.killed.lock().expect("kill mutex").is_empty());
    }

    #[tokio::test]
    async fn mobile_linux_process_runner_preserves_handle_when_kill_fails() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("workspace")).expect("workspace");
        fs::create_dir_all(temp.path().join("readonly")).expect("readonly");
        let runtime_impl = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let runtime: Arc<dyn MobileLinuxRuntime> = runtime_impl.clone();
        let sandbox =
            MobileLinuxSandbox::new(runtime.clone(), sample_mounts(&temp)).expect("sandbox");
        let prepared = sandbox
            .prepare(sample_command(&temp), &sample_policy(&temp))
            .expect("prepare");
        let runner = MobileLinuxProcessRunner::new(runtime);
        let handle = runner
            .spawn_background(&prepared)
            .await
            .expect("spawn background");
        *runtime_impl.kill_error.lock().expect("kill error mutex") =
            Some(MobileLinuxError::Io("transient".to_string()));

        runner
            .kill(&handle)
            .await
            .expect_err("first kill should surface the runtime failure");
        assert!(
            runner
                .background_handles
                .lock()
                .expect("background handles")
                .contains_key(&handle.task_id),
            "a transient kill failure must not discard task ownership"
        );
        runner.kill(&handle).await.expect("retry kill");
    }

    #[tokio::test]
    async fn mobile_linux_process_runner_reaps_completed_background_handles() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("workspace")).expect("workspace");
        fs::create_dir_all(temp.path().join("readonly")).expect("readonly");
        let runtime_impl = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let runtime: Arc<dyn MobileLinuxRuntime> = runtime_impl.clone();
        let sandbox =
            MobileLinuxSandbox::new(runtime.clone(), sample_mounts(&temp)).expect("sandbox");
        let prepared = sandbox
            .prepare(sample_command(&temp), &sample_policy(&temp))
            .expect("prepare");
        let runner = MobileLinuxProcessRunner::new(runtime);

        let handle = runner
            .spawn_background(&prepared)
            .await
            .expect("spawn background");
        runtime_impl.set_task_status("bg-task", MobileLinuxTaskStatus::Completed);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !runner
                    .background_handles
                    .lock()
                    .expect("background handles")
                    .contains_key(&handle.task_id)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("completed handles should be reaped");
    }

    #[tokio::test]
    async fn mobile_linux_process_runner_rejects_foreign_backend_tags() {
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let runner = MobileLinuxProcessRunner::new(runtime);
        let cmd = SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "sh".to_string(),
                args: vec![],
                cwd: None,
                env: HashMap::new(),
                timeout: None,
                stdin: None,
            },
            SandboxedTag::Wrapped {
                backend: SandboxBackend::IosIsh,
            },
        );

        let error = runner
            .run(&cmd)
            .await
            .expect_err("foreign backend must fail");
        assert!(matches!(error, ProcessError::MalformedSandboxPlan(_)));
    }
}
