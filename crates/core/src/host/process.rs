//! Process execution abstraction.
//!
//! `ProcessRunner` is the boundary between the engine's tool layer (Bash,
//! sub-shells, helper invocations) and platform-specific process spawning.
//! By accepting only [`SandboxedCommand`], the trait makes it impossible to
//! launch a child process that has not first passed through a
//! [`crate::host::sandbox::Sandbox`] decision (D2 / spec A1).
//!
//! See spec §24 (Sandbox) and D17 (Runtime boundary).

use crate::host::sandbox::SandboxedCommand;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

pub use mobile_linux_api::ProcessStreamSink;

/// SH-07 — live-output observer for a HOOK child, so the hook layer can emit
/// claude-code's `system/hook_progress` stream-json frames while the hook is
/// still running.
///
/// Upstream (`tWi`, oracle 2.1.238 @ 296463298) polls a `getOutput()` closure
/// every `intervalMs ?? 1000` and emits a frame whenever the ACCUMULATED
/// `output` changed. The port inverts the plumbing: the platform runner PUSHES
/// each raw chunk as it is read, and the hook layer keeps the accumulator +
/// change detection + the 1 s cadence, so nothing about the polling contract
/// leaks into the runner.
///
/// Deltas are raw BYTES, never pre-decoded strings: a chunk boundary can split
/// a multi-byte UTF-8 sequence, and lossy-decoding per chunk would corrupt it.
/// The accumulator decodes once, over the whole buffer.
#[async_trait]
pub trait HookOutputObserver: Send + Sync {
    /// Consume one newly read chunk. Exactly one of the two slices is non-empty
    /// per call (the runner reads the two pipes independently).
    async fn on_chunk(&self, stdout_delta: &[u8], stderr_delta: &[u8]);
}

/// Runs sandbox-vetted commands on the host.
///
/// Implementations live in platform crates. Every method consumes a
/// [`SandboxedCommand`] so the type system enforces that the sandbox
/// pipeline has already been consulted.
#[async_trait]
pub trait ProcessRunner: Send + Sync {
    /// Export a supervised shell without changing its current owner.
    async fn export_shell(
        &self,
        _handle: &ProcessHandle,
    ) -> Result<ShellProcessHandoff, ProcessError> {
        Err(ProcessError::Unsupported)
    }
    /// Authenticate the live supervisor and its held child/output identities.
    async fn validate_shell(&self, _handoff: &ShellProcessHandoff) -> Result<(), ProcessError> {
        Err(ProcessError::Unsupported)
    }
    /// Attach this host's completion observer to an authenticated supervisor.
    async fn adopt_shell(
        &self,
        _handoff: &ShellProcessHandoff,
        _sink: Arc<dyn BackgroundExitSink>,
    ) -> Result<ProcessHandle, ProcessError> {
        Err(ProcessError::Unsupported)
    }
    /// Detach only this host's observer; never signal the supervised child.
    async fn release_shell(&self, _handoff: &ShellProcessHandoff) -> Result<(), ProcessError> {
        Err(ProcessError::Unsupported)
    }

    /// Whether the foreground runner observes `BackgroundTaskBinding::on_demand`
    /// and can detach the existing OS child without respawning it.
    fn supports_foreground_backgrounding(&self) -> bool {
        false
    }

    /// SIGKILL the live process groups launched by one agent. Returns the
    /// pre-signal PID snapshot, not a count of retained background task rows.
    async fn kill_owner_processes(&self, _owner: &str) -> Vec<u32> {
        Vec::new()
    }

    /// Run a sandboxed command to completion and collect its output.
    ///
    /// # Errors
    /// Returns [`ProcessError`] when the platform does not support process
    /// execution, when I/O fails, or when the command's timeout fires.
    async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError>;

