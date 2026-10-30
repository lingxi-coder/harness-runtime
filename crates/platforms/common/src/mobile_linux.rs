//! Shared rootfs manifest and verification helpers for the mobile Linux phase-1 path.

#![allow(missing_docs)]

use async_trait::async_trait;
use lingxi_core::host::sandbox::{
    ProcessCleanupOutputTruncation, ProcessCleanupOwnerOutcome, ProcessCleanupOwnerToken,
    ProcessCleanupReceipt,
};
use lingxi_core::host::tool_invoker::{
    ProcessTreeStatus, UnverifiedCleanupOutput, UnverifiedCleanupOutputParts,
};
use lingxi_core::host::{
    BackendPlanHandle, ProcessCommand, ProcessHandle, ProcessRunner, Sandbox, SandboxCapability,
    SandboxError, SandboxFeatures, SandboxPolicy, SandboxedCommand, SandboxedTag,
};
use mobile_linux_api::{
    BrandRuntimeProfile, LinuxCommandRequest, LinuxProcessHandle, MobileLinuxError,
    MobileLinuxRuntime, MobileLinuxRuntimeMode, MobileLinuxSandboxPlan, MobileLinuxTaskStatus,
    MountSpec, ProcessError, ProcessOutput, ProcessStreamSink, RawStdioOpenRequest,
    RawStdioReadResult, RawStdioSessionHandle, RawStdioStopResult, RawStdioTreeStatus,
    SandboxBackend,
};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{oneshot, Notify, OwnedSemaphorePermit, Semaphore};

pub use mobile_linux_core::*;

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

fn raw_stdio_read_error(error: MobileLinuxError) -> ProcessError {
    match error {
        // A timeout returned by a read does not establish that this runner's
        // command deadline elapsed. ProcessError cannot carry captured output.
        MobileLinuxError::Timeout => ProcessError::Io(
            "raw stdio read returned timeout; command deadline is unconfirmed".into(),
        ),
        other => to_process_error(other),
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
    writable_guest_roots: &[String],
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
        if !writable_guest_roots
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
            .map(|cwd| map_cwd_to_guest(cwd, mounts, writable_guest_roots))
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

fn map_cwd_to_guest(
    value: &Path,
    mounts: &[MountSpec],
    writable_guest_roots: &[String],
) -> Result<String, MobileLinuxError> {
    match map_host_path_to_guest(value, mounts) {
        Ok((_, guest)) => Ok(guest),
        Err(_) => {
            let value = value.to_string_lossy().into_owned();
            validate_guest_path(&value)
                .map_err(|err| MobileLinuxError::InvalidRequest(err.to_string()))?;
            if writable_guest_roots
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
                )));
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
    writable_guest_roots: Vec<String>,
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
        profile: BrandRuntimeProfile,
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
            writable_guest_roots: mobile_linux_api::guest_paths::writable_roots_for_profile(
                &profile,
            ),
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
        let request = build_linux_request(&cmd, policy, &self.mounts, &self.writable_guest_roots)
            .map_err(to_sandbox_error)?;
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

#[derive(Clone)]
pub struct MobileLinuxProcessRunner {
    runtime: Arc<dyn MobileLinuxRuntime>,
    backend: SandboxBackend,
    background_handles: Arc<Mutex<HashMap<String, LinuxProcessHandle>>>,
    foreground_owners: ForegroundOwners,
    #[cfg(test)]
    cleanup_grace: std::time::Duration,
}

type ForegroundOwners = Arc<Mutex<HashMap<String, ForegroundOwner>>>;
type ForegroundGeneration = Arc<()>;

struct ForegroundOwner {
    generation: ForegroundGeneration,
    cancelled: bool,
    sessions: HashMap<String, Arc<OwnedRawStdio>>,
    unregistered_sessions: Vec<Arc<OwnedRawStdio>>,
    open_unverified: Option<String>,
    session_reaped: bool,
    // Both the caller's execution lease and the capacity slot stay owned until
    // the guest process is confirmed reaped, including failed cleanup.
    _lifetime_guard: Option<BackendPlanHandle>,
    _receipt_claim: Option<CleanupReceiptClaim>,
    _permit: Arc<OwnedSemaphorePermit>,
}

// A receipt describes exactly one execution. Sharing it with a second
// concurrent or completed execution must not let a rejection acknowledge the
// first execution's cleanup.
#[derive(Clone)]
struct CleanupReceiptClaim {
    receipt: Arc<ProcessCleanupReceipt>,
    token: ProcessCleanupOwnerToken,
}

impl CleanupReceiptClaim {
    fn acquire(
        receipt: Option<&Arc<lingxi_core::host::sandbox::ProcessCleanupReceipt>>,
    ) -> Result<Option<Self>, ProcessError> {
        let receipt = receipt.ok_or_else(|| {
            ProcessError::Io(
                "foreground cleanup receipt is required before starting a raw session".into(),
            )
        })?;
        let token = receipt.claim_owner().map_err(|error| {
            ProcessError::Io(format!(
                "foreground cleanup receipt cannot be claimed: {error:?}"
            ))
        })?;
        Ok(Some(Self {
            receipt: receipt.clone(),
            token,
        }))
    }

    fn no_process_started(&self) -> Result<(), ProcessError> {
        self.receipt
            .finish_owned(
                &self.token,
                ProcessCleanupOwnerOutcome::NoProcessStarted,
                None,
            )
            .map_err(|error| ProcessError::Io(format!("record no-process receipt: {error:?}")))
    }
}

fn publish_unverified_startup(claim: &CleanupReceiptClaim, reason: String, timed_out: bool) {
    let output = UnverifiedCleanupOutput::new(UnverifiedCleanupOutputParts {
        session_id: None,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        stdout_eof: false,
        stderr_eof: false,
        exit_code: None,
        timed_out,
        termination_requested: Some(false),
        tree_status: ProcessTreeStatus::Unverified,
        cleanup_detail: "raw stdio startup cleanup is unverified".into(),
        original_error: Some(reason),
    });
    let _ = claim.receipt.finish_owned(
        &claim.token,
        ProcessCleanupOwnerOutcome::Unverified("raw stdio startup cleanup is unverified".into()),
        Some(Arc::new(output)),
    );
}

struct OwnedRawStdio {
    handle: RawStdioSessionHandle,
    receipt_claim: Option<CleanupReceiptClaim>,
    capture: Mutex<RawStdioCapture>,
    capture_limit: usize,
    _runtime_guard: Arc<dyn MobileLinuxRuntime>,
    _owner_permit: Arc<OwnedSemaphorePermit>,
    _lifetime_guard: Option<BackendPlanHandle>,
    close_state: Mutex<RawStdioCloseState>,
    close_notify: Notify,
}

#[derive(Default)]
struct RawStdioCapture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
    exit_code: Option<i32>,
    exit_inconsistent: bool,
    timed_out: bool,
    original_error: Option<String>,
}

#[derive(Clone)]
enum RawStdioCloseState {
    Open,
    Closing,
    Closed(Result<RawStdioStopResult, String>),
}

const RAW_STDIO_READ_CHUNK_BYTES: usize = 64 * 1024;
const RAW_STDIO_IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(10);
const RAW_STDIO_DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
// The SDK's iOS close itself allows three seconds to reap and another 250 ms
// to dispose. Permit that cleanup to finish without extending the command's
// own execution deadline indefinitely.
const RAW_STDIO_CLEANUP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
const RAW_STDIO_DEFAULT_CAPTURE_BYTES: usize = 1024 * 1024;
static NEXT_UNOWNED_FOREGROUND_ID: AtomicU64 = AtomicU64::new(1);
// Pending native operations and unreaped sessions retain a slot. Refuse new
// work at capacity rather than accumulating unbounded background cleanup tasks.
const MAX_RAW_FOREGROUND_OWNERS: usize = 32;

fn raw_stdio_executor() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("mobile-raw-stdio")
            .enable_all()
            .build()
            .expect("build mobile raw stdio executor")
    })
}

fn raw_foreground_capacity() -> Arc<Semaphore> {
    static CAPACITY: OnceLock<Arc<Semaphore>> = OnceLock::new();
    CAPACITY
        .get_or_init(|| Arc::new(Semaphore::new(MAX_RAW_FOREGROUND_OWNERS)))
        .clone()
}

fn quarantine_foreground_owner(
    owners: ForegroundOwners,
    owner: String,
    generation: ForegroundGeneration,
) {
    // Keep the original owner, lease and permit alive while either process
    // tree exit or output draining remains unverified.
    static QUARANTINE: OnceLock<Mutex<Vec<(ForegroundOwners, String, ForegroundGeneration)>>> =
        OnceLock::new();
    let mut quarantine = QUARANTINE
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !quarantine
        .iter()
        .any(|(existing_owners, existing_owner, existing_generation)| {
            Arc::ptr_eq(existing_owners, &owners)
                && existing_owner == &owner
                && Arc::ptr_eq(existing_generation, &generation)
        })
    {
        quarantine.push((owners, owner, generation));
    }
}

fn quarantined_orphan_sessions() -> &'static Mutex<Vec<Arc<OwnedRawStdio>>> {
    static ORPHANS: OnceLock<Mutex<Vec<Arc<OwnedRawStdio>>>> = OnceLock::new();
    ORPHANS.get_or_init(|| Mutex::new(Vec::new()))
}

impl MobileLinuxProcessRunner {
    #[must_use]
    pub fn new(runtime: Arc<dyn MobileLinuxRuntime>) -> Self {
        Self {
            backend: runtime.backend(),
            runtime,
            background_handles: Arc::new(Mutex::new(HashMap::new())),
            foreground_owners: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            cleanup_grace: RAW_STDIO_CLEANUP_GRACE,
        }
    }

    fn cleanup_grace(&self) -> std::time::Duration {
        #[cfg(test)]
        {
            self.cleanup_grace
        }
        #[cfg(not(test))]
        {
            RAW_STDIO_CLEANUP_GRACE
        }
    }

    fn begin_foreground_owner(
        &self,
        owner: &str,
        lifetime_guard: Option<BackendPlanHandle>,
        _cleanup_receipt: Option<Arc<lingxi_core::host::sandbox::ProcessCleanupReceipt>>,
        receipt_claim: Option<CleanupReceiptClaim>,
        permit: OwnedSemaphorePermit,
    ) -> Result<ForegroundGeneration, ProcessError> {
        let mut owners = self
            .foreground_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if owners.contains_key(owner) {
            if let Some(claim) = &receipt_claim {
                claim.no_process_started()?;
            }
            return Err(ProcessError::Io(format!(
                "foreground owner is already active or cancelled: {owner}"
            )));
        }
        let generation = Arc::new(());
        owners.insert(
            owner.to_string(),
            ForegroundOwner {
                generation: generation.clone(),
                cancelled: false,
                sessions: HashMap::new(),
                unregistered_sessions: Vec::new(),
                open_unverified: None,
                session_reaped: false,
                _lifetime_guard: lifetime_guard,
                _receipt_claim: receipt_claim,
                _permit: Arc::new(permit),
            },
        );
        Ok(generation)
    }

