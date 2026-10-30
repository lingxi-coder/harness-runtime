//! Process sandbox abstraction.
//!
//! The `Sandbox` trait is the boundary between the engine's command
//! execution layer and platform-specific sandbox implementations. The
//! [`SandboxedCommand`] newtype is the only shape accepted by
//! [`crate::host::process::ProcessRunner`], so a command cannot reach the runner
//! without first passing through a [`Sandbox::prepare`] (or audited bypass)
//! call. This is the type-system enforcement behind decision D2 / spec A1.
//!
//! See spec §24 (Sandbox) and D17 (Runtime boundary).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

/// Platform-specific sandbox backend.
///
/// Implementations live in platform crates. The engine receives an
/// `Arc<dyn Sandbox>` and never invokes a backend directly.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Whether this sandbox can actually wrap commands on the current host.
    fn is_available(&self) -> bool;

    /// Backend kind for telemetry / decision logic.
    fn backend(&self) -> SandboxBackend;

    /// The ONLY way to construct a [`SandboxedCommand`]: through this method,
    /// which applies the policy and tags the command as sandboxed.
    ///
    /// # Errors
    /// Returns [`SandboxError::Unavailable`] when the backend cannot wrap the
    /// command, [`SandboxError::PathCanonicalize`] / [`SandboxError::SymlinkEscape`]
    /// when policy paths cannot be safely resolved, and [`SandboxError::Io`]
    /// for transient platform errors.
    fn prepare(
        &self,
        cmd: ProcessCommand,
        policy: &SandboxPolicy,
    ) -> Result<SandboxedCommand, SandboxError>;

    /// Audited bypass for cases that need it (e.g. user explicitly disabled
    /// sandbox). Records the reason so the audit log can later confirm intent.
    fn bypass_with_audit(&self, cmd: ProcessCommand, reason: &str) -> SandboxedCommand;

    /// Probe the live capabilities of the backend (what features actually
    /// work on this host, e.g. user-namespaces availability on Linux).
    async fn probe_capability(&self) -> SandboxCapability;
}

use mobile_linux_api::SandboxBackend;

/// Declarative sandbox policy applied by [`Sandbox::prepare`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxPolicy {
    /// Outbound network access policy.
    pub network: NetworkPolicy,
    /// Paths the sandboxed process is allowed to write to.
    pub writable_paths: Vec<PathBuf>,
    /// Paths that must remain inaccessible.
    pub denied_paths: Vec<PathBuf>,
    /// Whether the wrapped process may spawn subprocesses.
    pub allow_subprocess: bool,
    /// Resource ceilings enforced by the backend.
    pub limits: ResourceLimits,
}

use mobile_linux_api::{NetworkPolicy, ResourceLimits};

/// Live probe result from [`Sandbox::probe_capability`].
#[derive(Debug, Clone)]
pub struct SandboxCapability {
    /// Whether the backend can wrap commands right now.
    pub available: bool,
    /// Human-readable explanation when unavailable.
    pub reason: Option<String>,
    /// Per-feature flags for the backend.
    pub features: SandboxFeatures,
}

/// Granular feature flags reported by [`Sandbox::probe_capability`].
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // mirrors the OS capability matrix verbatim
pub struct SandboxFeatures {
    /// Backend can isolate the network namespace.
    pub network_isolation: bool,
    /// Backend can mount the root filesystem read-only.
    pub fs_readonly: bool,
    /// Backend can selectively expose writable paths.
    pub fs_readwrite_paths: bool,
    /// Backend can cap the number of processes.
    pub process_limit: bool,
    /// Backend can apply `NO_NEW_PRIVS` or equivalent.
    pub no_new_privileges: bool,
}

/// Failure modes shared by every [`Sandbox`] method.
#[derive(Debug, Clone, Error)]
pub enum SandboxError {
    /// Backend cannot wrap commands on this host.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Backend cannot wrap commands on this platform at all (e.g. Windows).
    /// Distinct from [`Unavailable`] — `Unavailable` means the backend exists
    /// but dependencies are missing; `Unsupported` means the backend itself
    /// is absent from claude-code on this OS.
    ///
    /// [`Unavailable`]: SandboxError::Unavailable
    #[error("sandbox not supported on this platform")]
    Unsupported,
    /// `realpath`-style canonicalization failed for a policy path.
    #[error("path canonicalization failed: {0}")]
    PathCanonicalize(String),
    /// A canonicalized policy path resolved outside the allowed root
    /// (symlink escape).
    #[error("symlink escape detected for {0}")]
    SymlinkEscape(String),
    /// Underlying I/O failure.
    #[error("io error: {0}")]
    Io(String),
}