    /// Run a sandboxed command while streaming stdout lines and stderr chunks.
    ///
    /// Platform runners with pipe support override this to deliver output live.
    /// The compatibility default preserves correctness for minimal/mobile test
    /// runners by executing through [`Self::run`] and replaying the captured
    /// output before returning.
    async fn run_streaming(
        &self,
        cmd: &SandboxedCommand,
        sink: std::sync::Arc<dyn ProcessStreamSink>,
    ) -> Result<ProcessOutput, ProcessError> {
        let output = self.run(cmd).await?;
        for line in output.stdout.lines() {
            sink.stdout_line(line.to_string()).await?;
        }
        if !output.stderr.is_empty() {
            sink.stderr_chunk(output.stderr.as_bytes().to_vec()).await?;
        }
        Ok(output)
    }

    /// Spawn a sandboxed command as a background process and return a
    /// handle that can later be passed to [`ProcessRunner::kill`].
    ///
    /// # Errors
    /// Returns [`ProcessError`] when the spawn fails or the platform does
    /// not support background processes.
    async fn spawn_background(&self, cmd: &SandboxedCommand)
        -> Result<ProcessHandle, ProcessError>;

    /// Kill a previously spawned background process.
    ///
    /// # Errors
    /// Returns [`ProcessError`] when the process cannot be terminated.
    async fn kill(&self, handle: &ProcessHandle) -> Result<(), ProcessError>;
    /// Confirm that the returned shell handle has been registered by its tool.
    async fn acknowledge_shell(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
        Ok(())
    }

    /// Whether this runner can spawn processes on the current host.
    fn is_available(&self) -> bool;

    /// Run a hook command with runtime async detection (claude-code
    /// `hooks.ts:1117-1166`): the child's FIRST stdout line is inspected, and if
    /// it parses as `{"async": true, "asyncTimeout"?: <ms>}` the hook is
    /// backgrounded (left running, detached, bounded by `asyncTimeout` or
    /// `default_async_timeout`) and [`HookRunOutcome::Backgrounded`] is returned
    /// immediately, so the hook never blocks the turn. Otherwise the hook runs to
    /// completion and its buffered output is returned as
    /// [`HookRunOutcome::Completed`].
    ///
    /// The default implementation performs NO streaming: it defers to
    /// [`ProcessRunner::run`] and always reports `Completed`, so a platform
    /// without a streaming runner keeps its current (buffered, never-backgrounded)
    /// behavior. Only runners that can stream stdout override this.
    async fn run_hook_with_async_detection(
        &self,
        cmd: &SandboxedCommand,
        _default_async_timeout: std::time::Duration,
    ) -> Result<HookRunOutcome, ProcessError> {
        Ok(HookRunOutcome::Completed(self.run(cmd).await?))
    }

    /// SH-07 — [`Self::run_hook_with_async_detection`] plus a live-output
    /// observer that receives every chunk of the child's stdout/stderr as it is
    /// read, so the hook layer can emit `system/hook_progress` frames while the
    /// child is still running (claude-code `tWi`, oracle 2.1.238 @ 296463298).
    ///
    /// The default implementation DROPS the observer and delegates, so a runner
    /// with no live pipes keeps its current buffered behavior and every existing
    /// [`ProcessRunner`] impl compiles unchanged. `observer: None` is identical
    /// to calling [`Self::run_hook_with_async_detection`] directly on every
    /// implementation, so the hook layer can use this one entry point always.
    ///
    /// # Errors
    /// Same as [`Self::run_hook_with_async_detection`].
    async fn run_hook_with_async_detection_observed(
        &self,
        cmd: &SandboxedCommand,
        default_async_timeout: std::time::Duration,
        _observer: Option<std::sync::Arc<dyn HookOutputObserver>>,
    ) -> Result<HookRunOutcome, ProcessError> {
        self.run_hook_with_async_detection(cmd, default_async_timeout)
            .await
    }