    fn register_foreground_session(
        &self,
        owner: &str,
        generation: &ForegroundGeneration,
        session: Arc<OwnedRawStdio>,
    ) -> Result<bool, ProcessError> {
        let mut owners = self
            .foreground_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if owners.values().any(|state| {
            state.sessions.contains_key(&session.handle.id)
                || state
                    .unregistered_sessions
                    .iter()
                    .any(|entry| entry.handle.id == session.handle.id)
        }) || quarantined_orphan_sessions()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|entry| entry.handle.id == session.handle.id)
        {
            return Err(ProcessError::Io("duplicate raw session identity".into()));
        }
        let Some(owner_state) = owners
            .get_mut(owner)
            .filter(|state| Arc::ptr_eq(&state.generation, generation))
        else {
            return Err(ProcessError::Io(
                "foreground owner disappeared before raw session registration".into(),
            ));
        };
        owner_state
            .sessions
            .insert(session.handle.id.clone(), session);
        Ok(owner_state.cancelled)
    }

    fn quarantine_unregistered_session(
        &self,
        owner: &str,
        generation: &ForegroundGeneration,
        session: Arc<OwnedRawStdio>,
    ) {
        if let Some(state) = self
            .foreground_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(owner)
            .filter(|state| Arc::ptr_eq(&state.generation, generation))
        {
            state.unregistered_sessions.push(session.clone());
            state.open_unverified = Some("raw session registration is unverified".into());
            return;
        }
        // Even a lost generation retains its runtime, execution lease and its
        // already-reserved permit. There can be at most one such session per
        // admitted foreground owner, so this is bounded by that capacity.
        let mut orphans = quarantined_orphan_sessions()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !orphans
            .iter()
            .any(|existing| Arc::ptr_eq(existing, &session))
        {
            orphans.push(session);
        }
    }

    fn foreground_owner_cancelled(&self, owner: &str, generation: &ForegroundGeneration) -> bool {
        self.foreground_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(owner)
            .is_none_or(|state| !Arc::ptr_eq(&state.generation, generation) || state.cancelled)
    }

    fn mark_open_unverified(&self, owner: &str, generation: &ForegroundGeneration, reason: String) {
        if let Some(state) = self
            .foreground_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(owner)
            .filter(|state| Arc::ptr_eq(&state.generation, generation))
        {
            if let Some(claim) = &state._receipt_claim {
                publish_unverified_startup(claim, reason, claim.receipt.wait_expired());
            }
            state.open_unverified = Some("raw stdio startup cleanup is unverified".into());
        }
    }

    fn foreground_cleanup_claim(
        &self,
        owner: &str,
        generation: &ForegroundGeneration,
    ) -> Option<CleanupReceiptClaim> {
        self.foreground_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(owner)
            .filter(|state| Arc::ptr_eq(&state.generation, generation))
            .and_then(|state| state._receipt_claim.clone())
    }

    fn foreground_retained_owner(
        &self,
        owner: &str,
        generation: &ForegroundGeneration,
    ) -> Option<(Arc<OwnedSemaphorePermit>, Option<BackendPlanHandle>)> {
        self.foreground_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(owner)
            .filter(|state| Arc::ptr_eq(&state.generation, generation))
            .map(|state| (state._permit.clone(), state._lifetime_guard.clone()))
    }

    fn unregister_foreground_session(
        &self,
        owner: &str,
        generation: &ForegroundGeneration,
        session_id: &str,
    ) {
        let mut owners = self
            .foreground_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(owner_state) = owners
            .get_mut(owner)
            .filter(|state| Arc::ptr_eq(&state.generation, generation))
        {
            if owner_state.sessions.remove(session_id).is_some() {
                owner_state.session_reaped = true;
            }
        }
    }

    fn finish_foreground_owner(&self, owner: &str, generation: &ForegroundGeneration) {
        let mut owners = self
            .foreground_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = owners
            .get(owner)
            .filter(|state| Arc::ptr_eq(&state.generation, generation))
        else {
            return;
        };
        if !state.sessions.is_empty()
            || !state.unregistered_sessions.is_empty()
            || state.open_unverified.is_some()
        {
            // The session did not yield a successful close receipt. Preserve
            // the owner's lease and capacity even after this runner is dropped.
            drop(owners);
            quarantine_foreground_owner(
                self.foreground_owners.clone(),
                owner.to_string(),
                generation.clone(),
            );
        } else {
            owners.remove(owner);
        }
    }

    async fn cleanup_after_foreground_panic(
        &self,
        owner: &str,
        generation: &ForegroundGeneration,
        reason: &str,
    ) -> bool {
        let state = self
            .foreground_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(owner)
            .filter(|state| Arc::ptr_eq(&state.generation, generation))
            .map(|state| {
                (
                    state.sessions.values().cloned().collect::<Vec<_>>(),
                    state.session_reaped,
                )
            });
        let Some((sessions, session_reaped)) = state else {
            return false;
        };
        if sessions.is_empty() {
            // A panic during native open can hide a spawned process and its ID;
            // after unregister, the same generation already reaped its session.
            if !session_reaped {
                self.mark_open_unverified(owner, generation, reason.to_owned());
            }
            return session_reaped;
        }
        let mut verified = true;
        for session in sessions {
            let stopped = self.close_raw_session(&session).await;
            let release = stop_proves_tree_and_drain(&stopped);
            let outcome = finish_raw_session(
                &session,
                stopped,
                session.capture_limit,
                Some(reason.to_owned()),
            );
            if release && raw_outcome_was_published(&session, &outcome) {
                self.unregister_foreground_session(owner, generation, &session.handle.id);
            } else {
                verified = false;
            }
        }
        verified
    }

    async fn cancel_foreground_instance(&self, owner: &str, generation: &ForegroundGeneration) {
        let sessions = {
            let mut owners = self
                .foreground_owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(state) = owners
                .get_mut(owner)
                .filter(|state| Arc::ptr_eq(&state.generation, generation))
            {
                state.cancelled = true;
                state.sessions.values().cloned().collect::<Vec<_>>()
            } else {
                Vec::new()
            }
        };
        for session in sessions {
            if let Err(error) = self.close_raw_session(&session).await {
                tracing::warn!(%error, session_id = %session.handle.id, "mobile-linux foreground reap failed");
            }
        }
    }

    async fn close_raw_session(
        &self,
        session: &Arc<OwnedRawStdio>,
    ) -> Result<RawStdioStopResult, ProcessError> {
        // The typed stop owner outlives every caller and publishes one shared
        // exact-session result. Caller cancellation never starts another stop.
        let executor = raw_stdio_executor();
        let start_close = {
            let mut state = session
                .close_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(&*state, RawStdioCloseState::Open) {
                *state = RawStdioCloseState::Closing;
                true
            } else {
                false
            }
        };
        if start_close {
            let runtime = self.runtime.clone();
            let session = session.clone();
            executor.spawn(async move {
                let handle = session.handle.clone();
                // A panic in the runtime must still publish a failed receipt;
                // otherwise all waiters could remain stuck in Closing forever.
                let expected_id = handle.id.clone();
                let result = match tokio::spawn(async move {
                    runtime
                        .stop_raw_stdio(&handle, RAW_STDIO_DEFAULT_CAPTURE_BYTES)
                        .await
                })
                .await
                {
                    Ok(Ok(result)) if result.session_id == expected_id => {
                        Ok(bound_raw_stop_result(result))
                    }
                    Ok(Ok(_)) => Err("raw stop returned a different session identity".into()),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(error) => Err(format!("native stop task failed: {error}")),
                };
                *session
                    .close_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    RawStdioCloseState::Closed(result);
                session.close_notify.notify_waiters();
            });
        }
        loop {
            let notified = session.close_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = session
                    .close_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let RawStdioCloseState::Closed(result) = &*state {
                    return result.clone().map_err(|error| {
                        ProcessError::Io(format!(
                            "{} {error}",
                            lingxi_core::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED
                        ))
                    });
                }
            }
            notified.await;
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

    async fn run_raw_stdio_foreground(
        &self,
        plan: &MobileLinuxSandboxPlan,
        owner: &str,
        generation: &ForegroundGeneration,
        max_output_bytes: Option<usize>,
        execution_deadline: tokio::time::Instant,
    ) -> Result<ProcessOutput, ProcessError> {
        let claim = self
            .foreground_cleanup_claim(owner, generation)
            .ok_or_else(|| {
                ProcessError::Io("raw foreground has no claimed cleanup receipt".into())
            })?;
        let (owner_permit, lifetime_guard) = self
            .foreground_retained_owner(owner, generation)
            .ok_or_else(|| {
                ProcessError::Io("raw foreground owner disappeared before open".into())
            })?;
        if plan.request.stdin.is_some() {
            // Raw-stdio's current pinned API has no stdin EOF/write mode. Do
            // not silently drop a caller's input; the shell-only EOF wrapper
            // below is valid exclusively for an absent payload.
            claim.no_process_started()?;
            return Err(ProcessError::Unsupported);
        }
        if self.foreground_owner_cancelled(owner, generation) {
            claim.no_process_started()?;
            return Err(ProcessError::Io(
                "foreground command was cancelled before raw session open".into(),
            ));
        }

        let requested = &plan.request;
        // Keep the target argv as data. The constant shell program implements
        // EOF redirection without interpolating user command text.
        let mut args = vec![
            "-c".to_string(),
            "exec \"$@\" </dev/null".to_string(),
            "lingxi-mobile-foreground".to_string(),
            requested.command.clone(),
        ];
        args.extend(requested.args.iter().cloned());
        let opened = self
            .runtime
            .open_raw_stdio(RawStdioOpenRequest {
                command: "/bin/sh".to_string(),
                args,
                cwd: requested.cwd.clone(),
                env: requested.env.clone(),
                network: requested.network,
                resource_limits: requested.resource_limits,
                mounts: plan.mounts.clone(),
            })
            .await;
        let opened = match opened {
            Ok(opened) => opened,
            Err(error) => {
                // The pinned SDK does not promise that an Err means no guest
                // was started or reaped. In particular, an iOS native open
                // can return an ID before enforcement validation fails.
                let reason = format!("raw stdio open failed without a cleanup receipt: {error}");
                publish_unverified_startup(
                    &claim,
                    reason.clone(),
                    tokio::time::Instant::now() >= execution_deadline,
                );
                self.mark_open_unverified(owner, generation, reason.clone());
                return Err(ProcessError::Io(format!(
                    "{} {reason}",
                    lingxi_core::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED
                )));
            }
        };
        let session_id = opened.id.clone();
        let capture_limit = max_output_bytes
            .unwrap_or(RAW_STDIO_DEFAULT_CAPTURE_BYTES)
            .min(RAW_STDIO_DEFAULT_CAPTURE_BYTES);
        let session = Arc::new(OwnedRawStdio {
            handle: opened,
            receipt_claim: Some(claim.clone()),
            capture: Mutex::new(RawStdioCapture {
                timed_out: tokio::time::Instant::now() >= execution_deadline,
                ..Default::default()
            }),
            capture_limit,
            _runtime_guard: self.runtime.clone(),
            _owner_permit: owner_permit,
            _lifetime_guard: lifetime_guard,
            close_state: Mutex::new(RawStdioCloseState::Open),
            close_notify: Notify::new(),
        });
        if let Err(error) = claim.receipt.bind_session(&claim.token, &session_id) {
            let reason =
                format!("raw session identity was rejected; cleanup is unverified: {error:?}");
            self.quarantine_unregistered_session(owner, generation, session);
            publish_unverified_startup(
                &claim,
                reason.clone(),
                tokio::time::Instant::now() >= execution_deadline,
            );
            self.mark_open_unverified(owner, generation, reason.clone());
            return Err(ProcessError::Io(reason));
        }
        let cancelled = match self.register_foreground_session(owner, generation, session.clone()) {
            Ok(cancelled) => cancelled,
            Err(error) => {
                // A duplicate/foreign identity must never be used to signal a
                // process. Retain the owner and report unknown cleanup.
                self.quarantine_unregistered_session(owner, generation, session.clone());
                return finish_raw_session(
                    &session,
                    Err(ProcessError::Io(
                        "raw session was not registered; no stop was attempted".into(),
                    )),
                    capture_limit,
                    Some(error.to_string()),
                );
            }
        };
        let admission_error = if cancelled {
            Some(ProcessError::Io(
                "foreground command was cancelled before raw session registration".into(),
            ))
        } else {
            session
                .handle
                .enforcement
                .ensure_for(requested.network, requested.resource_limits)
                .err()
                .map(to_process_error)
        };
        if let Some(error) = admission_error {
            let stopped = self.close_raw_session(&session).await;
            let release = stop_proves_tree_and_drain(&stopped);
            let result =
                finish_raw_session(&session, stopped, capture_limit, Some(error.to_string()));
            if release && raw_outcome_was_published(&session, &result) {
                self.unregister_foreground_session(owner, generation, &session_id);
            }
            return result;
        }

        // The owner retains captured bytes even if the caller or operation
        // fails; stopping may add unread tail bytes to the same observation.
        let capture = async {
            loop {
                let result: RawStdioReadResult = self
                    .runtime
                    .read_raw_stdio(&session.handle, RAW_STDIO_READ_CHUNK_BYTES)
                    .await
                    .map_err(raw_stdio_read_error)?;
                {
                    let mut captured = session
                        .capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let mut out_truncated = captured.stdout_truncated;
                    let mut err_truncated = captured.stderr_truncated;
                    append_bounded(
                        &mut captured.stdout,
                        &result.stdout,
                        capture_limit,
                        &mut out_truncated,
                    );
                    append_bounded(
                        &mut captured.stderr,
                        &result.stderr,
                        capture_limit,
                        &mut err_truncated,
                    );
                    captured.stdout_truncated = out_truncated;
                    captured.stderr_truncated = err_truncated;
                    if let Some(code) = result.exit_code {
                        if code < 0 {
                            captured.exit_code = None;
                            captured.exit_inconsistent = true;
                            append_original_error(
                                &mut captured.original_error,
                                "raw read returned a negative exit sentinel".into(),
                            );
                        } else if captured.exit_inconsistent {
                            // An earlier invalid or conflicting exit cannot be
                            // repaired by an unrelated later status.
                        } else if captured.exit_code.is_some_and(|old| old != code) {
                            captured.exit_code = None;
                            captured.exit_inconsistent = true;
                            append_original_error(
                                &mut captured.original_error,
                                "raw reads reported different exit codes".into(),
                            );
                        } else {
                            captured.exit_code = Some(code);
                        }
                    }
                }
                if result.closed {
                    break;
                }
                if result.stdout.is_empty() && result.stderr.is_empty() {
                    tokio::time::sleep(RAW_STDIO_IDLE_POLL).await;
                }
            }

            Ok::<(), ProcessError>(())
        };
        let (capture_result, timed_out) = if tokio::time::Instant::now() >= execution_deadline {
            drop(capture);
            (Ok(()), true)
        } else {
            match tokio::time::timeout_at(execution_deadline, capture).await {
                Ok(result) => (result, false),
                Err(_) => (Ok(()), true),
            }
        };
        {
            let mut captured = session
                .capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            captured.timed_out = timed_out;
            if let Err(error) = capture_result {
                append_original_error(&mut captured.original_error, error.to_string());
            }
        }
        let stopped = self.close_raw_session(&session).await;
        let release = stop_proves_tree_and_drain(&stopped);
        let result = finish_raw_session(&session, stopped, capture_limit, None);
        if release && raw_outcome_was_published(&session, &result) {
            self.unregister_foreground_session(owner, generation, &session_id);
        }
        result
    }
}