/// Raw, un-sandboxed command description.
///
/// This is the shape callers build; it can be fed into [`Sandbox::prepare`]
/// or [`Sandbox::bypass_with_audit`] to obtain a [`SandboxedCommand`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessCommand {
    /// Executable to spawn.
    pub command: String,
    /// CLI arguments.
    pub args: Vec<String>,
    /// Working directory for the child process.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables.
    pub env: std::collections::HashMap<String, String>,
    /// Optional timeout — backends should kill the process after this.
    pub timeout: Option<std::time::Duration>,
    /// Optional stdin payload to feed the child process.
    pub stdin: Option<String>,
}

/// Opaque, backend-owned prepared execution plan riding on a
/// [`SandboxedCommand`] from `prepare()` to the runner (spec r3 D7).
///
/// In-process only — never serialized. Only the backend that minted it can
/// (and should) downcast it back. `Debug` prints a placeholder so command
/// logging cannot leak plan internals.
#[derive(Clone)]
pub struct BackendPlanHandle(Arc<dyn Any + Send + Sync>);

impl BackendPlanHandle {
    /// Wrap a backend plan value.
    ///
    /// Pass the plan by value, not pre-`Arc`'d — wrapping an `Arc<T>` makes
    /// the stored type `Arc<T>` and `downcast::<T>()` will return `None`.
    #[must_use]
    pub fn new<T: Any + Send + Sync>(plan: T) -> Self {
        Self(Arc::new(plan))
    }

    /// Recover the concrete plan type. Returns `None` when the handle holds
    /// a different type (runners treat that as a malformed plan).
    #[must_use]
    pub fn downcast<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.0.clone().downcast::<T>().ok()
    }
}

impl std::fmt::Debug for BackendPlanHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BackendPlanHandle(..)")
    }
}

/// Trusted process-owner acknowledgement, independent of process output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessCleanupStatus {
    /// No final cleanup acknowledgement has arrived.
    Pending,
    /// The platform verified cleanup, or that no process was started.
    Verified,
    /// The platform cannot establish that cleanup completed.
    Unverified(String),
}

/// One immutable view of cleanup status and any incomplete-output observation.
#[derive(Debug, Clone)]
pub struct ProcessCleanupSnapshot {
    /// The owner's final cleanup acknowledgement, if known.
    pub status: ProcessCleanupStatus,
    /// The caller's wait elapsed, independently of the owner's eventual result.
    pub wait_expired: bool,
    /// The specific proof, if a claimed owner supplied one. A generic
    /// `Verified` status by itself does not identify which proof was used.
    pub evidence: Option<ProcessCleanupEvidence>,
    /// Producer-side truncation of an otherwise completed output, if reported.
    pub output_truncation: Option<ProcessCleanupOutputTruncation>,
    /// Captured incomplete output, never a successful process result.
    pub observation: Option<Arc<crate::host::tool_invoker::UnverifiedCleanupOutput>>,
}

/// Independent per-stream truncation facts for a completed process output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessCleanupOutputTruncation {
    /// Some observed stdout bytes were not retained.
    pub stdout_truncated: bool,
    /// Some observed stderr bytes were not retained.
    pub stderr_truncated: bool,
}

/// The specific fact that supports a claimed owner's verified cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessCleanupEvidence {
    /// No process was started by this owner.
    NoProcessStarted,
    /// The backend supplied an identity-bound process-tree exit receipt.
    TreeReaped,
}

/// Capability identifying one owner of a receipt. It cannot be reconstructed
/// from a session ID and is never reused by a subsequent execution.
#[derive(Debug, Clone)]
pub struct ProcessCleanupOwnerToken(Arc<()>);

/// Final evidence supplied by the owner of a claimed receipt.
#[derive(Debug, Clone)]
pub enum ProcessCleanupOwnerOutcome {
    /// The owner established that it did not start a process.
    NoProcessStarted,
    /// The backend supplied an identity-bound process-tree exit receipt.
    TreeReaped,
    /// Cleanup is not proven, with a diagnostic reason.
    Unverified(String),
}

/// A claimed receipt rejected a conflicting or malformed update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessCleanupOwnerError {
    /// This receipt has already been claimed by an owner.
    AlreadyClaimed,
    /// A final status was already published.
    AlreadyFinalized,
    /// The supplied token belongs to a different execution.
    WrongOwner,
    /// A session identity is empty, too long, or contains invalid characters.
    InvalidSessionId,
    /// The receipt is already bound to a different session.
    SessionAlreadyBound,
    /// The observation does not name the bound session.
    SessionMismatch,
    /// Diagnostic metadata exceeds the bounded receipt capacity.
    MetadataTooLong,
    /// A negative sentinel is not an observed process exit code.
    InvalidExitCode,
    /// The final outcome conflicts with the session or observation evidence.
    OutcomeMismatch,
}

