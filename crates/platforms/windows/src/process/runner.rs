//! `tokio::process`-backed [`ProcessRunner`] for Windows hosts.
//!
//! Mirrors the POSIX runner's spawn-env contract (`LINGXI=1`,
//! `GIT_EDITOR=true`, `SHELL=<inner.command>`) and the 30-minute default
//! timeout. Foreground and background Bash share one piped child and the
//! caller's task identity, output writer and exit sink. `kill` delegates to
//! [`super::kill_tree::kill_tree_windows`], which shells out to
//! `taskkill /T /F /PID <pid>`.

use async_trait::async_trait;
use lingxi_core::host::{ProcessHandle, ProcessRunner, SandboxedCommand};
use mobile_linux_api::ProcessStreamSink;
use mobile_linux_api::{ProcessError, ProcessOutput};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// 30-minute default timeout matches the POSIX runner and claude-code's
/// `DEFAULT_TIMEOUT` (`Shell.ts:44`).
pub(super) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

const ENV_LINGXI_MARKER: (&str, &str) = ("LINGXI", "1");
const ENV_GIT_EDITOR: (&str, &str) = ("GIT_EDITOR", "true");
const ENV_SHELL: &str = "SHELL";
const ENV_LINGXI_SESSION_ID: &str = "LINGXI_SESSION_ID";

/// Cancellation guard for streaming commands. Dropping a monitor worker's
/// future must terminate descendants too; Tokio's `kill_on_drop` only targets
/// the direct shell. The explicit completion/timeout paths use the async helper,
/// while this drop-only fallback invokes `taskkill /T /F` synchronously because
/// `Drop` cannot await.
pub(super) struct StreamingProcessTreeGuard(pub(super) Option<u32>);

impl Drop for StreamingProcessTreeGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .output();
        }
    }
}

/// Production [`ProcessRunner`] using `tokio::process`.
#[derive(Default)]
pub struct WindowsProcess;

impl WindowsProcess {
    /// Construct a new `WindowsProcess` runner.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    pub(super) fn build_command(cmd: &SandboxedCommand) -> Command {
        let inner = cmd.inner();
        let mut tcmd = Command::new(&inner.command);
        tcmd.args(&inner.args);
        if let Some(cwd) = &inner.cwd {
            tcmd.current_dir(cwd);
        }
        for (k, v) in &inner.env {
            tcmd.env(k, v);
        }
        // Spawn-env contract (overrides caller env to mirror posix).
        tcmd.env(ENV_LINGXI_MARKER.0, ENV_LINGXI_MARKER.1);
        tcmd.env(ENV_GIT_EDITOR.0, ENV_GIT_EDITOR.1);
        tcmd.env(ENV_SHELL, &inner.command);
        if let Some(sess) = inner.env.get(ENV_LINGXI_SESSION_ID) {
            tcmd.env(ENV_LINGXI_SESSION_ID, sess);
        }
        tcmd
    }
}

#[cfg(all(test, windows))]
mod owner_identity_tests {
    use super::*;

    #[tokio::test]
    async fn strict_unmanaged_entrypoints_reject_before_spawning() {
        struct ClaimingSink;
        #[async_trait]
        impl lingxi_core::host::BackgroundExitSink for ClaimingSink {
            fn requires_verified_supervised_stop(&self) -> bool {
                true
            }
            async fn on_exit(&self, _: &str, _: Option<i32>) {}
        }
        struct Stream;
        #[async_trait]
        impl ProcessStreamSink for Stream {
            async fn stdout_line(&self, _: String) -> Result<(), ProcessError> {
                Ok(())
            }
            async fn stderr_chunk(&self, _: Vec<u8>) -> Result<(), ProcessError> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed");
        let strict = SandboxedCommand::__new_sandboxed(
            lingxi_core::host::ProcessCommand {
                command: "powershell.exe".into(),
                args: vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    "Set-Content -LiteralPath $env:STRICT_FIXTURE_MARKER -Value executed".into(),
                ],
                cwd: Some(dir.path().to_path_buf()),
                env: std::collections::HashMap::from([(
                    "STRICT_FIXTURE_MARKER".into(),
                    marker.to_string_lossy().into_owned(),
                )]),
                stdin: None,
                timeout: Some(Duration::from_secs(30)),
            },
            lingxi_core::host::SandboxedTag::BypassAuditedWithReason {
                reason: "strict marker fixture".into(),
            },
        )
        .with_require_group_cleanup_on_exit(true)
        .with_background_task(lingxi_core::host::BackgroundTaskBinding {
            task_id: "unmanaged-strict-marker".into(),
            output_path: dir.path().join("output"),
            on_exit: Some(std::sync::Arc::new(ClaimingSink)),
            on_demand: None,
        });
        let process = WindowsProcess::new();
        assert!(matches!(
            process.run(&strict).await,
            Err(ProcessError::Unsupported)
        ));
        assert!(matches!(
            process
                .run_streaming(&strict, std::sync::Arc::new(Stream))
                .await,
            Err(ProcessError::Unsupported)
        ));
        assert!(matches!(
            process.run_foreground(&strict).await,
            Err(ProcessError::Unsupported)
        ));
        assert!(matches!(
            process.spawn_background(&strict).await,
            Err(ProcessError::Unsupported)
        ));
        assert!(matches!(
            process
                .run_hook_with_async_detection(&strict, Duration::from_secs(1))
                .await,
            Err(ProcessError::Unsupported)
        ));
        assert!(
            !marker.exists(),
            "no unsupported strict entrypoint may execute the payload"
        );
        let ordinary = strict.with_require_group_cleanup_on_exit(false);
        assert_eq!(process.run(&ordinary).await.unwrap().exit_code, 0);
        assert!(
            marker.exists(),
            "ordinary direct execution must remain available"
        );
    }

