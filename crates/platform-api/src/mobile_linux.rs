//! Mobile Linux runtime seam shared by Android and iOS.
//!
//! The execution backend that runs a Linux userspace on mobile devices (Android
//! PRoot, iOS iSH, or a temporary unavailable stub) hangs off this trait. The
//! desktop stack does not implement it: desktop keeps using the existing
//! `ProcessRunner` + `Sandbox` pipeline unchanged.

#![allow(missing_docs)]

use crate::process::{ProcessError, ProcessOutput, ProcessStreamSink};
use crate::sandbox::{NetworkPolicy, ResourceLimits, SandboxBackend};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

/// Maximum number of events a single `read_events` call should return.
pub const MAX_MOBILE_LINUX_EVENT_BATCH: usize = 512;

/// The guest-path atlas — the ONE definition of the well-known mobile-linux
/// guest paths and the `/workspace/<id>` format.
///
/// Before this module, the same knowledge was constructed independently in
/// four Rust sites (`platform-common`'s writable allow-list, `platform-ios`'s
/// workspace mount, `platform-ios-ish-runtime`'s status report and its config
/// helper) and seven Swift sites — drift between them produced real bugs
/// (display-label mount rows shipped into `openPty`; a managed-root spelling
/// split). Every Rust consumer now derives from here.
///
/// The Swift twin is `LXISHGuestPaths` (clients/ios
/// `LXISHNativeRootfs.swift`); the two are pinned to identical literals by
/// `guest_paths::tests` on this side and
/// `LXISHRuntimeBundleManifestTests.testGuestPathAtlasMatchesRustTwin` on the
/// Swift side — change one and the other's pin fails.
pub mod guest_paths {
    /// The guest home (`~` of the interactive shell). Host-backed by the
    /// rootfs manager's `persistent/root` bind mount.
    pub const HOME: &str = "/root";
    /// Scratch areas writable inside the guest.
    pub const SCRATCH: &[&str] = &["/tmp", "/var/tmp"];
    /// Parent of every per-workspace mount.
    pub const WORKSPACE_ROOT: &str = "/workspace";
    /// Root of the local-app build channels.
    pub const LOCAL_APP_BUILD_ROOT: &str = "/var/lingxi/local-app-build";
    /// Host-owned pnpm content-addressable store used only during dependency
    /// installation. It is never mounted for Vite builds or generated code.
    pub const LOCAL_APP_DEPENDENCY_STORE: &str = "/var/lingxi/local-app-dependency-store";
    /// The project-root leaf below a local-app build channel.
    pub const LOCAL_APP_BUILD_PROJECT_DIR: &str = "project";

    /// THE `/workspace/<id>` format — previously duplicated as a format
    /// string in six places across two languages.
    #[must_use]
    pub fn workspace(stable_workspace_id: &str) -> String {
        format!("{WORKSPACE_ROOT}/{stable_workspace_id}")
    }

    /// The isolated project root used by a local-app build.
    #[must_use]
    pub fn local_app_build_project(app_id: &str, channel: &str) -> String {
        format!("{LOCAL_APP_BUILD_ROOT}/{app_id}/{channel}/{LOCAL_APP_BUILD_PROJECT_DIR}")
    }

    /// Guest prefixes a sandbox policy may declare writable.
    #[must_use]
    pub fn writable_roots() -> [&'static str; 4] {
        [HOME, SCRATCH[0], SCRATCH[1], WORKSPACE_ROOT]
    }

    #[cfg(test)]
    mod tests {
        /// Byte-pins the atlas atoms. The Swift twin (`LXISHGuestPaths`)
        /// pins the SAME literals — drift on either side fails one of the
        /// twins.
        #[test]
        fn atlas_atoms_are_pinned() {
            assert_eq!(super::HOME, "/root");
            assert_eq!(super::SCRATCH, &["/tmp", "/var/tmp"]);
            assert_eq!(super::WORKSPACE_ROOT, "/workspace");
            assert_eq!(super::LOCAL_APP_BUILD_ROOT, "/var/lingxi/local-app-build");
            assert_eq!(
                super::LOCAL_APP_DEPENDENCY_STORE,
                "/var/lingxi/local-app-dependency-store"
            );
            assert_eq!(super::LOCAL_APP_BUILD_PROJECT_DIR, "project");
            assert_eq!(super::workspace("abc-123"), "/workspace/abc-123");
            assert_eq!(
                super::local_app_build_project("abc-123", "store"),
                "/var/lingxi/local-app-build/abc-123/store/project"
            );
            assert_eq!(
                super::writable_roots(),
                ["/root", "/tmp", "/var/tmp", "/workspace"]
            );
        }
    }
}