#[derive(Debug)]
struct ProcessCleanupState {
    status: ProcessCleanupStatus,
    wait_expired: bool,
    owner: Option<Arc<()>>,
    session_id: Option<String>,
    evidence: Option<ProcessCleanupEvidence>,
    output_truncation: Option<ProcessCleanupOutputTruncation>,
    observation: Option<Arc<crate::host::tool_invoker::UnverifiedCleanupOutput>>,
}

/// A single-assignment cleanup receipt shared with the actual process owner.
/// Writers are trusted host/runner code: this receipt prevents process output
/// from impersonating an acknowledgement, not malicious code in this process.
#[derive(Debug)]
pub struct ProcessCleanupReceipt {
    state: std::sync::Mutex<ProcessCleanupState>,
}

impl Default for ProcessCleanupReceipt {
    fn default() -> Self {
        Self {
            state: std::sync::Mutex::new(ProcessCleanupState {
                status: ProcessCleanupStatus::Pending,
                wait_expired: false,
                owner: None,
                session_id: None,
                evidence: None,
                output_truncation: None,
                observation: None,
            }),
        }
    }
}

impl ProcessCleanupReceipt {
    /// Mark this caller's bounded wait as expired without changing the
    /// underlying cleanup outcome; a later close may still verify cleanup.
    pub fn mark_wait_expired(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .wait_expired = true;
    }

    /// Whether the runner's caller-facing wait expired at least once.
    #[must_use]
    pub fn wait_expired(&self) -> bool {
        self.snapshot().wait_expired
    }

    /// Read the owner's final acknowledgement, if any.
    #[must_use]
    pub fn status(&self) -> ProcessCleanupStatus {
        self.snapshot().status
    }