    #[tokio::test]
    async fn host_owner_never_signals_a_registration_without_its_capability() {
        let mut child = tokio::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 60",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let owner = format!("host-tool:unmatched-registration-{pid}");
        let registration =
            lingxi_core::host::agent_processes::register(Some(&owner), Some(pid)).unwrap();

        let snapshot = WindowsProcess::new().kill_owner_processes(&owner).await;
        let still_running = child.try_wait().unwrap().is_none();
        // The test always terminates its own child before asserting.
        if still_running {
            child.kill().await.unwrap();
        }
        child.wait().await.unwrap();
        drop(registration);
        assert_eq!(snapshot, vec![pid]);
        assert!(still_running, "an unmatched host PID must not be signalled");
    }

    #[tokio::test]
    async fn mixed_owner_never_signals_capability_only_reused_pid() {
        fn spawn() -> tokio::process::Child {
            tokio::process::Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Start-Sleep -Seconds 60",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap()
        }
        let mut unrelated = spawn();
        let mut ordinary = spawn();
        let unrelated_pid = unrelated.id().unwrap();
        let ordinary_pid = ordinary.id().unwrap();
        let owner = format!("mixed-capability-owner-{unrelated_pid}");
        // Model a retired supervisor capability whose old numeric PID now
        // aliases an unrelated process, while the same owner has a normal PID.
        let stale = lingxi_core::host::agent_processes::register_with_capability_requirement(
            Some(&owner),
            Some(unrelated_pid),
            true,
        )
        .unwrap();
        let replacement = lingxi_core::host::agent_processes::register_with_capability_requirement(
            Some(&owner),
            Some(unrelated_pid),
            false,
        )
        .unwrap();
        assert!(
            replacement.entry().requires_capability(),
            "overlap cannot downgrade strict identity"
        );
        drop(stale);
        let direct =
            lingxi_core::host::agent_processes::register(Some(&owner), Some(ordinary_pid)).unwrap();
        let mut seen = WindowsProcess::new().kill_owner_processes(&owner).await;
        seen.sort_unstable();
        let mut expected = vec![unrelated_pid, ordinary_pid];
        expected.sort_unstable();
        let untouched = unrelated.try_wait().unwrap().is_none();
        let normal_exit =
            tokio::time::timeout(std::time::Duration::from_secs(10), ordinary.wait()).await;
        if normal_exit.is_err() {
            let _ = ordinary.kill().await;
        }
        let _ = unrelated.kill().await;
        let _ = unrelated.wait().await;
        drop(replacement);
        drop(direct);
        assert_eq!(seen, expected);
        assert!(
            untouched,
            "a capability-only numeric alias must not be signalled"
        );
        assert!(!normal_exit
            .expect("ordinary entry must remain stoppable")
            .unwrap()
            .success());
    }
}

fn require_native_supervisor_for_strict(cmd: &SandboxedCommand) -> Result<(), ProcessError> {
    if cmd.requires_group_cleanup_on_exit()
        && (!cmd.is_supervisor_request()
            || !cmd
                .background_task()
                .and_then(|binding| binding.on_exit.as_ref())
                .is_some_and(|sink| sink.requires_verified_supervised_stop()))
    {
        return Err(ProcessError::Unsupported);
    }
    Ok(())
}

#[async_trait]
impl ProcessRunner for WindowsProcess {
    fn supports_background_lifetime_guard(&self) -> bool {
        true
    }