/// Shared mobile runtime mode switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MobileLinuxRuntimeMode {
    /// Keep the legacy mobile execution path.
    Legacy,
    /// Route mobile shell/process operations through the Linux userspace runtime.
    MobileLinux,
}

/// Live capability snapshot of the mobile Linux backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MobileLinuxCapability {
    /// Whether the runtime can be used right now.
    pub available: bool,
    /// Backend kind to surface in telemetry / UI.
    pub backend: SandboxBackend,
    /// Runtime mode currently selected by the host.
    pub mode: MobileLinuxRuntimeMode,
    /// Human-readable reason when unavailable.
    pub reason: Option<String>,
    /// Whether stdout/stderr streaming is supported.
    pub streaming_output: bool,
    /// Whether background processes are supported.
    pub background_processes: bool,
    /// Whether PTY sessions are supported.
    pub pty: bool,
    /// Whether bind mounts are supported.
    pub bind_mounts: bool,
    /// Whether integrity verification / repair is supported.
    pub rootfs_integrity: bool,
}

/// High-level status of a mobile Linux task.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MobileLinuxTaskStatus {
    Queued,
    Running,
    Backgrounded,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

/// Detailed event payload emitted by a mobile Linux backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MobileLinuxEventKind {
    /// Process/task lifecycle transition.
    TaskStatusChanged {
        status: MobileLinuxTaskStatus,
        exit_code: Option<i32>,
        detail: Option<String>,
    },
    /// One stdout line.
    StdoutLine { line: String },
    /// One stderr chunk.
    StderrChunk { chunk: Vec<u8> },
    /// PTY output bytes.
    PtyOutput { session_id: String, data: Vec<u8> },
    /// PTY session closed.
    PtyClosed {
        session_id: String,
        exit_code: Option<i32>,
        detail: Option<String>,
    },
    /// Backend/runtime-level failure not tied to a single stdout/stderr frame.
    RuntimeError { detail: String },
}

/// Runtime lifecycle / IO event emitted by a mobile Linux backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MobileLinuxEvent {
    pub sequence: u64,
    pub task_id: Option<String>,
    pub kind: MobileLinuxEventKind,
}

/// Snapshot of one task/session managed by the runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MobileLinuxTaskSnapshot {
    pub task_id: String,
    pub status: MobileLinuxTaskStatus,
    pub command: String,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub detail: Option<String>,
}

/// High-level rootfs lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RootfsState {
    Missing,
    Installing,
    Ready,
    Corrupt,
    Repairing,
    Resetting,
    Unsupported,
    BlockedByLicense,
}

/// Rootfs inventory + lifecycle snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootfsStatus {
    pub state: RootfsState,
    pub backend: SandboxBackend,
    pub mode: MobileLinuxRuntimeMode,
    pub platform: String,
    pub abi: String,
    pub version: Option<String>,
    pub managed_root: Option<PathBuf>,
    pub active_root: Option<PathBuf>,
    pub staged_root: Option<PathBuf>,
    pub archive_sha256: Option<String>,
    pub installed_size_bytes: Option<u64>,
    pub writable_guest_paths: Vec<String>,
    pub last_error: Option<String>,
}

/// Host→guest bind mount description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub read_only: bool,
    pub purpose: MountPurpose,
}

/// Why a mount exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MountPurpose {
    Workspace,
    LocalAppBuild,
    Memory,
    Skills,
    Shared,
    External,
    Temp,
}

/// Structured request for a Linux command run inside the mobile runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinuxCommandRequest {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub stdin: Option<String>,
    pub timeout_ms: Option<u64>,
    pub network: NetworkPolicy,
    #[serde(default)]
    pub resource_limits: ResourceLimits,
    pub mounts: Vec<MountSpec>,
}

/// Prepared sandbox plan passed from the mobile-linux sandbox adapter to the
/// process runner through [`crate::sandbox::BackendPlanHandle`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MobileLinuxSandboxPlan {
    pub backend: SandboxBackend,
    pub mode: MobileLinuxRuntimeMode,
    pub request: LinuxCommandRequest,
    pub mounts: Vec<MountSpec>,
    pub limits: ResourceLimits,
    pub allow_subprocess: bool,
}

