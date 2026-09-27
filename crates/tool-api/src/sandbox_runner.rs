//! `SandboxRunner` — the async seam the shell/skill tools call to wrap a
//! command in a sandbox before spawning it.
//!
//! Today every tool calls the sync [`sandbox::wrap::wrap_with_sandbox`]
//! directly. This trait hoists that call behind an injectable handle so a
//! later task can swap in a live `sandbox-runtime`-backed runner (per-session
//! `SandboxManager`, proxy/MITM/seccomp) without touching the tool call sites.
//!
//! [`LegacyWrapRunner`] is the default and is byte-identical to the current
//! direct call: its [`wrap`](SandboxRunner::wrap) ignores `bin_shell`/`cwd`
//! and forwards `command`/`cfg`/`platform` straight to
//! [`sandbox::wrap::wrap_with_sandbox`]. The live runner (added later) uses the
//! extra arguments.

use sandbox::runtime_config::{Platform, SandboxRuntimeConfig};
use sandbox::wrap::SandboxWrapError;
use std::path::Path;
use std::sync::Arc;

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
    /// Wrap `command` so it runs under the host sandbox described by `cfg` on
    /// `platform`.
    ///
    /// `bin_shell` is the shell binary the command runs under (e.g. `/bin/bash`)
    /// and `cwd` the working directory; a live runner uses these to scope the
    /// sandbox, while [`LegacyWrapRunner`] ignores them.
    ///
    /// # Errors
    /// Returns [`SandboxWrapError`] when the host cannot produce a wrapped
    /// command (unsupported platform, SBPL write failure, …), matching the
    /// behavior of [`sandbox::wrap::wrap_with_sandbox`].
    async fn wrap(
        &self,
        command: &str,
        cfg: &SandboxRuntimeConfig,
        platform: Platform,
        bin_shell: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<String, SandboxWrapError>;

    /// Tear down any per-command sandbox state (e.g. bwrap mount points) after
    /// the wrapped command has finished. Default no-op.
    async fn cleanup_after_command(&self) {}

    /// Reset any per-session sandbox state (e.g. drop the `SandboxManager` and
    /// its proxies) at session teardown. Default no-op.
    async fn reset(&self) {}

    /// User-facing sandbox violations recorded for `command`.
    async fn command_violations(&self, _command: &str) -> SandboxCommandViolations {
        SandboxCommandViolations::default()
    }
}

/// The current behavior, hoisted behind the [`SandboxRunner`] seam.
///
/// [`wrap`](SandboxRunner::wrap) forwards straight to
/// [`sandbox::wrap::wrap_with_sandbox`], ignoring `bin_shell`/`cwd`, so it is
/// byte-identical to the direct call the tools make today. `cleanup`/`reset`
/// are the no-op defaults.
#[derive(Debug, Default, Clone, Copy)]
pub struct LegacyWrapRunner;

#[async_trait::async_trait]
impl SandboxRunner for LegacyWrapRunner {
    async fn wrap(
        &self,
        command: &str,
        cfg: &SandboxRuntimeConfig,
        platform: Platform,
        _bin_shell: Option<&str>,
        _cwd: Option<&Path>,
    ) -> Result<String, SandboxWrapError> {
        sandbox::wrap::wrap_with_sandbox(command, cfg, platform)
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
        async fn wrap(
            &self,
            command: &str,
            _cfg: &SandboxRuntimeConfig,
            platform: Platform,
            bin_shell: Option<&str>,
            cwd: Option<&Path>,
        ) -> Result<String, SandboxWrapError> {
            self.calls.lock().unwrap().push(RecordedCall {
                command: command.to_string(),
                platform,
                bin_shell: bin_shell.map(ToString::to_string),
                cwd: cwd.map(Path::to_path_buf),
            });
            Ok(format!("RECORDED::{command}"))
        }
    }

    /// Use Linux/bwrap for parity assertions: its wrapped string is a pure
    /// deterministic function of the inputs. (macOS/SBPL writes a random temp
    /// `.sb` file, so two equivalent calls differ only by that filename — not a
    /// behavioral difference, just non-determinism that would break byte-equal
    /// comparison.) The seam forwards to the same fn, so Linux parity proves the
    /// forwarding is faithful on every platform.
    const PARITY_PLATFORM: Platform = Platform::Linux;

    #[tokio::test]
    async fn legacy_runner_matches_wrap_with_sandbox() {
        let cfg = SandboxRuntimeConfig::default();
        let platform = PARITY_PLATFORM;
        let command = "echo hello";

        let runner = LegacyWrapRunner;
        let via_seam = runner
            .wrap(command, &cfg, platform, Some("/bin/bash"), None)
            .await;
        let direct = sandbox::wrap::wrap_with_sandbox(command, &cfg, platform);

        match (via_seam, direct) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "seam wrap must be byte-identical"),
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
            (a, b) => panic!("seam/direct disagreed: {a:?} vs {b:?}"),
        }
    }

    #[tokio::test]
    async fn legacy_runner_ignores_bin_shell_and_cwd() {
        // Faithful to today: bin_shell/cwd must NOT alter the legacy output.
        let cfg = SandboxRuntimeConfig::default();
        let platform = PARITY_PLATFORM;
        let runner = LegacyWrapRunner;

        let plain = runner.wrap("echo hi", &cfg, platform, None, None).await;
        let with_extras = runner
            .wrap(
                "echo hi",
                &cfg,
                platform,
                Some("/bin/zsh"),
                Some(Path::new("/some/where")),
            )
            .await;

        match (plain, with_extras) {
            (Ok(a), Ok(b)) => assert_eq!(a, b),
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
            (a, b) => panic!("extras changed legacy output: {a:?} vs {b:?}"),
        }
    }

    #[tokio::test]
    async fn default_runner_returns_legacy_behavior() {
        let cfg = SandboxRuntimeConfig::default();
        let platform = PARITY_PLATFORM;
        let runner = default_sandbox_runner();

        let via_default = runner.wrap("echo hi", &cfg, platform, None, None).await;
        let direct = sandbox::wrap::wrap_with_sandbox("echo hi", &cfg, platform);

        assert_eq!(via_default.is_ok(), direct.is_ok());
        if let (Ok(a), Ok(b)) = (via_default, direct) {
            assert_eq!(a, b);
        }
    }

    #[tokio::test]
    async fn recording_runner_captures_args() {
        let cfg = SandboxRuntimeConfig::default();
        let platform = PARITY_PLATFORM;
        let runner: Arc<dyn SandboxRunner> = Arc::new(RecordingRunner::default());

        let cwd = std::path::PathBuf::from("/work/dir");
        let out = runner
            .wrap("ls -la", &cfg, platform, Some("/bin/zsh"), Some(&cwd))
            .await
            .expect("recording runner never errors");
        assert_eq!(out, "RECORDED::ls -la");

        // Defaults are no-ops; they must be callable through the trait object.
        runner.cleanup_after_command().await;
        runner.reset().await;
        assert_eq!(
            runner.command_violations("ls -la").await,
            SandboxCommandViolations::default()
        );

        // Down-cast is not available through the trait object, so re-create a
        // concrete instance to assert the recorded args via a fresh call path.
        let concrete = RecordingRunner::default();
        let _ = concrete
            .wrap("ls -la", &cfg, platform, Some("/bin/zsh"), Some(&cwd))
            .await;
        let calls = concrete.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].command, "ls -la");
        assert_eq!(calls[0].platform, platform);
        assert_eq!(calls[0].bin_shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(calls[0].cwd.as_deref(), Some(Path::new("/work/dir")));
    }
}
