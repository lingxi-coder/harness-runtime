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
use futures::FutureExt as _;
use lingxi_core::host::sandbox::{ProcessCommand, SandboxedCommand};
use lingxi_core::host::{
    truncate_tail, FusionImplementHost, VerificationOutcome, VerificationRun, WorktreeManager,
    FUSION_VERIFICATION_OUTPUT_BYTE_CAP,
};
use mobile_linux_api::{ProcessError, ProcessStreamSink};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tool_api::BuiltinToolContext;

const VERIFY_SHELL: &str = "/bin/bash";

/// Grace after a command's own timeout before the host gives up waiting.
const VERIFY_GRACE: Duration = Duration::from_secs(5);

/// Time to wait for owned process cleanup before reporting it unverified.
const VERIFY_STOP_GRACE: Duration = Duration::from_secs(20);

const SANDBOX_REQUIRED: &str = "implement mode needs the sandbox, so the panels' commands cannot write outside their own worktrees. Turn it on with /sandbox (or `sandbox.enabled` in settings); it must also be available on this host.";

/// The desktop [`FusionImplementHost`].
pub struct DesktopFusionImplementHost {
    worktrees: Arc<dyn WorktreeManager>,
    /// Session workspace, where the worktrees live; the free-disk check looks here.
    workspace: PathBuf,
    tool_ctx: Arc<agent::RuntimeLink<Arc<BuiltinToolContext>>>,
}

impl DesktopFusionImplementHost {
    /// A host whose tool context is bound later.
    #[must_use]
    pub fn new(worktrees: Arc<dyn WorktreeManager>, workspace: PathBuf) -> Self {
        Self {
            worktrees,
            workspace,
            tool_ctx: Arc::new(agent::RuntimeLink::new()),
        }
    }

    /// The cell the composition root fills once the session's tool context
    /// exists.
    #[must_use]
    pub fn context_cell(&self) -> Arc<agent::RuntimeLink<Arc<BuiltinToolContext>>> {
        Arc::clone(&self.tool_ctx)
    }

    /// Release session services after producer shutdown and refuse late verification.
    pub(crate) fn clear_context(&self) {
        self.tool_ctx.clear();
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

enum OwnedVerificationEvent {
    Finished(Result<mobile_linux_api::ProcessOutput, ProcessError>),
    NotStarted,
    Unverified(String),
}

struct CancelVerificationOnDrop(CancellationToken);

impl Drop for CancelVerificationOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Keep polling an owned process after cancellation. If the caller stops
/// waiting, the worker continues to hold the command's sandbox lease.
async fn run_owned_verification(
    process: Arc<dyn lingxi_core::host::ProcessRunner>,
    command: SandboxedCommand,
    owner: String,
    sink: Arc<dyn ProcessStreamSink>,
    timeout: Duration,
    stop_grace: Duration,
    retain_on_unverified: bool,
    cancel: CancellationToken,
) -> VerificationOutcome {
    if cancel.is_cancelled() {
        return VerificationOutcome::Error {
            message: "cancelled".into(),
        };
    }
    let stop = CancellationToken::new();
    let _stop_on_drop = CancelVerificationOnDrop(stop.clone());
    let worker_stop = stop.clone();
    let (send, mut receive) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if worker_stop.is_cancelled() {
            let _ = send.send(OwnedVerificationEvent::NotStarted);
            return;
        }
        let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started = polled.clone();
        let run = async {
            started.store(true, Ordering::SeqCst);
            process.run_streaming(&command, sink).await
        };
        let run = std::panic::AssertUnwindSafe(run).catch_unwind();
        tokio::pin!(run);
        let mut stopping = false;
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let event = loop {
            tokio::select! {
                biased;
                () = worker_stop.cancelled(), if !stopping => {
                    if !polled.load(Ordering::SeqCst) {
                        break OwnedVerificationEvent::NotStarted;
                    }
                    stopping = true;
                }
                result = &mut run => {
                    break match result {
                        Ok(Err(ProcessError::Io(message)))
                            if message.starts_with(lingxi_core::host::process::OWNED_STREAMING_CLEANUP_UNVERIFIED) =>
                        {
                            OwnedVerificationEvent::Unverified(message)
                        }
                        Ok(result) => OwnedVerificationEvent::Finished(result),
                        Err(_) => OwnedVerificationEvent::Unverified("process runner panicked".into()),
                    };
                }
                _ = tick.tick(), if stopping => {
                    if std::panic::AssertUnwindSafe(process.kill_owner_processes(&owner))
                        .catch_unwind().await.is_err()
                    {
                        break OwnedVerificationEvent::Unverified("process owner cleanup panicked".into());
                    }
                }
            }
        };
        let unverified = matches!(event, OwnedVerificationEvent::Unverified(_));
        let _ = send.send(event);
        if unverified && retain_on_unverified {
            // No safe reaping evidence remains. Retain the command and its
            // sandbox lease instead of deleting resources used by a live child.
            std::future::pending::<()>().await;
            std::hint::black_box(&command);
        }
    });