/// Collected command result from the mobile runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinuxCommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub cancelled: bool,
    #[serde(default)]
    pub enforcement: LinuxEnforcementReceipt,
}

/// Proof returned by a mobile Linux backend for the isolation requested by a
/// command. Callers that require a policy must fail closed unless its bit is
/// true; a backend must never report enforcement it did not apply to the whole
/// guest process group.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinuxEnforcementReceipt {
    pub network_policy_enforced: bool,
    pub memory_limit_enforced: bool,
}

impl LinuxEnforcementReceipt {
    /// Validate the receipt against a request whose isolation is security
    /// critical. `Allowed` needs no network isolation proof; every stricter
    /// policy and every configured memory ceiling does.
    pub fn ensure_for(
        self,
        network: NetworkPolicy,
        limits: ResourceLimits,
    ) -> Result<(), MobileLinuxError> {
        if !matches!(network, NetworkPolicy::Allowed) && !self.network_policy_enforced {
            return Err(MobileLinuxError::NetworkPolicyUnavailable(format!(
                "backend did not enforce requested {network:?} network policy"
            )));
        }
        if limits.max_memory_mb.is_some() && !self.memory_limit_enforced {
            return Err(MobileLinuxError::ResourceLimitExceeded(
                "backend did not enforce requested resident-memory limit".into(),
            ));
        }
        Ok(())
    }
}

impl From<ProcessOutput> for LinuxCommandResult {
    fn from(value: ProcessOutput) -> Self {
        Self {
            stdout: value.stdout,
            stderr: value.stderr,
            exit_code: value.exit_code,
            timed_out: value.timed_out,
            cancelled: false,
            enforcement: LinuxEnforcementReceipt::default(),
        }
    }
}

impl From<LinuxCommandResult> for ProcessOutput {
    fn from(value: LinuxCommandResult) -> Self {
        Self {
            stdout: value.stdout,
            stderr: value.stderr,
            exit_code: value.exit_code,
            timed_out: value.timed_out,
        }
    }
}

/// Opaque background process handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinuxProcessHandle {
    pub id: String,
    #[serde(default)]
    pub enforcement: LinuxEnforcementReceipt,
}

/// Structured request for a long-lived raw stdio process inside the mobile
/// runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawStdioOpenRequest {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub network: NetworkPolicy,
    #[serde(default)]
    pub resource_limits: ResourceLimits,
    pub mounts: Vec<MountSpec>,
}

/// Opaque handle for one raw-stdio mobile process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawStdioSessionHandle {
    pub id: String,
    #[serde(default)]
    pub enforcement: LinuxEnforcementReceipt,
}

/// One bounded poll of stdout/stderr bytes from a raw-stdio mobile process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawStdioReadResult {
    #[serde(default)]
    pub stdout: Vec<u8>,
    #[serde(default)]
    pub stderr: Vec<u8>,
    pub closed: bool,
    pub exit_code: Option<i32>,
}

/// PTY open request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyOpenRequest {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub size: PtySize,
    pub mounts: Vec<MountSpec>,
}

/// Opaque PTY handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtySessionHandle {
    pub id: String,
}

/// PTY size in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

/// Error surface shared by mobile Linux runtime implementations.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum MobileLinuxError {
    #[error("unsupported on this platform")]
    Unsupported,
    #[error("runtime unavailable: {0}")]
    Unavailable(String),
    #[error("license blocked: {0}")]
    LicenseBlocked(String),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("network_policy_unavailable: {0}")]
    NetworkPolicyUnavailable(String),
    #[error("resource_limit_exceeded: {0}")]
    ResourceLimitExceeded(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("timeout")]
    Timeout,
}

impl From<ProcessError> for MobileLinuxError {
    fn from(value: ProcessError) -> Self {
        match value {
            ProcessError::Unsupported => Self::Unsupported,
            ProcessError::Timeout => Self::Timeout,
            ProcessError::Io(message)
            | ProcessError::PolicyUnsupported(message)
            | ProcessError::MalformedSandboxPlan(message)
            | ProcessError::SandboxEnforcementFailed(message) => Self::Io(message),
        }
    }
}

#[cfg(test)]
mod map_guest_path_tests {
    use super::*;

    fn mounts() -> Vec<MountSpec> {
        vec![
            MountSpec {
                host_path: PathBuf::from("/host/ws"),
                guest_path: "/workspace/abc".into(),
                read_only: false,
                purpose: MountPurpose::Workspace,
            },
            MountSpec {
                host_path: PathBuf::from("/host/ext"),
                guest_path: "/workspace/abc/ext".into(),
                read_only: false,
                purpose: MountPurpose::External,
            },
        ]
    }

