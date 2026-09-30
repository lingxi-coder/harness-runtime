//! Shared rootfs manifest and verification helpers for the mobile Linux phase-1 path.

#![allow(missing_docs)]

use async_trait::async_trait;
use lingxi_core::host::{
    BackendPlanHandle, ProcessCommand, ProcessHandle, ProcessRunner, Sandbox, SandboxCapability,
    SandboxError, SandboxFeatures, SandboxPolicy, SandboxedCommand, SandboxedTag,
};
use mobile_linux_api::{
    LinuxCommandRequest, LinuxProcessHandle, MobileLinuxError, MobileLinuxRuntime,
    MobileLinuxRuntimeMode, MobileLinuxSandboxPlan, MobileLinuxTaskStatus, MountSpec, ProcessError,
    ProcessOutput, ProcessStreamSink, SandboxBackend,
};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use mobile_linux_core::*;
const ALLOWED_WRITABLE_GUEST_PATHS: &[&str] = &[
    mobile_linux_api::guest_paths::HOME,
    mobile_linux_api::guest_paths::SCRATCH[0],
    mobile_linux_api::guest_paths::SCRATCH[1],
    mobile_linux_api::guest_paths::WORKSPACE_ROOT,
];

fn to_sandbox_error(error: MobileLinuxError) -> SandboxError {
    match error {
        MobileLinuxError::Unsupported => SandboxError::Unsupported,
        MobileLinuxError::Unavailable(message)
        | MobileLinuxError::RestartRequired(message)
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
        | MobileLinuxError::RestartRequired(message)
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

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::{
        LinuxCommandResult, LinuxEnforcementReceipt, MobileLinuxCapability,
        MobileLinuxTaskSnapshot, MobileLinuxTaskStatus, PtyOpenRequest, PtySessionHandle,
        ResourceLimits, RootfsStatus,
    };

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
            _size: mobile_linux_api::PtySize,
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
                purpose: mobile_linux_api::MountPurpose::Workspace,
            },
            MountSpec {
                host_path: temp.path().join("readonly"),
                guest_path: "/root/readonly".to_string(),
                read_only: true,
                purpose: mobile_linux_api::MountPurpose::Shared,
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
            network: mobile_linux_api::NetworkPolicy::Disabled,
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
        assert_eq!(
            plan.request.network,
            mobile_linux_api::NetworkPolicy::Disabled
        );
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
            purpose: mobile_linux_api::MountPurpose::Shared,
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
                purpose: mobile_linux_api::MountPurpose::Workspace,
            },
            MountSpec {
                host_path: temp.path().join("workspace-sub"),
                guest_path: "/workspace/project/sub".to_string(),
                read_only: false,
                purpose: mobile_linux_api::MountPurpose::Shared,
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
            purpose: mobile_linux_api::MountPurpose::External,
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
                purpose: mobile_linux_api::MountPurpose::Shared,
            },
            MountSpec {
                host_path: host.clone(),
                guest_path: "/workspace/writable-alias".to_string(),
                read_only: false,
                purpose: mobile_linux_api::MountPurpose::Workspace,
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
                purpose: mobile_linux_api::MountPurpose::Workspace,
            },
            MountSpec {
                host_path: host.join("nested"),
                guest_path: "/root/nested".to_string(),
                read_only: true,
                purpose: mobile_linux_api::MountPurpose::Shared,
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
            purpose: mobile_linux_api::MountPurpose::Workspace,
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