    let timed_out = tokio::time::sleep(timeout);
    tokio::pin!(timed_out);
    let timed_out = tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        event = &mut receive => {
            return verification_event(event);
        }
        () = &mut timed_out => true,
    };
    stop.cancel();
    match tokio::time::timeout(stop_grace, &mut receive).await {
        Ok(Ok(OwnedVerificationEvent::Finished(_) | OwnedVerificationEvent::NotStarted)) => {
            if timed_out {
                VerificationOutcome::TimedOut
            } else {
                VerificationOutcome::Error {
                    message: "cancelled".into(),
                }
            }
        }
        Ok(event) => verification_event(event),
        Err(_) => VerificationOutcome::Error {
            message: if timed_out {
                "timed out; process cleanup is unverified"
            } else {
                "cancelled; process cleanup is unverified"
            }
            .into(),
        },
    }
}

fn verification_event(
    event: Result<OwnedVerificationEvent, tokio::sync::oneshot::error::RecvError>,
) -> VerificationOutcome {
    match event {
        Ok(OwnedVerificationEvent::Finished(Ok(output))) if output.exit_code == 0 => {
            VerificationOutcome::Passed
        }
        Ok(OwnedVerificationEvent::Finished(Ok(output))) => VerificationOutcome::Failed {
            exit_code: (output.exit_code >= 0).then_some(output.exit_code),
        },
        Ok(OwnedVerificationEvent::Finished(Err(ProcessError::Timeout))) => {
            VerificationOutcome::TimedOut
        }
        Ok(OwnedVerificationEvent::Finished(Err(error))) => VerificationOutcome::Error {
            message: error.to_string(),
        },
        Ok(OwnedVerificationEvent::NotStarted) => VerificationOutcome::Error {
            message: "cancelled".into(),
        },
        Ok(OwnedVerificationEvent::Unverified(message)) => VerificationOutcome::Error {
            message: format!("process cleanup is unverified: {message}"),
        },
        Err(_) => VerificationOutcome::Error {
            message: "process cleanup is unverified: worker stopped unexpectedly".into(),
        },
    }
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
        if !sandbox_engaged(&ctx) {
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
        if !sandbox_engaged(&ctx) {
            return error("not run: the sandbox is off");
        }
        // The session's own network policy applies unchanged: the live runner
        // shares one proxy across commands, so a per-command network config
        // would swap the allow-list under the main session.
        let cfg = ctx.sandbox_runtime_at(worktree, sandbox::root::SandboxRootScope::Agent);
        let wrapped = match ctx
            .sandbox_runner
            .wrap_with_lease(
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
        let tool_api::sandbox_runner::SandboxWrappedCommand {
            command: wrapped,
            lease,
        } = wrapped;
        let owned_streaming = ctx.process.supports_owned_streaming_cleanup();
        if !lease.is_noop() && !owned_streaming {
            return error("the process runner cannot verify sandbox cleanup");
        }
        let owner = format!(
            "{}{}",
            lingxi_core::host::process::OWNED_STREAMING_OWNER_PREFIX,
            tool_api::util::ids::ulid_or_uuid()
        );
        let spawn = ProcessCommand {
            command: VERIFY_SHELL.to_string(),
            args: vec!["-c".into(), "-l".into(), wrapped],
            cwd: Some(worktree.to_path_buf()),
            env: HashMap::new(),
            // Owned execution manages the timeout while continuing to poll
            // the process through cancellation and verified reaping.
            timeout: (!owned_streaming).then_some(timeout),
            stdin: None,
        };
        let sandboxed = ctx
            .sandbox
            .bypass_with_audit(spawn, "fusion_verification")
            .with_lifetime_guard(lease.clone())
            .with_process_owner(owned_streaming.then_some(owner.clone()));
        let tail = Arc::new(TailSink::new(FUSION_VERIFICATION_OUTPUT_BYTE_CAP));
        let outcome = if owned_streaming {
            run_owned_verification(
                ctx.process.clone(),
                sandboxed,
                owner,
                tail.clone(),
                timeout,
                VERIFY_STOP_GRACE,
                !lease.is_noop(),
                cancel,
            )
            .await
        } else {
            let run = ctx.process.run_streaming(&sandboxed, tail.clone());
            tokio::select! {
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
            }
        };
        drop(lease);
        finish(
            outcome,
            truncate_tail(&tail.text(), FUSION_VERIFICATION_OUTPUT_BYTE_CAP),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct ControlledRunner {
        started: tokio::sync::Notify,
        stop_requested: tokio::sync::Notify,
        allow_exit: tokio::sync::Notify,
    }

    #[async_trait]
    impl lingxi_core::host::ProcessRunner for ControlledRunner {
        fn supports_owned_streaming_cleanup(&self) -> bool {
            true
        }

        async fn run(
            &self,
            _: &SandboxedCommand,
        ) -> Result<mobile_linux_api::ProcessOutput, ProcessError> {
            unreachable!("the verification uses streaming")
        }

        async fn run_streaming(
            &self,
            _: &SandboxedCommand,
            _: Arc<dyn ProcessStreamSink>,
        ) -> Result<mobile_linux_api::ProcessOutput, ProcessError> {
            self.started.notify_one();
            self.allow_exit.notified().await;
            Ok(mobile_linux_api::ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: -1,
                timed_out: false,
            })
        }

        async fn kill_owner_processes(&self, _: &str) -> Vec<u32> {
            self.stop_requested.notify_one();
            Vec::new()
        }

        async fn spawn_background(
            &self,
            _: &SandboxedCommand,
        ) -> Result<lingxi_core::host::ProcessHandle, ProcessError> {
            Err(ProcessError::Unsupported)
        }

        async fn kill(&self, _: &lingxi_core::host::ProcessHandle) -> Result<(), ProcessError> {
            Err(ProcessError::Unsupported)
        }

        fn is_available(&self) -> bool {
            true
        }
    }

    fn command_with_lease(released: Arc<std::sync::atomic::AtomicBool>) -> SandboxedCommand {
        let command = ProcessCommand {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "true".into()],
            cwd: None,
            env: HashMap::new(),
            timeout: None,
            stdin: None,
        };
        let lease = tool_api::sandbox_runner::SandboxLease::new(move || {
            released.store(true, Ordering::SeqCst);
        });
        SandboxedCommand::__new_sandboxed(
            command,
            lingxi_core::host::sandbox::SandboxedTag::BypassAuditedWithReason {
                reason: "fusion-test".into(),
            },
        )
        .with_lifetime_guard(lease)
    }

    async fn wait_for_release(released: &std::sync::atomic::AtomicBool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !released.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("reaped verification must release its lease");
    }

    #[tokio::test]
    async fn cancelled_verification_holds_lease_until_runner_exits() {
        let process = Arc::new(ControlledRunner::default());
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_owned_verification(
            process.clone(),
            command_with_lease(released.clone()),
            "owned-streaming:test".into(),
            Arc::new(TailSink::new(32)),
            Duration::from_secs(10),
            Duration::from_secs(5),
            true,
            cancel.clone(),
        ));
        process.started.notified().await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), process.stop_requested.notified())
            .await
            .expect("cancel must request owner termination");
        assert!(!released.load(Ordering::SeqCst));
        process.allow_exit.notify_one();
        assert!(matches!(
            task.await.unwrap(),
            VerificationOutcome::Error { message } if message == "cancelled"
        ));
        wait_for_release(&released).await;
    }

    #[tokio::test]
    async fn unverified_cancellation_retains_lease_until_late_exit() {
        let process = Arc::new(ControlledRunner::default());
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_owned_verification(
            process.clone(),
            command_with_lease(released.clone()),
            "owned-streaming:test".into(),
            Arc::new(TailSink::new(32)),
            Duration::from_secs(10),
            Duration::from_millis(30),
            true,
            cancel.clone(),
        ));
        process.started.notified().await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), process.stop_requested.notified())
            .await
            .expect("cancel must request owner termination");
        assert!(matches!(
            task.await.unwrap(),
            VerificationOutcome::Error { message } if message.contains("cleanup is unverified")
        ));
        assert!(!released.load(Ordering::SeqCst));
        process.allow_exit.notify_one();
        wait_for_release(&released).await;
    }

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
