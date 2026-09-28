//! Process sandbox abstraction.
//!
//! The `Sandbox` trait is the boundary between the engine's command
//! execution layer and platform-specific sandbox implementations. The
//! [`SandboxedCommand`] newtype is the only shape accepted by
//! [`crate::process::ProcessRunner`], so a command cannot reach the runner
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

pub use mobile_linux_api::SandboxBackend;

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

pub use mobile_linux_api::{NetworkPolicy, ResourceLimits};

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

/// Opaque newtype — only constructible via [`Sandbox::prepare`],
/// [`Sandbox::bypass_with_audit`], [`SandboxedCommand::__new_sandboxed`], or
/// [`SandboxedCommand::__new_sandboxed_with_plan`] (the seams external
/// [`Sandbox`] impls call into).
///
/// [`crate::process::ProcessRunner::run`] accepts only this type, making it
/// impossible to bypass the sandbox decision (D2 / spec A1).
#[derive(Debug, Clone)]
pub struct SandboxedCommand {
    inner: ProcessCommand,
    tag: SandboxedTag,
    plan: Option<BackendPlanHandle>,
    background: Option<crate::process::BackgroundTaskBinding>,
    auto_background_on_timeout: bool,
    process_owner: Option<String>,
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
            background: None,
            auto_background_on_timeout: true,
            process_owner: None,
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
            background: None,
            auto_background_on_timeout: true,
            process_owner: None,
        }
    }

    /// Bind a caller-owned task identity to this command, so that backgrounding
    /// it uses the caller's task id and output file instead of a runner-private
    /// one.
    ///
    /// This is the seam that keeps the id the model is handed, the id the task
    /// registry records, and the file the child writes to as ONE identity
    /// (claude-code mints all three together in its single shell spawn).
    #[must_use]
    pub fn with_background_task(mut self, binding: crate::process::BackgroundTaskBinding) -> Self {
        self.background = Some(binding);
        self
    }

    /// The caller-bound task identity, when one was attached.
    #[must_use]
    pub fn background_task(&self) -> Option<&crate::process::BackgroundTaskBinding> {
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
