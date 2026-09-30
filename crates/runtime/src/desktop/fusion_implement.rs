//! Desktop side of Fusion's implement mode.
//!
//! [`DesktopFusionImplementHost`] is what the orchestrator asks for the three
//! things only the host can do: hand out git worktrees, refuse to start when
//! it cannot confine the panels, and run the verification commands inside the
//! sandbox. The panels themselves go through the ordinary subagent spawner;
//! their Bash calls are sandboxed because they carry a `cwd` override (see
//! `sandbox::root`).
//!
//! The session's tool context does not exist yet when the Fusion executor is
//! built, so it is bound afterwards through [`DesktopFusionImplementHost::context_cell`].
//! Until then every request is refused rather than run unconfined.

use async_trait::async_trait;
use lingxi_core::host::sandbox::ProcessCommand;
use lingxi_core::host::{
    truncate_tail, FusionImplementHost, VerificationOutcome, VerificationRun, WorktreeManager,
    FUSION_VERIFICATION_OUTPUT_BYTE_CAP,
};
use mobile_linux_api::{ProcessError, ProcessStreamSink};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tool_api::BuiltinToolContext;

const VERIFY_SHELL: &str = "/bin/bash";

/// Grace after a command's own timeout before the host gives up waiting.
const VERIFY_GRACE: Duration = Duration::from_secs(5);

const SANDBOX_REQUIRED: &str = "implement mode needs the sandbox, so the panels' commands cannot write outside their own worktrees. Turn it on with /sandbox (or `sandbox.enabled` in settings); it must also be available on this host.";

/// The desktop [`FusionImplementHost`].
pub struct DesktopFusionImplementHost {
    worktrees: Arc<dyn WorktreeManager>,
    /// Session workspace, where the worktrees live; the free-disk check looks here.
    workspace: PathBuf,
    tool_ctx: Arc<OnceLock<BuiltinToolContext>>,
}

impl DesktopFusionImplementHost {
    /// A host whose tool context is bound later.
    #[must_use]
    pub fn new(worktrees: Arc<dyn WorktreeManager>, workspace: PathBuf) -> Self {
        Self {
            worktrees,
            workspace,
            tool_ctx: Arc::new(OnceLock::new()),
        }
    }

    /// The cell the composition root fills once the session's tool context
    /// exists.
    #[must_use]
    pub fn context_cell(&self) -> Arc<OnceLock<BuiltinToolContext>> {
        Arc::clone(&self.tool_ctx)
    }
}

/// Whether Bash commands would really be sandboxed right now: the boot-time
/// availability the Bash tool keys on, and the live `/sandbox` toggle.
fn sandbox_engaged(ctx: &BuiltinToolContext) -> bool {
    let enabled = ctx
        .sandbox_enabled_override
        .as_ref()
        .map_or(ctx.sandbox_runtime.enabled, |cell| {
            cell.load(Ordering::Relaxed)
        });
    ctx.sandbox_available && enabled
}

fn human_bytes(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{} MiB", bytes / MIB)
    }
}

/// Keeps the end of what a command prints, in arrival order.
struct TailSink {
    keep: usize,
    buf: Mutex<Vec<u8>>,
}

impl TailSink {
    fn new(keep: usize) -> Self {
        Self {
            keep,
            buf: Mutex::new(Vec::new()),
        }
    }

    fn push(&self, bytes: &[u8]) {
        let mut buf = self
            .buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        buf.extend_from_slice(bytes);
        // Trim in bulk so a chatty command does not shift the buffer per line.
        if buf.len() > self.keep.saturating_mul(2) {
            let cut = buf.len() - self.keep;
            buf.drain(..cut);
        }
    }

    fn text(&self) -> String {
        let buf = self
            .buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clean_output(&String::from_utf8_lossy(&buf))
    }
}

#[async_trait]
impl ProcessStreamSink for TailSink {
    async fn stdout_line(&self, line: String) -> Result<(), ProcessError> {
        self.push(line.as_bytes());
        self.push(b"\n");
        Ok(())
    }

    async fn stderr_chunk(&self, chunk: Vec<u8>) -> Result<(), ProcessError> {
        self.push(&chunk);
        Ok(())
    }
}

/// Command output without terminal escapes or control characters, which mean
/// nothing to the model reading it.
fn clean_output(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
            }
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[async_trait]
impl FusionImplementHost for DesktopFusionImplementHost {
    fn worktrees(&self) -> Arc<dyn WorktreeManager> {
        Arc::clone(&self.worktrees)
    }