    fn supports_verified_background_stop(&self, command: &SandboxedCommand) -> bool {
        #[cfg(windows)]
        {
            super::supervisor::enabled(command)
        }
        #[cfg(not(windows))]
        {
            let _ = command;
            false
        }
    }

    async fn terminate_and_wait_owned_background(
        &self,
        handle: &ProcessHandle,
        owner: &str,
    ) -> Result<(), ProcessError> {
        lingxi_core::host::shell_supervisor::terminate_and_wait(handle, owner)
            .await
            .unwrap_or(Err(ProcessError::Unsupported))
    }

    async fn kill_owner_processes(&self, owner: &str) -> Vec<u32> {
        let owned = lingxi_core::host::agent_processes::snapshot_entries(owner);
        for entry in &owned {
            if lingxi_core::host::shell_supervisor::kill_owned_registration(owner, *entry)
                .await
                .is_none()
                // Host-owned supervisor jobs require their authenticated
                // capability. A missing job is never permission to signal a
                // bare PID that may already have been recycled.
                && !entry.requires_capability()
                && !owner.starts_with("host-tool:")
                && lingxi_core::host::agent_processes::is_current(owner, *entry)
            {
                let _ = super::kill_tree::kill_tree_windows(entry.pid).await;
            }
        }
        owned.into_iter().map(|entry| entry.pid).collect()
    }

