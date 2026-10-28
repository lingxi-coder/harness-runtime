//! `SandboxRunner` — the async seam the shell/skill tools call to wrap a
//! command in a sandbox before spawning it.
//!
//! The injected runner can use either the sync wrapper or a live
//! `sandbox-runtime` manager with proxy/MITM/seccomp support.
//!
//! [`LegacyWrapRunner`] is the default: it ignores `bin_shell` and forwards
//! to [`sandbox::wrap::wrap_with_sandbox_at`] with `cwd` as the sandbox root.

use sandbox::runtime_config::{Platform, SandboxRuntimeConfig};
use sandbox::wrap::SandboxWrapError;
use std::path::Path;
use std::sync::Arc;

/// Per-command sandbox resources, released when the last clone is dropped.
///
/// Keep a lease until its command has completed, including after a foreground
/// command moves to the background. A no-op lease owns no resources.
#[derive(Clone)]
pub struct SandboxLease {
    inner: Arc<SandboxLeaseInner>,
}

struct SandboxLeaseInner {
    cleanup: Option<Box<dyn FnOnce() + Send + Sync + 'static>>,
}

impl Drop for SandboxLeaseInner {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

impl std::fmt::Debug for SandboxLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxLease")
            .field("is_noop", &self.is_noop())
            .finish_non_exhaustive()
    }
}

impl SandboxLease {
    /// Create a lease whose cleanup runs exactly once after the last clone.
    #[must_use]
    pub fn new(cleanup: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(SandboxLeaseInner {
                cleanup: Some(Box::new(cleanup)),
            }),
        }
    }

    /// Create a lease for a wrapper that owns no per-command resources.
    #[must_use]
    pub fn noop() -> Self {
        Self {
            inner: Arc::new(SandboxLeaseInner { cleanup: None }),
        }
    }

    /// Whether this lease has no cleanup to defer until process completion.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.inner.cleanup.is_none()
    }
}

/// A wrapped command and the resources needed for its entire execution.
#[derive(Debug)]
pub struct SandboxWrappedCommand {
    /// Shell string to run.
    pub command: String,
    /// Independent per-invocation resource lease.
    pub lease: SandboxLease,
}

/// User-facing sandbox-violation lines recorded for one command.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SandboxCommandViolations {
    /// One line per surfaced violation, in store order.
    pub lines: Vec<String>,
}

/// Async seam that turns a raw command string into a sandbox-wrapped command.
///
/// Implementations may be stateful (e.g. a per-session `SandboxManager`); the
/// trait is `Send + Sync` so a single instance can be shared across tool calls
/// via `Arc`.
#[async_trait::async_trait]
pub trait SandboxRunner: Send + Sync {
    /// Wrap `command` under the sandbox described by `cfg` on `platform`,
    /// returning an independent lease for the command's resources.
    ///
    /// `bin_shell` is the shell binary the command runs under (e.g. `/bin/bash`).
    /// `cwd` is the command's sandbox root — the agent's own directory or the
    /// session workspace, never the shell's current `cd` — and must be the
    /// root `cfg` was resolved against
    /// ([`crate::BuiltinToolContext::sandbox_runtime_at`]): backends anchor the
    /// mandatory `.git`/dotfile denies there.
    ///
    /// # Errors
    /// Callers must keep the lease until the command and any background
    /// handoff have finished.
    ///
    /// # Errors
    /// Returns [`SandboxWrapError`] when the host cannot wrap the command.
    async fn wrap_with_lease(
        &self,
        command: &str,
        cfg: &SandboxRuntimeConfig,
        platform: Platform,
        bin_shell: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<SandboxWrappedCommand, SandboxWrapError>;

    /// Reset any per-session sandbox state (e.g. drop the `SandboxManager` and
    /// its proxies) at session teardown. Default no-op.
    async fn reset(&self) {}

    /// User-facing sandbox violations recorded for `command`.
    async fn command_violations(&self, _command: &str) -> SandboxCommandViolations {
        SandboxCommandViolations::default()
    }
}

/// The sync wrap, hoisted behind the [`SandboxRunner`] seam.
///
/// [`wrap_with_lease`](SandboxRunner::wrap_with_lease) forwards to
/// [`sandbox::wrap::wrap_with_sandbox_at`] with `cwd` as the sandbox root,
/// ignoring `bin_shell`.
#[derive(Debug, Default, Clone, Copy)]
pub struct LegacyWrapRunner;

#[async_trait::async_trait]
impl SandboxRunner for LegacyWrapRunner {
    async fn wrap_with_lease(
        &self,
        command: &str,
        cfg: &SandboxRuntimeConfig,
        platform: Platform,
        _bin_shell: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<SandboxWrappedCommand, SandboxWrapError> {
        Ok(SandboxWrappedCommand {
            command: sandbox::wrap::wrap_with_sandbox_at(command, cfg, platform, cwd)?,
            lease: SandboxLease::noop(),
        })
    }
}

/// The default [`SandboxRunner`] every `BuiltinToolContext` starts with: the
/// legacy sync wrap. The live `sandbox-runtime` runner is injected by the
/// desktop composition root in a later task.
#[must_use]
pub fn default_sandbox_runner() -> Arc<dyn SandboxRunner> {
    Arc::new(LegacyWrapRunner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// One captured `wrap` invocation.
    #[derive(Debug)]
    struct RecordedCall {
        command: String,
        platform: Platform,
        bin_shell: Option<String>,
        cwd: Option<std::path::PathBuf>,
    }

    /// Records every `wrap` call's arguments so a test can prove the seam is
    /// injectable and threads its arguments through faithfully.
    #[derive(Default)]
    struct RecordingRunner {
        calls: Mutex<Vec<RecordedCall>>,
    }

    #[async_trait::async_trait]
    impl SandboxRunner for RecordingRunner {
        async fn wrap_with_lease(
            &self,
            command: &str,
            _cfg: &SandboxRuntimeConfig,
            platform: Platform,
            bin_shell: Option<&str>,
            cwd: Option<&Path>,
        ) -> Result<SandboxWrappedCommand, SandboxWrapError> {
            self.calls.lock().unwrap().push(RecordedCall {
                command: command.to_string(),
                platform,
                bin_shell: bin_shell.map(ToString::to_string),
                cwd: cwd.map(Path::to_path_buf),
            });
            Ok(SandboxWrappedCommand {
                command: format!("RECORDED::{command}"),
                lease: SandboxLease::noop(),
            })
        }
    }

    /// Use Linux/bwrap for parity assertions: its wrapped string is a pure
    /// deterministic function of the inputs. (macOS/SBPL writes a random temp
    /// `.sb` file, so two equivalent calls differ only by that filename — not a
    /// behavioral difference, just non-determinism that would break byte-equal
    /// comparison.) The seam forwards to the same fn, so Linux parity proves the
    /// forwarding is faithful on every platform.
    const PARITY_PLATFORM: Platform = Platform::Linux;

    #[test]
    fn lease_cleans_up_once_after_last_owner() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cleanups = Arc::new(AtomicUsize::new(0));
        let observed = cleanups.clone();
        let lease = SandboxLease::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        });
        assert!(!lease.is_noop());
        let background = lease.clone();
        drop(lease);
        assert_eq!(cleanups.load(Ordering::SeqCst), 0);
        drop(background);
        assert_eq!(cleanups.load(Ordering::SeqCst), 1);
        assert!(SandboxLease::noop().is_noop());
    }