    /// Run a sandboxed FOREGROUND tool command, but — matching claude-code
    /// 2.1.210+ — when the command hits its timeout, hand the still-running child
    /// off to the background instead of killing it. Returns
    /// [`ForegroundOutcome::Completed`] when the command finished within its
    /// timeout, or [`ForegroundOutcome::MovedToBackground`] carrying the new
    /// background task handle when it timed out and was left running (its output
    /// keeps streaming to the task file so the model can Read it).
    ///
    /// The default implementation does NOT background: it defers to
    /// [`ProcessRunner::run`], mapping a normal finish to
    /// [`ForegroundOutcome::Completed`] and propagating the runner's timeout
    /// error unchanged, so a platform without the streaming handoff keeps its
    /// current kill-on-timeout behavior (the caller then falls back to the
    /// interrupted result). Only runners that can detach a timed-out child
    /// override this.
    ///
    /// # Errors
    /// Returns [`ProcessError`] on spawn/I/O failure. The default impl also
    /// surfaces [`ProcessError::Timeout`] on the timeout path.
    async fn run_foreground(
        &self,
        cmd: &SandboxedCommand,
    ) -> Result<ForegroundOutcome, ProcessError> {
        Ok(ForegroundOutcome::Completed(self.run(cmd).await?))
    }

    /// Run a foreground command with an output spill limit.
    ///
    /// This is an additive seam for callers that need to distinguish a
    /// completed command whose output was retained in a rooted task file from
    /// one whose output stayed inline.  Existing runners keep their historical
    /// behavior through the compatibility default; runners with a confined
    /// output manager can override this method and return the file identity.
    async fn run_foreground_with_output_limit(
        &self,
        cmd: &SandboxedCommand,
        _max_output_bytes: Option<usize>,
    ) -> Result<ForegroundRunResult, ProcessError> {
        Ok(ForegroundRunResult {
            outcome: self.run_foreground(cmd).await?,
            output_file: None,
        })
    }
}

/// Outcome of [`ProcessRunner::run_foreground`].
#[derive(Debug, Serialize, Deserialize)]
pub enum ForegroundOutcome {
    /// The command finished within its timeout; carries its collected output.
    Completed(ProcessOutput),
    /// The command exceeded its timeout and was moved to the background
    /// (claude-code 2.1.210+). The child keeps running detached with its output
    /// streaming to the task file; the handle names the task so the caller can
    /// build the "moved to the background" note and the model can Read the file.
    MovedToBackground(ProcessHandle),
}

/// A foreground command outcome plus optional identity for a rooted output
/// file used when the command's captured output exceeded the caller's inline
/// limit.  The metadata is intentionally provider-neutral so tool and
/// orchestration layers do not need to know which platform opened the file.
#[derive(Debug, Serialize, Deserialize)]
pub struct ForegroundRunResult {
    /// Whether the process completed inline or moved to the background.
    pub outcome: ForegroundOutcome,
    /// Metadata for a non-redundant, completed output spill, when one exists.
    pub output_file: Option<ProcessOutputFile>,
}

/// Identity and size of a rooted process-output file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessOutputFile {
    /// Stable task identifier associated with the output file.
    pub task_id: String,
    /// Absolute path to the rooted output file.
    pub path: String,
    /// Number of bytes written to the output file.
    pub size: u64,
}

/// Outcome of [`ProcessRunner::run_hook_with_async_detection`].
///
/// Not `Clone` — the `Backgrounded` variant carries a single-consumer
/// [`tokio::sync::oneshot::Receiver`] for the hook's eventual output.
#[derive(Debug)]
pub enum HookRunOutcome {
    /// The hook ran to completion; carries its buffered output.
    Completed(ProcessOutput),
    /// The hook's first stdout line was `{"async": true, …}`; it has been
    /// backgrounded (detached, bounded by `async_timeout`). The eventual drained
    /// output is delivered once through `output` when the detached process
    /// finishes (or is killed at `async_timeout`), so the caller can fold it
    /// back as an `async_hook_response` (claude-code `registerPendingAsyncHook`).
    Backgrounded {
        /// Effective background timeout: the marker's `asyncTimeout` when
        /// present (and `> 0`), else the caller's `default_async_timeout`
        /// (claude-code `asyncTimeout || 15000`). The fold-back registration
        /// bounds itself by this same value.
        async_timeout: std::time::Duration,
        /// Delivers the hook's eventual drained [`ProcessOutput`] exactly once
        /// when the detached process finishes or is killed at `async_timeout`.
        /// `None` when the runner cannot retain the output (e.g. the default,
        /// non-streaming trait impl, which never backgrounds at all).
        output: Option<tokio::sync::oneshot::Receiver<ProcessOutput>>,
    },
}