    #[test]
    fn maps_longest_prefix_and_rejects_dirty_or_uncovered_paths() {
        let m = mounts();
        assert_eq!(
            map_guest_path_to_host("/workspace/abc/src/a.rs", &m),
            Some(PathBuf::from("/host/ws/src/a.rs"))
        );
        assert_eq!(
            map_guest_path_to_host("/workspace/abc", &m),
            Some(PathBuf::from("/host/ws"))
        );
        assert_eq!(
            map_guest_path_to_host("/workspace/abc/ext/d.txt", &m),
            Some(PathBuf::from("/host/ext/d.txt"))
        );
        assert_eq!(map_guest_path_to_host("/workspace/other/x", &m), None);
        assert_eq!(map_guest_path_to_host("/workspace/abc/../abc/x", &m), None);
        assert_eq!(map_guest_path_to_host("relative", &m), None);
        assert_eq!(map_guest_path_to_host("/tmp/x", &m), None);
    }

    #[test]
    fn maps_host_paths_back_to_the_longest_guest_mount() {
        let m = mounts();
        assert_eq!(
            map_host_path_to_guest(std::path::Path::new("/host/ws/src/a.rs"), &m).as_deref(),
            Some("/workspace/abc/src/a.rs")
        );
        assert_eq!(
            map_host_path_to_guest(std::path::Path::new("/host/ext/d.txt"), &m).as_deref(),
            Some("/workspace/abc/ext/d.txt")
        );
        assert_eq!(
            map_host_path_to_guest(std::path::Path::new("/host/other"), &m),
            None
        );
        assert_eq!(
            map_host_path_to_guest(std::path::Path::new("/host/ws/../escape"), &m),
            None
        );
    }
}

/// Map a guest path onto its host twin via the longest-prefix matching
/// mount. Pure and lexical: no filesystem access. Returns `None` when no
/// mount covers `path` or the path fails strict guest-path shape checks
/// (absolute, no `..`/`.` components, no `//`).
///
/// This is the shared guest→host hop for PRESENTATION consumers (the mobile
/// host's prompt-probe resolver). The file-tool translation layer
/// (`platform_common::GuestPathFileSystem`) keeps its own richer resolution
/// (fence + read-only enforcement) pinned by its own tests.
#[must_use]
pub fn map_guest_path_to_host(path: &str, mounts: &[MountSpec]) -> Option<std::path::PathBuf> {
    find_guest_mount(path, mounts).map(|(_, host)| host)
}

/// Map a native host path back to its model-visible guest coordinate via the
/// longest matching mount. Pure and lexical; dirty or relative paths fail
/// closed so prompt context never exposes a native backing path.
#[must_use]
pub fn map_host_path_to_guest(path: &std::path::Path, mounts: &[MountSpec]) -> Option<String> {
    use std::path::Component;

    if !path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return None;
    }
    let mount = mounts
        .iter()
        .filter(|mount| {
            mount.host_path.is_absolute()
                && (path == mount.host_path || path.starts_with(&mount.host_path))
        })
        .max_by_key(|mount| mount.host_path.components().count())?;
    let relative = path.strip_prefix(&mount.host_path).ok()?;
    let mut guest = std::path::PathBuf::from(&mount.guest_path);
    for component in relative.components() {
        match component {
            Component::Normal(segment) => guest.push(segment),
            Component::CurDir => {}
            _ => return None,
        }
    }
    let guest = guest.to_str()?;
    find_guest_mount(guest, mounts)?;
    Some(guest.to_owned())
}