    #[tokio::test]
    async fn legacy_runner_matches_wrap_with_sandbox() {
        let cfg = SandboxRuntimeConfig::default();
        let platform = PARITY_PLATFORM;
        let command = "echo hello";

        let runner = LegacyWrapRunner;
        let via_seam = runner
            .wrap_with_lease(command, &cfg, platform, Some("/bin/bash"), None)
            .await;
        let direct = sandbox::wrap::wrap_with_sandbox(command, &cfg, platform);

        match (via_seam, direct) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a.command, b, "seam wrap must be byte-identical");
                assert!(a.lease.is_noop());
            }
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
            (a, b) => panic!("seam/direct disagreed: {a:?} vs {b:?}"),
        }
    }

    #[tokio::test]
    async fn legacy_linux_wrap_ignores_bin_shell_and_cwd() {
        // bwrap binds the (already rooted) config as given: bin_shell/cwd only
        // matter to the macOS profile's mandatory denies.
        let cfg = SandboxRuntimeConfig::default();
        let platform = PARITY_PLATFORM;
        let runner = LegacyWrapRunner;

        let plain = runner
            .wrap_with_lease("echo hi", &cfg, platform, None, None)
            .await;
        let with_extras = runner
            .wrap_with_lease(
                "echo hi",
                &cfg,
                platform,
                Some("/bin/zsh"),
                Some(Path::new("/some/where")),
            )
            .await;

        match (plain, with_extras) {
            (Ok(a), Ok(b)) => assert_eq!(a.command, b.command),
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
            (a, b) => panic!("extras changed legacy output: {a:?} vs {b:?}"),
        }
    }

    #[tokio::test]
    async fn default_runner_returns_legacy_behavior() {
        let cfg = SandboxRuntimeConfig::default();
        let platform = PARITY_PLATFORM;
        let runner = default_sandbox_runner();

        let via_default = runner
            .wrap_with_lease("echo hi", &cfg, platform, None, None)
            .await;
        let direct = sandbox::wrap::wrap_with_sandbox("echo hi", &cfg, platform);

        assert_eq!(via_default.is_ok(), direct.is_ok());
        if let (Ok(a), Ok(b)) = (via_default, direct) {
            assert_eq!(a.command, b);
        }
    }

    #[tokio::test]
    async fn recording_runner_captures_args() {
        let cfg = SandboxRuntimeConfig::default();
        let platform = PARITY_PLATFORM;
        let runner: Arc<dyn SandboxRunner> = Arc::new(RecordingRunner::default());

        let cwd = std::path::PathBuf::from("/work/dir");
        let out = runner
            .wrap_with_lease("ls -la", &cfg, platform, Some("/bin/zsh"), Some(&cwd))
            .await
            .expect("recording runner never errors");
        assert_eq!(out.command, "RECORDED::ls -la");

        // Defaults are no-ops; they must be callable through the trait object.
        runner.reset().await;
        assert_eq!(
            runner.command_violations("ls -la").await,
            SandboxCommandViolations::default()
        );

        // Down-cast is not available through the trait object, so re-create a
        // concrete instance to assert the recorded args via a fresh call path.
        let concrete = RecordingRunner::default();
        let _ = concrete
            .wrap_with_lease("ls -la", &cfg, platform, Some("/bin/zsh"), Some(&cwd))
            .await;
        let calls = concrete.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].command, "ls -la");
        assert_eq!(calls[0].platform, platform);
        assert_eq!(calls[0].bin_shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(calls[0].cwd.as_deref(), Some(Path::new("/work/dir")));
    }
}