pub use mobile_linux_api::ProcessOutput;

/// A task identity a caller binds to a command it intends to background, so the
/// engine's task registry and the process runner agree on ONE id and ONE output
/// file for that command.
///
/// claude-code has a single shell spawn (`vV`, 2.1.263 `src_160988549.js`
/// @1879742) that mints the task identity up front for every command:
/// `Ur = Dh("local_bash"); Zr = new yI(Ur, ...)`. The id the model is handed as
/// `backgroundTaskId`, the id the task registry records, and the file the child
/// writes to are therefore the same identity. Without this binding the runner
/// mints a private id in a second id space and the registry can never resolve
/// the id the model was given.
#[derive(Clone)]
pub struct BackgroundTaskBinding {
    /// Registry task id. The runner reports it back on [`ProcessHandle`] and
    /// names the output file after it.
    pub task_id: String,
    /// Absolute path the child's captured output is appended to. The caller
    /// (the task registry) has already created it, so the runner opens it for
    /// append rather than creating it exclusively.
    pub output_path: PathBuf,
    /// Notified once, when the child is reaped, so the caller can settle the
    /// task record (claude-code `Ger`, which sets the terminal status from
    /// `Fpt(result)` and enqueues the completion `<task-notification>`).
    pub on_exit: Option<Arc<dyn BackgroundExitSink>>,
    /// Fired to move a still-running FOREGROUND child to the background on
    /// demand — the port of claude-code `I_t`'s first act, `t.background(e)`,
    /// where `t` is the live `shellCommand`.
    ///
    /// The runner waits on this alongside the deadline; either one takes the
    /// same move-to-background path, so an on-demand request and a timeout
    /// produce the same task record and the same output file. `None` means the
    /// command cannot be backgrounded on request (nothing holds the other end).
    ///
    /// One permit is enough: [`tokio::sync::Notify::notify_one`] stores a
    /// permit when no waiter is parked yet, so a request that races the
    /// runner's first poll is still observed rather than dropped.
    pub on_demand: Option<Arc<tokio::sync::Notify>>,
}

impl std::fmt::Debug for BackgroundTaskBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundTaskBinding")
            .field("task_id", &self.task_id)
            .field("output_path", &self.output_path)
            .field("on_exit", &self.on_exit.is_some())
            .field("on_demand", &self.on_demand.is_some())
            .finish()
    }
}

/// Receives the one-shot exit report for a backgrounded child.
#[async_trait]
pub trait BackgroundExitSink: Send + Sync {
    /// Report the live OS child before waiting for output or completion.
    async fn on_spawn(&self, _task_id: &str, _pid: u32) -> Result<(), ProcessError> {
        Ok(())
    }
    /// A process owner can request stop without signaling a possibly reused PID.
    fn stop_notify(&self) -> Option<Arc<tokio::sync::Notify>> {
        None
    }
    /// Output ownership moved to an independent supervisor before handle publication.
    async fn on_supervised_start(&self, _task_id: &str) {}
    /// Completion from a supervisor that owns the terminal output trailer.
    async fn on_supervised_exit(&self, task_id: &str, code: Option<i32>) {
        self.on_exit(task_id, code).await;
    }
    /// Distinguish an explicit stop from an unknown/signaled exit.
    async fn on_exit_with_status(&self, task_id: &str, code: Option<i32>, _killed: bool) {
        self.on_exit(task_id, code).await;
    }