/// Like [`map_guest_path_to_host`], but also returns WHICH mount matched —
/// the file-tool translation layer needs it to honor `read_only`. This is
/// the ONE longest-prefix guest-mount matcher; every consumer (the prompt
/// probe resolver, `GuestPathFileSystem`) resolves through it.
#[must_use]
pub fn find_guest_mount<'a>(
    path: &str,
    mounts: &'a [MountSpec],
) -> Option<(&'a MountSpec, std::path::PathBuf)> {
    use std::path::Component;
    if !path.starts_with('/') || path.contains("//") || path.as_bytes().contains(&0) {
        return None;
    }
    let components: Vec<&std::ffi::OsStr> = {
        let mut out = Vec::new();
        for component in std::path::Path::new(path).components() {
            match component {
                Component::RootDir => {}
                Component::Normal(segment) => out.push(segment),
                _ => return None,
            }
        }
        out
    };
    let mut best: Option<(&MountSpec, usize)> = None;
    for mount in mounts {
        let guest: Vec<&std::ffi::OsStr> = {
            let mut out = Vec::new();
            for component in std::path::Path::new(&mount.guest_path).components() {
                match component {
                    Component::RootDir => {}
                    Component::Normal(segment) => out.push(segment),
                    _ => out.clear(),
                }
            }
            out
        };
        if !guest.is_empty()
            && components.len() >= guest.len()
            && components[..guest.len()] == guest[..]
            && best.is_none_or(|(_, len)| guest.len() > len)
        {
            best = Some((mount, guest.len()));
        }
    }
    let (mount, prefix_len) = best?;
    let mut host = mount.host_path.clone();
    for segment in &components[prefix_len..] {
        host.push(segment);
    }
    Some((mount, host))
}

/// Android/iOS Linux userspace runtime.
#[async_trait]
pub trait MobileLinuxRuntime: Send + Sync {
    /// Telemetry / policy backend identity.
    fn backend(&self) -> SandboxBackend;

    /// Runtime mode selected by the host.
    fn mode(&self) -> MobileLinuxRuntimeMode;

    /// Probe the runtime without mutating state.
    async fn probe_capability(&self) -> MobileLinuxCapability;

    /// Ensure the backend is booted and ready to accept requests.
    async fn boot(&self) -> Result<RootfsStatus, MobileLinuxError>;

    /// Stop the backend and release transient resources.
    async fn shutdown(&self) -> Result<(), MobileLinuxError>;