    /// Read status, wait state, and observation from one atomic snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ProcessCleanupSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ProcessCleanupSnapshot {
            status: state.status.clone(),
            wait_expired: state.wait_expired,
            evidence: state.evidence,
            output_truncation: state.output_truncation,
            observation: state.observation.clone(),
        }
    }

    /// Record verified cleanup. A previously unverified result cannot be reset.
    pub fn mark_verified(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.owner.is_none() && matches!(state.status, ProcessCleanupStatus::Pending) {
            state.status = ProcessCleanupStatus::Verified;
        }
    }

    /// Record an ambiguous cleanup. This cannot overwrite an earlier receipt.
    pub fn mark_unverified(&self, detail: String) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.owner.is_none() && matches!(state.status, ProcessCleanupStatus::Pending) {
            state.status = ProcessCleanupStatus::Unverified(detail);
        }
    }

    /// Claim this receipt once. Once claimed, generic `mark_*` calls cannot
    /// finish it; only this token can atomically publish the final evidence.
    pub fn claim_owner(&self) -> Result<ProcessCleanupOwnerToken, ProcessCleanupOwnerError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(state.status, ProcessCleanupStatus::Pending) {
            return Err(ProcessCleanupOwnerError::AlreadyFinalized);
        }
        if state.owner.is_some() {
            return Err(ProcessCleanupOwnerError::AlreadyClaimed);
        }
        let identity = Arc::new(());
        state.owner = Some(identity.clone());
        Ok(ProcessCleanupOwnerToken(identity))
    }

    /// Bind the exact raw session before publishing any session observation.
    /// IDs are validated rather than truncated, which would change identity.
    pub fn bind_session(
        &self,
        owner: &ProcessCleanupOwnerToken,
        session_id: &str,
    ) -> Result<(), ProcessCleanupOwnerError> {
        if session_id.is_empty()
            || session_id.len() > 128
            || !session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(ProcessCleanupOwnerError::InvalidSessionId);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state
            .owner
            .as_ref()
            .is_some_and(|identity| Arc::ptr_eq(identity, &owner.0))
        {
            return Err(ProcessCleanupOwnerError::WrongOwner);
        }
        if !matches!(state.status, ProcessCleanupStatus::Pending) {
            return Err(ProcessCleanupOwnerError::AlreadyFinalized);
        }
        match &state.session_id {
            Some(existing) if existing != session_id => {
                Err(ProcessCleanupOwnerError::SessionAlreadyBound)
            }
            Some(_) => Ok(()),
            None => {
                state.session_id = Some(session_id.to_owned());
                Ok(())
            }
        }
    }

    /// Publish status and an optional incomplete-output observation together.
    /// Only a trusted backend-specific tree receipt justifies `TreeReaped`.
    pub fn finish_owned(
        &self,
        owner: &ProcessCleanupOwnerToken,
        outcome: ProcessCleanupOwnerOutcome,
        observation: Option<Arc<crate::host::tool_invoker::UnverifiedCleanupOutput>>,
    ) -> Result<(), ProcessCleanupOwnerError> {
        use crate::host::tool_invoker::ProcessTreeStatus;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state
            .owner
            .as_ref()
            .is_some_and(|identity| Arc::ptr_eq(identity, &owner.0))
        {
            return Err(ProcessCleanupOwnerError::WrongOwner);
        }
        if !matches!(state.status, ProcessCleanupStatus::Pending) {
            return Err(ProcessCleanupOwnerError::AlreadyFinalized);
        }
        if observation
            .as_ref()
            .is_some_and(|value| value.parts().session_id.as_deref() != state.session_id.as_deref())
        {
            return Err(ProcessCleanupOwnerError::SessionMismatch);
        }
        if observation
            .as_ref()
            .is_some_and(|value| value.parts().exit_code.is_some_and(|code| code < 0))
        {
            return Err(ProcessCleanupOwnerError::InvalidExitCode);
        }
        const MAX_METADATA_BYTES: usize = 16_384;
        if observation.as_ref().is_some_and(|value| {
            value.parts().cleanup_detail.len() > MAX_METADATA_BYTES
                || value
                    .parts()
                    .original_error
                    .as_ref()
                    .is_some_and(|error| error.len() > MAX_METADATA_BYTES)
        }) || matches!(&outcome, ProcessCleanupOwnerOutcome::Unverified(detail) if detail.len() > MAX_METADATA_BYTES)
        {
            return Err(ProcessCleanupOwnerError::MetadataTooLong);
        }
        let (status, evidence) = match outcome {
            ProcessCleanupOwnerOutcome::NoProcessStarted
                if state.session_id.is_none() && observation.is_none() =>
            {
                (
                    ProcessCleanupStatus::Verified,
                    Some(ProcessCleanupEvidence::NoProcessStarted),
                )
            }
            ProcessCleanupOwnerOutcome::TreeReaped
                if state.session_id.is_some()
                    && observation.as_ref().is_none_or(|value| {
                        value.parts().tree_status == ProcessTreeStatus::VerifiedReaped
                    }) =>
            {
                (
                    ProcessCleanupStatus::Verified,
                    Some(ProcessCleanupEvidence::TreeReaped),
                )
            }
            ProcessCleanupOwnerOutcome::Unverified(detail)
                if observation.as_ref().is_none_or(|value| {
                    value.parts().tree_status == ProcessTreeStatus::Unverified
                }) =>
            {
                (ProcessCleanupStatus::Unverified(detail), None)
            }
            _ => return Err(ProcessCleanupOwnerError::OutcomeMismatch),
        };
        state.status = status;
        state.evidence = evidence;
        state.observation = observation;
        Ok(())
    }

    /// Publish a complete process result with explicit output truncation.
    /// The caller must already hold an exact-session tree-reaped receipt,
    /// both stream EOFs, and the real process exit status.
    pub fn finish_owned_complete(
        &self,
        owner: &ProcessCleanupOwnerToken,
        truncation: ProcessCleanupOutputTruncation,
    ) -> Result<(), ProcessCleanupOwnerError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state
            .owner
            .as_ref()
            .is_some_and(|identity| Arc::ptr_eq(identity, &owner.0))
        {
            return Err(ProcessCleanupOwnerError::WrongOwner);
        }
        if !matches!(state.status, ProcessCleanupStatus::Pending) {
            return Err(ProcessCleanupOwnerError::AlreadyFinalized);
        }
        if state.session_id.is_none() {
            return Err(ProcessCleanupOwnerError::OutcomeMismatch);
        }
        state.status = ProcessCleanupStatus::Verified;
        state.evidence = Some(ProcessCleanupEvidence::TreeReaped);
        state.output_truncation = Some(truncation);
        Ok(())
    }
}

/// Opaque newtype — only constructible via [`Sandbox::prepare`],
/// [`Sandbox::bypass_with_audit`], [`SandboxedCommand::__new_sandboxed`], or
/// [`SandboxedCommand::__new_sandboxed_with_plan`] (the seams external
/// [`Sandbox`] impls call into).
///
/// [`crate::host::process::ProcessRunner::run`] accepts only this type, making it
/// impossible to bypass the sandbox decision (D2 / spec A1).
#[derive(Debug, Clone)]
pub struct SandboxedCommand {
    inner: ProcessCommand,
    tag: SandboxedTag,
    plan: Option<BackendPlanHandle>,
    lifetime_guard: Option<BackendPlanHandle>,
    cleanup_receipt: Option<Arc<ProcessCleanupReceipt>>,
    background: Option<crate::host::process::BackgroundTaskBinding>,
    auto_background_on_timeout: bool,
    background_timeout: Option<std::time::Duration>,
    process_owner: Option<String>,
    require_group_cleanup_on_exit: bool,
    supervisor_request: bool,
}

/// Provenance of a [`SandboxedCommand`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SandboxedTag {
    /// Command was wrapped by a real sandbox backend.
    Wrapped {
        /// Which backend wrapped the command.
        backend: SandboxBackend,
    },
    /// Command was admitted via [`Sandbox::bypass_with_audit`]; the reason is
    /// recorded for the audit log.
    BypassAuditedWithReason {
        /// Human-readable justification.
        reason: String,
    },
}