fn append_bounded(target: &mut Vec<u8>, bytes: &[u8], limit: usize, truncated: &mut bool) {
    let remaining = limit.saturating_sub(target.len());
    let keep = remaining.min(bytes.len());
    target.extend_from_slice(&bytes[..keep]);
    if keep < bytes.len() {
        *truncated = true;
    }
}

fn bound_raw_stop_result(mut result: RawStdioStopResult) -> RawStdioStopResult {
    if result.stdout.len() > RAW_STDIO_DEFAULT_CAPTURE_BYTES {
        result.stdout_truncated = true;
    }
    if result.stderr.len() > RAW_STDIO_DEFAULT_CAPTURE_BYTES {
        result.stderr_truncated = true;
    }
    result.stdout =
        result.stdout[..result.stdout.len().min(RAW_STDIO_DEFAULT_CAPTURE_BYTES)].to_vec();
    result.stderr =
        result.stderr[..result.stderr.len().min(RAW_STDIO_DEFAULT_CAPTURE_BYTES)].to_vec();
    result
}

fn append_original_error(target: &mut Option<String>, error: String) {
    match target {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(&error);
        }
        None => *target = Some(error),
    }
}

/// Combine already-consumed bytes with the exact session's stop tail. The
/// resulting observation is published before returning a generic ProcessError.
fn finish_raw_session(
    session: &OwnedRawStdio,
    stopped: Result<RawStdioStopResult, ProcessError>,
    capture_limit: usize,
    additional_error: Option<String>,
) -> Result<ProcessOutput, ProcessError> {
    let claim = session
        .receipt_claim
        .as_ref()
        .ok_or_else(|| ProcessError::Io("raw session has no claimed cleanup receipt".into()))?;
    let mut capture = session
        .capture
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(error) = additional_error {
        append_original_error(&mut capture.original_error, error);
    }
    let mut tree_status = ProcessTreeStatus::Unverified;
    let mut cleanup_detail = "raw stop did not provide process-tree exit evidence".to_string();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut termination_requested = None;
    let mut stop_exit = None;
    match stopped {
        Ok(stop) => {
            let RawStdioStopResult {
                session_id: _,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
                stdout_eof: out_eof,
                stderr_eof: err_eof,
                exit_code,
                termination_requested: requested,
                cleanup,
                failure,
            } = stop;
            let mut out_truncated = capture.stdout_truncated;
            let mut err_truncated = capture.stderr_truncated;
            append_bounded(
                &mut capture.stdout,
                &stdout,
                capture_limit,
                &mut out_truncated,
            );
            append_bounded(
                &mut capture.stderr,
                &stderr,
                capture_limit,
                &mut err_truncated,
            );
            capture.stdout_truncated = out_truncated || stdout_truncated;
            capture.stderr_truncated = err_truncated || stderr_truncated;
            stdout_eof = out_eof;
            stderr_eof = err_eof;
            termination_requested = requested;
            match cleanup {
                RawStdioTreeStatus::TreeReaped => {
                    tree_status = ProcessTreeStatus::VerifiedReaped;
                    cleanup_detail = "process tree reaped; result evidence is incomplete".into();
                }
                RawStdioTreeStatus::Unverified(detail) => cleanup_detail = detail,
            }
            if exit_code.is_some_and(|code| code < 0) {
                capture.exit_code = None;
                capture.exit_inconsistent = true;
                append_original_error(
                    &mut capture.original_error,
                    "raw stop returned a negative exit sentinel".into(),
                );
            } else {
                stop_exit = exit_code;
            }
            if let Some(error) = failure {
                append_original_error(&mut capture.original_error, format!("raw stop: {error}"));
            }
        }
        Err(error) => {
            append_original_error(&mut capture.original_error, format!("raw stop: {error}"))
        }
    }
    let exit_code = if capture.exit_inconsistent {
        None
    } else {
        match (capture.exit_code, stop_exit) {
            (Some(first), Some(last)) if first != last => {
                append_original_error(
                    &mut capture.original_error,
                    "raw read and stop reported different exit codes".into(),
                );
                None
            }
            (Some(code), _) | (_, Some(code)) => Some(code),
            _ => None,
        }
    };
    let complete = tree_status == ProcessTreeStatus::VerifiedReaped
        && stdout_eof
        && stderr_eof
        && exit_code.is_some()
        && capture.original_error.is_none();
    let stdout = String::from_utf8_lossy(&capture.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&capture.stderr).into_owned();
    if complete {
        claim
            .receipt
            .finish_owned_complete(
                &claim.token,
                ProcessCleanupOutputTruncation {
                    stdout_truncated: capture.stdout_truncated,
                    stderr_truncated: capture.stderr_truncated,
                },
            )
            .map_err(|error| ProcessError::Io(format!("record tree receipt: {error:?}")))?;
        return Ok(ProcessOutput {
            stdout,
            stderr,
            exit_code: exit_code.expect("checked exit"),
            timed_out: capture.timed_out,
        });
    }
    let output = UnverifiedCleanupOutput::new(UnverifiedCleanupOutputParts {
        session_id: Some(session.handle.id.clone()),
        stdout,
        stderr,
        stdout_truncated: capture.stdout_truncated,
        stderr_truncated: capture.stderr_truncated,
        stdout_eof,
        stderr_eof,
        exit_code,
        timed_out: capture.timed_out,
        termination_requested,
        tree_status,
        cleanup_detail,
        original_error: capture.original_error.clone(),
    });
    let outcome = match tree_status {
        ProcessTreeStatus::VerifiedReaped => ProcessCleanupOwnerOutcome::TreeReaped,
        ProcessTreeStatus::Unverified => {
            ProcessCleanupOwnerOutcome::Unverified("raw process cleanup is unverified".into())
        }
    };
    claim
        .receipt
        .finish_owned(&claim.token, outcome, Some(Arc::new(output)))
        .map_err(|error| ProcessError::Io(format!("record incomplete raw output: {error:?}")))?;
    Err(ProcessError::Io(format!(
        "{} raw process result is incomplete",
        lingxi_core::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED
    )))
}

fn stop_proves_tree_and_drain(stopped: &Result<RawStdioStopResult, ProcessError>) -> bool {
    stopped.as_ref().is_ok_and(|stop| {
        matches!(&stop.cleanup, RawStdioTreeStatus::TreeReaped)
            && stop.stdout_eof
            && stop.stderr_eof
    })
}

fn raw_outcome_was_published(
    session: &OwnedRawStdio,
    outcome: &Result<ProcessOutput, ProcessError>,
) -> bool {
    outcome.is_ok()
        || session
            .receipt_claim
            .as_ref()
            .is_some_and(|claim| claim.receipt.snapshot().observation.is_some())
}

#[async_trait]
impl ProcessRunner for MobileLinuxProcessRunner {
    fn supports_owned_foreground_cleanup_receipt(&self) -> bool {
        true
    }