    /// Run a command to completion.
    async fn run(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxCommandResult, MobileLinuxError>;

    /// Run a command to completion using ONLY the mounts supplied in
    /// `request.mounts`.
    ///
    /// This isolates host bind mounts from configured/default workspace and
    /// persistent-home mounts. It does NOT isolate writes inside the shared
    /// managed rootfs; callers that need filesystem write isolation must still
    /// provision a separate writable rootfs layer.
    ///
    /// This entry point is reserved for the local-app builder's single
    /// `LocalAppBuild` project mount. Platform implementations reject empty,
    /// additional, or differently purposed mount sets.
    ///
    /// The default implementation fails closed so an isolated request is never
    /// silently downgraded into the ordinary merged-mount execution path.
    async fn run_isolated(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        let _ = request;
        Err(MobileLinuxError::Unsupported)
    }

    /// Run a command while streaming its output.
    async fn run_streaming(
        &self,
        request: LinuxCommandRequest,
        sink: Arc<dyn ProcessStreamSink>,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        let result = self.run(request).await?;
        for line in result.stdout.lines() {
            sink.stdout_line(line.to_string())
                .await
                .map_err(MobileLinuxError::from)?;
        }
        if !result.stderr.is_empty() {
            sink.stderr_chunk(result.stderr.as_bytes().to_vec())
                .await
                .map_err(MobileLinuxError::from)?;
        }
        Ok(result)
    }

    /// Start a background command.
    async fn spawn_background(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxProcessHandle, MobileLinuxError>;

    /// Kill a background command and synchronously reap its complete guest
    /// process group/tree. Returning `Ok(())` guarantees that no descendant
    /// belonging to this task remains runnable.
    async fn kill(&self, handle: &LinuxProcessHandle) -> Result<(), MobileLinuxError>;

    /// Open a PTY session.
    async fn open_pty(&self, request: PtyOpenRequest)
        -> Result<PtySessionHandle, MobileLinuxError>;

    /// Write to a PTY session.
    async fn write_pty(
        &self,
        handle: &PtySessionHandle,
        input: Vec<u8>,
    ) -> Result<(), MobileLinuxError>;

    /// Resize a PTY session.
    async fn resize_pty(
        &self,
        handle: &PtySessionHandle,
        size: PtySize,
    ) -> Result<(), MobileLinuxError>;

    /// Close a PTY session and reap the complete guest process group/tree
    /// attached to it before returning success.
    async fn close_pty(&self, handle: &PtySessionHandle) -> Result<(), MobileLinuxError>;

    /// Open a long-lived raw stdio process whose stdin/stdout/stderr are
    /// exchanged as opaque byte streams rather than through a PTY.
    ///
    /// The default fails closed so a transport that requires raw byte fidelity
    /// (notably LSP) is never silently downgraded onto another surface.
    async fn open_raw_stdio(
        &self,
        request: RawStdioOpenRequest,
    ) -> Result<RawStdioSessionHandle, MobileLinuxError> {
        let _ = request;
        Err(MobileLinuxError::Unsupported)
    }

    /// Write raw stdin bytes into a previously opened stdio session.
    async fn write_raw_stdio(
        &self,
        handle: &RawStdioSessionHandle,
        input: Vec<u8>,
    ) -> Result<(), MobileLinuxError> {
        let _ = handle;
        let _ = input;
        Err(MobileLinuxError::Unsupported)
    }

    /// Read at most `max_bytes` of newly produced stdout/stderr.
    async fn read_raw_stdio(
        &self,
        handle: &RawStdioSessionHandle,
        max_bytes: usize,
    ) -> Result<RawStdioReadResult, MobileLinuxError> {
        let _ = handle;
        let _ = max_bytes;
        Err(MobileLinuxError::Unsupported)
    }

    /// Close one raw stdio session and synchronously reap its full guest
    /// process tree.
    async fn close_raw_stdio(
        &self,
        handle: &RawStdioSessionHandle,
    ) -> Result<(), MobileLinuxError> {
        let _ = handle;
        Err(MobileLinuxError::Unsupported)
    }

    /// Inspect rootfs state.
    async fn rootfs_status(&self) -> Result<RootfsStatus, MobileLinuxError>;

    /// Recompute and return the current rootfs status.
    async fn verify_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError>;

    /// Attempt a non-destructive repair.
    async fn repair_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError>;

    /// Reset managed rootfs state while preserving external user data.
    async fn reset_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError>;

    /// Replace the active bind-mount set.
    async fn configure_mounts(&self, mounts: Vec<MountSpec>) -> Result<(), MobileLinuxError>;

    /// Snapshot of the live guest↔host mount table, in mount order.
    ///
    /// Every entry is a host-backed bind mount, so a guest path under an
    /// entry has a host twin reachable without entering the emulated kernel —
    /// this is the table `GuestPathFileSystem` translates file-tool paths
    /// against. Runtimes that keep no host-backed mounts (stubs, unavailable
    /// placeholders) inherit the default empty table, which reads as
    /// "nothing is host-backed".
    fn current_mounts(&self) -> Vec<MountSpec> {
        Vec::new()
    }

    /// Read runtime events after the supplied sequence number (exclusive).
    async fn read_events(
        &self,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<MobileLinuxEvent>, MobileLinuxError> {
        let _ = after_sequence;
        let _ = limit.min(MAX_MOBILE_LINUX_EVENT_BATCH);
        Ok(Vec::new())
    }

    /// Enumerate known tasks/sessions managed by the runtime.
    async fn list_tasks(&self) -> Result<Vec<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        Ok(Vec::new())
    }

    /// Read one task/session snapshot by id.
    async fn task_status(
        &self,
        task_id: &str,
    ) -> Result<Option<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        let _ = task_id;
        Ok(None)
    }
}

/// Unavailable / blocked runtime used before the real backend is linked.
#[derive(Debug, Clone)]
pub struct UnavailableMobileLinuxRuntime {
    capability: MobileLinuxCapability,
    status: RootfsStatus,
}

impl UnavailableMobileLinuxRuntime {
    #[must_use]
    pub fn blocked(
        backend: SandboxBackend,
        mode: MobileLinuxRuntimeMode,
        platform: impl Into<String>,
        abi: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        Self {
            capability: MobileLinuxCapability {
                available: false,
                backend,
                mode,
                reason: Some(reason.clone()),
                streaming_output: false,
                background_processes: false,
                pty: false,
                bind_mounts: false,
                rootfs_integrity: false,
            },
            status: RootfsStatus {
                state: RootfsState::BlockedByLicense,
                backend,
                mode,
                platform: platform.into(),
                abi: abi.into(),
                version: None,
                managed_root: None,
                active_root: None,
                staged_root: None,
                archive_sha256: None,
                installed_size_bytes: None,
                writable_guest_paths: vec![],
                last_error: Some(reason),
            },
        }
    }