    async fn preflight(&self, min_free_disk_bytes: u64) -> Result<(), String> {
        let Some(ctx) = self.tool_ctx.get() else {
            return Err("the session is still starting; try again in a moment".into());
        };
        if !sandbox_engaged(ctx) {
            return Err(SANDBOX_REQUIRED.into());
        }
        if let Some(free) = platform_posix::worktree::available_disk_bytes(&self.workspace) {
            if free < min_free_disk_bytes {
                return Err(format!(
                    "only {} of disk space is free where the worktrees go; implement mode needs at least {} (fusion.implement.minFreeDiskBytes)",
                    human_bytes(free),
                    human_bytes(min_free_disk_bytes)
                ));
            }
        }
        Ok(())
    }

    async fn verify(
        &self,
        worktree: &Path,
        command: &str,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> VerificationRun {
        let started = Instant::now();
        let finish = |outcome: VerificationOutcome, output_tail: String| VerificationRun {
            command: command.to_string(),
            outcome,
            duration_ms: millis(started),
            output_tail,
        };
        let error = |message: &str| {
            finish(
                VerificationOutcome::Error {
                    message: message.to_string(),
                },
                String::new(),
            )
        };
        let Some(ctx) = self.tool_ctx.get() else {
            return error("the session is still starting");
        };
        // Never run a verification command unconfined, even if the sandbox
        // was switched off after the run began.
        if !sandbox_engaged(ctx) {
            return error("not run: the sandbox is off");
        }
        // The session's own network policy applies unchanged: the live runner
        // shares one proxy across commands, so a per-command network config
        // would swap the allow-list under the main session.
        let cfg = ctx.sandbox_runtime_at(worktree, sandbox::root::SandboxRootScope::Agent);
        let wrapped = match ctx
            .sandbox_runner
            .wrap(
                command,
                &cfg,
                ctx.platform,
                Some(VERIFY_SHELL),
                Some(worktree),
            )
            .await
        {
            Ok(wrapped) => wrapped,
            Err(refused) => return error(&format!("the sandbox refused the command: {refused}")),
        };
        let spawn = ProcessCommand {
            command: VERIFY_SHELL.to_string(),
            args: vec!["-c".into(), "-l".into(), wrapped],
            cwd: Some(worktree.to_path_buf()),
            env: HashMap::new(),
            timeout: Some(timeout),
            stdin: None,
        };
        let sandboxed = ctx.sandbox.bypass_with_audit(spawn, "fusion_verification");
        let tail = Arc::new(TailSink::new(FUSION_VERIFICATION_OUTPUT_BYTE_CAP));
        let run = ctx.process.run_streaming(&sandboxed, tail.clone());
        // Dropping the run kills the command's whole process group.
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => VerificationOutcome::Error { message: "cancelled".into() },
            ran = tokio::time::timeout(timeout + VERIFY_GRACE, run) => match ran {
                Err(_) | Ok(Err(ProcessError::Timeout)) => VerificationOutcome::TimedOut,
                Ok(Err(other)) => VerificationOutcome::Error { message: other.to_string() },
                Ok(Ok(output)) if output.exit_code == 0 => VerificationOutcome::Passed,
                Ok(Ok(output)) => VerificationOutcome::Failed {
                    exit_code: (output.exit_code >= 0).then_some(output.exit_code),
                },
            },
        };
        ctx.sandbox_runner.cleanup_after_command().await;
        finish(
            outcome,
            truncate_tail(&tail.text(), FUSION_VERIFICATION_OUTPUT_BYTE_CAP),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_loses_escapes_and_control_characters_but_keeps_lines() {
        assert_eq!(
            clean_output("\u{1b}[31merror\u{1b}[0m: bad\r\n\tat x\u{0}y"),
            "error: bad\n\tat xy"
        );
    }

    #[test]
    fn the_tail_keeps_the_end_and_stays_bounded() {
        let sink = TailSink::new(16);
        for i in 0..100 {
            sink.push(format!("line {i}\n").as_bytes());
        }
        let text = sink.text();
        assert!(text.len() <= 32, "{}", text.len());
        assert!(text.ends_with("line 99\n"));
    }

    #[tokio::test]
    async fn an_unbound_host_refuses_everything() {
        let host = DesktopFusionImplementHost::new(
            Arc::new(platform_posix::worktree::PosixWorktreeManager::new(
                std::env::temp_dir(),
            )),
            std::env::temp_dir(),
        );
        assert!(host.preflight(0).await.is_err());
        let run = host
            .verify(
                Path::new("."),
                "true",
                Duration::from_secs(1),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(run.outcome, VerificationOutcome::Error { .. }));
    }

    #[test]
    fn sizes_render_in_the_larger_unit() {
        assert_eq!(human_bytes(5 << 30), "5.0 GiB");
        assert_eq!(human_bytes(300 << 20), "300 MiB");
    }
}