    /// The independent supervisor and its durable receipt are both unavailable.
    async fn on_supervision_lost(&self, task_id: &str) {
        self.on_exit(task_id, Some(-1)).await;
    }

    /// Whether captured background output is owned by this sink. Legacy sinks
    /// retain direct runner-file output.
    fn manages_output(&self) -> bool {
        false
    }

    /// Append already-decoded, stderr-framed output through the task writer.
    async fn append_output(&self, _task_id: &str, _content: &str) -> Result<(), ProcessError> {
        Ok(())
    }

    /// Wait for accepted output before the terminal notification is published.
    async fn flush_output(&self, _task_id: &str) -> Result<(), ProcessError> {
        Ok(())
    }

    /// Cap a completed persisted copy and return its PRE-truncation stat size
    /// (oracle Ibt/N7e), which may exceed the retained file's byte length.
    async fn finalize_persisted_output(
        &self,
        _task_id: &str,
        _max_bytes: u64,
    ) -> Result<Option<u64>, ProcessError> {
        Ok(None)
    }

    /// Called exactly once after the background child is reaped.
    ///
    /// `exit_code` is `None` when the platform could not report one (signalled
    /// death, or a reaper that lost the child).
    async fn on_exit(&self, task_id: &str, exit_code: Option<i32>);

    /// A background shell has stopped producing output at an interactive prompt.
    /// This is advisory: it neither settles the task nor consumes its completion.
    async fn on_stall(&self, _task_id: &str, _tail: &str) {}

    /// Atomically claim a memory-pressure stop if the owning session is idle and
    /// this shell is eligible. The runner kills its held child only after `true`.
    /// Hosts without the necessary session state conservatively decline.
    async fn on_memory_pressure(&self, _task_id: &str) -> bool {
        false
    }
}

/// Authenticated supervisor capability used for a shell handoff. Paths and PIDs
/// are corroborated by the supervisor's held state; never authorize kill by PID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellProcessHandoff {
    pub task_id: String,
    pub pid: u32,
    pub supervisor_pid: u32,
    /// Birth of the independent writer; absent legacy receipts cannot prove death.
    #[serde(default)]
    pub supervisor_start_identity: Option<String>,
    #[serde(default)]
    pub supervisor_directory_identity: Option<crate::host::rooted_fs::RootIdentity>,
    #[serde(default)]
    pub output_root_identity: Option<crate::host::rooted_fs::RootIdentity>,
    #[serde(default)]
    pub output_file_identity: Option<crate::host::rooted_fs::RootIdentity>,
    /// Birth identity used only for guarded cleanup if the supervisor disappears.
    #[serde(default)]
    pub process_start_identity: Option<String>,
    /// Agent whose live process ownership follows this shell.
    #[serde(default)]
    pub owner: Option<String>,
    pub socket_path: String,
    pub receipt_path: String,
    pub output_path: String,
    pub nonce: String,
}

/// Handle to a background process previously spawned by
/// [`ProcessRunner::spawn_background`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessHandle {
    /// Engine-side task identifier.
    pub task_id: String,
    /// OS-level process id.
    pub pid: u32,
}

pub use mobile_linux_api::ProcessError;

#[cfg(test)]
mod tests {
    use super::ProcessError;

    #[test]
    fn structured_variants_name_the_guarantee() {
        assert_eq!(
            ProcessError::PolicyUnsupported("networked shell is not supported".into()).to_string(),
            "policy unsupported: networked shell is not supported"
        );
        assert_eq!(
            ProcessError::MalformedSandboxPlan("missing android plan".into()).to_string(),
            "malformed sandbox plan: missing android plan"
        );
        assert_eq!(
            ProcessError::SandboxEnforcementFailed("seccomp load failed".into()).to_string(),
            "sandbox enforcement failed: seccomp load failed"
        );
    }
}