    #[must_use]
    pub fn unavailable(
        backend: SandboxBackend,
        mode: MobileLinuxRuntimeMode,
        platform: impl Into<String>,
        abi: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        Self {
            capability: MobileLinuxCapability {
                available: false,
                backend,
                mode,
                reason: Some(reason.clone()),
                streaming_output: false,
                background_processes: false,
                pty: false,
                bind_mounts: false,
                rootfs_integrity: false,
            },
            status: RootfsStatus {
                state: RootfsState::Unsupported,
                backend,
                mode,
                platform: platform.into(),
                abi: abi.into(),
                version: None,
                managed_root: None,
                active_root: None,
                staged_root: None,
                archive_sha256: None,
                installed_size_bytes: None,
                writable_guest_paths: vec![],
                last_error: Some(reason),
            },
        }
    }

    fn err(&self) -> MobileLinuxError {
        match self.status.state {
            RootfsState::BlockedByLicense => MobileLinuxError::LicenseBlocked(
                self.status
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "mobile linux runtime is blocked".to_string()),
            ),
            _ => MobileLinuxError::Unavailable(
                self.status
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "mobile linux runtime is unavailable".to_string()),
            ),
        }
    }
}

#[async_trait]
impl MobileLinuxRuntime for UnavailableMobileLinuxRuntime {
    fn backend(&self) -> SandboxBackend {
        self.capability.backend
    }

    fn mode(&self) -> MobileLinuxRuntimeMode {
        self.capability.mode
    }

    async fn probe_capability(&self) -> MobileLinuxCapability {
        self.capability.clone()
    }

    async fn boot(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Err(self.err())
    }

    async fn shutdown(&self) -> Result<(), MobileLinuxError> {
        Ok(())
    }

    async fn run(
        &self,
        _request: LinuxCommandRequest,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        Err(self.err())
    }

    async fn spawn_background(
        &self,
        _request: LinuxCommandRequest,
    ) -> Result<LinuxProcessHandle, MobileLinuxError> {
        Err(self.err())
    }

    async fn kill(&self, _handle: &LinuxProcessHandle) -> Result<(), MobileLinuxError> {
        Err(self.err())
    }

    async fn open_pty(
        &self,
        _request: PtyOpenRequest,
    ) -> Result<PtySessionHandle, MobileLinuxError> {
        Err(self.err())
    }

    async fn write_pty(
        &self,
        _handle: &PtySessionHandle,
        _input: Vec<u8>,
    ) -> Result<(), MobileLinuxError> {
        Err(self.err())
    }

    async fn resize_pty(
        &self,
        _handle: &PtySessionHandle,
        _size: PtySize,
    ) -> Result<(), MobileLinuxError> {
        Err(self.err())
    }

    async fn close_pty(&self, _handle: &PtySessionHandle) -> Result<(), MobileLinuxError> {
        Err(self.err())
    }

    async fn open_raw_stdio(
        &self,
        _request: RawStdioOpenRequest,
    ) -> Result<RawStdioSessionHandle, MobileLinuxError> {
        Err(self.err())
    }

    async fn write_raw_stdio(
        &self,
        _handle: &RawStdioSessionHandle,
        _input: Vec<u8>,
    ) -> Result<(), MobileLinuxError> {
        Err(self.err())
    }

    async fn read_raw_stdio(
        &self,
        _handle: &RawStdioSessionHandle,
        _max_bytes: usize,
    ) -> Result<RawStdioReadResult, MobileLinuxError> {
        Err(self.err())
    }

    async fn close_raw_stdio(
        &self,
        _handle: &RawStdioSessionHandle,
    ) -> Result<(), MobileLinuxError> {
        Err(self.err())
    }

    async fn rootfs_status(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Ok(self.status.clone())
    }