impl SandboxedCommand {
    /// Attribute this OS process group to the subagent that launched it.
    #[must_use]
    pub fn with_process_owner(mut self, owner: Option<String>) -> Self {
        self.process_owner = owner;
        self
    }
    /// The subagent owning this command's process group.
    #[must_use]
    pub fn process_owner(&self) -> Option<&str> {
        self.process_owner.as_deref()
    }

    /// Require a verified group/Job cleanup even when the leader exits
    /// naturally. Ordinary commands may deliberately leave descendants alive.
    #[must_use]
    pub fn with_require_group_cleanup_on_exit(mut self, required: bool) -> Self {
        self.require_group_cleanup_on_exit = required;
        self
    }

    /// The trusted caller's natural-completion cleanup policy.
    #[must_use]
    pub fn requires_group_cleanup_on_exit(&self) -> bool {
        self.require_group_cleanup_on_exit
    }

    /// Whether the core supervisor created this command after validating the
    /// incoming Start and its pinned resources. External callers cannot set it.
    #[must_use]
    pub fn is_supervisor_request(&self) -> bool {
        self.supervisor_request
    }

    #[must_use]
    pub(crate) fn mark_supervisor_request(mut self) -> Self {
        self.supervisor_request = true;
        self
    }

    /// INTERNAL constructor for [`Sandbox`] implementations.
    ///
    /// Only [`Sandbox::prepare`] and [`Sandbox::bypass_with_audit`] impls
    /// should call this. The leading double-underscore is a convention to
    /// signal "do not call this from regular code". Regular code must go
    /// through the [`Sandbox`] trait so the policy / audit hook actually
    /// runs.
    #[must_use]
    pub fn __new_sandboxed(inner: ProcessCommand, tag: SandboxedTag) -> Self {
        Self {
            inner,
            tag,
            plan: None,
            lifetime_guard: None,
            cleanup_receipt: None,
            background: None,
            auto_background_on_timeout: true,
            background_timeout: None,
            process_owner: None,
            require_group_cleanup_on_exit: false,
            supervisor_request: false,
        }
    }

    /// INTERNAL constructor for backends that carry a prepared plan to their
    /// runner (Android). Same visibility convention as [`__new_sandboxed`].
    ///
    /// [`__new_sandboxed`]: SandboxedCommand::__new_sandboxed
    #[must_use]
    pub fn __new_sandboxed_with_plan(
        inner: ProcessCommand,
        tag: SandboxedTag,
        plan: BackendPlanHandle,
    ) -> Self {
        Self {
            inner,
            tag,
            plan: Some(plan),
            lifetime_guard: None,
            cleanup_receipt: None,
            background: None,
            auto_background_on_timeout: true,
            background_timeout: None,
            process_owner: None,
            require_group_cleanup_on_exit: false,
            supervisor_request: false,
        }
    }

    /// Keep a per-command resource alive while the runner owns the process.
    /// Background-capable runners must clone this guard into their reaper.
    #[must_use]
    pub fn with_lifetime_guard<T: Any + Send + Sync>(mut self, guard: T) -> Self {
        self.lifetime_guard = Some(BackendPlanHandle::new(guard));
        self
    }

    /// Borrow the resource guard for cloning into a process-owning task.
    #[must_use]
    pub fn lifetime_guard(&self) -> Option<&BackendPlanHandle> {
        self.lifetime_guard.as_ref()
    }

    /// Attach the invocation's trusted cleanup acknowledgement.
    #[must_use]
    pub fn with_cleanup_receipt(mut self, receipt: Arc<ProcessCleanupReceipt>) -> Self {
        self.cleanup_receipt = Some(receipt);
        self
    }

    /// Capture the receipt when process ownership moves to another task.
    #[must_use]
    pub fn cleanup_receipt(&self) -> Option<&Arc<ProcessCleanupReceipt>> {
        self.cleanup_receipt.as_ref()
    }

    /// Bind a caller-owned task identity to this command, so that backgrounding
    /// it uses the caller's task id and output file instead of a runner-private
    /// one.
    ///
    /// This is the seam that keeps the id the model is handed, the id the task
    /// registry records, and the file the child writes to as ONE identity
    /// (claude-code mints all three together in its single shell spawn).
    #[must_use]
    pub fn with_background_task(
        mut self,
        binding: crate::host::process::BackgroundTaskBinding,
    ) -> Self {
        self.background = Some(binding);
        self
    }

    /// The caller-bound task identity, when one was attached.
    #[must_use]
    pub fn background_task(&self) -> Option<&crate::host::process::BackgroundTaskBinding> {
        self.background.as_ref()
    }