    async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
        if cmd.requires_group_cleanup_on_exit() {
            return Err(ProcessError::Unsupported);
        }
        let inner = cmd.inner();
        let mut tcmd = Self::build_command(cmd);
        tcmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = tcmd.spawn().map_err(|e| ProcessError::Io(e.to_string()))?;
        let _agent_registration =
            lingxi_core::host::agent_processes::register_with_capability_requirement(
                cmd.process_owner(),
                child.id(),
                cmd.requires_group_cleanup_on_exit(),
            );
        if let Some(stdin_text) = &inner.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(stdin_text.as_bytes())
                    .await
                    .map_err(|e| ProcessError::Io(e.to_string()))?;
            }
        }

        let timeout = inner.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(r) => r.map_err(|e| ProcessError::Io(e.to_string()))?,
            Err(_) => return Err(ProcessError::Timeout),
        };

        Ok(ProcessOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
            timed_out: false,
        })
    }

    async fn run_streaming(
        &self,
        cmd: &SandboxedCommand,
        sink: std::sync::Arc<dyn ProcessStreamSink>,
    ) -> Result<ProcessOutput, ProcessError> {
        if cmd.requires_group_cleanup_on_exit() {
            return Err(ProcessError::Unsupported);
        }
        let inner = cmd.inner();
        let mut tcmd = Self::build_command(cmd);
        tcmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = tcmd.spawn().map_err(|e| ProcessError::Io(e.to_string()))?;
        let _agent_registration =
            lingxi_core::host::agent_processes::register_with_capability_requirement(
                cmd.process_owner(),
                child.id(),
                cmd.requires_group_cleanup_on_exit(),
            );
        let pid = child
            .id()
            .ok_or_else(|| ProcessError::Io("streaming child has no pid".into()))?;
        let mut tree_guard = StreamingProcessTreeGuard(Some(pid));
        if let Some(stdin_text) = &inner.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(stdin_text.as_bytes())
                    .await
                    .map_err(|e| ProcessError::Io(e.to_string()))?;
            }
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProcessError::Io("streaming child has no stdout pipe".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProcessError::Io("streaming child has no stderr pipe".into()))?;
        let stdout_sink = sink.clone();
        let stderr_sink = sink;
        let stdout_task = async move {
            let mut reader = BufReader::new(stdout);
            let mut captured = Vec::new();
            loop {
                let mut line = Vec::new();
                let read = reader
                    .read_until(b'\n', &mut line)
                    .await
                    .map_err(|e| ProcessError::Io(e.to_string()))?;
                if read == 0 {
                    break;
                }
                captured.extend_from_slice(&line);
                while matches!(line.last(), Some(b'\n' | b'\r')) {
                    line.pop();
                }
                stdout_sink
                    .stdout_line(String::from_utf8_lossy(&line).into_owned())
                    .await?;
            }
            Ok::<Vec<u8>, ProcessError>(captured)
        };
        let stderr_task = async move {
            let mut captured = Vec::new();
            let mut chunk = vec![0_u8; 8 * 1024];
            loop {
                let read = stderr
                    .read(&mut chunk)
                    .await
                    .map_err(|e| ProcessError::Io(e.to_string()))?;
                if read == 0 {
                    break;
                }
                let bytes = chunk[..read].to_vec();
                captured.extend_from_slice(&bytes);
                stderr_sink.stderr_chunk(bytes).await?;
            }
            Ok::<Vec<u8>, ProcessError>(captured)
        };
        let execution = async {
            let (stdout, stderr, status) = tokio::try_join!(stdout_task, stderr_task, async {
                child
                    .wait()
                    .await
                    .map_err(|e| ProcessError::Io(e.to_string()))
            })?;
            Ok::<_, ProcessError>((stdout, stderr, status))
        };

        let result =
            tokio::time::timeout(inner.timeout.unwrap_or(DEFAULT_TIMEOUT), execution).await;
        match result {
            Ok(Ok((stdout, stderr, status))) => {
                tree_guard.0 = None;
                Ok(ProcessOutput {
                    stdout: String::from_utf8_lossy(&stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&stderr).into_owned(),
                    exit_code: status.code().unwrap_or(-1),
                    timed_out: false,
                })
            }
            Ok(Err(error)) => {
                let _ = super::kill_tree::kill_tree_windows(pid).await;
                tree_guard.0 = None;
                let _ = child.wait().await;
                Err(error)
            }
            Err(_) => {
                let _ = super::kill_tree::kill_tree_windows(pid).await;
                tree_guard.0 = None;
                let _ = child.wait().await;
                Err(ProcessError::Timeout)
            }
        }
    }

    fn supports_foreground_backgrounding(&self) -> bool {
        true
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
        limit: Option<usize>,
    ) -> Result<lingxi_core::host::ForegroundRunResult, ProcessError> {
        #[cfg(windows)]
        if super::supervisor::enabled(cmd) {
            return super::supervisor::execute(cmd, limit, false).await;
        }
        require_native_supervisor_for_strict(cmd)?;
        super::background::run(cmd, limit, false).await
    }
    async fn spawn_background(
        &self,
        cmd: &SandboxedCommand,
    ) -> Result<ProcessHandle, ProcessError> {
        #[cfg(windows)]
        if super::supervisor::enabled(cmd) {
            return match super::supervisor::execute(cmd, None, true).await?.outcome {
                lingxi_core::host::ForegroundOutcome::MovedToBackground(handle) => Ok(handle),
                _ => Err(ProcessError::Io("supervisor did not detach shell".into())),
            };
        }
        require_native_supervisor_for_strict(cmd)?;
        match super::background::run(cmd, Some(8192), true).await?.outcome {
            lingxi_core::host::ForegroundOutcome::MovedToBackground(handle) => Ok(handle),
            lingxi_core::host::ForegroundOutcome::Completed(_) => {
                unreachable!("explicit background path always hands off its spawned child")
            }
        }
    }

    #[cfg(windows)]
    async fn acknowledge_shell(&self, handle: &ProcessHandle) -> Result<(), ProcessError> {
        lingxi_core::host::shell_supervisor::acknowledge(handle).await
    }
    #[cfg(windows)]
    async fn export_shell(
        &self,
        handle: &ProcessHandle,
    ) -> Result<lingxi_core::host::process::ShellProcessHandoff, ProcessError> {
        self.acknowledge_shell(handle).await?;
        super::supervisor::export(handle)
    }
    #[cfg(windows)]
    async fn validate_shell(
        &self,
        handoff: &lingxi_core::host::process::ShellProcessHandoff,
    ) -> Result<(), ProcessError> {
        super::supervisor::validate(handoff).await
    }
    #[cfg(windows)]
    async fn adopt_shell(
        &self,
        handoff: &lingxi_core::host::process::ShellProcessHandoff,
        sink: std::sync::Arc<dyn lingxi_core::host::BackgroundExitSink>,
    ) -> Result<ProcessHandle, ProcessError> {
        super::supervisor::adopt(handoff, sink).await
    }
    #[cfg(windows)]
    async fn release_shell(
        &self,
        handoff: &lingxi_core::host::process::ShellProcessHandoff,
    ) -> Result<(), ProcessError> {
        super::supervisor::release(handoff).await
    }
    async fn kill(&self, handle: &ProcessHandle) -> Result<(), ProcessError> {
        #[cfg(windows)]
        if let Some(result) = super::supervisor::kill(handle).await {
            return result;
        }
        super::kill_tree::kill_tree_windows(handle.pid).await
    }

    fn is_available(&self) -> bool {
        true
    }
}

/// Generate a unique task id of the form `local_bash_<nanos-hex>` —
/// matches the POSIX runner's scheme so engine-side log inspection looks
/// the same on either host.
pub(super) fn generate_task_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("local_bash_{nanos:x}")
}