    async fn verify_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Ok(self.status.clone())
    }

    async fn repair_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Ok(self.status.clone())
    }

    async fn reset_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Ok(self.status.clone())
    }

    async fn configure_mounts(&self, _mounts: Vec<MountSpec>) -> Result<(), MobileLinuxError> {
        Err(self.err())
    }

    async fn read_events(
        &self,
        _after_sequence: Option<u64>,
        _limit: usize,
    ) -> Result<Vec<MobileLinuxEvent>, MobileLinuxError> {
        Err(self.err())
    }

    async fn list_tasks(&self) -> Result<Vec<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        Err(self.err())
    }

    async fn task_status(
        &self,
        _task_id: &str,
    ) -> Result<Option<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        Err(self.err())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocked_runtime_reports_license_error_and_status() {
        let runtime = UnavailableMobileLinuxRuntime::blocked(
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            "arm64-v8a",
            "missing additional distribution grant",
        );

        let capability = runtime.probe_capability().await;
        assert!(!capability.available);
        assert_eq!(capability.backend, SandboxBackend::AndroidProot);

        let status = runtime.rootfs_status().await.expect("status");
        assert_eq!(status.state, RootfsState::BlockedByLicense);
        assert_eq!(status.platform, "android");

        let err = runtime.boot().await.expect_err("boot must fail");
        assert!(matches!(err, MobileLinuxError::LicenseBlocked(_)));
    }

    #[tokio::test]
    async fn unavailable_runtime_keeps_verification_non_destructive() {
        let runtime = UnavailableMobileLinuxRuntime::unavailable(
            SandboxBackend::IosIsh,
            MobileLinuxRuntimeMode::MobileLinux,
            "ios",
            "arm64",
            "runtime assets not linked",
        );

        let status = runtime.verify_rootfs().await.expect("verify should report");
        assert_eq!(status.state, RootfsState::Unsupported);
        assert_eq!(status.abi, "arm64");

        let err = runtime
            .open_pty(PtyOpenRequest {
                command: "/bin/sh".to_string(),
                args: vec![],
                cwd: None,
                env: BTreeMap::new(),
                size: PtySize { cols: 80, rows: 24 },
                mounts: vec![],
            })
            .await
            .expect_err("pty must fail");
        assert!(matches!(err, MobileLinuxError::Unavailable(_)));
    }

    #[tokio::test]
    async fn default_run_isolated_fails_closed() {
        let runtime = UnavailableMobileLinuxRuntime::unavailable(
            SandboxBackend::IosIsh,
            MobileLinuxRuntimeMode::MobileLinux,
            "ios",
            "arm64",
            "runtime assets not linked",
        );

        let err = runtime
            .run_isolated(LinuxCommandRequest {
                command: "/bin/true".to_string(),
                args: vec![],
                cwd: None,
                env: BTreeMap::new(),
                stdin: None,
                timeout_ms: None,
                network: NetworkPolicy::Allowed,
                resource_limits: ResourceLimits::default(),
                mounts: vec![],
            })
            .await
            .expect_err("isolated run must fail closed");
        assert!(matches!(err, MobileLinuxError::Unsupported));
    }

    #[tokio::test]
    async fn default_raw_stdio_fails_closed() {
        let runtime = UnavailableMobileLinuxRuntime::unavailable(
            SandboxBackend::IosIsh,
            MobileLinuxRuntimeMode::MobileLinux,
            "ios",
            "arm64",
            "runtime assets not linked",
        );

        let err = runtime
            .open_raw_stdio(RawStdioOpenRequest {
                command: "/bin/cat".to_string(),
                args: vec![],
                cwd: None,
                env: BTreeMap::new(),
                network: NetworkPolicy::Disabled,
                resource_limits: ResourceLimits::default(),
                mounts: vec![],
            })
            .await
            .expect_err("raw stdio must fail closed");
        assert!(matches!(err, MobileLinuxError::Unavailable(_)));
    }

    #[test]
    fn runtime_error_event_round_trips() {
        let event = MobileLinuxEvent {
            sequence: 9,
            task_id: Some("task-1".to_string()),
            kind: MobileLinuxEventKind::RuntimeError {
                detail: "pty backend crashed".to_string(),
            },
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: MobileLinuxEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, event);
    }

    #[test]
    fn enforcement_receipt_fails_closed_for_required_policies() {
        let limits = ResourceLimits {
            max_memory_mb: Some(800),
            ..ResourceLimits::default()
        };
        let missing_network = LinuxEnforcementReceipt {
            network_policy_enforced: false,
            memory_limit_enforced: true,
        }
        .ensure_for(NetworkPolicy::Disabled, limits)
        .expect_err("disabled network needs proof");
        assert!(matches!(
            missing_network,
            MobileLinuxError::NetworkPolicyUnavailable(_)
        ));

        let missing_memory = LinuxEnforcementReceipt {
            network_policy_enforced: true,
            memory_limit_enforced: false,
        }
        .ensure_for(NetworkPolicy::LoopbackOnly, limits)
        .expect_err("memory ceiling needs proof");
        assert!(matches!(
            missing_memory,
            MobileLinuxError::ResourceLimitExceeded(_)
        ));

        LinuxEnforcementReceipt {
            network_policy_enforced: true,
            memory_limit_enforced: true,
        }
        .ensure_for(NetworkPolicy::LoopbackOnly, limits)
        .expect("complete receipt");
    }
}