    async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
        if let Some(owner) = cmd
            .process_owner()
            .filter(|owner| owner.starts_with("side:"))
        {
            let result = self.run_foreground_with_output_limit(cmd, None).await?;
            return match result.outcome {
                lingxi_core::host::ForegroundOutcome::Completed(output) => Ok(output),
                lingxi_core::host::ForegroundOutcome::MovedToBackground(_) => {
                    Err(ProcessError::Io(format!(
                        "side-chat process owner {owner} unexpectedly moved to background"
                    )))
                }
            };
        }
        let plan = self.admitted_plan(cmd)?;
        let result = self
            .runtime
            .run(plan.request.clone())
            .await
            .map_err(to_process_error)?;
        Ok(result.into())
    }

    async fn run_foreground(
        &self,
        cmd: &SandboxedCommand,
    ) -> Result<lingxi_core::host::ForegroundOutcome, ProcessError> {
        Ok(self
            .run_foreground_with_output_limit(cmd, None)
            .await?
            .outcome)
    }

    async fn run_foreground_with_output_limit(
        &self,
        cmd: &SandboxedCommand,
        max_output_bytes: Option<usize>,
    ) -> Result<lingxi_core::host::ForegroundRunResult, ProcessError> {
        // Hold the claim through every admission decision and then transfer it
        // to the process owner. A shared receipt can never acknowledge a
        // different, still-running execution.
        let receipt_claim = CleanupReceiptClaim::acquire(cmd.cleanup_receipt())?;
        let plan = match self.admitted_plan(cmd) {
            Ok(plan) => plan,
            Err(error) => {
                if let Some(claim) = &receipt_claim {
                    claim.no_process_started()?;
                }
                return Err(error);
            }
        };
        if plan.request.stdin.is_some() {
            // Refuse before opening any child because the pinned raw-stdio
            // contract cannot currently feed a complete stdin payload and EOF.
            if let Some(claim) = &receipt_claim {
                claim.no_process_started()?;
            }
            return Err(ProcessError::Unsupported);
        }
        // Count startup against the command's execution timeout. The caller
        // waits an additional bounded grace period for cleanup; the persistent
        // owner itself remains alive beyond that deadline if native work is
        // still pending.
        let timeout = plan
            .request
            .timeout_ms
            .map(std::time::Duration::from_millis)
            .unwrap_or(RAW_STDIO_DEFAULT_TIMEOUT);
        let Some(execution_deadline) = tokio::time::Instant::now().checked_add(timeout) else {
            if let Some(claim) = &receipt_claim {
                claim.no_process_started()?;
            }
            return Err(ProcessError::Io(
                "mobile raw stdio timeout is too large".into(),
            ));
        };
        let Some(caller_deadline) = execution_deadline.checked_add(self.cleanup_grace()) else {
            if let Some(claim) = &receipt_claim {
                claim.no_process_started()?;
            }
            return Err(ProcessError::Io(
                "mobile raw stdio timeout is too large".into(),
            ));
        };
        let owner = cmd.process_owner().map(str::to_string).unwrap_or_else(|| {
            format!(
                "mobile-linux-unowned:{}",
                NEXT_UNOWNED_FOREGROUND_ID.fetch_add(1, Ordering::Relaxed)
            )
        });
        let executor = raw_stdio_executor();
        let permit = match raw_foreground_capacity().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                if let Some(claim) = &receipt_claim {
                    claim.no_process_started()?;
                }
                return Err(ProcessError::Io(
                    "mobile raw stdio capacity is exhausted".into(),
                ));
            }
        };
        let generation = self.begin_foreground_owner(
            &owner,
            cmd.lifetime_guard().cloned(),
            cmd.cleanup_receipt().cloned(),
            receipt_claim,
            permit,
        )?;
        let (_cancel_on_drop, cancellation) = oneshot::channel::<()>();
        let runner = self.clone();
        let worker = runner.clone();
        let worker_owner = owner.clone();
        let worker_generation = generation.clone();
        // The open, read and close futures all live on this process-lifetime
        // runtime. Dropping the caller signals cancellation but never drops an
        // in-flight native open or the only close/reap receipt.
        let running = executor.spawn(async move {
            let operation = worker.run_raw_stdio_foreground(
                &plan,
                &worker_owner,
                &worker_generation,
                max_output_bytes,
                execution_deadline,
            );
            tokio::pin!(operation);
            tokio::select! {
                biased;
                _ = cancellation => {
                    worker.cancel_foreground_instance(&worker_owner, &worker_generation).await;
                    operation.await
                }
                result = &mut operation => result,
            }
        });
        let mut monitor = executor.spawn(async move {
            let result = match running.await {
                Ok(result) => result,
                Err(error) => {
                    let reason = format!("raw foreground owner failed: {error}");
                    if runner
                        .cleanup_after_foreground_panic(&owner, &generation, &reason)
                        .await
                    {
                        Err(ProcessError::Io(reason))
                    } else {
                        Err(ProcessError::Io(format!(
                            "{} {reason}",
                            lingxi_core::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED
                        )))
                    }
                }
            };
            runner.finish_foreground_owner(&owner, &generation);
            result
        });
        let result = match tokio::time::timeout_at(caller_deadline, &mut monitor).await {
            Ok(result) => result.map_err(|error| {
                ProcessError::Io(format!(
                    "{} raw foreground monitor failed: {error}",
                    lingxi_core::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED
                ))
            })?,
            Err(_) => {
                // The receipt's deadline flag describes this caller's result,
                // independently of a later native cleanup acknowledgement.
                // Dropping the sender requests cleanup; neither the worker's
                // open nor its sole native close is cancelled by this timeout.
                if let Some(receipt) = cmd.cleanup_receipt() {
                    receipt.mark_wait_expired();
                }
                return Err(ProcessError::Io(format!(
                    "{} raw foreground operation exceeded its cleanup deadline",
                    lingxi_core::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED
                )));
            }
        };
        result.map(|output| lingxi_core::host::ForegroundRunResult {
            outcome: lingxi_core::host::ForegroundOutcome::Completed(output),
            output_file: None,
        })
    }

    async fn kill_owner_processes(&self, owner: &str) -> Vec<u32> {
        let sessions = {
            let mut owners = self
                .foreground_owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            owners
                .iter_mut()
                .filter(|(registered_owner, _)| {
                    registered_owner.as_str() == owner
                        || registered_owner
                            .strip_prefix(owner)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                })
                .flat_map(|(_, state)| {
                    state.cancelled = true;
                    state.sessions.values().cloned().collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        // A stuck close must not prevent cancellation of the other bounded
        // sessions belonging to the same owner prefix.
        futures::future::join_all(sessions.into_iter().map(|session| async move {
            if let Err(error) = self.close_raw_session(&session).await {
                tracing::warn!(%error, session_id = %session.handle.id, "mobile-linux foreground reap failed");
            }
        }))
        .await;
        // Raw stdio does not expose a host PID. The exact session's stop
        // result, not this empty PID list, carries cleanup evidence.
        Vec::new()
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
    use std::collections::{HashSet, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    #[test]
    fn raw_read_error_keeps_non_timeout_error_categories() {
        assert!(matches!(
            raw_stdio_read_error(MobileLinuxError::Unsupported),
            ProcessError::Unsupported
        ));
        assert!(matches!(
            raw_stdio_read_error(MobileLinuxError::ResourceLimitExceeded("limit".into())),
            ProcessError::SandboxEnforcementFailed(ref detail) if detail.contains("limit")
        ));
        assert!(matches!(
            raw_stdio_read_error(MobileLinuxError::NetworkPolicyUnavailable("policy".into())),
            ProcessError::PolicyUnsupported(ref detail) if detail.contains("policy")
        ));
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
        raw_closed: std::sync::atomic::AtomicBool,
        last_run: Mutex<Vec<LinuxCommandRequest>>,
        last_background: Mutex<Vec<LinuxCommandRequest>>,
        killed: Mutex<Vec<String>>,
        kill_error: Mutex<Option<MobileLinuxError>>,
        task_statuses: Mutex<HashMap<String, MobileLinuxTaskStatus>>,
        raw_stdio_requests: Mutex<Vec<RawStdioOpenRequest>>,
        raw_stdio_reads: Mutex<VecDeque<RawStdioReadResult>>,
        raw_stdio_closed: Mutex<Vec<String>>,
        raw_stdio_never_close: AtomicBool,
        raw_stdio_open_gate: Mutex<Option<Arc<Notify>>>,
        raw_stdio_open_started: Notify,
        raw_stdio_open_error: Mutex<Option<MobileLinuxError>>,
        raw_stdio_open_id: Mutex<Option<String>>,
        raw_stdio_open_panic: AtomicBool,
        raw_stdio_read_panic: AtomicBool,
        raw_stdio_read_error: Mutex<Option<MobileLinuxError>>,
        raw_stdio_close_gate: Mutex<Option<Arc<Notify>>>,
        raw_stdio_close_started: Notify,
        raw_stdio_close_calls: AtomicUsize,
        raw_stdio_close_claimed: Mutex<HashSet<String>>,
        raw_stdio_close_error: Mutex<Option<MobileLinuxError>>,
        raw_stdio_stop_result: Mutex<Option<RawStdioStopResult>>,
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
                raw_closed: std::sync::atomic::AtomicBool::new(false),
                last_run: Mutex::new(Vec::new()),
                last_background: Mutex::new(Vec::new()),
                killed: Mutex::new(Vec::new()),
                kill_error: Mutex::new(None),
                task_statuses: Mutex::new(HashMap::new()),
                raw_stdio_requests: Mutex::new(Vec::new()),
                raw_stdio_reads: Mutex::new(VecDeque::new()),
                raw_stdio_closed: Mutex::new(Vec::new()),
                raw_stdio_never_close: AtomicBool::new(false),
                raw_stdio_open_gate: Mutex::new(None),
                raw_stdio_open_started: Notify::new(),
                raw_stdio_open_error: Mutex::new(None),
                raw_stdio_open_id: Mutex::new(None),
                raw_stdio_open_panic: AtomicBool::new(false),
                raw_stdio_read_panic: AtomicBool::new(false),
                raw_stdio_read_error: Mutex::new(None),
                raw_stdio_close_gate: Mutex::new(None),
                raw_stdio_close_started: Notify::new(),
                raw_stdio_close_calls: AtomicUsize::new(0),
                raw_stdio_close_claimed: Mutex::new(HashSet::new()),
                raw_stdio_close_error: Mutex::new(None),
                raw_stdio_stop_result: Mutex::new(None),
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

        async fn open_raw_stdio(
            &self,
            request: RawStdioOpenRequest,
        ) -> Result<RawStdioSessionHandle, MobileLinuxError> {
            self.raw_closed.store(false, Ordering::Release);
            self.raw_stdio_requests
                .lock()
                .expect("raw stdio request mutex")
                .push(request.clone());
            self.raw_stdio_open_started.notify_one();
            let gate = self
                .raw_stdio_open_gate
                .lock()
                .expect("raw stdio open gate mutex")
                .clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            assert!(
                !self.raw_stdio_open_panic.swap(false, Ordering::AcqRel),
                "injected panic during native open"
            );
            if let Some(error) = self.raw_stdio_open_error.lock().unwrap().take() {
                return Err(error);
            }
            let id = self
                .raw_stdio_open_id
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| {
                    format!(
                        "raw-session-{}",
                        NEXT_UNOWNED_FOREGROUND_ID.fetch_add(1, Ordering::Relaxed)
                    )
                });
            Ok(RawStdioSessionHandle {
                id,
                enforcement: LinuxEnforcementReceipt {
                    network_policy_enforced: !matches!(
                        request.network,
                        mobile_linux_api::NetworkPolicy::Allowed
                    ),
                    memory_limit_enforced: request.resource_limits.max_memory_mb.is_some(),
                },
            })
        }

        async fn write_raw_stdio(
            &self,
            _handle: &RawStdioSessionHandle,
            _input: Vec<u8>,
        ) -> Result<(), MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }

        async fn read_raw_stdio(
            &self,
            _handle: &RawStdioSessionHandle,
            _max_bytes: usize,
        ) -> Result<RawStdioReadResult, MobileLinuxError> {
            assert!(
                !self.raw_stdio_read_panic.swap(false, Ordering::AcqRel),
                "injected panic during native read"
            );
            if let Some(read) = self
                .raw_stdio_reads
                .lock()
                .expect("raw stdio read mutex")
                .pop_front()
            {
                return Ok(read);
            }
            if let Some(error) = self.raw_stdio_read_error.lock().unwrap().take() {
                return Err(error);
            }
            if self.raw_stdio_never_close.load(Ordering::Acquire) {
                return Ok(RawStdioReadResult {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    closed: false,
                    exit_code: None,
                });
            }
            Ok(RawStdioReadResult {
                stdout: Vec::new(),
                stderr: Vec::new(),
                closed: true,
                exit_code: Some(0),
            })
        }

        async fn close_raw_stdio(
            &self,
            handle: &RawStdioSessionHandle,
        ) -> Result<(), MobileLinuxError> {
            self.raw_stdio_close_calls.fetch_add(1, Ordering::AcqRel);
            // Model Android's contract: the native handle is consumed before
            // awaiting the reap, and a repeated close returns unknown handle.
            if !self
                .raw_stdio_close_claimed
                .lock()
                .unwrap()
                .insert(handle.id.clone())
            {
                return Err(MobileLinuxError::InvalidRequest(
                    "unknown raw stdio session".into(),
                ));
            }
            self.raw_stdio_close_started.notify_one();
            let gate = self.raw_stdio_close_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            if let Some(error) = self.raw_stdio_close_error.lock().unwrap().take() {
                return Err(error);
            }
            self.raw_stdio_never_close.store(false, Ordering::Release);
            self.raw_closed.store(true, Ordering::Release);
            self.raw_stdio_closed
                .lock()
                .expect("raw stdio closed mutex")
                .push(handle.id.clone());
            Ok(())
        }

        async fn stop_raw_stdio(
            &self,
            handle: &RawStdioSessionHandle,
            _max_bytes_per_stream: usize,
        ) -> Result<RawStdioStopResult, MobileLinuxError> {
            // This fixture injects a tree receipt; legacy close alone never
            // establishes this proof for a real Android or iOS backend.
            self.close_raw_stdio(handle).await?;
            let mut result = self
                .raw_stdio_stop_result
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| RawStdioStopResult {
                    session_id: handle.id.clone(),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    stdout_eof: true,
                    stderr_eof: true,
                    exit_code: None,
                    termination_requested: Some(true),
                    cleanup: RawStdioTreeStatus::TreeReaped,
                    failure: None,
                });
            if result.session_id.is_empty() {
                result.session_id = handle.id.clone();
            }
            Ok(result)
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
                guest_path: "/home/lingxi/lingxi/project".to_string(),
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

    fn test_profile() -> BrandRuntimeProfile {
        BrandRuntimeProfile::default()
    }

    fn foreground_fixture(
        runtime: Arc<MockRuntime>,
        stdin: Option<String>,
        timeout: std::time::Duration,
        owner: &str,
    ) -> (MobileLinuxProcessRunner, SandboxedCommand) {
        let runtime_trait: Arc<dyn MobileLinuxRuntime> = runtime.clone();
        let sandbox = MobileLinuxSandbox::new(runtime_trait.clone(), Vec::new(), test_profile())
            .expect("sandbox");
        let command = ProcessCommand {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "printf safe".into(), "arg with spaces".into()],
            cwd: None,
            env: HashMap::from([("SHELL_FIXTURE".into(), "kept".into())]),
            timeout: Some(timeout),
            stdin,
        };
        let policy = SandboxPolicy {
            network: mobile_linux_api::NetworkPolicy::Disabled,
            writable_paths: Vec::new(),
            denied_paths: Vec::new(),
            allow_subprocess: true,
            limits: ResourceLimits::default(),
        };
        let sandboxed = sandbox
            .prepare(command, &policy)
            .expect("sandboxed command")
            .with_process_owner(Some(owner.to_string()))
            .with_cleanup_receipt(Arc::new(ProcessCleanupReceipt::default()));
        (MobileLinuxProcessRunner::new(runtime_trait), sandboxed)
    }

    #[tokio::test]
    async fn foreground_raw_stdio_preserves_argv_environment_and_split_utf8() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime.raw_stdio_reads.lock().expect("raw reads").extend([
            RawStdioReadResult {
                stdout: vec![0xE2],
                stderr: b"err".to_vec(),
                closed: false,
                exit_code: None,
            },
            RawStdioReadResult {
                stdout: vec![0x82, 0xAC],
                stderr: Vec::new(),
                closed: true,
                exit_code: Some(7),
            },
        ]);
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(1),
            "main/shell-utf8",
        );

        let result = runner
            .run_foreground_with_output_limit(&command, Some(4096))
            .await
            .expect("foreground command");
        let lingxi_core::host::ForegroundOutcome::Completed(output) = result.outcome else {
            panic!("raw stdio does not background commands");
        };
        assert_eq!(output.stdout, "€");
        assert_eq!(output.stderr, "err");
        assert_eq!(output.exit_code, 7);

        let requests = runtime.raw_stdio_requests.lock().expect("raw requests");
        let request = requests.first().expect("one raw process");
        assert_eq!(request.command, "/bin/sh");
        assert_eq!(request.args[0], "-c");
        assert_eq!(request.args[1], "exec \"$@\" </dev/null");
        assert_eq!(request.args[2], "lingxi-mobile-foreground");
        assert_eq!(request.args[3], "/bin/sh");
        assert_eq!(
            request.args[4..]
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["-c", "printf safe", "arg with spaces"]
        );
        assert_eq!(
            request.env.get("SHELL_FIXTURE").map(String::as_str),
            Some("kept")
        );
        assert_eq!(request.network, mobile_linux_api::NetworkPolicy::Disabled);
        assert_eq!(runtime.raw_stdio_closed.lock().expect("closed").len(), 1);
    }

    #[tokio::test]
    async fn verified_completed_output_reports_real_per_stream_truncation() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime
            .raw_stdio_reads
            .lock()
            .unwrap()
            .push_back(RawStdioReadResult {
                stdout: b"abcdef".to_vec(),
                stderr: b"warning".to_vec(),
                closed: true,
                exit_code: Some(0),
            });
        let (runner, command) = foreground_fixture(
            runtime,
            None,
            std::time::Duration::from_secs(1),
            "main/truncated-complete",
        );
        let (command, _, receipt) = with_test_cleanup_lease(command);
        let result = runner
            .run_foreground_with_output_limit(&command, Some(3))
            .await
            .unwrap();
        let lingxi_core::host::ForegroundOutcome::Completed(output) = result.outcome else {
            panic!("expected completed output")
        };
        assert_eq!(output.stdout, "abc");
        assert_eq!(output.stderr, "war");
        assert_eq!(
            receipt.snapshot().output_truncation,
            Some(ProcessCleanupOutputTruncation {
                stdout_truncated: true,
                stderr_truncated: true,
            })
        );
        assert!(receipt.snapshot().observation.is_none());
    }

    #[tokio::test]
    async fn foreground_timeout_returns_partial_output_after_verified_reap() {
        // This verifies the runner's handling of a mock receipt, not native
        // process-tree cleanup on Android or iOS.
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime.raw_stdio_never_close.store(true, Ordering::Release);
        runtime
            .raw_stdio_reads
            .lock()
            .unwrap()
            .push_back(RawStdioReadResult {
                stdout: b"partial stdout".to_vec(),
                stderr: b"partial stderr".to_vec(),
                closed: false,
                exit_code: Some(7),
            });
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(1),
            "main/shell-timeout",
        );
        let (command, _, receipt) = with_test_cleanup_lease(command);
        let result = runner
            .run_foreground_with_output_limit(&command, Some(64))
            .await
            .expect("timed-out command is returned only after cleanup");
        let lingxi_core::host::ForegroundOutcome::Completed(output) = result.outcome else {
            panic!("raw stdio does not background commands");
        };
        assert!(output.timed_out);
        assert_eq!(output.stdout, "partial stdout");
        assert_eq!(output.stderr, "partial stderr");
        assert_eq!(output.exit_code, 7);
        assert!(runtime.raw_stdio_reads.lock().unwrap().is_empty());
        assert_eq!(receipt.status(), ProcessCleanupStatus::Verified);
        assert_eq!(runtime.raw_stdio_closed.lock().expect("closed").len(), 1);
    }

    #[tokio::test]
    async fn foreground_timeout_without_exit_status_is_not_fabricated() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime.raw_stdio_never_close.store(true, Ordering::Release);
        runtime
            .raw_stdio_reads
            .lock()
            .unwrap()
            .push_back(RawStdioReadResult {
                stdout: b"partial stdout".to_vec(),
                stderr: b"partial stderr".to_vec(),
                closed: false,
                exit_code: None,
            });
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(1),
            "main/timeout-missing-exit",
        );
        let (command, _, receipt) = with_test_cleanup_lease(command);
        let error = runner
            .run_foreground_with_output_limit(&command, Some(64))
            .await
            .expect_err("an unknown exit status cannot be represented by ProcessOutput");
        assert!(matches!(error, ProcessError::Io(ref detail)
            if detail.contains("raw process result is incomplete")));
        assert!(runtime.raw_stdio_reads.lock().unwrap().is_empty());
        assert_eq!(
            receipt.status(),
            lingxi_core::host::sandbox::ProcessCleanupStatus::Verified
        );
        let observation = receipt
            .snapshot()
            .observation
            .expect("missing-exit observation");
        assert_eq!(observation.parts().exit_code, None);
        assert_eq!(observation.parts().stdout, "partial stdout");
        assert_eq!(observation.parts().stderr, "partial stderr");
        assert!(observation.parts().timed_out);
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn raw_read_timeout_is_not_reported_as_the_command_deadline() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime
            .raw_stdio_reads
            .lock()
            .unwrap()
            .push_back(RawStdioReadResult {
                stdout: b"partial stdout".to_vec(),
                stderr: b"partial stderr".to_vec(),
                closed: false,
                exit_code: None,
            });
        *runtime.raw_stdio_read_error.lock().unwrap() = Some(MobileLinuxError::Timeout);
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(5),
            "main/read-timeout",
        );
        let (command, _, receipt) = with_test_cleanup_lease(command);
        let error = runner
            .run_foreground_with_output_limit(&command, Some(64))
            .await
            .expect_err("a read failure is not a completed command timeout");
        assert!(matches!(error, ProcessError::Io(ref detail)
            if detail.contains("raw process result is incomplete")));
        assert!(runtime.raw_stdio_reads.lock().unwrap().is_empty());
        assert_eq!(
            receipt.status(),
            lingxi_core::host::sandbox::ProcessCleanupStatus::Verified
        );
        let observation = receipt
            .snapshot()
            .observation
            .expect("read failure observation");
        assert_eq!(observation.parts().stdout, "partial stdout");
        assert_eq!(observation.parts().stderr, "partial stderr");
        assert!(!observation.parts().timed_out);
        assert!(observation
            .parts()
            .original_error
            .as_deref()
            .unwrap()
            .contains("raw stdio read returned timeout"));
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn foreground_timeout_does_not_hide_unverified_cleanup() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime.raw_stdio_never_close.store(true, Ordering::Release);
        runtime
            .raw_stdio_reads
            .lock()
            .unwrap()
            .push_back(RawStdioReadResult {
                stdout: b"unconfirmed output".to_vec(),
                stderr: Vec::new(),
                closed: false,
                exit_code: None,
            });
        *runtime.raw_stdio_close_error.lock().unwrap() =
            Some(MobileLinuxError::Io("native reap failed".into()));
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_millis(200),
            "main/timeout-close-error",
        );
        let (command, dropped, receipt) = with_test_cleanup_lease(command);
        assert!(runner
            .run_foreground_with_output_limit(&command, Some(64))
            .await
            .is_err());
        assert!(matches!(
            receipt.status(),
            ProcessCleanupStatus::Unverified(_)
        ));
        let observation = receipt
            .snapshot()
            .observation
            .expect("failed-stop observation");
        assert_eq!(observation.parts().stdout, "unconfirmed output");
        assert!(observation.parts().timed_out);
        assert!(observation
            .parts()
            .original_error
            .as_deref()
            .unwrap()
            .contains("native reap failed"));
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
        drop(command);
        drop(runner);
        assert!(!dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn real_deadline_keeps_stop_tail_and_unverified_evidence() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime.raw_stdio_never_close.store(true, Ordering::Release);
        runtime
            .raw_stdio_reads
            .lock()
            .unwrap()
            .push_back(RawStdioReadResult {
                stdout: b"before".to_vec(),
                stderr: b"warning".to_vec(),
                closed: false,
                exit_code: None,
            });
        *runtime.raw_stdio_stop_result.lock().unwrap() = Some(RawStdioStopResult {
            session_id: String::new(),
            stdout: b"-late".to_vec(),
            stderr: b"-late".to_vec(),
            stdout_truncated: false,
            stderr_truncated: true,
            stdout_eof: false,
            stderr_eof: true,
            exit_code: None,
            termination_requested: Some(true),
            cleanup: RawStdioTreeStatus::Unverified("tree not proven".into()),
            failure: Some(MobileLinuxError::Io("drain failed".into())),
        });
        let (runner, command) = foreground_fixture(
            runtime,
            None,
            std::time::Duration::from_millis(30),
            "main/tail-timeout",
        );
        let (command, _, receipt) = with_test_cleanup_lease(command);
        assert!(runner
            .run_foreground_with_output_limit(&command, Some(128))
            .await
            .is_err());
        let snapshot = receipt.snapshot();
        assert!(matches!(
            snapshot.status,
            lingxi_core::host::sandbox::ProcessCleanupStatus::Unverified(_)
        ));
        let observation = snapshot.observation.unwrap();
        let output = observation.parts();
        assert_eq!(output.stdout, "before-late");
        assert_eq!(output.stderr, "warning-late");
        assert!(output.timed_out);
        assert_eq!(output.termination_requested, Some(true));
        assert!(!output.stdout_eof && output.stderr_eof);
        assert!(!output.stdout_truncated && output.stderr_truncated);
        assert_eq!(output.exit_code, None);
        assert_eq!(output.tree_status, ProcessTreeStatus::Unverified);
        assert!(output
            .original_error
            .as_deref()
            .unwrap()
            .contains("drain failed"));
    }

    #[tokio::test]
    async fn read_and_stop_failures_preserve_both_errors_and_captured_output() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime
            .raw_stdio_reads
            .lock()
            .unwrap()
            .push_back(RawStdioReadResult {
                stdout: b"prefix".to_vec(),
                stderr: Vec::new(),
                closed: false,
                exit_code: None,
            });
        *runtime.raw_stdio_read_error.lock().unwrap() =
            Some(MobileLinuxError::Io("read failed".into()));
        *runtime.raw_stdio_stop_result.lock().unwrap() = Some(RawStdioStopResult {
            session_id: String::new(),
            stdout: b"-tail".to_vec(),
            stderr: b"warning".to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_eof: false,
            stderr_eof: false,
            exit_code: None,
            termination_requested: Some(true),
            cleanup: RawStdioTreeStatus::Unverified("tree not proven".into()),
            failure: Some(MobileLinuxError::Io("stop drain failed".into())),
        });
        let (runner, command) = foreground_fixture(
            runtime,
            None,
            std::time::Duration::from_secs(2),
            "main/read-and-stop-errors",
        );
        let (command, _, receipt) = with_test_cleanup_lease(command);
        assert!(runner
            .run_foreground_with_output_limit(&command, Some(128))
            .await
            .is_err());
        let observation = receipt.snapshot().observation.unwrap();
        let output = observation.parts();
        assert_eq!(output.stdout, "prefix-tail");
        assert_eq!(output.stderr, "warning");
        assert!(!output.timed_out);
        let original = output.original_error.as_deref().unwrap();
        assert!(original.contains("read failed") && original.contains("stop drain failed"));
    }

    #[tokio::test]
    async fn no_cleanup_receipt_rejects_before_open() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let runner = MobileLinuxProcessRunner::new(runtime.clone());
        let command = SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "/bin/sh".into(),
                args: Vec::new(),
                cwd: None,
                env: HashMap::new(),
                timeout: None,
                stdin: None,
            },
            SandboxedTag::Wrapped {
                backend: SandboxBackend::AndroidProot,
            },
        );
        let result = runner
            .run_foreground_with_output_limit(&command, None)
            .await;
        assert!(
            matches!(result, Err(ProcessError::Io(ref error)) if error.contains("receipt is required"))
        );
        assert!(runtime.raw_stdio_requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn successful_stop_without_tree_proof_remains_unverified() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::IosIsh));
        runtime
            .raw_stdio_reads
            .lock()
            .unwrap()
            .push_back(RawStdioReadResult {
                stdout: b"ok".to_vec(),
                stderr: Vec::new(),
                closed: true,
                exit_code: Some(0),
            });
        *runtime.raw_stdio_stop_result.lock().unwrap() = Some(RawStdioStopResult {
            session_id: String::new(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_eof: false,
            stderr_eof: false,
            exit_code: Some(0),
            termination_requested: Some(false),
            cleanup: RawStdioTreeStatus::Unverified("no tree receipt".into()),
            failure: None,
        });
        let (runner, command) = foreground_fixture(
            runtime,
            None,
            std::time::Duration::from_secs(1),
            "main/unverified-normal",
        );
        let (command, _, receipt) = with_test_cleanup_lease(command);
        assert!(runner
            .run_foreground_with_output_limit(&command, Some(64))
            .await
            .is_err());
        let snapshot = receipt.snapshot();
        assert!(matches!(
            snapshot.status,
            lingxi_core::host::sandbox::ProcessCleanupStatus::Unverified(_)
        ));
        let output = snapshot.observation.unwrap();
        assert_eq!(output.parts().stdout, "ok");
        assert_eq!(output.parts().exit_code, Some(0));
        assert_eq!(output.parts().tree_status, ProcessTreeStatus::Unverified);
        assert!(!output.parts().stdout_eof && !output.parts().stderr_eof);
    }

    #[tokio::test]
    async fn stop_from_another_session_cannot_supply_tree_proof() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime
            .raw_stdio_reads
            .lock()
            .unwrap()
            .push_back(RawStdioReadResult {
                stdout: b"prefix".to_vec(),
                stderr: Vec::new(),
                closed: true,
                exit_code: Some(0),
            });
        *runtime.raw_stdio_stop_result.lock().unwrap() = Some(RawStdioStopResult {
            session_id: "another-session".into(),
            stdout: b"foreign".to_vec(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_eof: true,
            stderr_eof: true,
            exit_code: Some(0),
            termination_requested: Some(false),
            cleanup: RawStdioTreeStatus::TreeReaped,
            failure: None,
        });
        let (runner, command) = foreground_fixture(
            runtime,
            None,
            std::time::Duration::from_secs(1),
            "main/wrong-session",
        );
        let (command, _, receipt) = with_test_cleanup_lease(command);
        assert!(runner
            .run_foreground_with_output_limit(&command, Some(64))
            .await
            .is_err());
        let snapshot = receipt.snapshot();
        assert!(matches!(
            snapshot.status,
            lingxi_core::host::sandbox::ProcessCleanupStatus::Unverified(_)
        ));
        let output = snapshot.observation.unwrap();
        assert_eq!(output.parts().stdout, "prefix");
        assert_eq!(output.parts().tree_status, ProcessTreeStatus::Unverified);
        assert!(output
            .parts()
            .original_error
            .as_deref()
            .unwrap()
            .contains("different session identity"));
    }

    #[tokio::test]
    async fn negative_exit_sentinel_cannot_become_a_real_exit() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime
            .raw_stdio_reads
            .lock()
            .unwrap()
            .push_back(RawStdioReadResult {
                stdout: b"prefix".to_vec(),
                stderr: Vec::new(),
                closed: true,
                exit_code: Some(-1),
            });
        *runtime.raw_stdio_stop_result.lock().unwrap() = Some(RawStdioStopResult {
            session_id: String::new(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_eof: true,
            stderr_eof: true,
            exit_code: Some(0),
            termination_requested: Some(false),
            cleanup: RawStdioTreeStatus::TreeReaped,
            failure: None,
        });
        let (runner, command) = foreground_fixture(
            runtime,
            None,
            std::time::Duration::from_secs(1),
            "main/negative-exit",
        );
        let (command, _, receipt) = with_test_cleanup_lease(command);
        assert!(runner
            .run_foreground_with_output_limit(&command, Some(64))
            .await
            .is_err());
        let observation = receipt.snapshot().observation.unwrap();
        assert_eq!(observation.parts().exit_code, None);
        assert_eq!(observation.parts().stdout, "prefix");
        assert!(observation
            .parts()
            .original_error
            .as_deref()
            .unwrap()
            .contains("negative exit sentinel"));
    }

    #[tokio::test]
    async fn duplicate_session_is_quarantined_without_signalling_the_existing_owner() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        *runtime.raw_stdio_open_id.lock().unwrap() = Some("duplicate-session".into());
        runtime.raw_stdio_never_close.store(true, Ordering::Release);
        let (runner, first_command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(3),
            "main/first-duplicate",
        );
        let runner = Arc::new(runner);
        let first_runner = runner.clone();
        let first = tokio::spawn(async move {
            first_runner
                .run_foreground_with_output_limit(&first_command, Some(64))
                .await
        });
        wait_until(|| {
            runner
                .foreground_owners
                .lock()
                .unwrap()
                .get("main/first-duplicate")
                .is_some_and(|owner| !owner.sessions.is_empty())
        })
        .await;
        let (_, second_command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(1),
            "main/second-duplicate",
        );
        let (second_command, _, second_receipt) = with_test_cleanup_lease(second_command);
        assert!(runner
            .run_foreground_with_output_limit(&second_command, Some(64))
            .await
            .is_err());
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 0);
        let snapshot = second_receipt.snapshot();
        assert!(matches!(
            snapshot.status,
            lingxi_core::host::sandbox::ProcessCleanupStatus::Unverified(_)
        ));
        assert!(snapshot
            .observation
            .unwrap()
            .parts()
            .original_error
            .as_deref()
            .unwrap()
            .contains("duplicate raw session"));
        assert!(!runner.foreground_owners.lock().unwrap()["main/first-duplicate"].cancelled);
        runner.kill_owner_processes("main/first-duplicate").await;
        let _ = first.await.unwrap();
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn foreground_with_stdin_fails_before_opening_raw_session() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            Some("input".into()),
            std::time::Duration::from_secs(1),
            "main/shell-stdin",
        );
        let (command, _dropped, receipt) = with_test_cleanup_lease(command);
        assert!(matches!(
            runner
                .run_foreground_with_output_limit(&command, Some(64))
                .await,
            Err(ProcessError::Unsupported)
        ));
        assert!(runtime
            .raw_stdio_requests
            .lock()
            .expect("raw requests")
            .is_empty());
        assert_eq!(
            receipt.status(),
            lingxi_core::host::sandbox::ProcessCleanupStatus::Verified
        );
    }

    #[tokio::test]
    async fn owner_cancel_before_raw_session_registration_closes_the_late_session() {
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let open_gate = Arc::new(Notify::new());
        *runtime.raw_stdio_open_gate.lock().expect("open gate") = Some(open_gate.clone());
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(1),
            "agent:42/shell-cancel-race",
        );
        let runner = Arc::new(runner);
        let task_runner = runner.clone();
        let run = tokio::spawn(async move {
            task_runner
                .run_foreground_with_output_limit(&command, Some(64))
                .await
        });
        runtime.raw_stdio_open_started.notified().await;

        // The owner is registered before the async open. Cancellation latches
        // there, so the session returned afterward is closed before execution
        // can be reported as accepted.
        assert!(runner.kill_owner_processes("agent:42").await.is_empty());
        open_gate.notify_one();
        assert!(run.await.expect("runner task").is_err());
        assert_eq!(runtime.raw_stdio_closed.lock().expect("closed").len(), 1);
    }

    struct LeaseDropProbe(Arc<AtomicBool>);
    impl Drop for LeaseDropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    fn with_test_cleanup_lease(
        command: SandboxedCommand,
    ) -> (
        SandboxedCommand,
        Arc<AtomicBool>,
        Arc<lingxi_core::host::sandbox::ProcessCleanupReceipt>,
    ) {
        let dropped = Arc::new(AtomicBool::new(false));
        let lease = LeaseDropProbe(dropped.clone());
        let receipt = Arc::new(lingxi_core::host::sandbox::ProcessCleanupReceipt::default());
        (
            command
                .with_lifetime_guard(lease)
                .with_cleanup_receipt(receipt.clone()),
            dropped,
            receipt,
        )
    }

    async fn wait_until(mut ready: impl FnMut() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !ready() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("raw stdio condition did not become ready");
    }

    #[tokio::test]
    async fn dropped_engine_waits_for_late_open_and_closes_once() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let open_gate = Arc::new(Notify::new());
        *runtime.raw_stdio_open_gate.lock().unwrap() = Some(open_gate.clone());
        let owner = "main/late-open";
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(30),
            owner,
        );
        let runner = Arc::new(runner);
        let (command, dropped, receipt) = with_test_cleanup_lease(command);
        let engine = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let worker = runner.clone();
        engine.spawn(async move {
            let _ = worker
                .run_foreground_with_output_limit(&command, None)
                .await;
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            runtime.raw_stdio_open_started.notified(),
        )
        .await
        .unwrap();
        engine.shutdown_background();
        wait_until(|| {
            runner
                .foreground_owners
                .lock()
                .unwrap()
                .get(owner)
                .is_some_and(|state| state.cancelled)
        })
        .await;
        assert!(!dropped.load(Ordering::Acquire));
        assert_eq!(receipt.status(), ProcessCleanupStatus::Pending);
        assert!(!receipt.wait_expired());
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 0);

        open_gate.notify_one();
        wait_until(|| !runner.foreground_owners.lock().unwrap().contains_key(owner)).await;
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
        assert_eq!(runtime.raw_stdio_closed.lock().unwrap().len(), 1);
        assert_eq!(receipt.status(), ProcessCleanupStatus::Verified);
        wait_until(|| dropped.load(Ordering::Acquire)).await;
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn dropped_close_waiter_shares_one_android_close_receipt() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime.raw_stdio_never_close.store(true, Ordering::Release);
        let close_gate = Arc::new(Notify::new());
        *runtime.raw_stdio_close_gate.lock().unwrap() = Some(close_gate.clone());
        let owner = "main/close-waiter";
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(30),
            owner,
        );
        let runner = Arc::new(runner);
        let (command, dropped, receipt) = with_test_cleanup_lease(command);
        let engine = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let run_runner = runner.clone();
        engine.spawn(async move {
            let _ = run_runner
                .run_foreground_with_output_limit(&command, None)
                .await;
        });
        wait_until(|| {
            runner
                .foreground_owners
                .lock()
                .unwrap()
                .get(owner)
                .is_some_and(|state| !state.sessions.is_empty())
        })
        .await;
        let session = runner.foreground_owners.lock().unwrap()[owner]
            .sessions
            .values()
            .next()
            .unwrap()
            .clone();
        let cancel_runner = runner.clone();
        engine.spawn(async move {
            cancel_runner.kill_owner_processes(owner).await;
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            runtime.raw_stdio_close_started.notified(),
        )
        .await
        .unwrap();
        engine.shutdown_background();
        assert!(!dropped.load(Ordering::Acquire));
        assert_eq!(receipt.status(), ProcessCleanupStatus::Pending);
        let waiter_runner = runner.clone();
        let waiter = tokio::spawn(async move { waiter_runner.close_raw_session(&session).await });
        close_gate.notify_one();
        assert!(waiter.await.unwrap().is_ok());
        wait_until(|| !runner.foreground_owners.lock().unwrap().contains_key(owner)).await;
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
        assert_eq!(receipt.status(), ProcessCleanupStatus::Verified);
        wait_until(|| dropped.load(Ordering::Acquire)).await;
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn failed_android_close_is_not_retried_or_released_as_verified() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        *runtime.raw_stdio_close_error.lock().unwrap() =
            Some(MobileLinuxError::Io("reap failed".into()));
        let owner = "main/close-error";
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(3),
            owner,
        );
        let (command, dropped, receipt) = with_test_cleanup_lease(command);
        let error = runner
            .run_foreground_with_output_limit(&command, None)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains(lingxi_core::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED));
        assert!(matches!(
            receipt.status(),
            ProcessCleanupStatus::Unverified(_)
        ));
        let session = runner.foreground_owners.lock().unwrap()[owner]
            .sessions
            .values()
            .next()
            .unwrap()
            .clone();
        assert!(runner.close_raw_session(&session).await.is_err());
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
        drop(command);
        drop(runner);
        assert!(!dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn open_error_without_receipt_quarantines_the_owner() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::IosIsh));
        *runtime.raw_stdio_open_error.lock().unwrap() = Some(MobileLinuxError::Io(
            "native open failed after possible spawn".into(),
        ));
        let owner = "main/open-error";
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(3),
            owner,
        );
        let (command, dropped, receipt) = with_test_cleanup_lease(command);
        let error = runner
            .run_foreground_with_output_limit(&command, None)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains(lingxi_core::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED));
        assert!(matches!(
            receipt.status(),
            ProcessCleanupStatus::Unverified(_)
        ));
        assert!(runner.foreground_owners.lock().unwrap()[owner]
            .open_unverified
            .is_some());
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 0);
        drop(command);
        drop(runner);
        assert!(!dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn pending_close_keeps_lease_and_does_not_spawn_retries() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let close_gate = Arc::new(Notify::new());
        *runtime.raw_stdio_close_gate.lock().unwrap() = Some(close_gate.clone());
        let owner = "main/pending-close";
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(3),
            owner,
        );
        let runner = Arc::new(runner);
        let (command, dropped, receipt) = with_test_cleanup_lease(command);
        let worker = runner.clone();
        let mut running = tokio::spawn(async move {
            worker
                .run_foreground_with_output_limit(&command, None)
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            runtime.raw_stdio_close_started.notified(),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut running)
                .await
                .is_err()
        );
        assert_eq!(receipt.status(), ProcessCleanupStatus::Pending);
        assert!(!dropped.load(Ordering::Acquire));
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
        assert!(runner.foreground_owners.lock().unwrap().contains_key(owner));

        close_gate.notify_one();
        running.await.unwrap().unwrap();
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
        assert_eq!(receipt.status(), ProcessCleanupStatus::Verified);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn pending_open_deadline_retains_lease_and_closes_late_session_once() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let open_gate = Arc::new(Notify::new());
        *runtime.raw_stdio_open_gate.lock().unwrap() = Some(open_gate.clone());
        let owner = "main/open-deadline";
        let (mut runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_millis(300),
            owner,
        );
        runner.cleanup_grace = std::time::Duration::from_millis(80);
        let runner = Arc::new(runner);
        let (command, dropped, receipt) = with_test_cleanup_lease(command);
        let worker = runner.clone();
        let task = tokio::spawn(async move {
            worker
                .run_foreground_with_output_limit(&command, None)
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            runtime.raw_stdio_open_started.notified(),
        )
        .await
        .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .expect("the caller's wait is bounded")
            .unwrap();
        assert!(result.is_err());
        assert!(receipt.wait_expired());
        assert_eq!(receipt.status(), ProcessCleanupStatus::Pending);
        assert!(!dropped.load(Ordering::Acquire));
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 0);
        assert!(runner.foreground_owners.lock().unwrap().contains_key(owner));

        open_gate.notify_one();
        wait_until(|| !runner.foreground_owners.lock().unwrap().contains_key(owner)).await;
        wait_until(|| dropped.load(Ordering::Acquire)).await;
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
        assert_eq!(receipt.status(), ProcessCleanupStatus::Verified);
        assert!(
            receipt.wait_expired(),
            "the original caller still timed out"
        );
        let late = receipt
            .snapshot()
            .observation
            .expect("late open observation");
        assert!(late.parts().timed_out);
        assert_eq!(late.parts().tree_status, ProcessTreeStatus::VerifiedReaped);
        assert_eq!(late.parts().exit_code, None);
    }

    #[tokio::test]
    async fn pending_open_deadline_quarantines_a_late_open_error() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::IosIsh));
        let open_gate = Arc::new(Notify::new());
        *runtime.raw_stdio_open_gate.lock().unwrap() = Some(open_gate.clone());
        *runtime.raw_stdio_open_error.lock().unwrap() = Some(MobileLinuxError::Io(
            "native open failed after possible spawn".into(),
        ));
        let owner = "main/open-deadline-error";
        let (mut runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_millis(300),
            owner,
        );
        runner.cleanup_grace = std::time::Duration::from_millis(80);
        let runner = Arc::new(runner);
        let (command, dropped, receipt) = with_test_cleanup_lease(command);
        let worker = runner.clone();
        let task = tokio::spawn(async move {
            worker
                .run_foreground_with_output_limit(&command, None)
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            runtime.raw_stdio_open_started.notified(),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(receipt.wait_expired());
        assert_eq!(receipt.status(), ProcessCleanupStatus::Pending);
        assert!(!dropped.load(Ordering::Acquire));
        open_gate.notify_one();
        wait_until(|| matches!(receipt.status(), ProcessCleanupStatus::Unverified(_))).await;
        let late = receipt
            .snapshot()
            .observation
            .expect("late open error observation");
        assert!(late.parts().timed_out);
        assert!(late
            .parts()
            .original_error
            .as_deref()
            .unwrap()
            .contains("native open failed"));
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 0);
        drop(runner);
        assert!(!dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn pending_close_deadline_retains_owner_until_late_receipt() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        for fails in [false, true] {
            let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
            let close_gate = Arc::new(Notify::new());
            *runtime.raw_stdio_close_gate.lock().unwrap() = Some(close_gate.clone());
            if fails {
                *runtime.raw_stdio_close_error.lock().unwrap() =
                    Some(MobileLinuxError::Io("late reap failed".into()));
            }
            let owner = if fails {
                "main/close-deadline-error"
            } else {
                "main/close-deadline-success"
            };
            let (mut runner, command) = foreground_fixture(
                runtime.clone(),
                None,
                std::time::Duration::from_millis(300),
                owner,
            );
            runner.cleanup_grace = std::time::Duration::from_millis(80);
            let runner = Arc::new(runner);
            let (command, dropped, receipt) = with_test_cleanup_lease(command);
            let worker = runner.clone();
            let task = tokio::spawn(async move {
                worker
                    .run_foreground_with_output_limit(&command, None)
                    .await
            });
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                runtime.raw_stdio_close_started.notified(),
            )
            .await
            .unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(3), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
            assert!(receipt.wait_expired());
            assert_eq!(receipt.status(), ProcessCleanupStatus::Pending);
            assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
            assert!(!dropped.load(Ordering::Acquire));

            close_gate.notify_one();
            if fails {
                wait_until(|| matches!(receipt.status(), ProcessCleanupStatus::Unverified(_)))
                    .await;
                let late = receipt
                    .snapshot()
                    .observation
                    .expect("late stop observation");
                assert!(late
                    .parts()
                    .original_error
                    .as_deref()
                    .unwrap()
                    .contains("late reap failed"));
                assert!(receipt.wait_expired());
                drop(runner);
                assert!(!dropped.load(Ordering::Acquire));
            } else {
                wait_until(|| !runner.foreground_owners.lock().unwrap().contains_key(owner)).await;
                wait_until(|| dropped.load(Ordering::Acquire)).await;
                assert_eq!(receipt.status(), ProcessCleanupStatus::Verified);
            }
            assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
            assert!(receipt.wait_expired());
        }
    }

    #[tokio::test]
    async fn native_open_time_counts_against_execution_deadline() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let open_gate = Arc::new(Notify::new());
        *runtime.raw_stdio_open_gate.lock().unwrap() = Some(open_gate.clone());
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_millis(20),
            "main/open-counts-toward-timeout",
        );
        let (command, _, receipt) = with_test_cleanup_lease(command);
        let worker = Arc::new(runner);
        let task = tokio::spawn(async move {
            worker
                .run_foreground_with_output_limit(&command, None)
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            runtime.raw_stdio_open_started.notified(),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        open_gate.notify_one();
        let error = tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .expect_err("deadline elapsed before any exit status could be observed");
        assert!(matches!(error, ProcessError::Io(ref detail)
            if detail.contains("raw process result is incomplete")));
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
        assert_eq!(receipt.status(), ProcessCleanupStatus::Verified);
        let observation = receipt
            .snapshot()
            .observation
            .expect("deadline observation");
        assert!(observation.parts().timed_out);
        assert_eq!(observation.parts().exit_code, None);
        assert!(!receipt.wait_expired());
    }

    #[tokio::test]
    async fn reused_receipt_cannot_acknowledge_an_inflight_owner() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let open_gate = Arc::new(Notify::new());
        *runtime.raw_stdio_open_gate.lock().unwrap() = Some(open_gate.clone());
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(3),
            "main/shared-receipt",
        );
        let runner = Arc::new(runner);
        let (command, _, receipt) = with_test_cleanup_lease(command);
        let worker = runner.clone();
        let first_command = command.clone();
        let first = tokio::spawn(async move {
            worker
                .run_foreground_with_output_limit(&first_command, None)
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            runtime.raw_stdio_open_started.notified(),
        )
        .await
        .unwrap();

        // A second runner makes the same receipt race across runner instances.
        let other = MobileLinuxProcessRunner::new(runtime.clone());
        assert!(other
            .run_foreground_with_output_limit(&command, None)
            .await
            .is_err());
        assert_eq!(receipt.status(), ProcessCleanupStatus::Pending);
        assert_eq!(runtime.raw_stdio_requests.lock().unwrap().len(), 1);
        open_gate.notify_one();
        first.await.unwrap().unwrap();
        assert_eq!(receipt.status(), ProcessCleanupStatus::Verified);
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
        assert!(other
            .run_foreground_with_output_limit(&command, None)
            .await
            .is_err());
        assert_eq!(runtime.raw_stdio_requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn duplicate_owner_rejection_verifies_only_the_new_receipt() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let open_gate = Arc::new(Notify::new());
        *runtime.raw_stdio_open_gate.lock().unwrap() = Some(open_gate.clone());
        let (runner, first_command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(3),
            "main/duplicate-owner",
        );
        let runner = Arc::new(runner);
        let (first_command, _, first_receipt) = with_test_cleanup_lease(first_command);
        let second_receipt = Arc::new(lingxi_core::host::sandbox::ProcessCleanupReceipt::default());
        let second_command = first_command
            .clone()
            .with_cleanup_receipt(second_receipt.clone());
        let worker = runner.clone();
        let first = tokio::spawn(async move {
            worker
                .run_foreground_with_output_limit(&first_command, None)
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            runtime.raw_stdio_open_started.notified(),
        )
        .await
        .unwrap();
        assert!(runner
            .run_foreground_with_output_limit(&second_command, None)
            .await
            .is_err());
        assert_eq!(second_receipt.status(), ProcessCleanupStatus::Verified);
        assert_eq!(first_receipt.status(), ProcessCleanupStatus::Pending);
        assert_eq!(runtime.raw_stdio_requests.lock().unwrap().len(), 1);
        open_gate.notify_one();
        first.await.unwrap().unwrap();
        assert_eq!(first_receipt.status(), ProcessCleanupStatus::Verified);
    }

    #[tokio::test]
    async fn stale_generation_cannot_release_or_cancel_reused_owner() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let owner = "main/generation-race";
        let (runner, first_command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(3),
            owner,
        );
        let runner = Arc::new(runner);
        let (first_command, first_dropped, first_receipt) = with_test_cleanup_lease(first_command);
        let claim = CleanupReceiptClaim::acquire(first_command.cleanup_receipt()).unwrap();
        let old_claim = claim.clone().unwrap();
        old_claim
            .receipt
            .bind_session(&old_claim.token, "old-generation-session")
            .unwrap();
        let old_generation = runner
            .begin_foreground_owner(
                owner,
                first_command.lifetime_guard().cloned(),
                Some(first_receipt.clone()),
                claim,
                raw_foreground_capacity().try_acquire_owned().unwrap(),
            )
            .unwrap();
        let (old_permit, old_lifetime_guard) = runner
            .foreground_retained_owner(owner, &old_generation)
            .unwrap();
        drop(first_command);
        let old_session = Arc::new(OwnedRawStdio {
            handle: RawStdioSessionHandle {
                id: "old-generation-session".into(),
                enforcement: LinuxEnforcementReceipt::default(),
            },
            receipt_claim: Some(old_claim),
            capture: Mutex::new(RawStdioCapture {
                exit_code: Some(0),
                ..Default::default()
            }),
            capture_limit: RAW_STDIO_DEFAULT_CAPTURE_BYTES,
            _runtime_guard: runtime.clone(),
            _owner_permit: old_permit,
            _lifetime_guard: old_lifetime_guard,
            close_state: Mutex::new(RawStdioCloseState::Open),
            close_notify: Notify::new(),
        });
        assert!(!runner
            .register_foreground_session(owner, &old_generation, old_session.clone())
            .unwrap());
        let stopped = runner.close_raw_session(&old_session).await;
        finish_raw_session(&old_session, stopped, RAW_STDIO_DEFAULT_CAPTURE_BYTES, None).unwrap();
        runner.unregister_foreground_session(owner, &old_generation, &old_session.handle.id);
        assert_eq!(first_receipt.status(), ProcessCleanupStatus::Verified);
        assert!(!first_dropped.load(Ordering::Acquire));
        let old_session_id = old_session.handle.id.clone();
        drop(old_session);

        let (_, blocked_command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(3),
            owner,
        );
        let (blocked_command, _, blocked_receipt) = with_test_cleanup_lease(blocked_command);
        assert!(runner
            .run_foreground_with_output_limit(&blocked_command, None)
            .await
            .is_err());
        assert_eq!(blocked_receipt.status(), ProcessCleanupStatus::Verified);
        runner.finish_foreground_owner(owner, &old_generation);
        assert!(first_dropped.load(Ordering::Acquire));

        let open_gate = Arc::new(Notify::new());
        *runtime.raw_stdio_open_gate.lock().unwrap() = Some(open_gate.clone());
        let (_, second_command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(3),
            owner,
        );
        let (second_command, second_dropped, second_receipt) =
            with_test_cleanup_lease(second_command);
        let worker = runner.clone();
        let second = tokio::spawn(async move {
            worker
                .run_foreground_with_output_limit(&second_command, None)
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            runtime.raw_stdio_open_started.notified(),
        )
        .await
        .unwrap();
        // Replay delayed callbacks from the old generation while the new
        // generation's native open is still pending.
        runner.unregister_foreground_session(owner, &old_generation, &old_session_id);
        runner.mark_open_unverified(owner, &old_generation, "stale failure".into());
        runner
            .cancel_foreground_instance(owner, &old_generation)
            .await;
        runner.finish_foreground_owner(owner, &old_generation);
        {
            let owners = runner.foreground_owners.lock().unwrap();
            let current = owners.get(owner).expect("new owner remains registered");
            assert!(!Arc::ptr_eq(&current.generation, &old_generation));
            assert!(!current.cancelled);
            assert!(current.open_unverified.is_none());
        }
        assert!(!second_dropped.load(Ordering::Acquire));
        assert_eq!(second_receipt.status(), ProcessCleanupStatus::Pending);
        open_gate.notify_one();
        second.await.unwrap().unwrap();
        assert_eq!(second_receipt.status(), ProcessCleanupStatus::Verified);
        assert!(second_dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn panic_during_native_open_is_unverified_and_quarantined() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::IosIsh));
        runtime.raw_stdio_open_panic.store(true, Ordering::Release);
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(3),
            "main/open-panic",
        );
        let (command, dropped, receipt) = with_test_cleanup_lease(command);
        assert!(runner
            .run_foreground_with_output_limit(&command, None)
            .await
            .is_err());
        assert!(matches!(
            receipt.status(),
            ProcessCleanupStatus::Unverified(_)
        ));
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 0);
        drop(command);
        drop(runner);
        assert!(!dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn panic_during_read_reaps_known_session_before_releasing_owner() {
        use lingxi_core::host::sandbox::ProcessCleanupStatus;
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime.raw_stdio_read_panic.store(true, Ordering::Release);
        let (runner, command) = foreground_fixture(
            runtime.clone(),
            None,
            std::time::Duration::from_secs(3),
            "main/read-panic",
        );
        let (command, dropped, receipt) = with_test_cleanup_lease(command);
        assert!(runner
            .run_foreground_with_output_limit(&command, None)
            .await
            .is_err());
        assert_eq!(runtime.raw_stdio_close_calls.load(Ordering::Acquire), 1);
        assert_eq!(runtime.raw_stdio_closed.lock().unwrap().len(), 1);
        assert_eq!(receipt.status(), ProcessCleanupStatus::Verified);
        drop(command);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn foreground_capacity_refuses_open_before_side_effects() {
        const CHILD: &str = "LINGXI_RAW_FOREGROUND_CAPACITY_CHILD";
        const MARKER: &str = "raw-foreground-capacity-verified";
        if std::env::var_os(CHILD).is_some() {
            let permits = (0..MAX_RAW_FOREGROUND_OWNERS)
                .map(|_| raw_foreground_capacity().try_acquire_owned().unwrap())
                .collect::<Vec<_>>();
            let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
            let (runner, command) = foreground_fixture(
                runtime.clone(),
                None,
                std::time::Duration::from_secs(3),
                "main/capacity",
            );
            let (command, _, receipt) = with_test_cleanup_lease(command);
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = executor.block_on(runner.run_foreground_with_output_limit(&command, None));
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted"));
            assert_eq!(
                receipt.status(),
                lingxi_core::host::sandbox::ProcessCleanupStatus::Verified
            );
            assert!(runtime.raw_stdio_requests.lock().unwrap().is_empty());
            drop(permits);
            assert!(raw_foreground_capacity().try_acquire_owned().is_ok());
            println!("{MARKER}");
            return;
        }

        // This limit is process-global. A subprocess makes the saturation
        // test independent of other foreground tests running in parallel.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "mobile_linux::tests::foreground_capacity_refuses_open_before_side_effects",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "child stdout: {stdout}\nchild stderr: {stderr}"
        );
        assert!(
            stdout.contains(MARKER),
            "child did not execute the capacity test: {stdout}"
        );
    }

    #[test]
    fn mobile_linux_sandbox_maps_host_paths_into_guest_request() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("workspace")).expect("workspace");
        fs::create_dir_all(temp.path().join("readonly")).expect("readonly");
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let sandbox = MobileLinuxSandbox::new(runtime, sample_mounts(&temp), test_profile())
            .expect("sandbox");

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
        assert_eq!(
            plan.request.cwd.as_deref(),
            Some("/home/lingxi/lingxi/project")
        );
        assert_eq!(
            plan.request.network,
            mobile_linux_api::NetworkPolicy::Disabled
        );
        assert_eq!(plan.request.mounts.len(), 2);
    }

    #[test]
    fn mobile_linux_sandbox_uses_selected_profile_for_project_source_writes_and_cwd() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile = BrandRuntimeProfile::new("atlas", "atlas_guest", ".atlas");
        let home = temp.path().join("persistent/atlas_guest");
        let workspace = home.join("atlas/projects");
        let source = home.join(".atlas/.apps/calorie-a1b2c3d4/source");
        fs::create_dir_all(&workspace).expect("primary workspace");
        fs::create_dir_all(&source).expect("Project source");
        let mounts = vec![
            MountSpec {
                host_path: workspace,
                guest_path: mobile_linux_api::guest_paths::workspace_for_profile(
                    &profile, "projects",
                ),
                read_only: false,
                purpose: mobile_linux_api::MountPurpose::Workspace,
            },
            MountSpec {
                host_path: source.clone(),
                guest_path: profile
                    .apps_guest_path()
                    .join("calorie-a1b2c3d4/source")
                    .to_string_lossy()
                    .into_owned(),
                read_only: false,
                purpose: mobile_linux_api::MountPurpose::External,
            },
        ];
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(MockRuntime::new(SandboxBackend::IosIsh));
        let sandbox = MobileLinuxSandbox::new(runtime, mounts, profile).expect("sandbox");
        let policy = SandboxPolicy {
            network: mobile_linux_api::NetworkPolicy::Disabled,
            writable_paths: vec![source.clone()],
            denied_paths: Vec::new(),
            allow_subprocess: true,
            limits: ResourceLimits::default(),
        };
        let command = ProcessCommand {
            command: "/bin/sh".into(),
            args: Vec::new(),
            cwd: Some(source),
            env: HashMap::new(),
            timeout: None,
            stdin: None,
        };

        let prepared = sandbox
            .prepare(command.clone(), &policy)
            .expect("selected-profile source is writable and host-backed");
        let plan = prepared
            .backend_plan()
            .expect("backend plan")
            .downcast::<MobileLinuxSandboxPlan>()
            .expect("plan type");
        assert_eq!(
            plan.request.cwd.as_deref(),
            Some("/home/atlas_guest/.atlas/.apps/calorie-a1b2c3d4/source")
        );

        let foreign_account_command = ProcessCommand {
            cwd: Some(PathBuf::from(
                "/home/lingxi/.lingxi/.apps/calorie-a1b2c3d4/source",
            )),
            ..command
        };
        let error = sandbox
            .prepare(foreign_account_command, &policy)
            .expect_err("the default account guest path must not pass Atlas policy");
        assert!(matches!(error, SandboxError::Unavailable(_)));
    }

    #[tokio::test]
    async fn mobile_linux_sandbox_probe_does_not_overclaim_enforcement() {
        let runtime: Arc<dyn MobileLinuxRuntime> =
            Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let sandbox =
            MobileLinuxSandbox::new(runtime, Vec::new(), test_profile()).expect("sandbox");

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
        let sandbox = MobileLinuxSandbox::new(runtime, sample_mounts(&temp), test_profile())
            .expect("sandbox");
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
        let sandbox = MobileLinuxSandbox::new(runtime, sample_mounts(&temp), test_profile())
            .expect("sandbox");
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
        let sandbox = MobileLinuxSandbox::new(runtime, sample_mounts(&temp), test_profile())
            .expect("sandbox");
        let mut cmd = sample_command(&temp);
        // Linux tempdirs live under /tmp, which is also an explicitly allowed
        // guest cwd. Use an unmapped path outside every allowed guest root.
        cmd.cwd = Some(PathBuf::from("/unmounted-host").join(temp.path().file_name().unwrap()));

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
            guest_path: "/home/lingxi/lingxi/secrets".to_string(),
            read_only: true,
            purpose: mobile_linux_api::MountPurpose::Shared,
        }];
        let error = MobileLinuxSandbox::new(runtime.clone(), sensitive, test_profile())
            .expect_err("sensitive path must be rejected");
        assert!(matches!(error, SandboxError::Unavailable(_)));

        fs::create_dir_all(temp.path().join("workspace")).expect("workspace");
        fs::create_dir_all(temp.path().join("workspace-sub")).expect("workspace-sub");
        let overlapping = vec![
            MountSpec {
                host_path: temp.path().join("workspace"),
                guest_path: "/home/lingxi/lingxi/project".to_string(),
                read_only: false,
                purpose: mobile_linux_api::MountPurpose::Workspace,
            },
            MountSpec {
                host_path: temp.path().join("workspace-sub"),
                guest_path: "/home/lingxi/lingxi/project/sub".to_string(),
                read_only: false,
                purpose: mobile_linux_api::MountPurpose::Shared,
            },
        ];
        let error = MobileLinuxSandbox::new(runtime.clone(), overlapping, test_profile())
            .expect_err("overlapping guest targets must be rejected");
        assert!(matches!(error, SandboxError::Unavailable(_)));

        fs::create_dir_all(temp.path().join("external")).expect("external");
        let external_rw = vec![MountSpec {
            host_path: temp.path().join("external"),
            guest_path: "/home/lingxi/lingxi/external".to_string(),
            read_only: false,
            purpose: mobile_linux_api::MountPurpose::External,
        }];
        MobileLinuxSandbox::new(runtime, external_rw, test_profile())
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
                guest_path: "/home/lingxi/lingxi/writable-alias".to_string(),
                read_only: false,
                purpose: mobile_linux_api::MountPurpose::Workspace,
            },
        ];
        assert!(matches!(
            MobileLinuxSandbox::new(runtime.clone(), aliases, test_profile()),
            Err(SandboxError::Unavailable(_))
        ));

        let nested_alias = vec![
            MountSpec {
                host_path: host.clone(),
                guest_path: "/home/lingxi/lingxi/project".to_string(),
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
            MobileLinuxSandbox::new(runtime, nested_alias, test_profile()),
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
            guest_path: "/home/lingxi/lingxi/project".to_string(),
            read_only: false,
            purpose: mobile_linux_api::MountPurpose::Workspace,
        }];

        let error = MobileLinuxSandbox::new(runtime, mounts, test_profile())
            .expect_err("symlinked mount root must be rejected");
        assert!(matches!(error, SandboxError::Unavailable(_)));
    }

    #[tokio::test]
    async fn side_raw_foreground_cancellation_reaps_and_never_spawns_background() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("workspace")).unwrap();
        fs::create_dir_all(temp.path().join("readonly")).unwrap();
        let runtime = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        runtime.raw_stdio_never_close.store(true, Ordering::Release);
        let sandbox =
            MobileLinuxSandbox::new(runtime.clone(), sample_mounts(&temp), test_profile()).unwrap();
        let mut request = sample_command(&temp);
        request.stdin = None;
        request.args = vec!["-c".into(), "pwd".into()];
        let command = sandbox
            .prepare(request, &sample_policy(&temp))
            .unwrap()
            .with_process_owner(Some("side:one".into()));
        let runner = Arc::new(MobileLinuxProcessRunner::new(runtime.clone()));
        let run_runner = runner.clone();
        let running = tokio::spawn(async move { run_runner.run(&command).await });
        let owned_session_id = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(session_id) = runner
                    .foreground_owners
                    .lock()
                    .unwrap()
                    .get("side:one")
                    .and_then(|owner| owner.sessions.keys().next().cloned())
                {
                    break session_id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("side owner should expose its registered raw session");
        runner.kill_owner_processes("side:other").await;
        assert!(!runtime.raw_closed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(runtime.raw_stdio_closed.lock().unwrap().is_empty());
        runner.kill_owner_processes("side:one").await;
        running.await.unwrap().unwrap();
        assert!(runtime.raw_closed.load(std::sync::atomic::Ordering::SeqCst));
        let closed_sessions = runtime.raw_stdio_closed.lock().unwrap();
        assert_eq!(closed_sessions.len(), 1);
        assert_eq!(
            closed_sessions.first().map(String::as_str),
            Some(owned_session_id.as_str()),
            "the side owner must close its own raw session"
        );
        assert!(runtime.last_background.lock().unwrap().is_empty());
        assert!(runtime.last_run.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mobile_linux_process_runner_maps_run_and_streaming() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("workspace")).expect("workspace");
        fs::create_dir_all(temp.path().join("readonly")).expect("readonly");
        let runtime_impl = Arc::new(MockRuntime::new(SandboxBackend::AndroidProot));
        let runtime: Arc<dyn MobileLinuxRuntime> = runtime_impl.clone();
        let sandbox =
            MobileLinuxSandbox::new(runtime.clone(), sample_mounts(&temp), test_profile())
                .expect("sandbox");
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
            MobileLinuxSandbox::new(runtime.clone(), sample_mounts(&temp), test_profile())
                .expect("sandbox");
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
            MobileLinuxSandbox::new(runtime.clone(), sample_mounts(&temp), test_profile())
                .expect("sandbox");
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
            MobileLinuxSandbox::new(runtime.clone(), sample_mounts(&temp), test_profile())
                .expect("sandbox");
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