    /// Declare whether exceeding the timeout may move this command to the
    /// background instead of killing it.
    ///
    /// claude-code decides this per command before spawning
    /// (`dn = !Dl() && $es(command)`) and hands it to the shell as
    /// `shouldAutoBackground`; with it false the timeout kills the child
    /// (`Jje`'s `static #T`: `if (#h && #d) #d(background) else #b(143)`).
    /// Defaults to `true`, which is the behaviour every caller had before this
    /// existed.
    #[must_use]
    pub fn with_auto_background_on_timeout(mut self, allowed: bool) -> Self {
        self.auto_background_on_timeout = allowed;
        self
    }

    /// Whether a timeout may background this command rather than kill it.
    #[must_use]
    pub fn auto_background_on_timeout(&self) -> bool {
        self.auto_background_on_timeout
    }

    /// Bound the time a command may run after it enters the background.
    /// Independent of the foreground timeout; `None` leaves background lifetime
    /// to the caller (for example Monitor, which owns its own deadline).
    #[must_use]
    pub fn with_background_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.background_timeout = timeout;
        self
    }

    /// Deadline measured from explicit background start or foreground handoff.
    #[must_use]
    pub fn background_timeout(&self) -> Option<std::time::Duration> {
        self.background_timeout
    }

    /// Access the underlying [`ProcessCommand`] (for the runner to actually
    /// exec).
    #[must_use]
    pub fn inner(&self) -> &ProcessCommand {
        &self.inner
    }

    /// Read the provenance tag for audit / telemetry.
    #[must_use]
    pub fn tag(&self) -> &SandboxedTag {
        &self.tag
    }

    /// The backend-owned prepared plan, when the minting backend attached one.
    #[must_use]
    pub fn backend_plan(&self) -> Option<&BackendPlanHandle> {
        self.plan.as_ref()
    }
}

#[cfg(test)]
mod m2_01_tests {
    use super::*;

    #[test]
    fn plan_handle_is_send_sync_clone() {
        fn assert_bounds<T: Send + Sync + Clone>() {}
        assert_bounds::<BackendPlanHandle>();
        assert_bounds::<SandboxedCommand>();
    }

    #[test]
    fn cleanup_receipt_is_shared_and_terminal_states_are_monotonic() {
        let receipt = Arc::new(ProcessCleanupReceipt::default());
        let command = SandboxedCommand::__new_sandboxed(
            cmd_for_plan_tests(),
            SandboxedTag::Wrapped {
                backend: SandboxBackend::None,
            },
        )
        .with_cleanup_receipt(Arc::clone(&receipt));
        let clone = command.clone();
        let shared = clone.cleanup_receipt().unwrap();
        assert!(Arc::ptr_eq(shared, &receipt));
        assert_eq!(shared.status(), ProcessCleanupStatus::Pending);
        shared.mark_unverified("close failed".into());
        receipt.mark_verified();
        receipt.mark_unverified("later error".into());
        assert_eq!(
            receipt.status(),
            ProcessCleanupStatus::Unverified("close failed".into())
        );
        assert!(matches!(
            receipt.claim_owner(),
            Err(ProcessCleanupOwnerError::AlreadyFinalized)
        ));

        let verified = ProcessCleanupReceipt::default();
        verified.mark_wait_expired();
        verified.mark_verified();
        assert!(verified.wait_expired());
        verified.mark_unverified("later error".into());
        assert_eq!(verified.status(), ProcessCleanupStatus::Verified);
    }

    fn incomplete_output(
        session_id: Option<&str>,
        tree_status: crate::host::tool_invoker::ProcessTreeStatus,
    ) -> Arc<crate::host::tool_invoker::UnverifiedCleanupOutput> {
        use crate::host::tool_invoker::{UnverifiedCleanupOutput, UnverifiedCleanupOutputParts};
        Arc::new(UnverifiedCleanupOutput::new(UnverifiedCleanupOutputParts {
            session_id: session_id.map(str::to_owned),
            stdout: "early".into(),
            stderr: "late".into(),
            stdout_truncated: false,
            stderr_truncated: true,
            stdout_eof: true,
            stderr_eof: false,
            exit_code: None,
            timed_out: true,
            termination_requested: Some(true),
            tree_status,
            cleanup_detail: "cleanup evidence".into(),
            original_error: Some("original read error".into()),
        }))
    }

