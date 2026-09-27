//! `tokio::process`-backed [`ProcessRunner`] for Windows hosts.
//!
//! Mirrors the POSIX runner's spawn-env contract (`LINGXI=1`,
//! `GIT_EDITOR=true`, `SHELL=<inner.command>`) and the 30-minute default
//! timeout. Foreground and background Bash share one piped child and the
//! caller's task identity, output writer and exit sink. `kill` delegates to
//! [`super::kill_tree::kill_tree_windows`], which shells out to
//! `taskkill /T /F /PID <pid>`.

use async_trait::async_trait;
use platform_api::process::ProcessStreamSink;
use platform_api::{ProcessError, ProcessHandle, ProcessOutput, ProcessRunner, SandboxedCommand};
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

#[async_trait]
impl ProcessRunner for WindowsProcess {
    async fn kill_owner_processes(&self, owner: &str) -> Vec<u32> {
        let owned = platform_api::agent_processes::snapshot_entries(owner);
        for entry in &owned {
            if platform_api::shell_supervisor::kill_owned_registration(owner, *entry)
                .await
                .is_none()
                && platform_api::agent_processes::is_current(owner, *entry)
            {
                let _ = super::kill_tree::kill_tree_windows(entry.pid).await;
            }
        }
        owned.into_iter().map(|entry| entry.pid).collect()
    }

    async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
        let inner = cmd.inner();
        let mut tcmd = Self::build_command(cmd);
        tcmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = tcmd.spawn().map_err(|e| ProcessError::Io(e.to_string()))?;
        let _agent_registration =
            platform_api::agent_processes::register(cmd.process_owner(), child.id());
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
        let inner = cmd.inner();
        let mut tcmd = Self::build_command(cmd);
        tcmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = tcmd.spawn().map_err(|e| ProcessError::Io(e.to_string()))?;
        let _agent_registration =
            platform_api::agent_processes::register(cmd.process_owner(), child.id());
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
    ) -> Result<platform_api::ForegroundOutcome, ProcessError> {
        Ok(self
            .run_foreground_with_output_limit(cmd, None)
            .await?
            .outcome)
    }
    async fn run_foreground_with_output_limit(
        &self,
        cmd: &SandboxedCommand,
        limit: Option<usize>,
    ) -> Result<platform_api::ForegroundRunResult, ProcessError> {
        #[cfg(windows)]
        if super::supervisor::enabled(cmd) {
            return super::supervisor::execute(cmd, limit, false).await;
        }
        super::background::run(cmd, limit, false).await
    }
    async fn spawn_background(
        &self,
        cmd: &SandboxedCommand,
    ) -> Result<ProcessHandle, ProcessError> {
        #[cfg(windows)]
        if super::supervisor::enabled(cmd) {
            return match super::supervisor::execute(cmd, None, true).await?.outcome {
                platform_api::ForegroundOutcome::MovedToBackground(handle) => Ok(handle),
                _ => Err(ProcessError::Io("supervisor did not detach shell".into())),
            };
        }
        match super::background::run(cmd, Some(8192), true).await?.outcome {
            platform_api::ForegroundOutcome::MovedToBackground(handle) => Ok(handle),
            platform_api::ForegroundOutcome::Completed(_) => {
                unreachable!("explicit background path always hands off its spawned child")
            }
        }
    }

    #[cfg(windows)]
    async fn acknowledge_shell(&self, handle: &ProcessHandle) -> Result<(), ProcessError> {
        platform_api::shell_supervisor::acknowledge(handle).await
    }
    #[cfg(windows)]
    async fn export_shell(
        &self,
        handle: &ProcessHandle,
    ) -> Result<platform_api::process::ShellProcessHandoff, ProcessError> {
        self.acknowledge_shell(handle).await?;
        super::supervisor::export(handle)
    }
    #[cfg(windows)]
    async fn validate_shell(
        &self,
        handoff: &platform_api::process::ShellProcessHandoff,
    ) -> Result<(), ProcessError> {
        super::supervisor::validate(handoff).await
    }
    #[cfg(windows)]
    async fn adopt_shell(
        &self,
        handoff: &platform_api::process::ShellProcessHandoff,
        sink: std::sync::Arc<dyn platform_api::BackgroundExitSink>,
    ) -> Result<ProcessHandle, ProcessError> {
        super::supervisor::adopt(handoff, sink).await
    }
    #[cfg(windows)]
    async fn release_shell(
        &self,
        handoff: &platform_api::process::ShellProcessHandoff,
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