    #[test]
    fn claimed_cleanup_receipt_binds_identity_and_publishes_evidence_atomically() {
        use crate::host::tool_invoker::ProcessTreeStatus;
        let receipt = ProcessCleanupReceipt::default();
        let owner = receipt.claim_owner().unwrap();
        assert!(matches!(
            receipt.claim_owner(),
            Err(ProcessCleanupOwnerError::AlreadyClaimed)
        ));
        receipt.mark_verified();
        receipt.mark_unverified("unrelated generic failure".into());
        assert_eq!(receipt.status(), ProcessCleanupStatus::Pending);
        assert_eq!(
            receipt.bind_session(&owner, ""),
            Err(ProcessCleanupOwnerError::InvalidSessionId)
        );
        assert_eq!(
            receipt.bind_session(&owner, &"a".repeat(129)),
            Err(ProcessCleanupOwnerError::InvalidSessionId)
        );
        assert_eq!(
            receipt.bind_session(&owner, "bad/session"),
            Err(ProcessCleanupOwnerError::InvalidSessionId)
        );
        receipt.bind_session(&owner, "session-1").unwrap();
        receipt.bind_session(&owner, "session-1").unwrap();
        assert_eq!(
            receipt.bind_session(&owner, "session-2"),
            Err(ProcessCleanupOwnerError::SessionAlreadyBound)
        );

        let different = ProcessCleanupReceipt::default();
        let other_owner = different.claim_owner().unwrap();
        assert_eq!(
            receipt.finish_owned(
                &other_owner,
                ProcessCleanupOwnerOutcome::Unverified("wrong".into()),
                None
            ),
            Err(ProcessCleanupOwnerError::WrongOwner)
        );
        assert_eq!(
            receipt.finish_owned(
                &owner,
                ProcessCleanupOwnerOutcome::TreeReaped,
                Some(incomplete_output(
                    Some("session-2"),
                    ProcessTreeStatus::VerifiedReaped
                ))
            ),
            Err(ProcessCleanupOwnerError::SessionMismatch)
        );
        assert_eq!(
            receipt.finish_owned(
                &owner,
                ProcessCleanupOwnerOutcome::TreeReaped,
                Some(incomplete_output(
                    Some("session-1"),
                    ProcessTreeStatus::Unverified
                ))
            ),
            Err(ProcessCleanupOwnerError::OutcomeMismatch)
        );
        let mut invalid_parts = incomplete_output(Some("session-1"), ProcessTreeStatus::Unverified)
            .parts()
            .clone();
        invalid_parts.exit_code = Some(-1);
        assert_eq!(
            receipt.finish_owned(
                &owner,
                ProcessCleanupOwnerOutcome::Unverified("unknown exit".into()),
                Some(Arc::new(
                    crate::host::tool_invoker::UnverifiedCleanupOutput::new(invalid_parts)
                ))
            ),
            Err(ProcessCleanupOwnerError::InvalidExitCode)
        );
        assert_eq!(
            receipt.finish_owned(
                &owner,
                ProcessCleanupOwnerOutcome::Unverified("x".repeat(16_385)),
                None
            ),
            Err(ProcessCleanupOwnerError::MetadataTooLong)
        );
        assert_eq!(receipt.status(), ProcessCleanupStatus::Pending);

        let evidence = incomplete_output(Some("session-1"), ProcessTreeStatus::VerifiedReaped);
        receipt
            .finish_owned(
                &owner,
                ProcessCleanupOwnerOutcome::TreeReaped,
                Some(evidence.clone()),
            )
            .unwrap();
        receipt.mark_wait_expired();
        let snapshot = receipt.snapshot();
        assert_eq!(snapshot.status, ProcessCleanupStatus::Verified);
        assert!(snapshot.wait_expired);
        assert_eq!(snapshot.evidence, Some(ProcessCleanupEvidence::TreeReaped));
        assert!(Arc::ptr_eq(
            snapshot.observation.as_ref().unwrap(),
            &evidence
        ));
        assert_eq!(
            snapshot
                .observation
                .unwrap()
                .parts()
                .original_error
                .as_deref(),
            Some("original read error")
        );
        assert_eq!(
            receipt.finish_owned(
                &owner,
                ProcessCleanupOwnerOutcome::Unverified("later".into()),
                None
            ),
            Err(ProcessCleanupOwnerError::AlreadyFinalized)
        );
    }

    #[test]
    fn claimed_no_process_and_reaped_tree_are_distinct() {
        let receipt = ProcessCleanupReceipt::default();
        let owner = receipt.claim_owner().unwrap();
        assert_eq!(
            receipt.finish_owned(&owner, ProcessCleanupOwnerOutcome::TreeReaped, None),
            Err(ProcessCleanupOwnerError::OutcomeMismatch)
        );
        receipt
            .finish_owned(&owner, ProcessCleanupOwnerOutcome::NoProcessStarted, None)
            .unwrap();
        assert_eq!(receipt.status(), ProcessCleanupStatus::Verified);
        assert_eq!(
            receipt.snapshot().evidence,
            Some(ProcessCleanupEvidence::NoProcessStarted)
        );
        assert!(receipt.snapshot().observation.is_none());
    }

    #[test]
    fn completed_output_retains_independent_truncation_flags() {
        let receipt = ProcessCleanupReceipt::default();
        let owner = receipt.claim_owner().unwrap();
        assert_eq!(
            receipt.finish_owned_complete(
                &owner,
                ProcessCleanupOutputTruncation {
                    stdout_truncated: true,
                    stderr_truncated: false,
                }
            ),
            Err(ProcessCleanupOwnerError::OutcomeMismatch)
        );
        receipt.bind_session(&owner, "session-1").unwrap();
        receipt
            .finish_owned_complete(
                &owner,
                ProcessCleanupOutputTruncation {
                    stdout_truncated: true,
                    stderr_truncated: false,
                },
            )
            .unwrap();
        let snapshot = receipt.snapshot();
        assert_eq!(snapshot.status, ProcessCleanupStatus::Verified);
        assert_eq!(snapshot.evidence, Some(ProcessCleanupEvidence::TreeReaped));
        assert_eq!(
            snapshot.output_truncation,
            Some(ProcessCleanupOutputTruncation {
                stdout_truncated: true,
                stderr_truncated: false,
            })
        );
        assert!(snapshot.observation.is_none());
    }

    #[test]
    fn generic_completion_cannot_race_a_claimed_output_publication() {
        use crate::host::tool_invoker::ProcessTreeStatus;
        let receipt = Arc::new(ProcessCleanupReceipt::default());
        let owner = receipt.claim_owner().unwrap();
        receipt.bind_session(&owner, "session-1").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            let generic_receipt = receipt.clone();
            let generic_barrier = barrier.clone();
            scope.spawn(move || {
                generic_barrier.wait();
                generic_receipt.mark_unverified("generic failure".into());
                generic_receipt.mark_verified();
            });
            barrier.wait();
            receipt
                .finish_owned(
                    &owner,
                    ProcessCleanupOwnerOutcome::Unverified("no tree receipt".into()),
                    Some(incomplete_output(
                        Some("session-1"),
                        ProcessTreeStatus::Unverified,
                    )),
                )
                .unwrap();
        });
        let snapshot = receipt.snapshot();
        assert_eq!(
            snapshot.status,
            ProcessCleanupStatus::Unverified("no tree receipt".into())
        );
        assert_eq!(snapshot.observation.unwrap().parts().stdout, "early");
    }

    #[test]
    fn sandbox_error_unsupported_displays() {
        let e = SandboxError::Unsupported;
        assert_eq!(format!("{e}"), "sandbox not supported on this platform");
    }

    #[test]
    fn sandbox_error_unsupported_distinct_from_unavailable() {
        let u = SandboxError::Unsupported;
        let a = SandboxError::Unavailable("bwrap missing".into());
        assert!(!matches!(u, SandboxError::Unavailable(_)));
        assert!(matches!(a, SandboxError::Unavailable(_)));
    }

    #[test]
    fn android_minijail_backend_serde_roundtrip() {
        let b = SandboxBackend::AndroidMinijail;
        let json = serde_json::to_string(&b).expect("serialize");
        assert_eq!(json, "\"AndroidMinijail\"");
        let back: SandboxBackend = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, SandboxBackend::AndroidMinijail);
    }

    #[derive(Debug, PartialEq)]
    struct FakePlan {
        marker: u32,
    }

    fn cmd_for_plan_tests() -> ProcessCommand {
        ProcessCommand {
            command: "echo".into(),
            args: vec![],
            cwd: None,
            env: std::collections::HashMap::new(),
            timeout: None,
            stdin: None,
        }
    }

    #[test]
    fn new_sandboxed_has_no_plan() {
        let sc = SandboxedCommand::__new_sandboxed(
            cmd_for_plan_tests(),
            SandboxedTag::Wrapped {
                backend: SandboxBackend::None,
            },
        );
        assert!(sc.backend_plan().is_none());
    }

    #[test]
    fn with_plan_roundtrips_through_downcast() {
        let sc = SandboxedCommand::__new_sandboxed_with_plan(
            cmd_for_plan_tests(),
            SandboxedTag::Wrapped {
                backend: SandboxBackend::AndroidMinijail,
            },
            BackendPlanHandle::new(FakePlan { marker: 7 }),
        );
        let plan = sc
            .backend_plan()
            .expect("plan attached")
            .downcast::<FakePlan>()
            .expect("downcast to FakePlan");
        assert_eq!(plan.marker, 7);
        // Wrong type downcasts to None, not a panic.
        assert!(sc.backend_plan().unwrap().downcast::<String>().is_none());
    }

    #[test]
    fn plan_handle_debug_is_opaque_and_clone_shares() {
        let h = BackendPlanHandle::new(FakePlan { marker: 1 });
        assert_eq!(format!("{h:?}"), "BackendPlanHandle(..)");
        let sc = SandboxedCommand::__new_sandboxed_with_plan(
            cmd_for_plan_tests(),
            SandboxedTag::Wrapped {
                backend: SandboxBackend::AndroidMinijail,
            },
            h,
        );
        let cloned = sc.clone();
        assert!(cloned
            .backend_plan()
            .unwrap()
            .downcast::<FakePlan>()
            .is_some());
    }
}
