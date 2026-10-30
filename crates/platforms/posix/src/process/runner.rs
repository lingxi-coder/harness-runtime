//! `tokio::process`-backed [`ProcessRunner`] for desktop hosts.
//!
//! Foreground `run` applies the claude-code spawn-env contract
//! (`LINGXI=1`, `GIT_EDITOR=true`, `AI_AGENT=<…>`, `SHELL=<bin>` for the
//! bash provider) plus a 30-minute
//! default timeout. `spawn_background` lands a real child with
//! file-mode stdio (POSIX `O_NOFOLLOW`) wired to a per-task output file
//! and a setsid call so [`super::kill_tree::kill_tree_force`] can later
//! terminate the descendant process group. `kill(handle)` delegates to
//! `kill_tree_force(handle.pid)` — immediate SIGKILL with no grace period,
//! matching `treeKill(pid, 'SIGKILL')` in `src/utils/ShellCommand.ts:337-343`.

use crate::process::active_children;
use crate::process::kill_tree::kill_tree_force;
use crate::process::spawn_unsafe::attach_setsid;
use crate::process::wrap::{
    ai_agent_value, is_bash_provider_shell, task_output_dir, DEFAULT_TIMEOUT, ENV_AI_AGENT,
    ENV_GIT_EDITOR, ENV_LINGXI_CHILD_SESSION, ENV_LINGXI_MARKER, ENV_LINGXI_SESSION_ID, ENV_SHELL,
};
use async_trait::async_trait;
use lingxi_core::host::{
    HookRunOutcome, ProcessHandle, ProcessRunner, SandboxedCommand, SandboxedTag,
};
use mobile_linux_api::ProcessStreamSink;
use mobile_linux_api::{ProcessError, ProcessOutput};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Audit reason stamped on a hook command by the hooks crate
/// (`hooks/src/executor.rs` — `bypass_with_audit(pcmd, "hook_command")`).
///
/// claude-code assembles a hook command's env as `{...WO(), ...Uot(o),
/// LINGXI_PROJECT_DIR}` with `o.source==="harness"` (BIN off 205727901 /
/// 199137330). For `source==="harness"`, `Uot` does NOT emit `AI_AGENT`
/// (that is gated `source==="agent"`), and the hook env carries no
/// `GIT_EDITOR` (that is a Bash-spawn-only var). The hooks crate already
/// folds the full `Uot(harness)` set (`LINGXI` / `LINGXI_SESSION_ID`
/// / `LINGXI_CHILD_SESSION` / `LINGXI_EFFORT`) into the command env, so
/// the runner must NOT layer the Bash-spawn `AI_AGENT` / `GIT_EDITOR` on top
/// of a hook child. We detect a hook command by this audit reason and skip
/// those two vars — every other command (Bash / REPL / PowerShell tool calls)
/// keeps the full Bash-spawn contract unchanged.
const HOOK_COMMAND_AUDIT_REASON: &str = "hook_command";

// Hook commands are allowed to ignore their event input. If the child closes
// stdin early, EPIPE says only that the remaining input cannot be delivered;
// the hook's stdout, stderr and exit status still determine its result. Other
// write errors remain failures, and the caller must still wait for the child.
async fn write_hook_stdin<W: tokio::io::AsyncWrite + Unpin>(
    stdin: &mut W,
    input: &[u8],
) -> std::io::Result<()> {
    match stdin.write_all(input).await {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

/// SH-07 — drain `r` to EOF into `buf`, pushing every chunk to `observer` as it
/// is read.
///
/// This is the live half of claude-code's `hook_progress` polling: upstream's
/// `tWi` reads a growing accumulator every second, which only works because the
/// child's `stdout`/`stderr` `data` listeners append to it as bytes arrive. A
/// `read_to_end` publishes nothing until EOF, so a progress poll layered over it
/// would emit exactly zero frames.
///
/// Chunks are handed over as raw bytes — decoding per chunk would corrupt a
/// multi-byte UTF-8 sequence that straddles a read boundary.
async fn drain_observed<R>(
    r: &mut R,
    buf: &mut Vec<u8>,
    observer: Option<&std::sync::Arc<dyn lingxi_core::host::HookOutputObserver>>,
    is_stderr: bool,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    // No observer ⇒ the cheap bulk read, byte-identical to the previous code.
    if observer.is_none() {
        r.read_to_end(buf).await?;
        return Ok(());
    }
    let mut chunk = vec![0u8; 8192];
    loop {
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(obs) = observer {
            if is_stderr {
                obs.on_chunk(&[], &chunk[..n]).await;
            } else {
                obs.on_chunk(&chunk[..n], &[]).await;
            }
        }
    }
}

/// Auth / session / OTEL env keys claude-code STRIPS from a hook command's
/// environment. Mirrors `WO()` (claude-code BIN off ~195508064), which builds
/// the hook child env from `process.env` and then `delete`s each of these keys
/// (plus every `OTEL_*` key, handled separately below) so a hook script can
/// never read the user's OAuth token, subscription/rate-limit tier, the
/// background-session auth handles, or the resume/session bookkeeping. These
/// are EXACT key names — `WO()` deletes specific keys (NOT wildcard patterns)
/// for everything except the `OTEL_` prefix sweep. `AI_AGENT` / `GIT_EDITOR`
/// are deliberately NOT here: they are not in `WO()`'s denylist (they are
/// handled by the `is_hook_command` Bash-spawn gate above) and stripping them
/// is already correct via that path.
const HOOK_ENV_DENYLIST: &[&str] = &[
    "CLAUDE_CODE_OAUTH_TOKEN",
    "LINGXI_SUBSCRIPTION_TYPE",
    "LINGXI_RATE_LIMIT_TIER",
    "LINGXI_BG_AUTH_SNAPSHOT_PATH",
    "LINGXI_BG_SOCKET_TOKENS_PATH",
    "LINGXI_BG_RV_AUTH",
    "LINGXI_BG_PTY_AUTH",
    "LINGXI_SESSION_KIND",
    "LINGXI_BG_SOURCE",
    "LINGXI_BG_ISOLATION",
    "LINGXI_BG_BACKEND",
    "LINGXI_SESSION_NAME",
    // ⚠️ Scrub-list only — nothing in this port SETS or READS this, and the
    // backlog entry that calls it "中断回合自动续跑 + 6h 上限" (auto-resume an
    // interrupted turn, with a 6h cap) understates it by a lot. Upstream it
    // gates a crash-RESPAWN recovery protocol, not a resume convenience:
    //
    // * The only writer is the background-PTY supervisor, which sets it when
    //   re-launching a worker it already tried once (`attempt > 1` with the
    //   same session), alongside `CLAUDE_CODE_RESUME_PROMPT` and
    //   `..._MAX_AGE_MS`. A user never sets it and `--resume` never sets it.
    // * Behind it sits the whole `turnInterruptionState` reconstruction:
    //   replaying a trailing `interrupted_prompt`, resolving trailing
    //   unresolved `tool_use` ids, superseded-tool bookkeeping, adopting a
    //   parked permission request, re-notifying orphaned background tasks, and
    //   resuming a `workflow_launch` — most of it on the SERVED/remote
    //   transport, which is out of scope here.
    // * The "6h cap" is `E6o()`: env override, else a statsig flag inside a
    //   min/max band, else a compiled default — an age bound on how stale a
    //   turn may be before the resume is suppressed, not the feature itself.
    //
    // So this is a subsystem to schedule against the background-session
    // supervisor, not a gap to patch. Verified at the 2.1.270 oracle
    // 2026-09-14; scrubbing it stays correct either way, since a hook child
    // must not learn that its parent is a crash respawn.
    "LINGXI_RESUME_INTERRUPTED_TURN",
    "LINGXI_RESUME_PROMPT",
    "LINGXI_BG_SESSION_PERMISSION_RULES",
    "LINGXI_BG_MEMORY_TOGGLED_OFF",
    "LINGXI_OTEL_DIAG_STDERR",
];

/// Prefix `WO()` sweeps from the hook child env: every key starting with
/// `OTEL_` is `delete`d (`for(let u of Object.keys(process.env))if(u
/// .startsWith("OTEL_"))delete c[u]`). Applied in addition to
/// [`HOOK_ENV_DENYLIST`].
const HOOK_ENV_DENY_PREFIX: &str = "OTEL_";

/// Env var gating the GHA subprocess secret-scrub (`subprocessEnv()`,
/// `utils/subprocessEnv.ts:86`). claude-code-action sets it when running with
/// untrusted content; truthy ⇒ scrub [`GHA_SUBPROCESS_SCRUB`] from EVERY
/// subprocess env (Bash AND hook children both spawn via `subprocessEnv()`).
const ENV_SUBPROCESS_ENV_SCRUB: &str = "LINGXI_SUBPROCESS_ENV_SCRUB";

/// Secret-bearing keys `subprocessEnv()` `delete`s from a child env when
/// [`ENV_SUBPROCESS_ENV_SCRUB`] is truthy (`subprocessEnv.ts:15-53`,
/// `GHA_SUBPROCESS_SCRUB`): Anthropic auth, OTLP exporter headers (carry bearer
/// tokens), cloud-provider creds, GitHub-Actions OIDC/runtime tokens, and
/// claude-code-action input duplicates. Each key's GitHub-Actions `INPUT_<KEY>`
/// twin is stripped too. Without the flag (the common case) this is inert.
const GHA_SUBPROCESS_SCRUB: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_CUSTOM_HEADERS",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "AZURE_CLIENT_SECRET",
    "AZURE_CLIENT_CERTIFICATE_PATH",
    "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
    "ACTIONS_ID_TOKEN_REQUEST_URL",
    "ACTIONS_RUNTIME_TOKEN",
    "ACTIONS_RUNTIME_URL",
    "ALL_INPUTS",
    "OVERRIDE_GITHUB_TOKEN",
    "DEFAULT_WORKFLOW_TOKEN",
    "SSH_SIGNING_KEY",
];

/// RAII guard that drops a foreground child's pgid from the print/SDK-mode
/// [`active_children`] registry when its `run` completes or its future is dropped
/// (cancel). Paired with a `setsid` spawn + [`active_children::register`] so the
/// print/SDK signal handler tree-kills only children that are still running.
struct ChildRegistration(u32);

impl Drop for ChildRegistration {
    fn drop(&mut self) {
        active_children::unregister(self.0);
    }
}

#[derive(Default)]
struct ProcessRegistrations {
    child: Option<ChildRegistration>,
    owner: Option<lingxi_core::host::agent_processes::Registration>,
    supervisor_committed: bool,
    group_cleanup_verified: bool,
}

impl ProcessRegistrations {
    fn release_before_reap(&mut self) {
        drop(self.child.take());
        drop(self.owner.take());
    }
}

/// Cancellation guard for streaming commands. Monitor workers are cancelled by
/// dropping their `run_streaming` future, so `Child::kill_on_drop` alone would
/// only kill the shell and could orphan grandchildren. Streaming children are
/// always session leaders; this guard synchronously kills their full process
/// group on every early-return/drop path and is disarmed after a clean reap.
struct StreamingProcessGroupGuard(Option<u32>);

impl Drop for StreamingProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            let _ = kill_tree_force(pid);
        }
    }
}

const STDERR_FILE_PREFIX: &[u8] = b"[stderr] ";
/// Exit code reported for a command killed at its deadline because it was not
/// eligible to be moved to the background (claude-code `fAt = 143`, the shell's
/// SIGTERM code).
/// Why the foreground wait loop stopped.
///
/// The port of claude-code's two exits from the `Wes` poll loop: the child
/// finished, the deadline fired, or something asked for the child to be
/// backgrounded while it was still running (`I_t` → `shellCommand.background`).
/// The last two share the move-to-background path; only the deadline consults
/// `shouldAutoBackground`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ForegroundBreak {
    /// The child exited and both pipes reached EOF.
    Finished,
    /// The timeout fired first.
    Deadline,
    /// A background request arrived while the child was still running.
    Requested,
}

const TIMEOUT_KILL_EXIT_CODE: i32 = 143;

const TASK_OUTPUT_COLLISION_RETRIES: usize = 16;

/// Mirror the oracle's `TaskOutput` transition: stdout/stderr remain separate
/// while inline, then the first over-limit chunk converts the buffered prefix
/// into `stdout + "[stderr] " + stderr`; later stderr chunks each carry their
/// own marker in observed read order.
use lingxi_core::host::task_output::Utf8StreamDecoder;

#[derive(Default)]
struct BackgroundOutput {
    initial: Vec<u8>,
    stdout_decoder: Utf8StreamDecoder,
    stderr_decoder: Utf8StreamDecoder,
}

struct FramedOutputCapture {
    limit: usize,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    spilled: Option<Vec<u8>>,
    stdout_decoder: Utf8StreamDecoder,
    stderr_decoder: Utf8StreamDecoder,
}

impl FramedOutputCapture {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            stdout: Vec::new(),
            stderr: Vec::new(),
            spilled: None,
            stdout_decoder: Utf8StreamDecoder::default(),
            stderr_decoder: Utf8StreamDecoder::default(),
        }
    }

    fn observe_stdout(&mut self, chunk: &[u8]) {
        self.observe(chunk, false);
    }

    fn observe_stderr(&mut self, chunk: &[u8]) {
        self.observe(chunk, true);
    }

    fn observe(&mut self, chunk: &[u8], stderr: bool) {
        let decoded = if stderr {
            self.stderr_decoder.decode(chunk, false)
        } else {
            self.stdout_decoder.decode(chunk, false)
        };
        self.observe_decoded(decoded.as_bytes(), stderr);
    }

    fn observe_decoded(&mut self, chunk: &[u8], stderr: bool) {
        if chunk.is_empty() {
            return;
        }
        if let Some(spilled) = self.spilled.as_mut() {
            append_framed_chunk(spilled, chunk, stderr);
            return;
        }
        let buffered = self.stdout.len().saturating_add(self.stderr.len());
        if buffered.saturating_add(chunk.len()) <= self.limit {
            if stderr {
                self.stderr.extend_from_slice(chunk);
            } else {
                self.stdout.extend_from_slice(chunk);
            }
            return;
        }

        let mut spilled = std::mem::take(&mut self.stdout);
        if !self.stderr.is_empty() {
            spilled.extend_from_slice(STDERR_FILE_PREFIX);
            spilled.extend_from_slice(&self.stderr);
            self.stderr.clear();
        }
        append_framed_chunk(&mut spilled, chunk, stderr);
        self.spilled = Some(spilled);
    }

    fn into_spilled(mut self) -> Option<Vec<u8>> {
        let stdout = self.stdout_decoder.decode(&[], true);
        let stderr = self.stderr_decoder.decode(&[], true);
        self.observe_decoded(stdout.as_bytes(), false);
        self.observe_decoded(stderr.as_bytes(), true);
        self.spilled
    }

    fn into_background(mut self) -> BackgroundOutput {
        let stdout_decoder = std::mem::take(&mut self.stdout_decoder);
        let stderr_decoder = std::mem::take(&mut self.stderr_decoder);
        BackgroundOutput {
            initial: self.force_spilled(),
            stdout_decoder,
            stderr_decoder,
        }
    }

    fn force_spilled(mut self) -> Vec<u8> {
        let stdout = self.stdout_decoder.decode(&[], true);
        let stderr = self.stderr_decoder.decode(&[], true);
        self.observe_decoded(stdout.as_bytes(), false);
        self.observe_decoded(stderr.as_bytes(), true);
        if let Some(spilled) = self.spilled.take() {
            return spilled;
        }
        let mut framed = self.stdout;
        if !self.stderr.is_empty() {
            framed.extend_from_slice(STDERR_FILE_PREFIX);
            framed.extend_from_slice(&self.stderr);
        }
        framed
    }
}

fn append_framed_chunk(output: &mut Vec<u8>, chunk: &[u8], stderr: bool) {
    if stderr {
        output.extend_from_slice(STDERR_FILE_PREFIX);
    }
    output.extend_from_slice(chunk);
}

async fn publish_background_output(
    file: &mut Option<tokio::fs::File>,
    binding: Option<&lingxi_core::host::BackgroundTaskBinding>,
    content: &str,
) -> std::io::Result<()> {
    if content.is_empty() {
        return Ok(());
    }
    if let Some(bound) = binding {
        if let Some(sink) = bound.on_exit.as_ref().filter(|sink| sink.manages_output()) {
            return sink
                .append_output(&bound.task_id, content)
                .await
                .map_err(|error| std::io::Error::other(error.to_string()));
        }
    }
    match file.as_mut() {
        Some(file) => file.write_all(content.as_bytes()).await,
        None => Err(std::io::Error::other("background output has no writer")),
    }
}

/// Signal an owned host process group without treating EPERM as proof that it
/// has disappeared. ESRCH is safe to accept only with the subsequent child wait.
fn signal_host_process_group(pid: u32) -> Result<(), ProcessError> {
    if pid == 0 {
        return Err(ProcessError::Io("process group id must be positive".into()));
    }
    let pid = i32::try_from(pid)
        .map_err(|_| ProcessError::Io(format!("process group id {pid} does not fit in i32")))?;
    match nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    ) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(ProcessError::Io(format!("killpg SIGKILL: {error}"))),
    }
}

fn observe_host_child_exit(pid: u32) -> Result<bool, ProcessError> {
    super::spawn_unsafe::child_exited_without_reaping(pid)
        .map_err(|error| ProcessError::Io(format!("owned child exit observation failed: {error}")))
}

#[cfg(test)]
static REJECT_REPEAT_GROUP_STOP: OnceLock<Mutex<std::collections::HashMap<u32, usize>>> =
    OnceLock::new();
#[cfg(test)]
static FAIL_NEXT_FOREGROUND_GROUP_STOP: OnceLock<Mutex<std::collections::HashSet<u32>>> =
    OnceLock::new();

fn stop_host_process_group(pid: u32) -> Result<(), ProcessError> {
    #[cfg(test)]
    if FAIL_NEXT_FOREGROUND_GROUP_STOP
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .remove(&pid)
    {
        return Err(ProcessError::Io(
            "injected foreground group stop failure".into(),
        ));
    }
    #[cfg(test)]
    if let Some(count) = REJECT_REPEAT_GROUP_STOP
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .get_mut(&pid)
    {
        if *count != 0 {
            return Err(ProcessError::Io("injected repeated group stop".into()));
        }
        *count += 1;
    }
    // macOS returns EPERM when the only group member is already a zombie.
    // Prove that case without signaling; otherwise propagate every signal
    // failure rather than interpreting EPERM as successful termination.
    #[cfg(target_os = "macos")]
    if observe_host_child_exit(pid)?
        && super::spawn_unsafe::macos_process_group_has_only_leader(pid).map_err(|error| {
            ProcessError::Io(format!("owned process group query failed: {error}"))
        })?
    {
        return Ok(());
    }
    signal_host_process_group(pid)
}

// Ordinary natural completion calls this while the leader is still held.
// Strict cleanup calls it only after the group has been verified empty; its
// registrations were already removed safely before that verification's reap.
fn commit_group_exit(
    binding: Option<&lingxi_core::host::BackgroundTaskBinding>,
    registrations: &mut ProcessRegistrations,
) -> Result<bool, ProcessError> {
    let Some(sink) = binding
        .and_then(|bound| bound.on_exit.as_ref())
        .filter(|sink| sink.requires_verified_supervised_stop())
    else {
        return Ok(true);
    };
    let decision = sink.commit_foreground(&mut || {
        registrations.release_before_reap();
        Ok(())
    });
    if matches!(&decision, Ok(true)) {
        registrations.supervisor_committed = true;
    }
    decision
}

#[cfg(test)]
#[derive(Default)]
struct CleanupProofBarrier {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(test)]
static CLEANUP_PROOF_BARRIERS: OnceLock<
    Mutex<std::collections::HashMap<u32, std::sync::Arc<CleanupProofBarrier>>>,
> = OnceLock::new();

/// Wait for a previously signaled group to disappear. The caller bounds this
/// future and holds the direct child until it is safe to remove registrations.
async fn reap_host_process_group(
    child: &mut tokio::process::Child,
    pid: u32,
    registrations: &mut ProcessRegistrations,
    identity_released: Option<&std::sync::atomic::AtomicBool>,
) -> Result<std::process::ExitStatus, ProcessError> {
    #[cfg(test)]
    {
        let barrier = CLEANUP_PROOF_BARRIERS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(&pid);
        if let Some(barrier) = barrier {
            barrier.entered.notify_one();
            barrier.release.notified().await;
        }
    }
    #[cfg(target_os = "macos")]
    loop {
        if observe_host_child_exit(pid)?
            && super::spawn_unsafe::macos_process_group_has_only_leader(pid).map_err(|error| {
                ProcessError::Io(format!("owned process group query failed: {error}"))
            })?
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err(ProcessError::Io(
        "owned process group verification is unavailable on this platform".into(),
    ));

    // No registered raw PGID can outlive this point. On macOS the group was
    // proved to contain only the held, exited leader; on Linux SIGKILL has
    // already been delivered and we only probe (never signal) after reaping.
    if let Some(released) = identity_released {
        released.store(true, Ordering::SeqCst);
    }
    registrations.release_before_reap();
    let status = child
        .wait()
        .await
        .map_err(|error| ProcessError::Io(format!("owned child wait failed: {error}")))?;
    #[cfg(target_os = "linux")]
    loop {
        let group = nix::unistd::Pid::from_raw(
            i32::try_from(pid)
                .map_err(|_| ProcessError::Io("invalid owned process group id".into()))?,
        );
        match nix::sys::signal::killpg(group, None) {
            Err(nix::errno::Errno::ESRCH) => break,
            Ok(()) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            Err(error) => {
                return Err(ProcessError::Io(format!(
                    "owned process group query failed: {error}"
                )))
            }
        }
    }
    registrations.group_cleanup_verified = true;
    Ok(status)
}

// For ordinary natural completion, arbitrate while the leader is held. A
// rejected commit retains that identity for stopping and verifying the group.
// For strict cleanup, leave the decision pending until the already-signaled
// group has actually been verified empty, so an intervening stop can be accepted
// and satisfied by that same cleanup.
async fn finish_held_foreground(
    child: &mut tokio::process::Child,
    pid: u32,
    registrations: &mut ProcessRegistrations,
    binding: Option<&lingxi_core::host::BackgroundTaskBinding>,
    verify_cleanup: bool,
    group_stop_sent: bool,
    stop_requested: bool,
) -> Result<std::process::ExitStatus, ProcessError> {
    let decision = if verify_cleanup || stop_requested {
        None
    } else {
        Some(commit_group_exit(binding, registrations))
    };
    let needs_verified_cleanup = verify_cleanup
        || stop_requested
        || decision
            .as_ref()
            .is_some_and(|decision| !matches!(decision, Ok(true)));
    let status = if needs_verified_cleanup {
        if !group_stop_sent {
            stop_host_process_group(pid)?;
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            reap_host_process_group(child, pid, registrations, None),
        )
        .await
        .map_err(|_| {
            ProcessError::Io(
                "owned foreground group cleanup timed out; termination is unverified".into(),
            )
        })??
    } else {
        child
            .wait()
            .await
            .map_err(|error| ProcessError::Io(format!("foreground child wait failed: {error}")))?
    };
    let decision = decision.unwrap_or_else(|| commit_group_exit(binding, registrations));
    match decision {
        Err(error) => Err(error),
        Ok(false) => Err(ProcessError::Io("supervisor command cancelled".into())),
        Ok(true) if stop_requested => Err(ProcessError::Io("supervisor command cancelled".into())),
        Ok(true) => Ok(status),
    }
}

// An error after a supervised spawn must not drop the last owner and leave the
// supervisor with neither a cleanup receipt nor a safe way to retry. Report
// the failure first, then transfer the original child and lease to a reaper.
// Once the leader was reaped (or natural completion was committed), its raw
// PGID is no longer a safe signaling capability and is never used for retry.
async fn retain_failed_supervised_foreground(
    mut child: tokio::process::Child,
    pid: u32,
    mut registrations: ProcessRegistrations,
    binding: lingxi_core::host::BackgroundTaskBinding,
    lifetime_guard: Option<lingxi_core::host::sandbox::BackendPlanHandle>,
    error: ProcessError,
    failure_reported: bool,
) -> ProcessError {
    let reason = format!("supervised foreground cleanup failed: {error}");
    let unverified = ProcessError::Io(format!(
        "{} {reason}",
        lingxi_core::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED,
    ));
    let Some(sink) = binding.on_exit.as_ref().cloned() else {
        return unverified;
    };
    if !failure_reported {
        sink.on_wait_failure(&binding.task_id, &reason).await;
    }
    if registrations.group_cleanup_verified {
        sink.on_cleanup_verified(&binding.task_id).await;
        return unverified;
    }
    tokio::spawn(async move {
        let guard = lifetime_guard;
        loop {
            if registrations.supervisor_committed
                || child.id() != Some(pid)
                || observe_host_child_exit(pid).is_err()
            {
                // No identity-matched signal is possible. Keep the process
                // owner and lease for recovery without claiming termination.
                std::future::pending::<()>().await;
                drop((child, registrations, guard));
                return;
            }
            if let Err(error) = stop_host_process_group(pid) {
                tracing::warn!(%error, "foreground cleanup retry failed");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
            let released = std::sync::atomic::AtomicBool::new(false);
            let result = {
                let settled =
                    reap_host_process_group(&mut child, pid, &mut registrations, Some(&released));
                tokio::pin!(settled);
                loop {
                    tokio::select! {
                        result = &mut settled => break result,
                        _ = async {
                            if let Some(stop) = sink.stop_notify() { stop.notified().await; }
                            else { std::future::pending::<()>().await; }
                        }, if !released.load(Ordering::SeqCst) => {
                            // The original wait still holds the leader. Never
                            // signal again after it releases that identity.
                            if !released.load(Ordering::SeqCst) {
                                if let Err(error) = stop_host_process_group(pid) {
                                    tracing::warn!(%error, "foreground cleanup notification retry failed");
                                }
                            }
                        }
                    }
                }
            };
            match result {
                Ok(_) => {
                    if let Err(error) = commit_group_exit(Some(&binding), &mut registrations) {
                        tracing::warn!(%error, "verified cleanup could not commit completion");
                    }
                    sink.on_cleanup_verified(&binding.task_id).await;
                    return;
                }
                Err(error) => {
                    tracing::warn!(%error, "foreground cleanup verification failed");
                    // The next iteration signals only if the original leader
                    // is still unreaped and remains an observable child.
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    });
    unverified
}

// Keep the Child and registrations together so cancellation or panic can
// transfer them to the same retrying owner as an explicit error. A natural
// commit or a completed strict cleanup has already made that transfer
// unnecessary; a background handoff explicitly moves both fields out.
struct ForegroundOwnerGuard {
    child: Option<tokio::process::Child>,
    registrations: Option<ProcessRegistrations>,
    pid: u32,
    binding: Option<lingxi_core::host::BackgroundTaskBinding>,
    lifetime_guard: Option<lingxi_core::host::sandbox::BackendPlanHandle>,
    failure_reported: bool,
    failure_reason: String,
}

impl Drop for ForegroundOwnerGuard {
    fn drop(&mut self) {
        let (Some(child), Some(registrations), Some(binding)) = (
            self.child.take(),
            self.registrations.take(),
            self.binding.take(),
        ) else {
            return;
        };
        if registrations.supervisor_committed || registrations.group_cleanup_verified {
            return;
        }
        let guard = self.lifetime_guard.take();
        let pid = self.pid;
        let error = ProcessError::Io(std::mem::take(&mut self.failure_reason));
        let failure_reported = self.failure_reported;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = retain_failed_supervised_foreground(
                    child,
                    pid,
                    registrations,
                    binding,
                    guard,
                    error,
                    failure_reported,
                )
                .await;
            });
        } else {
            // Runtime shutdown cannot safely run the async verifier. Signal
            // only while the original unreaped leader is still observable,
            // then deliberately retain the owner and lease as unverified.
            if child.id() == Some(pid) && observe_host_child_exit(pid).is_ok() {
                let _ = stop_host_process_group(pid);
            }
            std::mem::forget((child, registrations, binding, guard));
        }
    }
}

static UNKNOWN_RECOVERY_HOOKS: OnceLock<Mutex<Vec<lingxi_core::host::sandbox::BackendPlanHandle>>> =
    OnceLock::new();

struct RecoveryHookOwnerGuard(Option<lingxi_core::host::sandbox::BackendPlanHandle>);

impl Drop for RecoveryHookOwnerGuard {
    fn drop(&mut self) {
        if let Some(guard) = self.0.take() {
            UNKNOWN_RECOVERY_HOOKS
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(guard);
        }
    }
}

/// A recovery-managed hook owns its process group even after its caller drops
/// the output receiver. Never release its lifetime guard based on a signal or
/// receiver completion; retain it if cleanup cannot be verified.
async fn run_recovery_hook_owner(
    cmd: SandboxedCommand,
    default_async_timeout: std::time::Duration,
    observer: Option<std::sync::Arc<dyn lingxi_core::host::HookOutputObserver>>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
    ready: tokio::sync::oneshot::Sender<Result<HookRunOutcome, ProcessError>>,
) {
    let mut lifetime_guard = RecoveryHookOwnerGuard(cmd.lifetime_guard().cloned());
    let leases = cmd.lifetime_guard().and_then(|guard| {
        guard
            .downcast::<std::sync::Arc<lingxi_core::host::process::RecoveryHookProcessLeases>>()
            .map(|leases| std::sync::Arc::clone(leases.as_ref()))
    });
    let mut ready = Some(ready);
    let mut tcmd = PosixProcess::build_command(&cmd);
    tcmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    attach_setsid(&mut tcmd);
    let mut child = match tcmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            lifetime_guard.0.take();
            let _ = ready
                .take()
                .unwrap()
                .send(Err(ProcessError::Io(error.to_string())));
            return;
        }
    };
    let Some(pid) = child.id() else {
        let _ = ready.take().unwrap().send(Err(ProcessError::Io(
            "recovery hook child identity is unavailable; cleanup is unverified".into(),
        )));
        std::future::pending::<()>().await;
        return;
    };
    let mut group_guard = StreamingProcessGroupGuard(Some(pid));
    let mut registrations = ProcessRegistrations {
        child: None,
        owner: super::agent_processes::register_with_capability_requirement(
            cmd.process_owner(),
            Some(pid),
            true,
        ),
        ..ProcessRegistrations::default()
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let deadline = tokio::time::Instant::now() + cmd.inner().timeout.unwrap_or(DEFAULT_TIMEOUT);
    let mut reader = stdout.map(BufReader::new);
    let mut first_line = Vec::new();
    let initial = async {
        if let Some(text) = cmd.inner().stdin.as_ref() {
            if let Some(mut stdin) = child.stdin.take() {
                write_hook_stdin(&mut stdin, text.as_bytes())
                    .await
                    .map_err(|e| ProcessError::Io(e.to_string()))?;
            }
        }
        let reader = reader
            .as_mut()
            .ok_or_else(|| ProcessError::Io("hook child has no stdout pipe".into()))?;
        reader
            .read_until(b'\n', &mut first_line)
            .await
            .map_err(|e| ProcessError::Io(e.to_string()))?;
        Ok::<(), ProcessError>(())
    };
    let initial = tokio::select! {
        biased;
        () = async {
            if let Some(cancel) = cancellation.as_ref() { cancel.cancelled().await; }
            else { std::future::pending::<()>().await; }
        } => Err(ProcessError::Io("hook execution cancelled".into())),
        result = tokio::time::timeout_at(deadline, initial) => result.unwrap_or(Err(ProcessError::Timeout)),
    };
    let mut output_sender = None;
    let mut marker = None;
    if initial.is_ok() {
        if let (Some(obs), false) = (observer.as_ref(), first_line.is_empty()) {
            obs.on_chunk(&first_line, &[]).await;
        }
        marker = parse_async_first_line(&first_line, default_async_timeout);
        if let Some(async_timeout) = marker {
            if let Some(leases) = leases.as_ref() {
                leases
                    .execution
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            output_sender = Some(tx);
            let _ = ready.take().unwrap().send(Ok(HookRunOutcome::Backgrounded {
                async_timeout,
                output: Some(rx),
            }));
        }
    }
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    let mut stderr = stderr;
    let run_result = {
        let finish = async {
            let stdout_read = async {
                if let Some(reader) = reader.as_mut() {
                    drain_observed(reader, &mut stdout_buf, observer.as_ref(), false).await
                } else {
                    Ok(())
                }
            };
            let stderr_read = async {
                if let Some(stderr) = stderr.as_mut() {
                    drain_observed(stderr, &mut stderr_buf, observer.as_ref(), true).await
                } else {
                    Ok(())
                }
            };
            let (out, err) = tokio::join!(stdout_read, stderr_read);
            out.map_err(|e| ProcessError::Io(e.to_string()))?;
            err.map_err(|e| ProcessError::Io(e.to_string()))?;
            while !observe_host_child_exit(pid)? {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Ok::<(), ProcessError>(())
        };
        if let Err(error) = initial {
            Err(error)
        } else {
            let finish_deadline =
                marker.map_or(deadline, |timeout| tokio::time::Instant::now() + timeout);
            tokio::select! {
                biased;
                () = async {
                    if let Some(cancel) = cancellation.as_ref() { cancel.cancelled().await; }
                    else { std::future::pending::<()>().await; }
                } => Err(ProcessError::Io("hook execution cancelled".into())),
                result = tokio::time::timeout_at(finish_deadline, finish) => result.unwrap_or(Err(ProcessError::Timeout)),
            }
        }
    };

    let cleanup = async {
        stop_host_process_group(pid)?;
        // The signal must never be repeated after the leader is reaped and
        // its numeric group identity can be recycled.
        group_guard.0 = None;
        reap_host_process_group(&mut child, pid, &mut registrations, None).await
    };
    let status = match tokio::time::timeout(std::time::Duration::from_secs(10), cleanup).await {
        Ok(Ok(status)) => status,
        failure => {
            let message = format!("recovery hook process-group cleanup is unverified: {failure:?}");
            tracing::error!(%message);
            if let Some(sender) = ready.take() {
                let _ = sender.send(Err(ProcessError::Io(message.clone())));
            }
            if let Some(sender) = output_sender.take() {
                let _ = sender.send(ProcessOutput {
                    stdout: String::new(),
                    stderr: message,
                    exit_code: -1,
                    timed_out: true,
                });
            }
            // Keep the original turn work lease and any held group identity.
            std::future::pending::<()>().await;
            return;
        }
    };
    lifetime_guard.0.take();
    let timed_out = matches!(&run_result, Err(ProcessError::Timeout));
    let output = ProcessOutput {
        stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
        stderr: match &run_result {
            Ok(()) => String::from_utf8_lossy(&stderr_buf).into_owned(),
            Err(error) => format!("hook did not complete: {error}"),
        },
        exit_code: status.code().unwrap_or(-1),
        timed_out,
    };
    if let Some(sender) = output_sender {
        let _ = sender.send(output);
    } else if let Some(sender) = ready {
        match run_result {
            Ok(()) => {
                let mut output = output;
                output
                    .stdout
                    .insert_str(0, &String::from_utf8_lossy(&first_line));
                let _ = sender.send(Ok(HookRunOutcome::Completed(output)));
            }
            Err(error) => {
                let _ = sender.send(Err(error));
            }
        }
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

fn owned_streaming_error(reason: impl std::fmt::Display) -> ProcessError {
    ProcessError::Io(format!(
        "{} {reason}",
        lingxi_core::host::process::OWNED_STREAMING_CLEANUP_UNVERIFIED
    ))
}

async fn owned_streaming_failure(
    child: &mut tokio::process::Child,
    pid: u32,
    group_guard: &mut StreamingProcessGroupGuard,
    registrations: &mut ProcessRegistrations,
    reason: impl std::fmt::Display,
) -> ProcessError {
    let signal = if child.id().is_some() {
        signal_host_process_group(pid)
    } else {
        Err(ProcessError::Io(
            "child process identity is unavailable".into(),
        ))
    };
    // Do not signal the same numeric group after the leader has been reaped.
    group_guard.0 = None;
    registrations.release_before_reap();
    let wait = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await;
    owned_streaming_error(format!("{reason}; signal={signal:?}; wait={wait:?}"))
}

struct BackgroundDrainResult {
    output: std::io::Result<()>,
    deadline_expired: bool,
    requested_stop: bool,
    status: std::io::Result<std::process::ExitStatus>,
    failure_reported: bool,
    group_cleanup_verified: bool,
}

struct BackgroundDrainPolicy {
    deadline: Option<tokio::time::Instant>,
    verify_host_cleanup: bool,
}

// Test-only, task-scoped injection exercises the actual reaper and its retry
// callback without affecting other concurrently running process fixtures.
#[cfg(test)]
static FAIL_NEXT_BACKGROUND_STOP: OnceLock<Mutex<std::collections::HashSet<String>>> =
    OnceLock::new();
#[cfg(test)]
static SKIP_NEXT_BACKGROUND_STOP: OnceLock<Mutex<std::collections::HashSet<String>>> =
    OnceLock::new();

fn request_background_group_stop(
    pid: u32,
    binding: Option<&lingxi_core::host::BackgroundTaskBinding>,
) -> Result<(), ProcessError> {
    #[cfg(test)]
    if binding.is_some_and(|bound| {
        FAIL_NEXT_BACKGROUND_STOP
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(&bound.task_id)
    }) {
        return Err(ProcessError::Io("injected background stop failure".into()));
    }
    #[cfg(test)]
    if binding.is_some_and(|bound| {
        SKIP_NEXT_BACKGROUND_STOP
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(&bound.task_id)
    }) {
        // Simulate a signal API reporting success while a member remains.
        return Ok(());
    }
    #[cfg(not(test))]
    let _ = binding;
    stop_host_process_group(pid)
}

async fn report_stop_failure(
    binding: Option<&lingxi_core::host::BackgroundTaskBinding>,
    reason: &str,
    already_reported: &mut bool,
) {
    if *already_reported {
        return;
    }
    *already_reported = true;
    if let Some((binding, sink)) =
        binding.and_then(|binding| binding.on_exit.as_ref().map(|sink| (binding, sink)))
    {
        sink.on_wait_failure(&binding.task_id, reason).await;
    } else {
        tracing::warn!(reason, "background process termination is unverified");
    }
}

async fn report_background_completion(
    sink: &dyn lingxi_core::host::BackgroundExitSink,
    task_id: &str,
    result: std::io::Result<std::process::ExitStatus>,
    deadline_expired: bool,
    requested_stop: bool,
    failure_reported: bool,
    group_cleanup_verified: bool,
) {
    if failure_reported {
        return;
    }
    match result {
        Ok(status) if deadline_expired && !sink.requires_verified_supervised_stop() => {
            sink.on_background_deadline_exit(task_id, status.code())
                .await;
        }
        // A stop or deadline can race with an independent exit or signal. Neither
        // killpg success nor a signal status identifies the cause of exit.
        Ok(status)
            if (deadline_expired || requested_stop) && sink.requires_verified_supervised_stop() =>
        {
            sink.on_exit_with_proof(task_id, status.code(), None, group_cleanup_verified)
                .await
        }
        Ok(status) if deadline_expired || requested_stop => {
            sink.on_exit(task_id, status.code()).await
        }
        Ok(status) => {
            sink.on_exit_with_proof(task_id, status.code(), Some(false), group_cleanup_verified)
                .await
        }
        Err(error) => {
            sink.on_wait_failure(task_id, &format!("background child wait failed: {error}"))
                .await;
        }
    }
}

async fn drain_framed_output<SO, SE>(
    stdout: &mut SO,
    stderr: &mut SE,
    file: &mut Option<tokio::fs::File>,
    mut output: BackgroundOutput,
    health: Option<(&lingxi_core::host::BackgroundTaskBinding, u32)>,
    child: &mut tokio::process::Child,
    policy: BackgroundDrainPolicy,
    registrations: &mut ProcessRegistrations,
) -> BackgroundDrainResult
where
    SO: AsyncRead + Unpin,
    SE: AsyncRead + Unpin,
{
    let BackgroundDrainPolicy {
        deadline,
        mut verify_host_cleanup,
    } = policy;
    let binding = health.map(|(binding, _)| binding);
    let verify_supervised_stop = binding
        .and_then(|bound| bound.on_exit.as_ref())
        .is_some_and(|sink| sink.requires_verified_supervised_stop());
    let mut deadline_expired = false;
    let mut deadline_handled = false;
    let managed = binding
        .and_then(|bound| bound.on_exit.as_ref())
        .is_some_and(|sink| sink.manages_output());
    let stop_notify = binding
        .and_then(|b| b.on_exit.as_ref())
        .and_then(|s| s.stop_notify());
    let mut stop_handled = false;
    let mut handoff_ready = false;
    let mut retry_stop = false;
    let mut group_stop_sent = false;
    let mut stop_verification_deadline = None;
    let mut watchdog = super::watchdog::ShellWatchdog::new(tokio::time::Instant::now());
    watchdog.observe(&output.initial);
    let mut health_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + super::watchdog::POLL_INTERVAL,
        super::watchdog::POLL_INTERVAL,
    );
    health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pressure_stopped = false;
    let mut first_write_error =
        publish_background_output(file, binding, &String::from_utf8_lossy(&output.initial))
            .await
            .err();
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut failure_reported = false;
    let mut stdout_chunk = vec![0u8; 8192];
    let mut stderr_chunk = vec![0u8; 8192];
    let status = loop {
        let mut awaiting_handoff = false;
        if stdout_done && stderr_done {
            if verify_host_cleanup || verify_supervised_stop {
                let Some(pid) = child.id() else {
                    report_stop_failure(
                        binding,
                        "background child identity is unavailable",
                        &mut failure_reported,
                    )
                    .await;
                    std::future::pending::<()>().await;
                    unreachable!();
                };
                match observe_host_child_exit(pid) {
                    Ok(true) => {
                        if verify_supervised_stop && !verify_host_cleanup && !handoff_ready {
                            // The caller must actually accept the background
                            // capability before natural completion can release
                            // a group that may contain redirected descendants.
                            // Keep the leader pinned while the select below also
                            // remains responsive to stop and deadline.
                            awaiting_handoff = true;
                        } else {
                            if verify_host_cleanup && !group_stop_sent {
                                if let Err(error) = stop_host_process_group(pid) {
                                    report_stop_failure(
                                        binding,
                                        &format!("background group stop failed: {error}"),
                                        &mut failure_reported,
                                    )
                                    .await;
                                    // The leader remains unreaped, so retrying cannot
                                    // accidentally signal a recycled process group.
                                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                                    continue;
                                }
                                group_stop_sent = true;
                            }
                            // Ordinary completion must decide while the unreaped
                            // leader still reserves the PGID. A stop winner upgrades
                            // it to a real group stop and verified reap. Strict
                            // cleanup instead leaves the gate pending until proof.
                            let decide_before_reap = !verify_host_cleanup;
                            if decide_before_reap {
                                match commit_group_exit(binding, registrations) {
                                    Ok(true) => {}
                                    Ok(false) => {
                                        stop_handled = true;
                                        deadline_handled = true;
                                        verify_host_cleanup = true;
                                    }
                                    Err(error) => {
                                        report_stop_failure(
                                            binding,
                                            &format!(
                                                "background completion decision failed: {error}"
                                            ),
                                            &mut failure_reported,
                                        )
                                        .await;
                                        verify_host_cleanup = true;
                                    }
                                }
                            }
                            if !verify_host_cleanup {
                                break child.wait().await;
                            }
                            if !group_stop_sent {
                                if let Err(error) = stop_host_process_group(pid) {
                                    report_stop_failure(
                                        binding,
                                        &format!("background group stop failed: {error}"),
                                        &mut failure_reported,
                                    )
                                    .await;
                                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                                    continue;
                                }
                            }
                            let result = {
                                let identity_released = std::sync::atomic::AtomicBool::new(false);
                                let settled = reap_host_process_group(
                                    child,
                                    pid,
                                    registrations,
                                    Some(&identity_released),
                                );
                                tokio::pin!(settled);
                                loop {
                                    tokio::select! {
                                        result = &mut settled => break result,
                                        () = tokio::time::sleep(std::time::Duration::from_secs(10)), if !failure_reported => {
                                            report_stop_failure(binding, "background group cleanup timed out; termination is unverified", &mut failure_reported).await;
                                        }
                                        _ = async { if let Some(stop) = &stop_notify { stop.notified().await; } else { std::future::pending::<()>().await; } },
                                            if failure_reported && !identity_released.load(Ordering::SeqCst) => {
                                            // A retry is safe only while the original
                                            // unreaped leader still reserves its PGID.
                                            // A failed repeat remains unverified; the
                                            // original diagnostic is never overwritten.
                                            if !identity_released.load(Ordering::SeqCst) {
                                                if let Err(error) = stop_host_process_group(pid) {
                                                    tracing::warn!(%error, "background group retry remains unverified");
                                                }
                                            }
                                        }
                                    }
                                }
                            };
                            match result {
                                Ok(status) => {
                                    if !decide_before_reap {
                                        match commit_group_exit(binding, registrations) {
                                            Ok(true) => {}
                                            Ok(false) => {
                                                stop_handled = true;
                                            }
                                            Err(error) => {
                                                report_stop_failure(
                                                binding,
                                                &format!("background completion decision failed: {error}"),
                                                &mut failure_reported,
                                            )
                                            .await;
                                            }
                                        }
                                    }
                                    break Ok(status);
                                }
                                Err(error) => {
                                    report_stop_failure(
                                        binding,
                                        &format!("background group cleanup failed: {error}"),
                                        &mut failure_reported,
                                    )
                                    .await;
                                    // Identity or verification is lost. Retain the
                                    // reaper and lease rather than claim cleanup.
                                    std::future::pending::<()>().await;
                                    unreachable!();
                                }
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(error) => {
                        report_stop_failure(
                            binding,
                            &format!("background child observation failed: {error}"),
                            &mut failure_reported,
                        )
                        .await;
                        std::future::pending::<()>().await;
                        unreachable!();
                    }
                }
            } else {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) => {}
                    Err(_) => break child.wait().await,
                }
            }
        }
        let framed = tokio::select! {
            result = async {
                match binding.and_then(|bound| bound.on_exit.as_ref()) {
                    Some(sink) => sink.wait_for_supervisor_handoff().await,
                    None => Ok(true),
                }
            }, if awaiting_handoff => {
                match result {
                    Ok(true) => handoff_ready = true,
                    Ok(false) => {
                        stop_handled = true;
                        deadline_handled = true;
                        verify_host_cleanup = true;
                    }
                    Err(error) => {
                        report_stop_failure(binding, &format!("background handoff decision failed: {error}"), &mut failure_reported).await;
                        verify_host_cleanup = true;
                    }
                }
                continue;
            }
            _ = async {
                if let Some(deadline) = deadline { tokio::time::sleep_until(deadline).await; }
                else { std::future::pending::<()>().await; }
            }, if !deadline_handled => {
                deadline_handled = true;
                // A supervisor deadline has the same cleanup and causal
                // uncertainty as an explicit supervisor stop.
                verify_host_cleanup |= verify_supervised_stop;
                if verify_host_cleanup {
                    stop_verification_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
                }
                // Do not reap the leader here: it may have exited while a
                // descendant still owns a pipe. The held, unreaped leader
                // reserves this process-group identity until the final wait.
                if let Some(pid) = child.id() {
                    deadline_expired = true;
                    let result = if verify_host_cleanup {
                        stop_host_process_group(pid)
                    } else {
                        kill_tree_force(pid)
                    };
                    match (verify_host_cleanup, result) {
                        (true, Ok(())) => group_stop_sent = true,
                        (true, Err(error)) => report_stop_failure(binding, &format!("deadline stop failed: {error}"), &mut failure_reported).await,
                        _ => {}
                    }
                } else if verify_host_cleanup {
                    report_stop_failure(binding, "deadline stop failed: child has no process id", &mut failure_reported).await;
                }
                continue;
            }
            _ = async { if let Some(stop) = &stop_notify { stop.notified().await; } else { std::future::pending::<()>().await; } }, if !stop_handled || retry_stop => {
                stop_handled = true;
                retry_stop = false;
                // A supervisor owns the held session from spawn. Once its stop
                // wins, retain the unreaped leader until the group is verified
                // empty. Other sinks keep their existing leader-wait behavior.
                verify_host_cleanup |= verify_supervised_stop;
                if verify_host_cleanup {
                    stop_verification_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
                }
                // Keep an exited leader unreaped while its descendants hold
                // pipes, just as the deadline path does. A user stop owns the
                // outcome even if output draining crosses the later deadline.
                deadline_handled = true;
                if let Some(pid) = child.id() {
                    let result = if verify_host_cleanup {
                        request_background_group_stop(pid, binding)
                    } else {
                        kill_tree_force(pid)
                    };
                    match (verify_host_cleanup, result) {
                        (true, Ok(())) => group_stop_sent = true,
                        (true, Err(error)) => {
                            // The unreaped leader still reserves this PGID. A
                            // later stop may safely retry without erasing the
                            // original unverified diagnostic.
                            retry_stop = true;
                            report_stop_failure(binding, &format!("requested stop failed: {error}"), &mut failure_reported).await;
                        }
                        _ => {}
                    }
                } else if verify_host_cleanup {
                    report_stop_failure(binding, "requested stop failed: child has no process id", &mut failure_reported).await;
                }
                continue;
            }
            result = child.wait(), if stdout_done && stderr_done && !verify_host_cleanup && !verify_supervised_stop => {
                if result.is_ok() {
                    break result;
                }
                break child.wait().await;
            },
            () = tokio::time::sleep(std::time::Duration::from_millis(20)), if stdout_done && stderr_done && (verify_host_cleanup || verify_supervised_stop) && !awaiting_handoff => continue,
            () = async {
                if let Some(deadline) = stop_verification_deadline {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if !failure_reported => {
                report_stop_failure(binding, "background stop did not finish draining and verifying within 10 seconds", &mut failure_reported).await;
                continue;
            }
            _ = health_tick.tick(), if health.is_some() && !pressure_stopped => {
                if let Some((binding, pid)) = health {
                    if let Some(sink) = binding.on_exit.as_ref() {
                        if let Some(tail) = watchdog.poll(tokio::time::Instant::now()) {
                            sink.on_stall(&binding.task_id, &tail).await;
                        }
                        if super::watchdog::memory_pressure().await
                            && sink.on_memory_pressure(&binding.task_id).await {
                            pressure_stopped = true;
                            stop_handled = true;
                            deadline_handled = true;
                            verify_host_cleanup |= verify_supervised_stop;
                            if verify_host_cleanup {
                                stop_verification_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
                            }
                            let result = if verify_host_cleanup {
                                stop_host_process_group(pid)
                            } else {
                                kill_tree_force(pid)
                            };
                            match (verify_host_cleanup, result) {
                                (true, Ok(())) => group_stop_sent = true,
                                (true, Err(error)) => {
                                    retry_stop = true;
                                    report_stop_failure(Some(binding), &format!("memory-pressure stop failed: {error}"), &mut failure_reported).await;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                continue;
            }
            result = stdout.read(&mut stdout_chunk), if !stdout_done => {
                match result {
                    Ok(0) | Err(_) => { stdout_done = true; output.stdout_decoder.decode(&[], true) }
                    Ok(count) => output.stdout_decoder.decode(&stdout_chunk[..count], false),
                }
            }
            result = stderr.read(&mut stderr_chunk), if !stderr_done => {
                let decoded = match result {
                    Ok(0) | Err(_) => { stderr_done = true; output.stderr_decoder.decode(&[], true) }
                    Ok(count) => output.stderr_decoder.decode(&stderr_chunk[..count], false),
                };
                if decoded.is_empty() { decoded } else { format!("[stderr] {decoded}") }
            }
        };
        watchdog.observe(framed.as_bytes());
        // The bounded task writer retries later generations after a failure;
        // do not silently discard all subsequent output in the process runner.
        if managed || first_write_error.is_none() {
            if let Err(error) = publish_background_output(file, binding, &framed).await {
                first_write_error.get_or_insert(error);
            }
        }
    };
    let flushed = if managed {
        match binding.and_then(|bound| bound.on_exit.as_ref().map(|sink| (bound, sink))) {
            Some((bound, sink)) => sink
                .flush_output(&bound.task_id)
                .await
                .map_err(|error| std::io::Error::other(error.to_string())),
            None => Ok(()),
        }
    } else if let Some(file) = file.as_mut() {
        file.flush().await
    } else {
        Ok(())
    };
    if let Err(error) = flushed {
        first_write_error.get_or_insert(error);
    }
    let group_cleanup_verified = verify_host_cleanup && status.is_ok();
    BackgroundDrainResult {
        output: first_write_error.map_or(Ok(()), Err),
        deadline_expired,
        requested_stop: stop_handled,
        status,
        failure_reported,
        group_cleanup_verified,
    }
}

/// Production [`ProcessRunner`] using `tokio::process`.
pub struct PosixProcess {
    task_output_dir: PathBuf,
    task_output_root_identity: Mutex<Option<lingxi_core::host::rooted_fs::RootIdentity>>,
}

impl Default for PosixProcess {
    fn default() -> Self {
        Self {
            task_output_dir: task_output_dir(),
            task_output_root_identity: Mutex::new(None),
        }
    }
}

impl PosixProcess {
    /// Construct a new `PosixProcess` runner.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a runner whose task-output files land in `task_output_dir`.
    ///
    /// Composition roots point this at the SAME directory the task registry's
    /// output manager owns, so a backgrounded command's file is readable
    /// through the registry (claude-code has one task-output root per session:
    /// `join(bR(), sessionId, "tasks")`, 2.1.263 `src_159581235.js` @10603).
    #[must_use]
    pub fn with_task_output_dir(task_output_dir: PathBuf) -> Self {
        Self {
            task_output_dir,
            task_output_root_identity: Mutex::new(None),
        }
    }

    /// Open the output file a caller bound to this command, for append.
    ///
    /// The bound file was created by the task registry's output manager, so it
    /// already exists; the runner pins its parent directory's identity and
    /// opens without following symlinks, exactly like the runner-owned path.
    fn open_bound_output_file(
        &self,
        binding: &lingxi_core::host::BackgroundTaskBinding,
    ) -> Result<std::fs::File, ProcessError> {
        let path = &binding.output_path;
        let parent = path
            .parent()
            .ok_or_else(|| self.task_output_pin_error("bound task output has no parent"))?;
        let name = path
            .file_name()
            .map(Path::new)
            .ok_or_else(|| self.task_output_pin_error("bound task output has no file name"))?;
        let grandparent = parent
            .parent()
            .ok_or_else(|| self.task_output_pin_error("bound task output root has no parent"))?;
        let dir_name = parent
            .file_name()
            .map(Path::new)
            .ok_or_else(|| self.task_output_pin_error("bound task output root has no name"))?;
        let identity = lingxi_core::host::rooted_fs::ensure_private_directory(
            grandparent,
            dir_name,
            lingxi_core::host::rooted_fs::PRIVATE_DIR_MODE,
        )
        .map_err(|error| self.task_output_pin_error(error))?;
        lingxi_core::host::rooted_fs::open_append_file_pinned(parent, name, Some(&identity))
            .map_err(|error| self.task_output_pin_error(error))
    }

    fn task_output_pin_error(&self, error: impl std::fmt::Display) -> ProcessError {
        let message = format!(
            "task output: pin of {} refused: {error}; tasks dir moved or linked",
            self.task_output_dir.display()
        );
        tracing::warn!("{message}");
        ProcessError::Io(message)
    }

    fn pinned_task_output_root(
        &self,
    ) -> Result<lingxi_core::host::rooted_fs::RootIdentity, ProcessError> {
        let parent = self
            .task_output_dir
            .parent()
            .ok_or_else(|| self.task_output_pin_error("task output directory has no parent"))?;
        let directory_name = self
            .task_output_dir
            .file_name()
            .map(Path::new)
            .ok_or_else(|| self.task_output_pin_error("task output directory has no name"))?;
        let observed = lingxi_core::host::rooted_fs::ensure_private_directory(
            parent,
            directory_name,
            lingxi_core::host::rooted_fs::PRIVATE_DIR_MODE,
        )
        .map_err(|error| self.task_output_pin_error(error))?;
        Ok({
            let mut pinned = self
                .task_output_root_identity
                .lock()
                .map_err(|_| self.task_output_pin_error("root identity lock poisoned"))?;
            match *pinned {
                Some(expected) if expected != observed => {
                    return Err(self.task_output_pin_error("directory identity changed"));
                }
                Some(expected) => expected,
                None => {
                    *pinned = Some(observed);
                    observed
                }
            }
        })
    }

    fn open_task_output_file_with_root(
        &self,
        task_id: &str,
        expected: &lingxi_core::host::rooted_fs::RootIdentity,
    ) -> Result<(PathBuf, std::fs::File), lingxi_core::host::FsError> {
        let relative = PathBuf::from(format!("{task_id}.out"));
        let file = lingxi_core::host::rooted_fs::open_create_new_file_pinned(
            &self.task_output_dir,
            &relative,
            Some(expected),
        )?;
        Ok((self.task_output_dir.join(relative), file))
    }

    #[cfg(test)]
    fn open_task_output_file(
        &self,
        task_id: &str,
    ) -> Result<(PathBuf, std::fs::File), ProcessError> {
        let expected = self.pinned_task_output_root()?;
        self.open_task_output_file_with_root(task_id, &expected)
            .map_err(|error| self.task_output_pin_error(error))
    }

    fn create_task_output_file(&self) -> Result<(String, PathBuf, std::fs::File), ProcessError> {
        self.create_task_output_file_with(generate_task_id)
    }

    fn create_task_output_file_with<F>(
        &self,
        mut next_task_id: F,
    ) -> Result<(String, PathBuf, std::fs::File), ProcessError>
    where
        F: FnMut() -> Result<String, ProcessError>,
    {
        let expected = self.pinned_task_output_root()?;
        for _ in 0..TASK_OUTPUT_COLLISION_RETRIES {
            let task_id = next_task_id()?;
            match self.open_task_output_file_with_root(&task_id, &expected) {
                Ok((path, file)) => return Ok((task_id, path, file)),
                Err(lingxi_core::host::FsError::AlreadyExists(_)) => continue,
                Err(error) => return Err(self.task_output_pin_error(error)),
            }
        }
        Err(self.task_output_pin_error(format!(
            "could not allocate an exclusive task output after {TASK_OUTPUT_COLLISION_RETRIES} collisions"
        )))
    }

    fn spill_completed_output(
        &self,
        framed_output: &[u8],
    ) -> Result<lingxi_core::host::process::ProcessOutputFile, ProcessError> {
        use std::io::Write as _;

        let (task_id, path, mut file) = self.create_task_output_file()?;
        file.write_all(
            &framed_output[..framed_output
                .len()
                .min(lingxi_core::host::task_output::MAX_PERSISTED_OUTPUT_BYTES as usize)],
        )
        .and_then(|()| file.flush())
        .map_err(|error| ProcessError::Io(format!("write task output: {error}")))?;
        let size = u64::try_from(framed_output.len()).unwrap_or(u64::MAX);
        Ok(lingxi_core::host::process::ProcessOutputFile {
            task_id,
            path: path.to_string_lossy().into_owned(),
            size,
        })
    }

    fn completed_foreground_result(
        &self,
        stdout: &[u8],
        stderr: &[u8],
        status: std::process::ExitStatus,
        spilled_output: Option<Vec<u8>>,
    ) -> Result<lingxi_core::host::ForegroundRunResult, ProcessError> {
        let output = ProcessOutput {
            stdout: String::from_utf8_lossy(stdout).into_owned(),
            stderr: String::from_utf8_lossy(stderr).into_owned(),
            exit_code: status.code().unwrap_or(-1),
            timed_out: false,
        };
        let output_file = spilled_output
            .as_deref()
            .map(|output| self.spill_completed_output(output))
            .transpose()?;
        Ok(lingxi_core::host::ForegroundRunResult {
            outcome: lingxi_core::host::ForegroundOutcome::Completed(output),
            output_file,
        })
    }

    /// Build a `tokio::process::Command` from a sandboxed command, applying
    /// the claude-code spawn-env contract.
    ///
    /// Env-var precedence (matches claude-code `Shell.ts:317-328`):
    /// 1. Caller-supplied env vars on the [`SandboxedCommand`].
    /// 2. `LINGXI=1`, `GIT_EDITOR=true`, `AI_AGENT=<Mer("agent")>`, and
    ///    `SHELL=<inner.command>` (the last only for the bash provider) —
    ///    overwritten on top so callers cannot accidentally clobber them.
    /// 3. `LINGXI_SESSION_ID` is propagated only when the caller has
    ///    explicitly injected it through the env map (the engine layer
    ///    decides whether to set it).
    ///
    /// `SHELL` (#6) follows claude-code's `SHELL: n==="bash"?S:void 0`: it is
    /// set to the resolved shell binary ONLY for the bash provider
    /// (`bash`/`zsh` spawns) and OMITTED for the powershell provider (whose
    /// binary is `pwsh`/`powershell.exe`).
    ///
    /// `AI_AGENT` (#7) mirrors `Uot`'s `t.AI_AGENT=Mer("agent")`, which is
    /// gated on `source==="agent"` — the Bash spawn site hardcodes that, so the
    /// Bash/REPL/PowerShell tool commands get it. A HOOK command runs with
    /// `source:"harness"` (#43), so it gets NEITHER `AI_AGENT` NOR `GIT_EDITOR`;
    /// the runner detects a hook by its [`HOOK_COMMAND_AUDIT_REASON`] tag and
    /// skips both. `SHELL` needs no special-casing: a hook's `command` is a
    /// script path (not `bash`/`zsh`), so `is_bash_provider_shell` is already
    /// false and the inherited `SHELL` is removed — matching the hook env's
    /// absence of `SHELL`.
    fn build_command(cmd: &SandboxedCommand) -> Command {
        let inner = cmd.inner();
        // A hook command (`source:"harness"`) must NOT receive the Bash-spawn
        // `AI_AGENT` / `GIT_EDITOR` vars — see [`HOOK_COMMAND_AUDIT_REASON`].
        // Every other command keeps the full Bash-spawn contract.
        let is_hook_command = matches!(
            cmd.tag(),
            SandboxedTag::BypassAuditedWithReason { reason }
                if reason == HOOK_COMMAND_AUDIT_REASON
        );
        let mut tcmd = Command::new(&inner.command);
        tcmd.args(&inner.args);
        if let Some(cwd) = &inner.cwd {
            tcmd.current_dir(cwd);
        }

        // 1. Caller env first.
        for (k, v) in &inner.env {
            tcmd.env(k, v);
        }
        // 2. Spawn-env contract (overrides anything the caller set).
        tcmd.env(ENV_LINGXI_MARKER.0, ENV_LINGXI_MARKER.1);
        // #7: claude-code `Uot` always marks child processes as a child session.
        tcmd.env(ENV_LINGXI_CHILD_SESSION.0, ENV_LINGXI_CHILD_SESSION.1);
        // #7: claude-code `Uot` injects `AI_AGENT=Mer("agent")` for the Bash
        // spawn (`source:"agent"`); a hook child (`source:"harness"`) gets
        // neither `AI_AGENT` nor `GIT_EDITOR` (#43 — the hook env is
        // `{...WO(), ...Uot(harness), LINGXI_PROJECT_DIR}`).
        if !is_hook_command {
            tcmd.env(ENV_AI_AGENT, ai_agent_value());
            tcmd.env(ENV_GIT_EDITOR.0, ENV_GIT_EDITOR.1);
        } else {
            // R-O4: strip claude-code's `WO()` auth/OTEL denylist from a hook
            // child's INHERITED env (BIN off ~195508064). A hook command runs
            // with `source:"harness"` and `WO()` `delete`s these keys from the
            // env it builds for the child, so the script can never read the
            // user's OAuth token / subscription / rate-limit tier, the
            // background-session auth handles, the resume/session bookkeeping,
            // or any OTEL telemetry config. `tokio::process::Command` inherits
            // the parent env by default (no `env_clear` here), so we
            // `env_remove` each denylisted key + sweep every inherited `OTEL_*`
            // key. A caller-supplied entry of the same name was applied above
            // (step 1) and is removed too — matching `WO()`, which deletes from
            // the FINAL merged env (`{...process.env,...}`), so a hook never
            // sees these regardless of source.
            for key in HOOK_ENV_DENYLIST {
                tcmd.env_remove(key);
            }
            // Sweep every inherited `OTEL_*` key from the parent env. We read
            // the keys (lossily, ignoring any non-UTF-8 name — an OTEL var is
            // always ASCII) and `env_remove` each match.
            for key in std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()) {
                if key.starts_with(HOOK_ENV_DENY_PREFIX) {
                    tcmd.env_remove(&key);
                }
            }
        }
        // #6: SHELL only for the bash provider; powershell omits it. claude-code
        // spreads `{...WO(), SHELL: n==="bash"?S:void 0}` — for powershell the
        // `void 0` overwrites any inherited `SHELL` to `undefined`, which Node
        // drops from the child env. We mirror that by REMOVING the inherited
        // `SHELL` rather than merely not setting it.
        if is_bash_provider_shell(&inner.command) {
            tcmd.env(ENV_SHELL, &inner.command);
        } else {
            tcmd.env_remove(ENV_SHELL);
        }
        // 3. LINGXI_SESSION_ID propagated only if explicitly provided.
        if let Some(sess) = inner.env.get(ENV_LINGXI_SESSION_ID) {
            tcmd.env(ENV_LINGXI_SESSION_ID, sess);
        }
        // 4. GHA subprocess secret-scrub (`subprocessEnv()`, subprocessEnv.ts:86-97):
        //    when `LINGXI_SUBPROCESS_ENV_SCRUB` is truthy (claude-code-action's
        //    untrusted-content mode), `delete` each secret-bearing key + its
        //    `INPUT_<KEY>` GitHub-Actions twin from the child env — for BOTH the
        //    Bash and the hook child (both spawn via `subprocessEnv()`), so a
        //    prompt-injected command can't read Anthropic/cloud/Actions creds in
        //    that CI mode. Inert (no-op) without the flag — the common case.
        if lingxi_core::host::env::is_env_truthy(
            std::env::var(ENV_SUBPROCESS_ENV_SCRUB).ok().as_deref(),
        ) {
            for key in GHA_SUBPROCESS_SCRUB {
                tcmd.env_remove(key);
                tcmd.env_remove(format!("INPUT_{key}"));
            }
        }
        tcmd
    }
}

impl PosixProcess {
    async fn run_foreground_native(
        &self,
        cmd: &SandboxedCommand,
        max_output_bytes: Option<usize>,
    ) -> Result<lingxi_core::host::ForegroundRunResult, ProcessError> {
        let inner = cmd.inner();
        let mut tcmd = Self::build_command(cmd);
        tcmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // As in `run`: a dropped `Child` (cancellation / early return)
            // SIGKILLs the process. On the timeout→background handoff below the
            // child is MOVED into a detached reaper before this future returns,
            // so it is NOT dropped and survives; only cancellation still kills.
            .kill_on_drop(true);

        let print_mode_cleanup = active_children::print_mode_child_cleanup_enabled();
        // This path can hand its child to the background reaper or terminate
        // it at a foreground deadline. Both operations signal its owned group,
        // so the group must exist before the first fork even in an interactive
        // main-agent call with no print-mode cleanup or subagent owner.
        attach_setsid(&mut tcmd);

        let child = super::spawn_unsafe::spawn_with_capability(tcmd, cmd.background_task()).await?;
        let agent_registration = super::agent_processes::register_with_capability_requirement(
            cmd.process_owner(),
            child.id(),
            cmd.requires_group_cleanup_on_exit(),
        );
        // Capture this before polling `wait()`: Tokio clears `Child::id()` once
        // the direct child has been reaped. A busy executor can observe the
        // timeout and the already-completed child in the same poll, so looking
        // the id up only during the background handoff is racy.
        let spawned_pid = child
            .id()
            .ok_or_else(|| ProcessError::Io("spawned child has no pid".into()))?;
        let child_registration = if print_mode_cleanup {
            active_children::register(spawned_pid);
            Some(ChildRegistration(spawned_pid))
        } else {
            None
        };
        let registrations = ProcessRegistrations {
            child: child_registration,
            owner: agent_registration,
            ..ProcessRegistrations::default()
        };
        let binding = cmd.background_task();
        let verify_supervised_stop = binding
            .and_then(|bound| bound.on_exit.as_ref())
            .is_some_and(|sink| sink.requires_verified_supervised_stop());
        let stop_notify = binding
            .and_then(|bound| bound.on_exit.as_ref())
            .and_then(|sink| sink.stop_notify());
        let mut owner = ForegroundOwnerGuard {
            child: Some(child),
            registrations: Some(registrations),
            pid: spawned_pid,
            binding: verify_supervised_stop.then(|| binding.expect("supervised binding").clone()),
            lifetime_guard: cmd.lifetime_guard().cloned(),
            failure_reported: false,
            failure_reason: "foreground owner dropped before cleanup completed".into(),
        };
        let mut child = owner.child.as_mut().expect("owned foreground child");
        let mut registrations = owner
            .registrations
            .as_mut()
            .expect("foreground registrations");
        macro_rules! foreground_failure {
            ($cause:expr) => {{
                let error = $cause;
                if verify_supervised_stop
                    && !(registrations.group_cleanup_verified
                        && matches!(&error, ProcessError::Io(message) if message == "supervisor command cancelled"))
                {
                    let reason = format!("supervised foreground cleanup failed: {error}");
                    let bound = binding.expect("supervised binding");
                    let sink = bound.on_exit.as_ref().expect("supervised exit sink");
                    sink.on_wait_failure(&bound.task_id, &reason).await;
                    if registrations.group_cleanup_verified {
                        sink.on_cleanup_verified(&bound.task_id).await;
                    }
                    owner.failure_reported = true;
                    owner.failure_reason = error.to_string();
                    return Err(ProcessError::Io(format!(
                        "{} {reason}",
                        lingxi_core::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED,
                    )));
                }
                return Err(error);
            }};
        }
        if let Some(stdin_text) = &inner.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                if let Err(error) = stdin.write_all(stdin_text.as_bytes()).await {
                    if verify_supervised_stop {
                        if let Err(error) = finish_held_foreground(
                            &mut child,
                            spawned_pid,
                            &mut registrations,
                            binding,
                            true,
                            false,
                            false,
                        )
                        .await
                        {
                            foreground_failure!(error);
                        }
                    }
                    foreground_failure!(ProcessError::Io(error.to_string()));
                }
            }
        }

        let timeout = inner.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let mut verify_host_cleanup = cmd.requires_group_cleanup_on_exit();
        let mut group_stop_sent = false;
        let mut stop_requested = false;
        let mut stop_verification_deadline = None;
        let mut sout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => foreground_failure!(ProcessError::Io("child has no stdout pipe".into())),
        };
        let mut serr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => foreground_failure!(ProcessError::Io("child has no stderr pipe".into())),
        };
        let mut out_buf: Vec<u8> = Vec::new();
        let mut err_buf: Vec<u8> = Vec::new();
        let mut out_done = false;
        let mut err_done = false;
        let mut exit_status: Option<std::process::ExitStatus> = None;
        let mut framed_output = max_output_bytes.map(FramedOutputCapture::new);
        let mut framed_stdout_observed = 0usize;
        let mut framed_stderr_observed = 0usize;

        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);
        // claude-code `I_t` backgrounds a still-running FOREGROUND child by
        // calling `t.background(e)` on the live `shellCommand`. That request
        // arrives here, on the same footing as the deadline: whichever fires
        // first takes the identical move-to-background path below, so an
        // on-demand background and a timeout produce the same task record and
        // append to the same output file.
        let on_demand = cmd
            .background_task()
            .and_then(|bound| bound.on_demand.clone());
        let background_requested = async {
            match on_demand.as_ref() {
                Some(notify) => notify.notified().await,
                // No requester wired: park forever so the branch never fires.
                None => std::future::pending().await,
            }
        };
        tokio::pin!(background_requested);

        // Drive both pipes and the child's exit concurrently, bounded by the
        // deadline. Reading into buffers (rather than `wait_with_output`) keeps
        // the child + its still-open pipes in hand if the deadline fires.
        let backgrounding = loop {
            if out_done && err_done && exit_status.is_some() {
                break ForegroundBreak::Finished;
            }
            if (verify_host_cleanup || verify_supervised_stop) && out_done && err_done {
                let exited = match observe_host_child_exit(spawned_pid) {
                    Ok(exited) => exited,
                    Err(error) => foreground_failure!(error),
                };
                if exited {
                    exit_status = Some(
                        match finish_held_foreground(
                            &mut child,
                            spawned_pid,
                            &mut registrations,
                            binding,
                            verify_host_cleanup,
                            group_stop_sent,
                            stop_requested,
                        )
                        .await
                        {
                            Ok(status) => status,
                            Err(error) => foreground_failure!(error),
                        },
                    );
                    break ForegroundBreak::Finished;
                }
            }
            tokio::select! {
                biased;
                () = &mut sleep, if !stop_requested => break ForegroundBreak::Deadline,
                () = &mut background_requested, if !stop_requested => break ForegroundBreak::Requested,
                _ = async {
                    if let Some(stop) = &stop_notify { stop.notified().await; }
                    else { std::future::pending::<()>().await; }
                }, if verify_supervised_stop && !stop_requested => {
                    if let Err(error) = stop_host_process_group(spawned_pid) {
                        foreground_failure!(error);
                    }
                    group_stop_sent = true;
                    verify_host_cleanup = true;
                    stop_requested = true;
                    stop_verification_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
                }
                () = async {
                    if let Some(deadline) = stop_verification_deadline { tokio::time::sleep_until(deadline).await; }
                    else { std::future::pending::<()>().await; }
                }, if stop_requested => {
                    foreground_failure!(ProcessError::Io("foreground stop drain timed out; termination is unverified".into()));
                }
                r = sout.read_buf(&mut out_buf), if !out_done => {
                    match r {
                        Ok(0) => out_done = true,
                        Err(error) if verify_host_cleanup => foreground_failure!(ProcessError::Io(format!("foreground stdout drain failed: {error}"))),
                        Err(_) => out_done = true,
                        Ok(_) => {
                            if let Some(capture) = framed_output.as_mut() {
                                capture.observe_stdout(&out_buf[framed_stdout_observed..]);
                                framed_stdout_observed = out_buf.len();
                            }
                        }
                    }
                }
                r = serr.read_buf(&mut err_buf), if !err_done => {
                    match r {
                        Ok(0) => err_done = true,
                        Err(error) if verify_host_cleanup => foreground_failure!(ProcessError::Io(format!("foreground stderr drain failed: {error}"))),
                        Err(_) => err_done = true,
                        Ok(_) => {
                            if let Some(capture) = framed_output.as_mut() {
                                capture.observe_stderr(&err_buf[framed_stderr_observed..]);
                                framed_stderr_observed = err_buf.len();
                            }
                        }
                    }
                }
                // Ordinary commands keep their historic wait/background rules.
                // Managed commands observe exit without reaping and stop the
                // whole still-pinned group before reporting completion.
                s = child.wait(), if exit_status.is_none() && !verify_host_cleanup && !verify_supervised_stop => {
                    exit_status = Some(s.map_err(|e| ProcessError::Io(e.to_string()))?);
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(20)), if (verify_host_cleanup || verify_supervised_stop) && out_done && err_done => {}
            }
        };

        if matches!(backgrounding, ForegroundBreak::Finished) {
            let status = exit_status.expect("loop breaks with a status when not timed out");
            return self.completed_foreground_result(
                &out_buf,
                &err_buf,
                status,
                framed_output.and_then(FramedOutputCapture::into_spilled),
            );
        }

        // The timer may become ready while this future is starved by other
        // workspace work, after the OS process has already exited. Because the
        // select is deliberately timer-biased, that used to enter the
        // background branch and then fail (`Child::id()` is `None` after
        // `wait()`). Report an ordinary already-exited child as completed after
        // a bounded tail drain. For a managed host, keep the leader unreaped
        // while inherited pipes remain open so its group can still be stopped.
        let completed_status = if verify_host_cleanup || verify_supervised_stop {
            let can_complete = out_done && err_done || !verify_host_cleanup;
            let exited = if can_complete {
                match observe_host_child_exit(spawned_pid) {
                    Ok(exited) => exited,
                    Err(error) => foreground_failure!(error),
                }
            } else {
                false
            };
            if exited {
                Some(
                    match finish_held_foreground(
                        &mut child,
                        spawned_pid,
                        &mut registrations,
                        binding,
                        verify_host_cleanup,
                        group_stop_sent,
                        stop_requested,
                    )
                    .await
                    {
                        Ok(status) => status,
                        Err(error) => foreground_failure!(error),
                    },
                )
            } else {
                None
            }
        } else {
            match exit_status.take() {
                Some(status) => Some(status),
                None => child
                    .try_wait()
                    .map_err(|e| ProcessError::Io(e.to_string()))?,
            }
        };
        if let Some(status) = completed_status {
            const MAX_POST_EXIT_READS: usize = 256;
            let grace = tokio::time::sleep(std::time::Duration::from_millis(50));
            tokio::pin!(grace);
            let mut reads = 0usize;
            while !(out_done && err_done) && reads < MAX_POST_EXIT_READS {
                tokio::select! {
                    // Prefer already-buffered output/EOF over an elapsed grace
                    // timer. The read-count cap prevents a descendant that
                    // inherited the pipes from starving this loop indefinitely.
                    biased;
                    r = sout.read_buf(&mut out_buf), if !out_done => {
                        reads += 1;
                        match r {
                            Ok(0) | Err(_) => out_done = true,
                            Ok(_) => {
                                if let Some(capture) = framed_output.as_mut() {
                                    capture.observe_stdout(&out_buf[framed_stdout_observed..]);
                                    framed_stdout_observed = out_buf.len();
                                }
                            }
                        }
                    }
                    r = serr.read_buf(&mut err_buf), if !err_done => {
                        reads += 1;
                        match r {
                            Ok(0) | Err(_) => err_done = true,
                            Ok(_) => {
                                if let Some(capture) = framed_output.as_mut() {
                                    capture.observe_stderr(&err_buf[framed_stderr_observed..]);
                                    framed_stderr_observed = err_buf.len();
                                }
                            }
                        }
                    }
                    () = &mut grace => break,
                }
            }
            return self.completed_foreground_result(
                &out_buf,
                &err_buf,
                status,
                framed_output.and_then(FramedOutputCapture::into_spilled),
            );
        }

        // A command the caller marked non-auto-backgroundable is KILLED at its
        // deadline instead (claude-code `Jje::#T`: with `shouldAutoBackground`
        // off the timer runs `#b(143)` rather than `background()`). The partial
        // output collected so far still comes back, with `timed_out` set, which
        // is the interrupted-result shape the tool layer already renders.
        // An EXPLICIT request always backgrounds: the caller has already decided,
        // and `shouldAutoBackground` only ever governed what the DEADLINE does.
        if matches!(backgrounding, ForegroundBreak::Deadline) && !cmd.auto_background_on_timeout() {
            if verify_host_cleanup || verify_supervised_stop {
                if let Err(error) = stop_host_process_group(spawned_pid) {
                    foreground_failure!(error);
                }
                let cleanup = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    // Preserve the leader's process-group identity until the
                    // inherited pipes close, then reap it inside the same bound.
                    while !(out_done && err_done) {
                        tokio::select! {
                            result = sout.read_buf(&mut out_buf), if !out_done => {
                                out_done = result.map_err(|error| {
                                    ProcessError::Io(format!("foreground stdout drain failed: {error}"))
                                })? == 0;
                            }
                            result = serr.read_buf(&mut err_buf), if !err_done => {
                                err_done = result.map_err(|error| {
                                    ProcessError::Io(format!("foreground stderr drain failed: {error}"))
                                })? == 0;
                            }
                        }
                    }
                    finish_held_foreground(
                        &mut child,
                        spawned_pid,
                        &mut registrations,
                        binding,
                        true,
                        true,
                        false,
                    ).await?;
                    Ok::<(), ProcessError>(())
                })
                .await;
                match cleanup {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => foreground_failure!(error),
                    Err(_) => foreground_failure!(ProcessError::Io(
                        "foreground child drain and reap timed out; termination is unverified"
                            .into(),
                    )),
                }
            } else {
                let _ = kill_tree_force(spawned_pid);
                let _ = child.wait().await;
            }
            return Ok(lingxi_core::host::ForegroundRunResult {
                outcome: lingxi_core::host::ForegroundOutcome::Completed(ProcessOutput {
                    stdout: String::from_utf8_lossy(&out_buf).into_owned(),
                    stderr: String::from_utf8_lossy(&err_buf).into_owned(),
                    exit_code: TIMEOUT_KILL_EXIT_CODE,
                    timed_out: true,
                }),
                output_file: None,
            });
        }

        // ===== Deadline or on-demand request → move to background =====
        // Open the per-task output through the runner's pinned, no-follow root
        // before handing the live child to a detached reaper. The partial
        // output captured before the deadline is flushed first so a `Read` on
        // the file shows everything from the start; the reaper then keeps
        // copying both pipes to the same confined file handles until EOF.
        let background_binding = cmd.background_task().cloned();
        let (task_id, std_file) = match background_binding.as_ref() {
            Some(bound) => (
                bound.task_id.clone(),
                if bound
                    .on_exit
                    .as_ref()
                    .is_some_and(|sink| sink.manages_output())
                {
                    None
                } else {
                    Some(match self.open_bound_output_file(bound) {
                        Ok(file) => file,
                        Err(error) => foreground_failure!(error),
                    })
                },
            ),
            None => {
                let (task_id, _out_path, std_file) = self.create_task_output_file()?;
                (task_id, Some(std_file))
            }
        };
        let initial_output = framed_output
            .unwrap_or_else(|| {
                let mut capture = FramedOutputCapture::new(usize::MAX);
                capture.observe_stdout(&out_buf);
                capture.observe_stderr(&err_buf);
                capture
            })
            .into_background();
        let pid = spawned_pid;
        let exit_task_id = task_id.clone();
        let background_deadline = cmd
            .background_timeout()
            .map(|timeout| tokio::time::Instant::now() + timeout);
        let lifetime_guard = cmd.lifetime_guard().cloned();
        let child = owner.child.take().expect("owned foreground child");
        let registrations = owner
            .registrations
            .take()
            .expect("foreground registrations");
        let background_owner = ForegroundOwnerGuard {
            child: Some(child),
            registrations: Some(registrations),
            pid,
            binding: background_binding.clone().filter(|bound| {
                bound
                    .on_exit
                    .as_ref()
                    .is_some_and(|sink| sink.requires_verified_supervised_stop())
            }),
            lifetime_guard,
            failure_reported: false,
            failure_reason: "background owner dropped before cleanup completed".into(),
        };

        tokio::spawn(async move {
            // Hold the print-mode registration for the child's remaining life so
            // an abrupt process SIGTERM can still `killpg` the backgrounded tree.
            let mut owner = background_owner;
            let mut registrations = owner
                .registrations
                .as_mut()
                .expect("background registrations");
            let mut child = owner.child.as_mut().expect("background child");
            let mut file = std_file.map(tokio::fs::File::from_std);
            let drained = drain_framed_output(
                &mut sout,
                &mut serr,
                &mut file,
                initial_output,
                background_binding.as_ref().map(|binding| (binding, pid)),
                &mut child,
                BackgroundDrainPolicy {
                    deadline: background_deadline,
                    verify_host_cleanup,
                },
                &mut registrations,
            )
            .await;
            let mut failure_reported = drained.failure_reported;
            if let Err(error) = drained.output {
                tracing::warn!(%error, task_id = %exit_task_id, "background output drain failed");
                if (drained.requested_stop || drained.deadline_expired)
                    && background_binding
                        .as_ref()
                        .and_then(|bound| bound.on_exit.as_ref())
                        .is_some_and(|sink| sink.requires_verified_supervised_stop())
                {
                    report_stop_failure(
                        background_binding.as_ref(),
                        &format!("background output drain failed: {error}"),
                        &mut failure_reported,
                    )
                    .await;
                }
            }
            if failure_reported && drained.status.is_ok() {
                if let Some((bound, sink)) = background_binding
                    .as_ref()
                    .and_then(|bound| bound.on_exit.as_ref().map(|sink| (bound, sink)))
                {
                    sink.on_cleanup_verified(&bound.task_id).await;
                }
            }
            // Same one-shot exit report as the explicit-background path, so an
            // auto-backgrounded (timed-out) command settles its task record.
            if let Some(sink) = background_binding.and_then(|bound| bound.on_exit) {
                report_background_completion(
                    sink.as_ref(),
                    &exit_task_id,
                    drained.status,
                    drained.deadline_expired,
                    drained.requested_stop,
                    failure_reported,
                    drained.group_cleanup_verified,
                )
                .await;
            }
        });

        Ok(lingxi_core::host::ForegroundRunResult {
            outcome: lingxi_core::host::ForegroundOutcome::MovedToBackground(ProcessHandle {
                task_id,
                pid,
            }),
            output_file: None,
        })
    }

    async fn spawn_background_native(
        &self,
        cmd: &SandboxedCommand,
    ) -> Result<ProcessHandle, ProcessError> {
        // When the caller bound a task identity (the Bash tool does, so the
        // model-facing id, the registry record and this file are one identity —
        // claude-code `vV`/`Xne`), write into the caller's already-created file
        // and report the caller's id. Otherwise fall back to a runner-owned
        // identity.
        //
        // Root and leaf are both opened without following symlinks. The
        // runner pins the root identity on first use, so a later rename/swap
        // fails closed instead of redirecting command output.
        let binding = cmd.background_task().cloned();
        let (task_id, file) = match binding.as_ref() {
            Some(bound) => (
                bound.task_id.clone(),
                if bound
                    .on_exit
                    .as_ref()
                    .is_some_and(|sink| sink.manages_output())
                {
                    None
                } else {
                    Some(self.open_bound_output_file(bound)?)
                },
            ),
            None => {
                let (task_id, _out_path, file) = self.create_task_output_file()?;
                (task_id, Some(file))
            }
        };

        let mut tcmd = Self::build_command(cmd);
        tcmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        attach_setsid(&mut tcmd);
        if binding
            .as_ref()
            .and_then(|b| b.on_exit.as_ref())
            .and_then(|s| s.stop_notify())
            .is_some()
        {
            tcmd.kill_on_drop(true);
        }

        let mut child = super::spawn_unsafe::spawn_with_capability(tcmd, binding.as_ref()).await?;
        let pid = child
            .id()
            .ok_or_else(|| ProcessError::Io("spawn_background: child has no pid".into()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProcessError::Io("spawn_background: child has no stdout".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProcessError::Io("spawn_background: child has no stderr".into()))?;

        // Drain both pipes in one task so stderr chunks receive the oracle's
        // marker and the file reflects the order in which reads become ready.
        // The task also reaps the child to avoid zombies.
        let agent_registration = super::agent_processes::register_with_capability_requirement(
            cmd.process_owner(),
            Some(pid),
            cmd.requires_group_cleanup_on_exit(),
        );
        let exit_task_id = task_id.clone();
        let background_deadline = cmd
            .background_timeout()
            .map(|timeout| tokio::time::Instant::now() + timeout);
        let verify_host_cleanup = cmd.requires_group_cleanup_on_exit();
        let lifetime_guard = cmd.lifetime_guard().cloned();
        let background_owner = ForegroundOwnerGuard {
            child: Some(child),
            registrations: Some(ProcessRegistrations {
                child: None,
                owner: agent_registration,
                ..ProcessRegistrations::default()
            }),
            pid,
            binding: binding.clone().filter(|bound| {
                bound
                    .on_exit
                    .as_ref()
                    .is_some_and(|sink| sink.requires_verified_supervised_stop())
            }),
            lifetime_guard,
            failure_reported: false,
            failure_reason: "background owner dropped before cleanup completed".into(),
        };
        tokio::spawn(async move {
            let mut owner = background_owner;
            let mut registrations = owner
                .registrations
                .as_mut()
                .expect("background registrations");
            let mut child = owner.child.as_mut().expect("background child");
            let mut file = file.map(tokio::fs::File::from_std);
            let drained = drain_framed_output(
                &mut stdout,
                &mut stderr,
                &mut file,
                BackgroundOutput::default(),
                binding.as_ref().map(|binding| (binding, pid)),
                &mut child,
                BackgroundDrainPolicy {
                    deadline: background_deadline,
                    verify_host_cleanup,
                },
                &mut registrations,
            )
            .await;
            let mut failure_reported = drained.failure_reported;
            if let Err(error) = drained.output {
                tracing::warn!(%error, task_id = %exit_task_id, "background output drain failed");
                if (drained.requested_stop || drained.deadline_expired)
                    && binding
                        .as_ref()
                        .and_then(|bound| bound.on_exit.as_ref())
                        .is_some_and(|sink| sink.requires_verified_supervised_stop())
                {
                    report_stop_failure(
                        binding.as_ref(),
                        &format!("background output drain failed: {error}"),
                        &mut failure_reported,
                    )
                    .await;
                }
            }
            if failure_reported && drained.status.is_ok() {
                if let Some((bound, sink)) = binding
                    .as_ref()
                    .and_then(|bound| bound.on_exit.as_ref().map(|sink| (bound, sink)))
                {
                    sink.on_cleanup_verified(&bound.task_id).await;
                }
            }
            // One-shot exit report so the caller can settle the task record
            // (claude-code `Ger`: terminal status from the child's result, then
            // the completion notification).
            if let Some(sink) = binding.and_then(|bound| bound.on_exit) {
                report_background_completion(
                    sink.as_ref(),
                    &exit_task_id,
                    drained.status,
                    drained.deadline_expired,
                    drained.requested_stop,
                    failure_reported,
                    drained.group_cleanup_verified,
                )
                .await;
            }
        });

        Ok(ProcessHandle { task_id, pid })
    }
}

#[async_trait]
impl ProcessRunner for PosixProcess {
    fn supports_owned_streaming_cleanup(&self) -> bool {
        true
    }

    fn supports_recovery_hook_group_cleanup(&self) -> bool {
        cfg!(any(target_os = "linux", target_os = "macos"))
    }

    fn supports_background_lifetime_guard(&self) -> bool {
        true
    }

    fn supports_verified_background_stop(&self, command: &SandboxedCommand) -> bool {
        super::supervisor::enabled(command)
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

    fn supports_foreground_backgrounding(&self) -> bool {
        true
    }

    async fn kill_owner_processes(&self, owner: &str) -> Vec<u32> {
        let owned = lingxi_core::host::agent_processes::snapshot_entries(owner);
        for &entry in &owned {
            if lingxi_core::host::shell_supervisor::kill_owned_registration(owner, entry)
                .await
                .is_none()
                // A host capability may have been retired while its numeric
                // registration is still visible.  Never turn that gap into a
                // signal to a recycled PID/PGID.
                && !entry.requires_capability()
                && !owner.starts_with("host-tool:")
            {
                if owner.starts_with(lingxi_core::host::process::OWNED_STREAMING_OWNER_PREFIX) {
                    let _ = lingxi_core::host::agent_processes::with_current(owner, entry, || {
                        kill_tree_force(entry.pid)
                    });
                } else if lingxi_core::host::agent_processes::is_current(owner, entry) {
                    let _ = kill_tree_force(entry.pid);
                }
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
            .stderr(Stdio::piped())
            // On the timeout path below we `return Err(..)` and drop the
            // `Child`. Tokio's default drop does NOT kill the OS process, so a
            // timed-out `bash -c` would otherwise be orphaned and keep running
            // (the handler reports `Killed` while the process is still alive).
            // `kill_on_drop` makes the drop send SIGKILL, so the timeout
            // actually terminates the child. (Descendants escaping the direct
            // child are the `spawn_background` setsid path's concern, not the
            // foreground capture path.)
            .kill_on_drop(true);

        // PARITY 2.1.212 (print/SDK SIGTERM cleanup): in print/SDK mode, spawn the
        // foreground child in its OWN process group (`setsid`, like claude-code's
        // `detached: true`) and register its pgid so a process-level
        // SIGTERM/SIGHUP/SIGINT handler can `killpg` the whole subtree before exit
        // — `kill_on_drop` alone never fires on an abrupt signal. Interactive mode
        // leaves this off, so its spawn path is byte-identical to before.
        let print_mode_cleanup = active_children::print_mode_child_cleanup_enabled();
        if print_mode_cleanup || cmd.process_owner().is_some() {
            attach_setsid(&mut tcmd);
        }

        let mut child = tcmd.spawn().map_err(|e| ProcessError::Io(e.to_string()))?;
        let _agent_registration = super::agent_processes::register_with_capability_requirement(
            cmd.process_owner(),
            child.id(),
            cmd.requires_group_cleanup_on_exit(),
        );
        // Register the setsid child's pgid (== pid) for the duration of this run;
        // the guard unregisters it on completion OR on future-drop (cancel).
        let _child_registration = if print_mode_cleanup {
            child.id().map(|pid| {
                active_children::register(pid);
                ChildRegistration(pid)
            })
        } else {
            None
        };
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
        let owned_streaming = cmd.process_owner().is_some_and(|owner| {
            owner.starts_with(lingxi_core::host::process::OWNED_STREAMING_OWNER_PREFIX)
        });
        let mut tcmd = Self::build_command(cmd);
        tcmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A Monitor/streaming worker can be cancelled at any await point. Give
        // it an independent process group so the drop guard removes descendants
        // as well as the direct shell.
        attach_setsid(&mut tcmd);

        let mut child = tcmd.spawn().map_err(|e| ProcessError::Io(e.to_string()))?;
        let agent_registration = super::agent_processes::register_with_capability_requirement(
            cmd.process_owner(),
            child.id(),
            cmd.requires_group_cleanup_on_exit(),
        );
        let mut registrations = ProcessRegistrations {
            child: None,
            owner: agent_registration,
            ..ProcessRegistrations::default()
        };
        let pid = child.id().ok_or_else(|| {
            if owned_streaming {
                owned_streaming_error("spawned streaming child has no pid")
            } else {
                ProcessError::Io("streaming child has no pid".into())
            }
        })?;
        let mut group_guard = StreamingProcessGroupGuard(Some(pid));

        if let Some(stdin_text) = &inner.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(stdin_text.as_bytes())
                    .await
                    .map_err(|error| {
                        if owned_streaming {
                            owned_streaming_error(format!("stdin write failed: {error}"))
                        } else {
                            ProcessError::Io(error.to_string())
                        }
                    })?;
            }
        }

        let stdout = child.stdout.take().ok_or_else(|| {
            if owned_streaming {
                owned_streaming_error("streaming child has no stdout pipe")
            } else {
                ProcessError::Io("streaming child has no stdout pipe".into())
            }
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| {
            if owned_streaming {
                owned_streaming_error("streaming child has no stderr pipe")
            } else {
                ProcessError::Io("streaming child has no stderr pipe".into())
            }
        })?;
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
            let (stdout, stderr, status) = if owned_streaming {
                // Hold the leader and group identity through pipe draining and
                // its natural exit, then stop and verify redirected same-group
                // children before releasing sandbox resources.
                let (stdout, stderr) = tokio::try_join!(stdout_task, stderr_task)?;
                while !observe_host_child_exit(pid)? {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                stop_host_process_group(pid)?;
                group_guard.0 = None;
                let status =
                    reap_host_process_group(&mut child, pid, &mut registrations, None).await?;
                (stdout, stderr, status)
            } else {
                tokio::try_join!(stdout_task, stderr_task, async {
                    child
                        .wait()
                        .await
                        .map_err(|e| ProcessError::Io(e.to_string()))
                })?
            };
            Ok::<_, ProcessError>((stdout, stderr, status))
        };

        let timeout = inner.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let result = if owned_streaming && inner.timeout.is_none() {
            Ok(execution.await)
        } else {
            tokio::time::timeout(timeout, execution).await
        };
        match result {
            Ok(Ok((stdout, stderr, status))) => {
                group_guard.0 = None;
                Ok(ProcessOutput {
                    stdout: String::from_utf8_lossy(&stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&stderr).into_owned(),
                    exit_code: status.code().unwrap_or(-1),
                    timed_out: false,
                })
            }
            Ok(Err(error)) if owned_streaming => Err(owned_streaming_failure(
                &mut child,
                pid,
                &mut group_guard,
                &mut registrations,
                error,
            )
            .await),
            Ok(Err(error)) => {
                let _ = kill_tree_force(pid);
                group_guard.0 = None;
                let _ = child.wait().await;
                Err(error)
            }
            Err(_) if owned_streaming => Err(owned_streaming_failure(
                &mut child,
                pid,
                &mut group_guard,
                &mut registrations,
                "streaming deadline elapsed",
            )
            .await),
            Err(_) => {
                let _ = kill_tree_force(pid);
                group_guard.0 = None;
                let _ = child.wait().await;
                Err(ProcessError::Timeout)
            }
        }
    }

    // PARITY 2.1.210 (timeout → move-to-background): a foreground Bash command
    // that exceeds its timeout is NOT killed — the still-running child is handed
    // off to a detached reaper that keeps draining its output into a per-task
    // file, and `MovedToBackground` is returned so the tool layer surfaces the
    // "…did not complete within its Ns timeout and was moved to the background"
    // note. The spawn preamble mirrors `run` exactly (identical env, the same
    // print/SDK-mode setsid + active-children registration) so the
    // finished-in-time path is byte-identical to `run`.
    async fn run_foreground_with_output_limit(
        &self,
        cmd: &SandboxedCommand,
        max_output_bytes: Option<usize>,
    ) -> Result<lingxi_core::host::ForegroundRunResult, ProcessError> {
        if super::supervisor::enabled(cmd) {
            return super::supervisor::execute(cmd, max_output_bytes, false).await;
        }
        require_native_supervisor_for_strict(cmd)?;
        self.run_foreground_native(cmd, max_output_bytes).await
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

    async fn run_hook_with_async_detection(
        &self,
        cmd: &SandboxedCommand,
        default_async_timeout: std::time::Duration,
    ) -> Result<HookRunOutcome, ProcessError> {
        self.run_hook_with_async_detection_observed(cmd, default_async_timeout, None)
            .await
    }

    /// SH-07 — the observed variant. Identical to the buffered one except that
    /// every chunk read off the child's stdout/stderr is pushed to `observer` as
    /// it arrives, which is what lets the hook layer run claude-code's `tWi`
    /// progress poll (oracle 2.1.238 @ 296463298) over a live accumulator.
    ///
    /// RESIDUAL: the BACKGROUNDED (`{"async":true}` first line) arm detaches the
    /// child into a drain task; upstream keeps polling it through the pending-
    /// async-hook registry (`n3m`). The port's detached drain carries no
    /// observer, so a backgrounded hook emits no further `hook_progress` frames.
    async fn run_hook_with_async_detection_observed(
        &self,
        cmd: &SandboxedCommand,
        default_async_timeout: std::time::Duration,
        observer: Option<std::sync::Arc<dyn lingxi_core::host::HookOutputObserver>>,
    ) -> Result<HookRunOutcome, ProcessError> {
        self.run_hook_with_async_detection_observed_cancellable(
            cmd,
            default_async_timeout,
            observer,
            None,
        )
        .await
    }

    async fn run_hook_with_async_detection_observed_cancellable(
        &self,
        cmd: &SandboxedCommand,
        default_async_timeout: std::time::Duration,
        observer: Option<std::sync::Arc<dyn lingxi_core::host::HookOutputObserver>>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<HookRunOutcome, ProcessError> {
        if cmd.requires_group_cleanup_on_exit() {
            if !self.supports_recovery_hook_group_cleanup() || cmd.lifetime_guard().is_none() {
                return Err(ProcessError::Unsupported);
            }
            if cancellation
                .as_ref()
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            {
                return Err(ProcessError::Io("hook execution cancelled".into()));
            }
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(run_recovery_hook_owner(
                cmd.clone(),
                default_async_timeout,
                observer,
                cancellation,
                ready_tx,
            ));
            return ready_rx.await.map_err(|_| {
                ProcessError::Io("recovery hook process owner exited unexpectedly".into())
            })?;
        }
        if cancellation
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Err(ProcessError::Io("hook execution cancelled".into()));
        }
        let inner = cmd.inner();
        let mut tcmd = Self::build_command(cmd);
        tcmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // As in `run`: a dropped `Child` (timeout / early return) must SIGKILL
            // the process so a timed-out hook is not orphaned.
            .kill_on_drop(true);

        if cmd.process_owner().is_some() {
            attach_setsid(&mut tcmd);
        }

        let mut child = tcmd.spawn().map_err(|e| ProcessError::Io(e.to_string()))?;
        let agent_registration = super::agent_processes::register_with_capability_requirement(
            cmd.process_owner(),
            child.id(),
            cmd.requires_group_cleanup_on_exit(),
        );
        if let Some(stdin_text) = &inner.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                write_hook_stdin(&mut stdin, stdin_text.as_bytes())
                    .await
                    .map_err(|e| ProcessError::Io(e.to_string()))?;
            }
            // `stdin` drops here → the child sees EOF on stdin.
        }

        let timeout = inner.timeout.unwrap_or(DEFAULT_TIMEOUT);
        // A single deadline bounds the whole operation, matching `run`'s single
        // `timeout(wait_with_output)` budget.
        let deadline = tokio::time::Instant::now() + timeout;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProcessError::Io("hook child has no stdout pipe".into()))?;
        let mut reader = BufReader::new(stdout);
        let mut first_line: Vec<u8> = Vec::new();
        match tokio::time::timeout_at(deadline, reader.read_until(b'\n', &mut first_line)).await {
            Err(_) => return Err(ProcessError::Timeout),
            Ok(Err(e)) => return Err(ProcessError::Io(e.to_string())),
            Ok(Ok(_)) => {}
        }
        // Upstream attaches its `data` listeners BEFORE writing stdin, so the
        // first line is part of the observed output too.
        if let (Some(obs), false) = (observer.as_ref(), first_line.is_empty()) {
            obs.on_chunk(&first_line, &[]).await;
        }

        // Runtime async detection: a first line of `{"async":true,...}` backgrounds
        // the hook (claude-code `hooks.ts:1117-1166`).
        if let Some(async_timeout) = parse_async_first_line(&first_line, default_async_timeout) {
            let stderr = child.stderr.take();
            // The eventual (post-marker) drained output is delivered once through
            // this channel so the hook layer can fold it back as an
            // `async_hook_response` (claude-code `registerPendingAsyncHook`).
            let (output_tx, output_rx) = tokio::sync::oneshot::channel::<ProcessOutput>();
            // Detach: drain remaining output (so the pipe never blocks the child)
            // and reap it, bounded by the async timeout; on timeout the child is
            // dropped → `kill_on_drop` SIGKILLs it.
            let owner = cmd.process_owner().map(str::to_owned);
            tokio::spawn(async move {
                let _agent_registration = agent_registration;
                let mut child = child;
                let mut drain_and_wait = Box::pin(async {
                    let mut stdout_buf = Vec::new();
                    let mut stderr_buf = Vec::new();
                    let stderr_read = async {
                        if let Some(mut se) = stderr {
                            se.read_to_end(&mut stderr_buf).await
                        } else {
                            Ok(0)
                        }
                    };
                    let (_stdout_res, _stderr_res) =
                        tokio::join!(reader.read_to_end(&mut stdout_buf), stderr_read);
                    let status = child.wait().await;
                    (stdout_buf, stderr_buf, status)
                });
                // The drained stdout is the hook's output AFTER the consumed
                // `{"async":true}` marker line — exactly the payload the fold-back
                // maps through the command-hook contract (`map_command_output`).
                let drained = if let Some(cancellation) = cancellation.clone() {
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => {
                            if let Some(owner) = owner.as_deref() {
                                PosixProcess::new().kill_owner_processes(owner).await;
                            }
                            match tokio::time::timeout(
                                std::time::Duration::from_millis(500),
                                &mut drain_and_wait,
                            ).await {
                                Ok(result) => Some(result),
                                Err(_) => {
                                    drop(drain_and_wait);
                                    if let Some(owner) = owner.as_deref() {
                                        PosixProcess::new().kill_owner_processes(owner).await;
                                    }
                                    None
                                }
                            }
                        }
                        result = tokio::time::timeout(async_timeout, &mut drain_and_wait) => {
                            match result {
                                Ok(result) => Some(result),
                                Err(_) => {
                                    drop(drain_and_wait);
                                    if let Some(owner) = owner.as_deref() {
                                        PosixProcess::new().kill_owner_processes(owner).await;
                                    }
                                    None
                                }
                            }
                        }
                    }
                } else {
                    tokio::time::timeout(async_timeout, &mut drain_and_wait)
                        .await
                        .ok()
                };
                let was_cancelled = cancellation
                    .as_ref()
                    .is_some_and(tokio_util::sync::CancellationToken::is_cancelled);
                let output = match drained {
                    Some((stdout_buf, stderr_buf, status)) => ProcessOutput {
                        stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
                        stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
                        exit_code: status.map_or(-1, |s| s.code().unwrap_or(-1)),
                        timed_out: false,
                    },
                    None => ProcessOutput {
                        stdout: String::new(),
                        stderr: if was_cancelled {
                            "async hook was cancelled; process was terminated".to_string()
                        } else {
                            "async hook timed out; process was terminated".to_string()
                        },
                        exit_code: -1,
                        timed_out: true,
                    },
                };
                // `child` drops at end of scope → `kill_on_drop` SIGKILLs a
                // still-running (timed-out) child.
                let _ = output_tx.send(output);
            });
            return Ok(HookRunOutcome::Backgrounded {
                async_timeout,
                output: Some(output_rx),
            });
        }

        // Not async: read the rest of stdout and all of stderr CONCURRENTLY (as
        // `wait_with_output` does, so a large stderr can't deadlock the stdout
        // read), then wait — all bounded by the same deadline.
        let mut stderr_pipe = child.stderr.take();
        let obs_out = observer.clone();
        let obs_err = observer.clone();
        let complete = async {
            let mut rest: Vec<u8> = Vec::new();
            let mut stderr_buf: Vec<u8> = Vec::new();
            let stderr_read = async {
                if let Some(se) = stderr_pipe.as_mut() {
                    drain_observed(se, &mut stderr_buf, obs_err.as_ref(), true).await
                } else {
                    Ok(())
                }
            };
            let (rest_res, stderr_res) = tokio::join!(
                drain_observed(&mut reader, &mut rest, obs_out.as_ref(), false),
                stderr_read
            );
            rest_res.map_err(|e| ProcessError::Io(e.to_string()))?;
            stderr_res.map_err(|e| ProcessError::Io(e.to_string()))?;
            let status = child
                .wait()
                .await
                .map_err(|e| ProcessError::Io(e.to_string()))?;
            Ok::<_, ProcessError>((rest, stderr_buf, status))
        };

        let _agent_registration = agent_registration;
        let mut complete = Box::pin(complete);
        let completed = if let Some(cancellation) = cancellation {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    if let Some(owner) = cmd.process_owner() {
                        self.kill_owner_processes(owner).await;
                    }
                    if tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        &mut complete,
                    )
                    .await
                    .is_err()
                    {
                        drop(complete);
                        if let Some(owner) = cmd.process_owner() {
                            self.kill_owner_processes(owner).await;
                        }
                    }
                    return Err(ProcessError::Io("hook execution cancelled".into()));
                }
                result = tokio::time::timeout_at(deadline, &mut complete) => {
                    result.map_err(|_| ProcessError::Timeout)?
                }
            }
        } else {
            tokio::time::timeout_at(deadline, &mut complete)
                .await
                .map_err(|_| ProcessError::Timeout)?
        };
        match completed {
            Err(e) => Err(e),
            Ok((rest, stderr_buf, status)) => {
                // Reconstruct stdout exactly: consumed first line + the rest.
                let mut stdout_bytes = first_line;
                stdout_bytes.extend_from_slice(&rest);
                Ok(HookRunOutcome::Completed(ProcessOutput {
                    stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
                    stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
                    exit_code: status.code().unwrap_or(-1),
                    timed_out: false,
                }))
            }
        }
    }

    async fn spawn_background(
        &self,
        cmd: &SandboxedCommand,
    ) -> Result<ProcessHandle, ProcessError> {
        if super::supervisor::enabled(cmd) {
            return match super::supervisor::execute(cmd, None, true).await?.outcome {
                lingxi_core::host::ForegroundOutcome::MovedToBackground(handle) => Ok(handle),
                _ => Err(ProcessError::Io(
                    "supervisor returned foreground result".into(),
                )),
            };
        }
        require_native_supervisor_for_strict(cmd)?;
        self.spawn_background_native(cmd).await
    }

    async fn acknowledge_shell(&self, handle: &ProcessHandle) -> Result<(), ProcessError> {
        lingxi_core::host::shell_supervisor::acknowledge(handle).await
    }
    async fn export_shell(
        &self,
        handle: &ProcessHandle,
    ) -> Result<lingxi_core::host::process::ShellProcessHandoff, ProcessError> {
        self.acknowledge_shell(handle).await?;
        super::supervisor::export(handle)
    }
    async fn validate_shell(
        &self,
        handoff: &lingxi_core::host::process::ShellProcessHandoff,
    ) -> Result<(), ProcessError> {
        super::supervisor::validate(handoff).await
    }
    async fn adopt_shell(
        &self,
        handoff: &lingxi_core::host::process::ShellProcessHandoff,
        sink: std::sync::Arc<dyn lingxi_core::host::BackgroundExitSink>,
    ) -> Result<ProcessHandle, ProcessError> {
        super::supervisor::adopt(handoff, sink).await
    }
    async fn release_shell(
        &self,
        handoff: &lingxi_core::host::process::ShellProcessHandoff,
    ) -> Result<(), ProcessError> {
        super::supervisor::release(handoff).await
    }
    async fn kill(&self, handle: &ProcessHandle) -> Result<(), ProcessError> {
        if let Some(result) = super::supervisor::kill(handle).await {
            return result;
        }
        // Parity: claude-code's `#doKill` calls `treeKill(pid, 'SIGKILL')` directly
        // (ShellCommand.ts:337-343) — no SIGTERM grace period. Use kill_tree_force
        // (immediate SIGKILL) instead of kill_tree_unix (SIGTERM + 5 s + SIGKILL).
        kill_tree_force(handle.pid)
    }

    fn is_available(&self) -> bool {
        true
    }
}

/// Parse a hook's first stdout line for the async-response marker
/// `{"async": true, "asyncTimeout"?: <ms>}` (claude-code's `FZe` schema).
/// Returns the effective background timeout (`asyncTimeout` ms, or
/// `default_async_timeout` when absent/zero — claude's `asyncTimeout || 15000`),
/// or `None` when the line is not that marker.
fn parse_async_first_line(
    line: &[u8],
    default_async_timeout: std::time::Duration,
) -> Option<std::time::Duration> {
    let text = std::str::from_utf8(line).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("async") != Some(&serde_json::Value::Bool(true)) {
        return None;
    }
    let timeout = value
        .get("asyncTimeout")
        .and_then(serde_json::Value::as_u64)
        .filter(|&ms| ms > 0)
        .map_or(default_async_timeout, std::time::Duration::from_millis);
    Some(timeout)
}

/// Generate a task id unique across processes and concurrent calls. The
/// process nonce comes from the OS CSPRNG; PID prevents a fork from inheriting
/// an identical nonce/sequence namespace, and the atomic counter prevents
/// same-tick collisions inside one process.
fn generate_task_id() -> Result<String, ProcessError> {
    static PROCESS_NONCE: OnceLock<Result<u64, String>> = OnceLock::new();
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let nonce = PROCESS_NONCE.get_or_init(|| {
        let mut bytes = [0u8; 8];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut random| random.read_exact(&mut bytes))
            .map(|()| u64::from_ne_bytes(bytes))
            .map_err(|error| format!("read process randomness: {error}"))
    });
    let nonce = nonce
        .as_ref()
        .map_err(|error| ProcessError::Io(error.clone()))?;
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(format!(
        "local_bash_{:x}_{nonce:016x}_{sequence:016x}",
        std::process::id()
    ))
}

#[cfg(test)]
mod async_hook_tests {
    use super::*;
    use lingxi_core::host::ProcessCommand;
    use std::collections::{HashMap, HashSet};
    use std::io::Write as _;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    struct HookPidObserver {
        bytes: std::sync::Mutex<Vec<u8>>,
        ready: tokio::sync::Notify,
    }

    #[async_trait]
    impl lingxi_core::host::HookOutputObserver for HookPidObserver {
        async fn on_chunk(&self, stdout_delta: &[u8], _stderr_delta: &[u8]) {
            if !stdout_delta.is_empty() {
                self.bytes.lock().unwrap().extend_from_slice(stdout_delta);
                self.ready.notify_one();
            }
        }
    }

    #[test]
    fn background_utf8_decoder_preserves_every_multibyte_boundary() {
        for text in ["中", "🦀", "é", "a中🦀éz"] {
            for split in 0..=text.len() {
                let mut decoder = Utf8StreamDecoder::default();
                let first = decoder.decode(&text.as_bytes()[..split], false);
                let second = decoder.decode(&text.as_bytes()[split..], false);
                assert_eq!(
                    format!("{first}{second}{}", decoder.decode(&[], true)),
                    text
                );
            }
        }
    }

    #[test]
    fn background_transition_retains_partial_characters_before_stderr_framing() {
        let mut capture = FramedOutputCapture::new(0);
        capture.observe_stdout(&[0xe4, 0xb8]);
        capture.observe_stderr(&[0xf0, 0x9f]);
        let mut background = capture.into_background();
        assert!(background.initial.is_empty());
        assert_eq!(background.stdout_decoder.decode(&[0xad], false), "中");
        assert_eq!(background.stderr_decoder.decode(&[0xa6, 0x80], false), "🦀");
        let mut complete = FramedOutputCapture::new(0);
        complete.observe_stderr(&[0xe4, 0xb8]);
        complete.observe_stderr(&[0xad]);
        assert_eq!(complete.into_spilled().unwrap(), "[stderr] 中".as_bytes());
    }

    #[test]
    fn foreground_spill_flushes_incomplete_utf8_at_eof() {
        let mut capture = FramedOutputCapture::new(0);
        capture.observe_stdout(b"prefix\xe4\xb8");
        assert_eq!(capture.force_spilled(), "prefix\u{fffd}".as_bytes());
        let mut capture = FramedOutputCapture::new(0);
        capture.observe_stderr(&[0xf0, 0x9f]);
        assert_eq!(
            capture.into_spilled().unwrap(),
            "[stderr] \u{fffd}".as_bytes()
        );
    }

    #[tokio::test]
    async fn managed_background_drain_calls_sink_and_flushes_before_exit() {
        #[derive(Default)]
        struct Sink {
            chunks: std::sync::Mutex<String>,
            flushed: std::sync::atomic::AtomicBool,
            done: tokio::sync::Notify,
        }
        #[async_trait]
        impl lingxi_core::host::BackgroundExitSink for Sink {
            fn manages_output(&self) -> bool {
                true
            }
            async fn append_output(&self, _: &str, content: &str) -> Result<(), ProcessError> {
                self.chunks.lock().unwrap().push_str(content);
                Ok(())
            }
            async fn flush_output(&self, _: &str) -> Result<(), ProcessError> {
                self.flushed.store(true, Ordering::SeqCst);
                Ok(())
            }
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                self.done.notify_one();
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let path = dir.path().join("no-direct-file/output");
        let command =
            sh("printf routed").with_background_task(lingxi_core::host::BackgroundTaskBinding {
                task_id: "bmanaged1".into(),
                output_path: path.clone(),
                on_exit: Some(sink.clone()),
                on_demand: None,
            });
        let process = PosixProcess::new();
        let handle = process.spawn_background(&command).await.unwrap();
        let done = tokio::time::timeout(Duration::from_secs(5), sink.done.notified()).await;
        if done.is_err() {
            let _ = process.kill(&handle).await;
        }
        done.unwrap();
        assert_eq!(*sink.chunks.lock().unwrap(), "routed");
        assert!(sink.flushed.load(Ordering::SeqCst));
        assert!(
            !path.exists(),
            "managed output must not also be written directly"
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    enum ExitObservation {
        Reaped(Option<i32>),
        Known(Option<i32>, bool),
        Proof(Option<i32>, Option<bool>, bool),
        Deadline(Option<i32>),
        Unverified(String),
    }

    #[derive(Default)]
    struct ExitObserver {
        events: std::sync::Mutex<Vec<ExitObservation>>,
        done: tokio::sync::Notify,
    }

    #[async_trait]
    impl lingxi_core::host::BackgroundExitSink for ExitObserver {
        async fn on_exit(&self, _: &str, code: Option<i32>) {
            self.events
                .lock()
                .unwrap()
                .push(ExitObservation::Reaped(code));
            self.done.notify_one();
        }

        async fn on_exit_with_status(&self, _: &str, code: Option<i32>, killed: bool) {
            self.events
                .lock()
                .unwrap()
                .push(ExitObservation::Known(code, killed));
            self.done.notify_one();
        }

        async fn on_wait_failure(&self, _: &str, error: &str) {
            self.events
                .lock()
                .unwrap()
                .push(ExitObservation::Unverified(error.into()));
            self.done.notify_one();
        }

        async fn on_background_deadline_exit(&self, _: &str, code: Option<i32>) {
            self.events
                .lock()
                .unwrap()
                .push(ExitObservation::Deadline(code));
            self.done.notify_one();
        }
    }

    fn process_with_birth_is_running(pid: u32, birth: &str) -> bool {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let pid = Pid::from_raw(i32::try_from(pid).unwrap());
        match kill(pid, None) {
            Err(Errno::ESRCH) => return false,
            Ok(()) => {}
            Err(error) => panic!("could not inspect descendant: {error}"),
        }
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-o", "lstart=", "-p", &pid.to_string()])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .output()
            .unwrap();
        if !output.status.success() {
            assert_eq!(
                kill(pid, None),
                Err(Errno::ESRCH),
                "ps failed for a live pid"
            );
            return false;
        }
        let text = String::from_utf8(output.stdout).unwrap();
        let (state, actual_birth) = text.trim().split_once(char::is_whitespace).unwrap();
        actual_birth.trim() == birth && !matches!(state.chars().next(), Some('Z' | 'X' | 'x'))
    }

    // Force both orders at the actual POSIX release point. The leader and its
    // redirected descendant are real processes; the sink only supplies the
    // same synchronous arbitration contract as the supervisor's gate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervised_completion_and_stop_keep_the_held_process_group() {
        use std::sync::atomic::AtomicU32;

        #[derive(Default)]
        struct Decision {
            proceed: bool,
            release: bool,
            stopped: bool,
            committed: bool,
            actions: usize,
        }
        #[derive(Default)]
        struct GateSink {
            decision: std::sync::Mutex<Decision>,
            changed: std::sync::Condvar,
            entered: tokio::sync::Notify,
            decided: tokio::sync::Notify,
            handoff_started: tokio::sync::Notify,
            handoff_changed: tokio::sync::Notify,
            handoff_accepted: std::sync::atomic::AtomicBool,
            require_handoff: bool,
            stop: Arc<tokio::sync::Notify>,
            done: tokio::sync::Notify,
            leader: AtomicU32,
            expect_held_leader: bool,
            observed_held_leader: std::sync::atomic::AtomicBool,
            events: std::sync::Mutex<Vec<ExitObservation>>,
        }
        impl GateSink {
            fn request_stop(&self) -> bool {
                let mut state = self.decision.lock().unwrap();
                if state.committed {
                    return false;
                }
                state.stopped = true;
                self.stop.notify_one();
                self.handoff_changed.notify_one();
                true
            }

            fn accept_handoff(&self) {
                self.handoff_accepted.store(true, Ordering::SeqCst);
                self.handoff_changed.notify_one();
            }

            fn allow_decision(&self) {
                self.decision.lock().unwrap().proceed = true;
                self.changed.notify_all();
            }

            fn allow_return(&self) {
                self.decision.lock().unwrap().release = true;
                self.changed.notify_all();
            }
        }
        #[async_trait]
        impl lingxi_core::host::BackgroundExitSink for GateSink {
            fn requires_verified_supervised_stop(&self) -> bool {
                true
            }
            fn manages_output(&self) -> bool {
                true
            }
            fn stop_notify(&self) -> Option<Arc<tokio::sync::Notify>> {
                Some(self.stop.clone())
            }
            async fn wait_for_supervisor_handoff(&self) -> Result<bool, ProcessError> {
                if !self.require_handoff {
                    return Ok(true);
                }
                self.handoff_started.notify_one();
                loop {
                    let notified = self.handoff_changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if self.decision.lock().unwrap().stopped {
                        return Ok(false);
                    }
                    if self.handoff_accepted.load(Ordering::SeqCst) {
                        return Ok(true);
                    }
                    notified.await;
                }
            }
            fn commit_foreground(
                &self,
                action: &mut dyn FnMut() -> Result<(), ProcessError>,
            ) -> Result<bool, ProcessError> {
                if self.expect_held_leader {
                    let leader = self.leader.load(Ordering::SeqCst);
                    self.observed_held_leader.store(
                        matches!(observe_host_child_exit(leader), Ok(true)),
                        Ordering::SeqCst,
                    );
                }
                self.entered.notify_one();
                let mut state = self.decision.lock().unwrap();
                while !state.proceed {
                    let (next, timeout) = self
                        .changed
                        .wait_timeout(state, Duration::from_secs(10))
                        .unwrap();
                    state = next;
                    if timeout.timed_out() && !state.proceed {
                        return Err(ProcessError::Io("test decision barrier timed out".into()));
                    }
                }
                if state.stopped {
                    self.decided.notify_one();
                    return Ok(false);
                }
                action()?;
                state.actions += 1;
                state.committed = true;
                self.decided.notify_one();
                while !state.release {
                    let (next, timeout) = self
                        .changed
                        .wait_timeout(state, Duration::from_secs(10))
                        .unwrap();
                    state = next;
                    if timeout.timed_out() && !state.release {
                        return Err(ProcessError::Io("test return barrier timed out".into()));
                    }
                }
                Ok(true)
            }
            async fn on_spawn(&self, _: &str, pid: u32) -> Result<(), ProcessError> {
                self.leader.store(pid, Ordering::SeqCst);
                Ok(())
            }
            async fn append_output(&self, _: &str, _: &str) -> Result<(), ProcessError> {
                Ok(())
            }
            async fn flush_output(&self, _: &str) -> Result<(), ProcessError> {
                Ok(())
            }
            async fn on_exit(&self, _: &str, code: Option<i32>) {
                self.events
                    .lock()
                    .unwrap()
                    .push(ExitObservation::Reaped(code));
                self.done.notify_one();
            }
            async fn on_exit_with_proof(
                &self,
                _: &str,
                code: Option<i32>,
                killed: Option<bool>,
                group: bool,
            ) {
                self.events
                    .lock()
                    .unwrap()
                    .push(ExitObservation::Proof(code, killed, group));
                self.done.notify_one();
            }
            async fn on_wait_failure(&self, _: &str, error: &str) {
                self.events
                    .lock()
                    .unwrap()
                    .push(ExitObservation::Unverified(error.into()));
                self.done.notify_one();
            }
        }

        for strict in [false, true] {
            for mode in ["foreground", "automatic", "explicit"] {
                for stop_first in [false, true] {
                    let dir = tempfile::tempdir().unwrap();
                    let release = dir.path().join("leader-release");
                    let pid_path = dir.path().join("descendant-pid");
                    let quote =
                        |path: &std::path::Path| path.to_string_lossy().replace('\'', "'\\''");
                    let script = format!(
                        "sleep 30 </dev/null >/dev/null 2>&1 & printf '%s' \"$!\" > '{}'; while [ ! -f '{}' ]; do sleep 0.02; done",
                        quote(&pid_path),
                        quote(&release),
                    );
                    let sink = Arc::new(GateSink {
                        expect_held_leader: !strict && !(mode != "foreground" && stop_first),
                        require_handoff: mode != "foreground" && !strict,
                        ..GateSink::default()
                    });
                    let command = sh_with_timeout(
                        &script,
                        if mode == "automatic" {
                            Duration::from_millis(100)
                        } else {
                            Duration::from_secs(30)
                        },
                    )
                    .with_require_group_cleanup_on_exit(strict)
                    .with_background_task(
                        lingxi_core::host::BackgroundTaskBinding {
                            task_id: "held-group-decision".into(),
                            output_path: dir.path().join("output"),
                            on_exit: Some(sink.clone()),
                            on_demand: None,
                        },
                    );
                    let mut task = tokio::spawn(async move {
                        let runner = PosixProcess::new();
                        if mode == "explicit" {
                            runner
                                .spawn_background_native(&command)
                                .await
                                .map(lingxi_core::host::ForegroundOutcome::MovedToBackground)
                        } else {
                            runner
                                .run_foreground_native(&command, None)
                                .await
                                .map(|result| result.outcome)
                        }
                    });
                    let descendant = tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            if let Ok(text) = std::fs::read_to_string(&pid_path) {
                                if let Ok(pid) = text.parse::<u32>() {
                                    break pid;
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .expect("fixture must publish its descendant");
                    let birth =
                        lingxi_core::host::live_sessions::process_start_identity(descendant)
                            .expect("descendant must have a start identity");
                    assert!(process_with_birth_is_running(descendant, &birth));
                    if mode != "foreground" {
                        let result = tokio::time::timeout(Duration::from_secs(10), &mut task)
                            .await
                            .unwrap()
                            .unwrap()
                            .unwrap();
                        assert!(matches!(
                            result,
                            lingxi_core::host::ForegroundOutcome::MovedToBackground(_)
                        ));
                    }
                    let proof_barrier =
                        (strict && stop_first).then(|| Arc::new(CleanupProofBarrier::default()));
                    if let Some(barrier) = &proof_barrier {
                        CLEANUP_PROOF_BARRIERS
                            .get_or_init(Default::default)
                            .lock()
                            .unwrap()
                            .insert(sink.leader.load(Ordering::SeqCst), barrier.clone());
                    }
                    std::fs::write(&release, "go").unwrap();
                    let accepted_during_handoff = if mode != "foreground" && !strict {
                        tokio::time::timeout(
                            Duration::from_secs(10),
                            sink.handoff_started.notified(),
                        )
                        .await
                        .expect("natural background exit must wait for acceptance");
                        assert!(
                            observe_host_child_exit(sink.leader.load(Ordering::SeqCst)).unwrap()
                        );
                        assert!(process_with_birth_is_running(descendant, &birth));
                        if stop_first {
                            sink.request_stop()
                        } else {
                            sink.accept_handoff();
                            false
                        }
                    } else {
                        false
                    };
                    let accepted_during_proof = if let Some(barrier) = &proof_barrier {
                        tokio::time::timeout(Duration::from_secs(10), barrier.entered.notified())
                            .await
                            .expect("strict cleanup must reach the proof barrier");
                        let accepted = sink.request_stop();
                        barrier.release.notify_one();
                        accepted
                    } else {
                        false
                    };
                    tokio::time::timeout(Duration::from_secs(10), sink.entered.notified())
                        .await
                        .expect("runner must enter the completion gate");
                    let accepted_before_decision = if strict && stop_first {
                        accepted_during_proof
                    } else if mode != "foreground" && stop_first {
                        accepted_during_handoff
                    } else if stop_first {
                        sink.request_stop()
                    } else {
                        false
                    };
                    sink.allow_decision();
                    tokio::time::timeout(Duration::from_secs(10), sink.decided.notified())
                        .await
                        .expect("gate must decide");
                    let accepted_after_commit = if stop_first {
                        false
                    } else {
                        sink.request_stop()
                    };
                    let still_running = process_with_birth_is_running(descendant, &birth);
                    // Before releasing the barrier the leader is still held,
                    // so this test-only cleanup cannot signal a reused PGID.
                    if !stop_first && !strict {
                        signal_host_process_group(sink.leader.load(Ordering::SeqCst)).unwrap();
                    }
                    sink.allow_return();
                    assert_eq!(
                        sink.observed_held_leader.load(Ordering::SeqCst),
                        sink.expect_held_leader,
                        "natural completion must arbitrate with a held leader; verified stop waits for proof"
                    );
                    assert_eq!(accepted_before_decision, stop_first);
                    assert!(!accepted_after_commit);
                    assert_eq!(
                        sink.decision.lock().unwrap().actions,
                        usize::from(!stop_first)
                    );
                    if !stop_first && !strict {
                        assert!(
                            still_running,
                            "natural completion must preserve redirected descendants"
                        );
                    } else if strict {
                        assert!(
                            !still_running,
                            "strict completion must prove cleanup before the gate decides"
                        );
                    }
                    if mode == "foreground" {
                        let result = tokio::time::timeout(Duration::from_secs(15), &mut task)
                            .await
                            .unwrap()
                            .unwrap();
                        if stop_first {
                            assert!(
                                matches!(result, Err(ProcessError::Io(ref message)) if message == "supervisor command cancelled")
                            );
                        } else {
                            assert!(matches!(
                                result,
                                Ok(lingxi_core::host::ForegroundOutcome::Completed(_))
                            ));
                        }
                    } else {
                        tokio::time::timeout(Duration::from_secs(15), sink.done.notified())
                            .await
                            .expect("background reaper must settle");
                        let expected = ExitObservation::Proof(
                            Some(0),
                            (!stop_first).then_some(false),
                            strict || stop_first,
                        );
                        assert_eq!(*sink.events.lock().unwrap(), [expected]);
                    }
                    if stop_first || strict {
                        assert!(
                            !process_with_birth_is_running(descendant, &birth),
                            "verified cleanup left the original descendant running"
                        );
                    }
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_supervised_foreground_retains_owner_until_verified_cleanup() {
        use std::sync::atomic::{AtomicBool, AtomicU32};

        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        #[derive(Default)]
        struct Sink {
            leader: AtomicU32,
            stop: Arc<tokio::sync::Notify>,
            failed: tokio::sync::Notify,
            cleaned: tokio::sync::Notify,
            events: std::sync::Mutex<Vec<&'static str>>,
        }
        #[async_trait]
        impl lingxi_core::host::BackgroundExitSink for Sink {
            fn requires_verified_supervised_stop(&self) -> bool {
                true
            }
            fn stop_notify(&self) -> Option<Arc<tokio::sync::Notify>> {
                Some(self.stop.clone())
            }
            async fn on_spawn(&self, _: &str, pid: u32) -> Result<(), ProcessError> {
                self.leader.store(pid, Ordering::SeqCst);
                Ok(())
            }
            async fn on_wait_failure(&self, _: &str, _: &str) {
                self.events.lock().unwrap().push("unverified");
                self.failed.notify_one();
            }
            async fn on_cleanup_verified(&self, _: &str) {
                self.events.lock().unwrap().push("verified");
                self.cleaned.notify_one();
            }
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                panic!("aborted foreground must not publish natural completion");
            }
        }

        for fail_first_stop in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let pid_path = dir.path().join("descendant-pid");
            let script = format!(
                "sleep 30 </dev/null >/dev/null 2>&1 & printf '%s' \"$!\" > '{}'; wait",
                pid_path.to_string_lossy().replace('\'', "'\\''"),
            );
            let sink = Arc::new(Sink::default());
            let guard_dropped = Arc::new(AtomicBool::new(false));
            let command = sh(&script)
                .with_lifetime_guard(Guard(guard_dropped.clone()))
                .with_background_task(lingxi_core::host::BackgroundTaskBinding {
                    task_id: "aborted-foreground-owner".into(),
                    output_path: dir.path().join("output"),
                    on_exit: Some(sink.clone()),
                    on_demand: None,
                });
            let task = tokio::spawn(async move {
                PosixProcess::new()
                    .run_foreground_native(&command, None)
                    .await
            });
            let descendant = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(&pid_path) {
                        if let Ok(pid) = text.parse::<u32>() {
                            break pid;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("foreground descendant must start");
            let birth = lingxi_core::host::live_sessions::process_start_identity(descendant)
                .expect("descendant must have a start identity");
            assert!(process_with_birth_is_running(descendant, &birth));
            let leader = sink.leader.load(Ordering::SeqCst);
            let barrier = Arc::new(CleanupProofBarrier::default());
            CLEANUP_PROOF_BARRIERS
                .get_or_init(Default::default)
                .lock()
                .unwrap()
                .insert(leader, barrier.clone());

            if fail_first_stop {
                FAIL_NEXT_FOREGROUND_GROUP_STOP
                    .get_or_init(Default::default)
                    .lock()
                    .unwrap()
                    .insert(leader);
                sink.stop.notify_one();
                let error = task.await.unwrap().unwrap_err();
                assert!(
                    matches!(error, ProcessError::Io(message) if message.starts_with(lingxi_core::host::process::OWNED_SHELL_CLEANUP_UNVERIFIED))
                );
            } else {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            }
            tokio::time::timeout(Duration::from_secs(10), sink.failed.notified())
                .await
                .expect("dropped owner must publish an unverified observation");
            tokio::time::timeout(Duration::from_secs(10), barrier.entered.notified())
                .await
                .expect("retry must reach the group proof barrier");
            assert!(
                observe_host_child_exit(leader).is_ok(),
                "retry still owns its leader"
            );
            assert!(
                !guard_dropped.load(Ordering::SeqCst),
                "lease must survive until proof"
            );
            barrier.release.notify_one();
            tokio::time::timeout(Duration::from_secs(15), sink.cleaned.notified())
                .await
                .expect("the same group must be verified before cleanup notification");
            assert!(!process_with_birth_is_running(descendant, &birth));
            assert_eq!(*sink.events.lock().unwrap(), ["unverified", "verified"]);
            tokio::time::timeout(Duration::from_secs(5), async {
                while !guard_dropped.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("verified cleanup may release the lease");
        }
    }

    #[test]
    fn host_stop_rejects_invalid_process_group_ids() {
        assert!(signal_host_process_group(0).is_err());
        assert!(signal_host_process_group(u32::MAX).is_err());
    }

    #[tokio::test]
    async fn host_owner_never_signals_an_unmatched_numeric_registration() {
        let owner = "host-tool:registration-without-capability";
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]).kill_on_drop(true);
        attach_setsid(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let registration = lingxi_core::host::agent_processes::register(Some(owner), Some(pid));

        PosixProcess::new().kill_owner_processes(owner).await;
        assert!(
            child.try_wait().unwrap().is_none(),
            "no capability must mean no raw signal"
        );
        drop(registration);
        signal_host_process_group(pid).unwrap();
        child.wait().await.unwrap();
    }

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
        let script = format!(
            "printf executed > '{}'",
            marker.to_string_lossy().replace('\'', "'\\''")
        );
        let strict = sh(&script)
            .with_require_group_cleanup_on_exit(true)
            .with_background_task(lingxi_core::host::BackgroundTaskBinding {
                task_id: "unmanaged-strict-marker".into(),
                output_path: dir.path().join("output"),
                on_exit: Some(Arc::new(ClaimingSink)),
                on_demand: None,
            });
        let process = PosixProcess::new();
        assert!(matches!(
            process.run(&strict).await,
            Err(ProcessError::Unsupported)
        ));
        assert!(matches!(
            process.run_streaming(&strict, Arc::new(Stream)).await,
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
        let ordinary = sh(&script);
        assert!(process.run(&ordinary).await.unwrap().exit_code == 0);
        assert!(
            marker.exists(),
            "ordinary direct execution must remain available"
        );
    }

    #[tokio::test]
    async fn mixed_owner_never_signals_capability_only_reused_pid() {
        fn spawn() -> tokio::process::Child {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "exec sleep 60"]).kill_on_drop(true);
            attach_setsid(&mut command);
            command.spawn().unwrap()
        }
        let mut unrelated = spawn();
        let mut ordinary = spawn();
        let unrelated_pid = unrelated.id().unwrap();
        let ordinary_pid = ordinary.id().unwrap();
        let owner = format!("mixed-capability-owner-{unrelated_pid}");
        // Model a retired supervisor capability whose retained numeric PID now
        // aliases a live unrelated process. No supervisor capability exists.
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
        let mut seen = PosixProcess::new().kill_owner_processes(&owner).await;
        seen.sort_unstable();
        let mut expected = vec![unrelated_pid, ordinary_pid];
        expected.sort_unstable();
        let untouched = unrelated.try_wait().unwrap().is_none();
        let normal_exit = tokio::time::timeout(Duration::from_secs(10), ordinary.wait()).await;
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

    #[tokio::test]
    async fn host_foreground_and_background_completion_reap_redirected_group_children() {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;

        // The release file forces the child to exist before its leader exits.
        // No descendant holds a runner pipe, reproducing the original race.
        for mode in ["foreground", "explicit-background", "auto-background"] {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("child-pid");
            let release = dir.path().join("release");
            let pid_path = pid_file.display().to_string().replace('\'', "'\\''");
            let release_path = release.display().to_string().replace('\'', "'\\''");
            let script = format!(
                "sleep 60 </dev/null >/dev/null 2>&1 & printf '%s' \"$!\" > '{pid_path}'; while [ ! -f '{release_path}' ]; do sleep 0.02; done; exit 0"
            );
            let sink = Arc::new(ExitObserver::default());
            let owner = format!("host-tool:redirected-{mode}");
            let command = sh_with_timeout(
                &script,
                if mode == "auto-background" {
                    Duration::from_millis(100)
                } else {
                    Duration::from_secs(8)
                },
            )
            .with_process_owner(Some(owner.clone()))
            .with_require_group_cleanup_on_exit(true)
            .with_background_task(lingxi_core::host::BackgroundTaskBinding {
                task_id: format!("redirected-{mode}"),
                output_path: dir.path().join("output"),
                on_exit: Some(sink.clone()),
                on_demand: None,
            });
            std::fs::write(dir.path().join("output"), "").unwrap();
            let mut task = tokio::spawn(async move {
                let process = PosixProcess::new();
                if mode == "explicit-background" {
                    process
                        .spawn_background_native(&command)
                        .await
                        .map(lingxi_core::host::ForegroundOutcome::MovedToBackground)
                } else {
                    process
                        .run_foreground_native(&command, None)
                        .await
                        .map(|result| result.outcome)
                }
            });
            let pid: i32 = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(&pid_file) {
                        if let Ok(pid) = text.parse() {
                            break pid;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("background descendant must start before leader release");
            assert!(
                kill(Pid::from_raw(pid), None).is_ok(),
                "descendant must initially be alive: {mode}"
            );
            let early_result = if mode != "foreground" {
                Some(
                    tokio::time::timeout(Duration::from_secs(5), &mut task)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap(),
                )
            } else {
                None
            };
            std::fs::write(&release, "go").unwrap();
            let outcome = if let Some(outcome) = early_result {
                outcome
            } else {
                tokio::time::timeout(Duration::from_secs(10), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
            };
            if mode == "foreground" {
                assert!(matches!(
                    outcome,
                    lingxi_core::host::ForegroundOutcome::Completed(_)
                ));
                // Model cancellation arriving after completion but before the
                // response is delivered. There must be no surviving group.
                PosixProcess::new().kill_owner_processes(&owner).await;
            } else {
                assert!(matches!(
                    outcome,
                    lingxi_core::host::ForegroundOutcome::MovedToBackground(_)
                ));
                tokio::time::timeout(Duration::from_secs(10), sink.done.notified())
                    .await
                    .unwrap();
                assert_eq!(
                    *sink.events.lock().unwrap(),
                    [ExitObservation::Known(Some(0), false)]
                );
            }
            let alive = kill(Pid::from_raw(pid), None).is_ok();
            if alive {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
            assert!(
                !alive,
                "verified completion left a live group child: {mode}"
            );
        }
    }

    #[tokio::test]
    async fn ordinary_foreground_keeps_redirected_background_child() {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;

        let command = sh("sleep 60 </dev/null >/dev/null 2>&1 & printf '%s' \"$!\"");
        let output = match PosixProcess::new().run_foreground(&command).await.unwrap() {
            lingxi_core::host::ForegroundOutcome::Completed(output) => output,
            other => panic!("expected ordinary completion: {other:?}"),
        };
        let pid = Pid::from_raw(output.stdout.parse().unwrap());
        let alive = kill(pid, None).is_ok();
        if alive {
            let _ = kill(pid, Signal::SIGKILL);
        }
        assert!(
            alive,
            "ordinary agent background behavior must remain unchanged"
        );
    }

    #[tokio::test]
    async fn host_background_reapers_distinguish_signal_exit_from_wait_failure() {
        struct DropObserver(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropObserver {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        for explicit in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("release");
            let marker_quoted = marker.display().to_string().replace('\'', "'\\''");
            let script =
                format!("while [ ! -f '{marker_quoted}' ]; do sleep 0.02; done; kill -TERM $$");
            let output = dir.path().join("output");
            std::fs::write(&output, "").unwrap();
            let sink = Arc::new(ExitObserver::default());
            let guard_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let command = sh_with_timeout(&script, Duration::from_millis(100))
                .with_process_owner(Some("host-tool:signal-test".into()))
                .with_require_group_cleanup_on_exit(true)
                .with_lifetime_guard(DropObserver(guard_dropped.clone()))
                .with_background_task(lingxi_core::host::BackgroundTaskBinding {
                    task_id: "host-signal".into(),
                    output_path: output,
                    on_exit: Some(sink.clone()),
                    on_demand: None,
                });
            let process = PosixProcess::new();
            let handle = if explicit {
                process.spawn_background_native(&command).await.unwrap()
            } else {
                match process
                    .run_foreground_native(&command, None)
                    .await
                    .map(|result| result.outcome)
                    .unwrap()
                {
                    lingxi_core::host::ForegroundOutcome::MovedToBackground(handle) => handle,
                    other => panic!("expected foreground handoff: {other:?}"),
                }
            };
            drop(command);
            assert!(
                !guard_dropped.load(Ordering::SeqCst),
                "background reaper must retain the guard: explicit={explicit}"
            );
            std::fs::write(marker, "go").unwrap();
            let done = tokio::time::timeout(Duration::from_secs(10), sink.done.notified()).await;
            if done.is_err() {
                let _ = process.kill(&handle).await;
            }
            done.expect("signal-terminated child should be reaped");
            assert_eq!(
                *sink.events.lock().unwrap(),
                [ExitObservation::Known(None, false)],
                "explicit={explicit}"
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                while !guard_dropped.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("reaped process must release its lifetime guard");
        }
    }

    #[tokio::test]
    async fn background_stop_and_natural_completion_have_distinct_outcomes() {
        #[derive(Default)]
        struct Sink {
            stop: Arc<tokio::sync::Notify>,
            ready: tokio::sync::Notify,
            done: tokio::sync::Notify,
            cleanup: tokio::sync::Notify,
            reject_flush: std::sync::atomic::AtomicBool,
            output: std::sync::Mutex<String>,
            events: std::sync::Mutex<Vec<ExitObservation>>,
        }
        #[async_trait]
        impl lingxi_core::host::BackgroundExitSink for Sink {
            fn manages_output(&self) -> bool {
                true
            }
            fn stop_notify(&self) -> Option<Arc<tokio::sync::Notify>> {
                Some(self.stop.clone())
            }
            fn requires_verified_supervised_stop(&self) -> bool {
                true
            }
            async fn append_output(&self, _: &str, text: &str) -> Result<(), ProcessError> {
                let mut output = self.output.lock().unwrap();
                output.push_str(text);
                if output.contains("ready") {
                    self.ready.notify_one();
                }
                Ok(())
            }
            async fn flush_output(&self, _: &str) -> Result<(), ProcessError> {
                if self.reject_flush.load(Ordering::SeqCst) {
                    Err(ProcessError::Io("injected output flush failure".into()))
                } else {
                    Ok(())
                }
            }
            async fn on_cleanup_verified(&self, _: &str) {
                self.cleanup.notify_one();
            }
            async fn on_exit(&self, _: &str, code: Option<i32>) {
                self.events
                    .lock()
                    .unwrap()
                    .push(ExitObservation::Reaped(code));
                self.done.notify_one();
            }
            async fn on_exit_with_status(&self, _: &str, code: Option<i32>, killed: bool) {
                self.events
                    .lock()
                    .unwrap()
                    .push(ExitObservation::Known(code, killed));
                self.done.notify_one();
            }
            async fn on_exit_with_proof(
                &self,
                _: &str,
                code: Option<i32>,
                killed: Option<bool>,
                group: bool,
            ) {
                self.events
                    .lock()
                    .unwrap()
                    .push(ExitObservation::Proof(code, killed, group));
                self.done.notify_one();
            }
            async fn on_wait_failure(&self, _: &str, error: &str) {
                self.events
                    .lock()
                    .unwrap()
                    .push(ExitObservation::Unverified(error.into()));
                self.done.notify_one();
            }
        }

        for (explicit, supervisor_only, deadline) in [
            (true, false, false),
            (false, false, false),
            (true, true, false),
            (false, true, false),
            (true, true, true),
            (false, true, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let sink = Arc::new(Sink::default());
            let pid_path = dir.path().join("descendant.pid");
            let script = format!(
                "sleep 60 & printf '%s\\n' \"$!\" > '{}'; printf ready; wait",
                pid_path.to_string_lossy().replace('\'', "'\\''")
            );
            let command = sh_with_timeout(
                &script,
                if explicit {
                    Duration::from_secs(30)
                } else {
                    Duration::from_millis(100)
                },
            )
            .with_process_owner(
                (!supervisor_only).then(|| "host-tool:verified-stop-outcome".into()),
            )
            .with_require_group_cleanup_on_exit(!supervisor_only)
            .with_background_timeout(deadline.then_some(Duration::from_secs(2)))
            .with_background_task(lingxi_core::host::BackgroundTaskBinding {
                task_id: "host-stop-outcome".into(),
                output_path: dir.path().join("output"),
                on_exit: Some(sink.clone()),
                on_demand: None,
            });
            let process = PosixProcess::new();
            let handle = if explicit {
                process.spawn_background_native(&command).await.unwrap()
            } else {
                match process
                    .run_foreground_native(&command, None)
                    .await
                    .map(|result| result.outcome)
                    .unwrap()
                {
                    lingxi_core::host::ForegroundOutcome::MovedToBackground(handle) => handle,
                    other => panic!("expected background handoff: {other:?}"),
                }
            };
            tokio::time::timeout(Duration::from_secs(10), sink.ready.notified())
                .await
                .expect("child output confirms the live command started");
            let descendant: u32 = std::fs::read_to_string(&pid_path)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(lingxi_core::host::live_sessions::process_start_identity(descendant).is_some());
            assert!(!observe_host_child_exit(handle.pid).unwrap());
            REJECT_REPEAT_GROUP_STOP
                .get_or_init(Default::default)
                .lock()
                .unwrap()
                .insert(handle.pid, 0);
            if !deadline {
                sink.stop.notify_one();
            }
            let done = tokio::time::timeout(Duration::from_secs(20), sink.done.notified()).await;
            if done.is_err() {
                let _ = process.kill(&handle).await;
            }
            done.expect("stopped process group must finish");
            assert_eq!(
                REJECT_REPEAT_GROUP_STOP
                    .get()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .remove(&handle.pid),
                Some(1),
                "a verified stop must not send a redundant second signal"
            );
            assert_eq!(
                *sink.events.lock().unwrap(),
                [ExitObservation::Proof(None, None, true)],
                "a stop or deadline does not prove causation: explicit={explicit}, supervisor_only={supervisor_only}, deadline={deadline}"
            );
            let ps = tokio::process::Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", &descendant.to_string()])
                .output()
                .await
                .unwrap();
            let state = String::from_utf8_lossy(&ps.stdout);
            assert!(
                !ps.status.success() || state.trim().is_empty() || state.contains('Z'),
                "descendant must be stopped before outcome: {state}"
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = sh("printf ready; exit 0")
            .with_process_owner(Some("host-tool:natural-outcome".into()))
            .with_require_group_cleanup_on_exit(true)
            .with_background_task(lingxi_core::host::BackgroundTaskBinding {
                task_id: "host-natural-outcome".into(),
                output_path: dir.path().join("output"),
                on_exit: Some(sink.clone()),
                on_demand: None,
            });
        let handle = PosixProcess::new()
            .spawn_background_native(&command)
            .await
            .unwrap();
        let done = tokio::time::timeout(Duration::from_secs(20), sink.done.notified()).await;
        if done.is_err() {
            let _ = PosixProcess::new().kill(&handle).await;
        }
        done.expect("natural completion must be reaped");
        sink.stop.notify_one();
        assert_eq!(
            *sink.events.lock().unwrap(),
            [ExitObservation::Proof(Some(0), Some(false), true)],
            "a later stop notification must not rewrite a completed result"
        );

        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        sink.reject_flush.store(true, Ordering::SeqCst);
        let command = sh("printf ready; exec sleep 60").with_background_task(
            lingxi_core::host::BackgroundTaskBinding {
                task_id: "supervised-stop-flush-failure".into(),
                output_path: dir.path().join("output"),
                on_exit: Some(sink.clone()),
                on_demand: None,
            },
        );
        let process = PosixProcess::new();
        let handle = process.spawn_background_native(&command).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), sink.ready.notified())
            .await
            .unwrap();
        sink.stop.notify_one();
        let done = tokio::time::timeout(Duration::from_secs(20), sink.done.notified()).await;
        if done.is_err() {
            let _ = process.kill(&handle).await;
        }
        done.expect("output failure must be reported");
        tokio::time::timeout(Duration::from_secs(5), sink.cleanup.notified())
            .await
            .expect("verified process cleanup must be reported separately");
        let events = sink.events.lock().unwrap();
        assert!(
            matches!(&events[..], [ExitObservation::Unverified(error)] if error.contains("injected output flush failure"))
        );
    }

    #[tokio::test]
    async fn supervised_leader_and_group_proofs_follow_actual_cleanup() {
        #[derive(Default)]
        struct Sink {
            stop: Arc<tokio::sync::Notify>,
            ready: tokio::sync::Notify,
            done: tokio::sync::Notify,
            output: std::sync::Mutex<String>,
            events: std::sync::Mutex<Vec<ExitObservation>>,
        }
        #[async_trait]
        impl lingxi_core::host::BackgroundExitSink for Sink {
            fn manages_output(&self) -> bool {
                true
            }
            fn requires_verified_supervised_stop(&self) -> bool {
                true
            }
            fn stop_notify(&self) -> Option<Arc<tokio::sync::Notify>> {
                Some(self.stop.clone())
            }
            async fn append_output(&self, _: &str, text: &str) -> Result<(), ProcessError> {
                let mut output = self.output.lock().unwrap();
                output.push_str(text);
                if output.contains("ready") {
                    self.ready.notify_one();
                }
                Ok(())
            }
            async fn on_exit_with_proof(
                &self,
                _: &str,
                code: Option<i32>,
                killed: Option<bool>,
                group: bool,
            ) {
                self.events
                    .lock()
                    .unwrap()
                    .push(ExitObservation::Proof(code, killed, group));
                self.done.notify_one();
            }
            async fn on_exit(&self, _: &str, code: Option<i32>) {
                self.events
                    .lock()
                    .unwrap()
                    .push(ExitObservation::Reaped(code));
                self.done.notify_one();
            }
            async fn on_wait_failure(&self, _: &str, error: &str) {
                self.events
                    .lock()
                    .unwrap()
                    .push(ExitObservation::Unverified(error.into()));
                self.done.notify_one();
            }
        }
        struct ReleaseOnDrop(PathBuf);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                let _ = std::fs::write(&self.0, "release");
            }
        }

        // With no strict policy, the leader may complete while a deliberately
        // detached descendant is still live. Strict mode must first stop and
        // verify the same group, even though the leader itself exited normally.
        for strict in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let sink = Arc::new(Sink::default());
            let release = dir.path().join("release");
            let _release_guard = ReleaseOnDrop(release.clone());
            let quote = |path: &Path| path.to_string_lossy().replace('\'', "'\\''");
            let child_ready = dir.path().join("child-ready");
            let child_pid = dir.path().join("child.pid");
            let script = format!(
                "(printf yes > '{}'; i=0; while [ \"$i\" -lt 600 ]; do [ -e '{}' ] && exit 0; i=$((i+1)); sleep 0.1; done) </dev/null >/dev/null 2>&1 & printf '%s' \"$!\" > '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; printf ready; exit 0",
                quote(&child_ready), quote(&release), quote(&child_pid), quote(&child_ready),
            );
            let command = sh(&script)
                .with_require_group_cleanup_on_exit(strict)
                .with_background_task(lingxi_core::host::BackgroundTaskBinding {
                    task_id: format!("leader-proof-{strict}"),
                    output_path: dir.path().join("output"),
                    on_exit: Some(sink.clone()),
                    on_demand: None,
                });
            let process = PosixProcess::new();
            let handle = process.spawn_background_native(&command).await.unwrap();
            let done = tokio::time::timeout(Duration::from_secs(20), sink.done.notified()).await;
            if done.is_err() {
                let _ = process.kill(&handle).await;
            }
            done.expect("leader completion was not reported");
            assert_eq!(
                *sink.events.lock().unwrap(),
                [ExitObservation::Proof(Some(0), Some(false), strict)]
            );
            let pid: u32 = std::fs::read_to_string(child_pid).unwrap().parse().unwrap();
            if !strict {
                assert!(
                    lingxi_core::host::live_sessions::process_start_identity(pid).is_some(),
                    "ordinary natural completion must leave the descendant alive"
                );
                std::fs::write(&release, "release").unwrap();
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let ps = Command::new("/bin/ps")
                        .args(["-o", "stat=", "-p", &pid.to_string()])
                        .output()
                        .await
                        .unwrap();
                    let state = String::from_utf8_lossy(&ps.stdout);
                    if !ps.status.success() || state.trim().is_empty() || state.contains('Z') {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("descendant must be gone after strict cleanup or test release");
        }

        // The leader exits but a live descendant still holds the pipes. Stop
        // wins before leader publication while the original group ID is held.
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let pid_file = dir.path().join("child.pid");
        let script = format!(
            "sleep 60 & printf '%s' \"$!\" > '{}'; printf ready; exit 0",
            pid_file.to_string_lossy().replace('\'', "'\\''")
        );
        let command = sh(&script).with_background_task(lingxi_core::host::BackgroundTaskBinding {
            task_id: "leader-then-stop-proof".into(),
            output_path: dir.path().join("output"),
            on_exit: Some(sink.clone()),
            on_demand: None,
        });
        let process = PosixProcess::new();
        let handle = process.spawn_background_native(&command).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), sink.ready.notified())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !observe_host_child_exit(handle.pid).unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("leader did not exit before stop");
        let descendant: u32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
        assert!(lingxi_core::host::live_sessions::process_start_identity(descendant).is_some());
        assert!(sink.events.lock().unwrap().is_empty());
        sink.stop.notify_one();
        let done = tokio::time::timeout(Duration::from_secs(20), sink.done.notified()).await;
        if done.is_err() {
            let _ = process.kill(&handle).await;
        }
        done.expect("stop after leader exit must verify its group");
        assert_eq!(
            *sink.events.lock().unwrap(),
            [ExitObservation::Proof(Some(0), None, true)]
        );
    }

    #[tokio::test]
    async fn supervised_stop_retries_after_signal_failure_without_losing_diagnostic() {
        #[derive(Default)]
        struct Sink {
            stop: Arc<tokio::sync::Notify>,
            ready: tokio::sync::Notify,
            failed: tokio::sync::Notify,
            cleaned: tokio::sync::Notify,
            output: std::sync::Mutex<String>,
            events: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait]
        impl lingxi_core::host::BackgroundExitSink for Sink {
            fn manages_output(&self) -> bool {
                true
            }
            fn requires_verified_supervised_stop(&self) -> bool {
                true
            }
            fn stop_notify(&self) -> Option<Arc<tokio::sync::Notify>> {
                Some(self.stop.clone())
            }
            async fn append_output(&self, _: &str, text: &str) -> Result<(), ProcessError> {
                let mut output = self.output.lock().unwrap();
                output.push_str(text);
                if output.contains("ready") {
                    self.ready.notify_one();
                }
                Ok(())
            }
            async fn on_wait_failure(&self, _: &str, error: &str) {
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("failure: {error}"));
                self.failed.notify_one();
            }
            async fn on_cleanup_verified(&self, _: &str) {
                self.events.lock().unwrap().push("cleanup verified".into());
                self.cleaned.notify_one();
            }
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                self.events
                    .lock()
                    .unwrap()
                    .push("unexpected terminal".into());
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let task_id = format!("stop-retry-{}", dir.path().display());
        FAIL_NEXT_BACKGROUND_STOP
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(task_id.clone());
        let sink = Arc::new(Sink::default());
        let command = sh("printf ready; exec sleep 60").with_background_task(
            lingxi_core::host::BackgroundTaskBinding {
                task_id,
                output_path: dir.path().join("output"),
                on_exit: Some(sink.clone()),
                on_demand: None,
            },
        );
        let process = PosixProcess::new();
        let handle = process.spawn_background(&command).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), sink.ready.notified())
            .await
            .unwrap();
        sink.stop.notify_one();
        tokio::time::timeout(Duration::from_secs(10), sink.failed.notified())
            .await
            .expect("first injected signal failure was not reported");
        assert!(
            !observe_host_child_exit(handle.pid).unwrap(),
            "first stop must leave real child live"
        );
        sink.stop.notify_one();
        let cleaned = tokio::time::timeout(Duration::from_secs(20), sink.cleaned.notified()).await;
        if cleaned.is_err() {
            let _ = process.kill(&handle).await;
        }
        cleaned.expect("second stop must verify cleanup");
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].contains("injected background stop failure"));
        assert_eq!(events[1], "cleanup verified");
        assert!(lingxi_core::host::live_sessions::process_start_identity(handle.pid).is_none());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_reaper_accepts_retry_before_releasing_group_identity() {
        #[derive(Default)]
        struct Sink {
            stop: Arc<tokio::sync::Notify>,
            ready: tokio::sync::Notify,
            failed: tokio::sync::Notify,
            cleaned: tokio::sync::Notify,
            output: std::sync::Mutex<String>,
            events: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait]
        impl lingxi_core::host::BackgroundExitSink for Sink {
            fn manages_output(&self) -> bool {
                true
            }
            fn requires_verified_supervised_stop(&self) -> bool {
                true
            }
            fn stop_notify(&self) -> Option<Arc<tokio::sync::Notify>> {
                Some(self.stop.clone())
            }
            async fn append_output(&self, _: &str, text: &str) -> Result<(), ProcessError> {
                let mut output = self.output.lock().unwrap();
                output.push_str(text);
                if output.contains("ready") {
                    self.ready.notify_one();
                }
                Ok(())
            }
            async fn on_wait_failure(&self, _: &str, error: &str) {
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("failure: {error}"));
                self.failed.notify_one();
            }
            async fn on_cleanup_verified(&self, _: &str) {
                self.events.lock().unwrap().push("cleanup verified".into());
                self.cleaned.notify_one();
            }
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                self.events
                    .lock()
                    .unwrap()
                    .push("unexpected terminal".into());
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let task_id = format!("reaper-retry-{}", dir.path().display());
        FAIL_NEXT_BACKGROUND_STOP
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(task_id.clone());
        SKIP_NEXT_BACKGROUND_STOP
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(task_id.clone());
        let pid_file = dir.path().join("child.pid");
        let release = dir.path().join("release");
        let quote = |path: &Path| path.to_string_lossy().replace('\'', "'\\''");
        let script = format!("sleep 60 </dev/null >/dev/null 2>&1 & printf '%s' \"$!\" > '{}'; printf ready; exec 1>&- 2>&-; i=0; while [ ! -e '{}' ] && [ \"$i\" -lt 600 ]; do i=$((i+1)); sleep 0.1; done; exit 0", quote(&pid_file), quote(&release));
        let sink = Arc::new(Sink::default());
        let command = sh(&script).with_background_task(lingxi_core::host::BackgroundTaskBinding {
            task_id: task_id.clone(),
            output_path: dir.path().join("output"),
            on_exit: Some(sink.clone()),
            on_demand: None,
        });
        let process = PosixProcess::new();
        let handle = process.spawn_background(&command).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), sink.ready.notified())
            .await
            .unwrap();
        let descendant: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        sink.stop.notify_one();
        tokio::time::timeout(Duration::from_secs(10), sink.failed.notified())
            .await
            .unwrap();
        sink.stop.notify_one();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let pending = SKIP_NEXT_BACKGROUND_STOP
                    .get()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .contains(&task_id);
                if !pending {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("simulated incomplete stop was not processed");
        std::fs::write(&release, "release").unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !observe_host_child_exit(handle.pid).unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("leader did not exit before the retry");
        assert!(lingxi_core::host::live_sessions::process_start_identity(descendant).is_some());
        sink.stop.notify_one();
        let cleaned = tokio::time::timeout(Duration::from_secs(20), sink.cleaned.notified()).await;
        if cleaned.is_err() {
            let _ = process.kill(&handle).await;
        }
        cleaned.expect("retry must stop the live group before reporting cleanup");
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].contains("injected background stop failure"));
        assert_eq!(events[1], "cleanup verified");
        assert!(lingxi_core::host::live_sessions::process_start_identity(handle.pid).is_none());
    }

    #[tokio::test]
    async fn host_background_wait_failures_are_unverified_and_reported_once() {
        use std::os::unix::process::ExitStatusExt as _;

        let sink = Arc::new(ExitObserver::default());
        report_background_completion(
            sink.as_ref(),
            "host-wait",
            Err(std::io::Error::other("wait failed")),
            false,
            false,
            false,
            false,
        )
        .await;
        assert_eq!(
            *sink.events.lock().unwrap(),
            [ExitObservation::Unverified(
                "background child wait failed: wait failed".into()
            )]
        );

        sink.events.lock().unwrap().clear();
        report_background_completion(
            sink.as_ref(),
            "ordinary-deadline",
            Err(std::io::Error::other("wait failed")),
            true,
            false,
            false,
            false,
        )
        .await;
        assert_eq!(
            *sink.events.lock().unwrap(),
            [ExitObservation::Unverified(
                "background child wait failed: wait failed".into()
            )]
        );

        sink.events.lock().unwrap().clear();
        let binding = lingxi_core::host::BackgroundTaskBinding {
            task_id: "host-stop".into(),
            output_path: PathBuf::new(),
            on_exit: Some(sink.clone()),
            on_demand: None,
        };
        let mut reported = false;
        report_stop_failure(Some(&binding), "stop failed", &mut reported).await;
        report_stop_failure(Some(&binding), "second failure", &mut reported).await;
        report_background_completion(
            sink.as_ref(),
            "host-stop",
            Ok(std::process::ExitStatus::from_raw(0)),
            false,
            true,
            reported,
            true,
        )
        .await;
        assert_eq!(
            *sink.events.lock().unwrap(),
            [ExitObservation::Unverified("stop failed".into())]
        );
    }

    #[tokio::test]
    async fn host_foreground_deadline_kills_descendants_after_leader_exit() {
        let command = sh_with_timeout(
            "sleep 15 & printf '%s\\n' \"$!\"; exit 0",
            Duration::from_secs(1),
        )
        .with_process_owner(Some("host-tool:foreground-deadline".into()))
        .with_require_group_cleanup_on_exit(true)
        .with_auto_background_on_timeout(false);
        let process = PosixProcess::new();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            process.run_foreground_native(&command, None),
        )
        .await
        .expect("foreground deadline must settle")
        .unwrap();
        let output = match result.outcome {
            lingxi_core::host::ForegroundOutcome::Completed(output) => output,
            other => panic!("expected timed-out foreground result: {other:?}"),
        };
        let pid: u32 = output.stdout.trim().parse().expect("descendant pid");
        assert!(
            output.timed_out,
            "leader exit must not hide a live descendant"
        );
        assert_eq!(output.exit_code, TIMEOUT_KILL_EXIT_CODE);

        let stopped = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = tokio::process::Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", &pid.to_string()])
                    .output()
                    .await
                    .unwrap();
                let state = String::from_utf8_lossy(&status.stdout);
                if !status.status.success() || state.trim().is_empty() || state.contains('Z') {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        stopped.expect("descendant must be stopped by its original process group");
    }

    #[tokio::test]
    async fn background_deadline_kills_explicit_and_handed_off_children_and_flushes_output() {
        #[derive(Default)]
        struct Sink {
            output: std::sync::Mutex<String>,
            expired: std::sync::atomic::AtomicBool,
            done: tokio::sync::Notify,
        }
        #[async_trait]
        impl lingxi_core::host::BackgroundExitSink for Sink {
            fn manages_output(&self) -> bool {
                true
            }
            async fn append_output(&self, _: &str, text: &str) -> Result<(), ProcessError> {
                self.output.lock().unwrap().push_str(text);
                Ok(())
            }
            async fn on_background_deadline_exit(&self, _: &str, _: Option<i32>) {
                self.expired.store(true, Ordering::SeqCst);
                self.done.notify_one();
            }
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                self.done.notify_one();
            }
        }
        for (explicit, parent_exits) in [(true, false), (false, false), (true, true), (false, true)]
        {
            let dir = tempfile::tempdir().unwrap();
            let sink = Arc::new(Sink::default());
            let script = if parent_exits {
                "printf before; sleep 0.15; sleep 60 & printf after"
            } else {
                "printf before; sleep 0.15; printf after; exec sleep 60"
            };
            let command = sh_with_timeout(script, Duration::from_millis(100))
                .with_background_timeout(Some(Duration::from_millis(250)))
                .with_background_task(lingxi_core::host::BackgroundTaskBinding {
                    task_id: "bdeadline".into(),
                    output_path: dir.path().join("output"),
                    on_exit: Some(sink.clone()),
                    on_demand: None,
                });
            let process = PosixProcess::new();
            let handle = if explicit {
                process.spawn_background(&command).await.unwrap()
            } else {
                match process.run_foreground(&command).await.unwrap() {
                    lingxi_core::host::ForegroundOutcome::MovedToBackground(handle) => handle,
                    other => panic!("must hand off: {other:?}"),
                }
            };
            let settled = tokio::time::timeout(Duration::from_secs(5), sink.done.notified()).await;
            if settled.is_err() {
                let _ = process.kill(&handle).await;
            }
            settled.unwrap_or_else(|error| panic!(
                "background deadline settles without a foreground waiter: explicit={explicit}, parent_exits={parent_exits}, pid={}, output={:?}: {error}",
                handle.pid, *sink.output.lock().unwrap()
            ));
            assert!(
                sink.expired.load(Ordering::SeqCst),
                "explicit={explicit}, parent_exits={parent_exits}"
            );
            assert_eq!(
                *sink.output.lock().unwrap(),
                "beforeafter",
                "explicit={explicit}, parent_exits={parent_exits}"
            );
            assert!(
                lingxi_core::host::live_sessions::process_start_identity(handle.pid).is_none(),
                "held child is reaped before settlement: explicit={explicit}, parent_exits={parent_exits}"
            );
        }
    }

    #[tokio::test]
    async fn stop_after_background_leader_exit_kills_descendants_without_deadline_cause() {
        #[derive(Default)]
        struct Sink {
            stop: Arc<tokio::sync::Notify>,
            output: std::sync::Mutex<String>,
            expired: std::sync::atomic::AtomicBool,
            done: tokio::sync::Notify,
        }
        #[async_trait]
        impl lingxi_core::host::BackgroundExitSink for Sink {
            fn manages_output(&self) -> bool {
                true
            }
            fn stop_notify(&self) -> Option<Arc<tokio::sync::Notify>> {
                Some(self.stop.clone())
            }
            async fn append_output(&self, _: &str, text: &str) -> Result<(), ProcessError> {
                self.output.lock().unwrap().push_str(text);
                Ok(())
            }
            async fn on_background_deadline_exit(&self, _: &str, _: Option<i32>) {
                self.expired.store(true, Ordering::SeqCst);
                self.done.notify_one();
            }
            async fn on_exit(&self, _: &str, _: Option<i32>) {
                self.done.notify_one();
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let command = sh("sleep 60 & printf before-stop")
            .with_background_timeout(Some(Duration::from_secs(5)))
            .with_background_task(lingxi_core::host::BackgroundTaskBinding {
                task_id: "bstop-exited-leader".into(),
                output_path: dir.path().join("output"),
                on_exit: Some(sink.clone()),
                on_demand: None,
            });
        let process = PosixProcess::new();
        let handle = process.spawn_background(&command).await.unwrap();
        // Observe the zombie without wait/try_wait: the runner must retain its
        // process-group identity until its pipe-holding descendant is stopped.
        let exited = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let status = tokio::process::Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", &handle.pid.to_string()])
                    .output()
                    .await
                    .unwrap();
                if String::from_utf8_lossy(&status.stdout).contains('Z') {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        sink.stop.notify_one();
        let stopped = tokio::time::timeout(Duration::from_secs(1), sink.done.notified()).await;
        if stopped.is_err() {
            let _ = process.kill(&handle).await;
        }
        exited.expect("leader exits before stop without being reaped");
        stopped.expect("stop settles before the five-second background deadline");
        assert_eq!(*sink.output.lock().unwrap(), "before-stop");
        assert!(!sink.expired.load(Ordering::SeqCst));
        assert!(
            lingxi_core::host::live_sessions::process_start_identity(handle.pid).is_none(),
            "stop joins the held leader before settlement"
        );
    }

    fn sh(script: &str) -> SandboxedCommand {
        sh_with_timeout(script, Duration::from_secs(5))
    }

    fn sh_with_timeout(script: &str, timeout: Duration) -> SandboxedCommand {
        let pcmd = ProcessCommand {
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: None,
            env: HashMap::new(),
            timeout: Some(timeout),
            stdin: None,
        };
        SandboxedCommand::__new_sandboxed(
            pcmd,
            SandboxedTag::BypassAuditedWithReason {
                reason: HOOK_COMMAND_AUDIT_REASON.to_string(),
            },
        )
    }

    #[test]
    fn completed_output_copy_caps_bytes_but_reports_original_size() {
        let dir = tempfile::tempdir().unwrap();
        let process = PosixProcess::with_task_output_dir(dir.path().join("tasks"));
        let cap = lingxi_core::host::task_output::MAX_PERSISTED_OUTPUT_BYTES as usize;
        let bytes = vec![b'x'; cap + 3];
        let output = process.spill_completed_output(&bytes).unwrap();
        assert_eq!(output.size, cap as u64 + 3);
        assert_eq!(std::fs::metadata(output.path).unwrap().len(), cap as u64);
    }

    #[cfg(unix)]
    #[test]
    fn task_output_root_is_owner_only_and_replacement_fails_closed() {
        use std::os::unix::fs::PermissionsExt as _;

        let parent = tempfile::tempdir().unwrap();
        let output_dir = parent.path().join("tasks");
        let runner = PosixProcess::with_task_output_dir(output_dir.clone());
        let (output_path, mut file) = runner.open_task_output_file("first").unwrap();
        file.write_all(b"private output").unwrap();
        drop(file);

        assert_eq!(
            std::fs::symlink_metadata(&output_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            lingxi_core::host::rooted_fs::PRIVATE_DIR_MODE
        );
        assert_eq!(
            std::fs::symlink_metadata(&output_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            lingxi_core::host::rooted_fs::PRIVATE_FILE_MODE
        );

        std::fs::rename(&output_dir, parent.path().join("tasks-original")).unwrap();
        std::fs::create_dir(&output_dir).unwrap();
        let error = runner
            .open_task_output_file("second")
            .expect_err("a same-path directory replacement must be refused");
        assert!(error.to_string().contains("tasks dir moved or linked"));
        assert!(!output_dir.join("second.out").exists());
    }

    #[cfg(unix)]
    #[test]
    fn task_output_root_symlink_is_refused_without_touching_target() {
        let parent = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        let output_dir = parent.path().join("tasks");
        std::os::unix::fs::symlink(victim.path(), &output_dir).unwrap();
        let runner = PosixProcess::with_task_output_dir(output_dir);

        let error = runner
            .open_task_output_file("escaped")
            .expect_err("a symlinked task-output root must be refused");
        assert!(error.to_string().contains("tasks dir moved or linked"));
        assert!(!victim.path().join("escaped.out").exists());
    }

    #[test]
    fn task_output_collision_retries_without_appending_existing_bytes() {
        let parent = tempfile::tempdir().unwrap();
        let runner = PosixProcess::with_task_output_dir(parent.path().join("tasks"));
        let (occupied_path, mut occupied) = runner.open_task_output_file("collision").unwrap();
        occupied.write_all(b"do not append").unwrap();
        drop(occupied);

        let mut attempt = 0usize;
        let (task_id, fresh_path, _fresh) = runner
            .create_task_output_file_with(|| {
                attempt += 1;
                Ok(if attempt == 1 {
                    "collision".to_string()
                } else {
                    "fresh".to_string()
                })
            })
            .unwrap();

        assert_eq!(task_id, "fresh");
        assert_eq!(std::fs::read(occupied_path).unwrap(), b"do not append");
        assert_eq!(std::fs::metadata(fresh_path).unwrap().len(), 0);
    }

    #[test]
    fn concurrent_task_output_allocations_are_unique() {
        const THREADS: usize = 32;
        let parent = tempfile::tempdir().unwrap();
        let runner = Arc::new(PosixProcess::with_task_output_dir(
            parent.path().join("tasks"),
        ));
        let barrier = Arc::new(Barrier::new(THREADS));
        let threads = (0..THREADS)
            .map(|_| {
                let runner = runner.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let (task_id, _, _) = runner.create_task_output_file().unwrap();
                    task_id
                })
            })
            .collect::<Vec<_>>();
        let ids = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), THREADS);
        assert_eq!(
            std::fs::read_dir(parent.path().join("tasks"))
                .unwrap()
                .count(),
            THREADS
        );
    }

    #[test]
    fn framed_spill_preserves_observed_chunk_order_and_stderr_markers() {
        let mut capture = FramedOutputCapture::new(0);
        capture.observe_stdout(b"out");
        capture.observe_stderr(b"err");
        capture.observe_stdout(b"tail");
        capture.observe_stderr(b"last");
        assert_eq!(
            capture.into_spilled().unwrap(),
            b"out[stderr] errtail[stderr] last"
        );

        let mut crossing = FramedOutputCapture::new(4);
        crossing.observe_stdout(b"ab");
        crossing.observe_stderr(b"cd");
        crossing.observe_stderr(b"ef");
        assert_eq!(
            crossing.into_spilled().unwrap(),
            b"ab[stderr] cd[stderr] ef"
        );
    }

    #[tokio::test]
    async fn drain_observed_preserves_output_larger_than_eight_mebibytes() {
        let payload = vec![b'x'; 8 * 1024 * 1024 + 257];
        let mut reader = payload.as_slice();
        let mut captured = Vec::new();

        drain_observed(&mut reader, &mut captured, None, false)
            .await
            .expect("drain");

        assert_eq!(captured, payload);
    }

    #[tokio::test]
    async fn foreground_output_spill_reports_rooted_identity_and_exact_bytes() {
        let parent = tempfile::tempdir().unwrap();
        let runner = PosixProcess::with_task_output_dir(parent.path().join("tasks"));
        let result = runner
            .run_foreground_with_output_limit(&sh("printf out; printf err >&2"), Some(3))
            .await
            .expect("foreground command");
        let output = match result.outcome {
            lingxi_core::host::ForegroundOutcome::Completed(output) => output,
            other => panic!("expected completed output, got {other:?}"),
        };
        assert_eq!(output.stdout, "out");
        assert_eq!(output.stderr, "err");
        let file = result.output_file.expect("output over limit spills");
        assert!(file.task_id.starts_with("local_bash_"));
        assert_eq!(file.size, 15);
        assert_eq!(std::fs::read(&file.path).unwrap(), b"out[stderr] err");
    }

    #[tokio::test]
    async fn foreground_output_under_limit_stays_inline_without_identity() {
        let parent = tempfile::tempdir().unwrap();
        let runner = PosixProcess::with_task_output_dir(parent.path().join("tasks"));
        let result = runner
            .run_foreground_with_output_limit(&sh("printf out"), Some(3))
            .await
            .expect("foreground command");
        assert!(result.output_file.is_none());
        assert!(matches!(
            result.outcome,
            lingxi_core::host::ForegroundOutcome::Completed(_)
        ));
        assert!(!parent.path().join("tasks").exists());
    }

    #[tokio::test]
    async fn foreground_output_spill_refuses_a_replaced_root() {
        let parent = tempfile::tempdir().unwrap();
        let output_dir = parent.path().join("tasks");
        let runner = PosixProcess::with_task_output_dir(output_dir.clone());
        runner
            .run_foreground_with_output_limit(&sh("printf first"), Some(1))
            .await
            .expect("initial spill");

        std::fs::rename(&output_dir, parent.path().join("tasks-original")).unwrap();
        std::fs::create_dir(&output_dir).unwrap();
        let error = runner
            .run_foreground_with_output_limit(&sh("printf second"), Some(1))
            .await
            .expect_err("a replaced root must fail closed");
        assert!(error.to_string().contains("tasks dir moved or linked"));
        assert!(!output_dir.join("second.out").exists());
    }

    #[test]
    fn parse_async_first_line_recognizes_marker_and_timeout() {
        let def = Duration::from_millis(15_000);
        // Not the marker.
        assert!(parse_async_first_line(b"hello\n", def).is_none());
        assert!(parse_async_first_line(b"{\"async\":false}\n", def).is_none());
        assert!(parse_async_first_line(b"", def).is_none());
        // Marker without timeout → default; with 0 → default; with N → N.
        assert_eq!(
            parse_async_first_line(b"{\"async\":true}\n", def),
            Some(def)
        );
        assert_eq!(
            parse_async_first_line(b"{\"async\":true,\"asyncTimeout\":0}", def),
            Some(def)
        );
        assert_eq!(
            parse_async_first_line(b"{\"async\":true,\"asyncTimeout\":250}\n", def),
            Some(Duration::from_millis(250))
        );
    }

    #[tokio::test]
    async fn normal_hook_output_is_reconstructed_exactly() {
        // A multi-line, non-marker hook: stdout/stderr/exit must match `run`.
        let cmd = sh("printf 'line1\\nline2\\n'; printf 'err1\\n' 1>&2; exit 3");
        let outcome = PosixProcess::new()
            .run_hook_with_async_detection(&cmd, Duration::from_millis(15_000))
            .await
            .expect("runs");
        match outcome {
            HookRunOutcome::Completed(o) => {
                assert_eq!(o.stdout, "line1\nline2\n");
                assert_eq!(o.stderr, "err1\n");
                assert_eq!(o.exit_code, 3);
            }
            HookRunOutcome::Backgrounded { .. } => panic!("normal hook must not background"),
        }
    }

    #[tokio::test]
    async fn empty_output_hook_completes() {
        let cmd = sh("exit 0");
        let outcome = PosixProcess::new()
            .run_hook_with_async_detection(&cmd, Duration::from_millis(15_000))
            .await
            .expect("runs");
        match outcome {
            HookRunOutcome::Completed(o) => {
                assert_eq!(o.stdout, "");
                assert_eq!(o.exit_code, 0);
            }
            HookRunOutcome::Backgrounded { .. } => panic!("empty hook must not background"),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn hook_closing_stdin_early_preserves_output_and_exit_status() {
        for owned in [false, true] {
            for exit_code in [0, 7] {
                let base = sh(&format!(
                    "exec 0<&-; printf 'hook-output\\n'; printf 'hook-error\\n' >&2; exit {exit_code}"
                ));
                let mut input = base.inner().clone();
                // Larger than a pipe buffer: write_all must observe the closed
                // read end even when it runs before the child closes stdin.
                input.stdin = Some("x".repeat(8 * 1024 * 1024));
                let mut command = SandboxedCommand::__new_sandboxed(input, base.tag().clone());
                if owned {
                    command = command
                        .with_lifetime_guard(Arc::new(()))
                        .with_require_group_cleanup_on_exit(true);
                }
                let outcome = tokio::time::timeout(
                    Duration::from_secs(20),
                    PosixProcess::new()
                        .run_hook_with_async_detection(&command, Duration::from_secs(5)),
                )
                .await
                .expect("hook completes and owned group cleanup is verified")
                .expect("closed stdin does not replace the hook result");
                let HookRunOutcome::Completed(output) = outcome else {
                    panic!("hook must return its actual process result");
                };
                assert_eq!(output.stdout, "hook-output\n", "owned={owned}");
                assert_eq!(output.stderr, "hook-error\n", "owned={owned}");
                assert_eq!(output.exit_code, exit_code, "owned={owned}");
                assert!(!output.timed_out, "owned={owned}");
            }
        }
    }

    #[tokio::test]
    async fn hook_stdin_other_write_errors_are_not_ignored() {
        struct DeniedWriter;
        impl tokio::io::AsyncWrite for DeniedWriter {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "fixture write failure",
                )))
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        let error = write_hook_stdin(&mut DeniedWriter, b"event")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "fixture write failure");
    }

    #[tokio::test]
    async fn async_marker_backgrounds_without_blocking() {
        // Prints the marker, then sleeps far longer than the async timeout. The
        // call must return Backgrounded immediately (well under the sleep).
        let cmd = sh("echo '{\"async\":true,\"asyncTimeout\":100}'; sleep 30");
        let start = tokio::time::Instant::now();
        let outcome = PosixProcess::new()
            .run_hook_with_async_detection(&cmd, Duration::from_millis(15_000))
            .await
            .expect("runs");
        let HookRunOutcome::Backgrounded {
            async_timeout,
            output: Some(_output_rx),
        } = outcome
        else {
            panic!("async hook must background with an eventual-output handle");
        };
        // The marker's `asyncTimeout` (100ms) overrides the caller default.
        assert_eq!(async_timeout, Duration::from_millis(100));
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "async hook must not block the turn (took {:?})",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn async_marker_retains_eventual_stdout_and_stderr() {
        // The post-marker stdout/stderr + exit are drained and delivered once
        // through the eventual-output channel (the fold-back payload).
        // asyncTimeout is generous ON PURPOSE. This test asserts the FOLD-BACK
        // PAYLOAD, not the timeout, and the marker's timeout is a hard kill
        // deadline for the backgrounded process. At 1000ms it raced the
        // scheduler: under a saturated run three `echo`s can exceed a second of
        // wall clock, the hook was terminated, and the test failed with an
        // empty stdout and `timed_out: true`. A test that asserts on output
        // must not also be a wall-clock benchmark.
        let cmd =
            sh("echo '{\"async\":true,\"asyncTimeout\":60000}'; echo after; echo err 1>&2; exit 0");
        let outcome = PosixProcess::new()
            .run_hook_with_async_detection(&cmd, Duration::from_millis(15_000))
            .await
            .expect("runs");
        let HookRunOutcome::Backgrounded {
            output: Some(output_rx),
            ..
        } = outcome
        else {
            panic!("async hook must background with an eventual-output handle");
        };

        let out = tokio::time::timeout(Duration::from_secs(60), output_rx)
            .await
            .expect("eventual output must arrive before the async timeout")
            .expect("sender must not drop");
        // The consumed marker line is NOT part of the eventual output; only the
        // post-marker content is.
        assert_eq!(out.stdout, "after\n", "eventual stdout: {out:?}");
        assert_eq!(out.stderr, "err\n", "eventual stderr: {out:?}");
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn recovery_marker_retains_work_without_output_receiver_until_group_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("descendant.pid");
        let lease = Arc::new(());
        let weak = Arc::downgrade(&lease);
        let command = sh(&format!(
            "echo '{{\"async\":true,\"asyncTimeout\":5000}}'; sleep 30 & echo $! > '{}'; wait",
            pid_path.display()
        ))
        .with_lifetime_guard(lease.clone())
        .with_require_group_cleanup_on_exit(true);
        let outcome = PosixProcess::new()
            .run_hook_with_async_detection(&command, Duration::from_secs(3))
            .await
            .expect("owned marker hook starts");
        let HookRunOutcome::Backgrounded {
            output: Some(receiver),
            ..
        } = outcome
        else {
            panic!("hook backgrounds");
        };
        drop(receiver); // no registry or a failed registration
        drop(command);
        drop(lease);
        let descendant = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_path) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        if pid > 0 {
                            break pid.to_string();
                        }
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("descendant writes a valid positive PID");
        let initially_alive = std::process::Command::new("/bin/kill")
            .args(["-0", &descendant])
            .stderr(Stdio::null())
            .status()
            .expect("query running descendant")
            .success();
        assert!(
            initially_alive,
            "fixture descendant must start before cleanup"
        );
        assert!(
            weak.upgrade().is_some(),
            "physical owner must retain work without a receiver"
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            while weak.upgrade().is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("lease released only after timeout and group verification");
        let alive = std::process::Command::new("/bin/kill")
            .args(["-0", &descendant])
            .stderr(Stdio::null())
            .status()
            .expect("query descendant")
            .success();
        assert!(
            !alive,
            "same-group descendant must exit before lease release"
        );
    }

    #[tokio::test]
    async fn recovery_marker_cancellation_waits_for_owned_group_before_completion() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let lease = Arc::new(());
        let weak = Arc::downgrade(&lease);
        let command = sh("echo '{\"async\":true}'; sleep 30 & wait")
            .with_lifetime_guard(lease.clone())
            .with_require_group_cleanup_on_exit(true);
        let outcome = PosixProcess::new()
            .run_hook_with_async_detection_observed_cancellable(
                &command,
                Duration::from_secs(30),
                None,
                Some(cancellation.clone()),
            )
            .await
            .expect("owned marker hook starts");
        let HookRunOutcome::Backgrounded {
            output: Some(receiver),
            ..
        } = outcome
        else {
            panic!("hook backgrounds");
        };
        drop(command);
        drop(lease);
        assert!(weak.upgrade().is_some());
        cancellation.cancel();
        let output = tokio::time::timeout(Duration::from_secs(15), receiver)
            .await
            .expect("verified cancellation")
            .expect("completion delivered");
        assert!(output.stderr.contains("cancelled"));
        tokio::time::timeout(Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("work released after physical cleanup");
    }
    #[tokio::test]
    async fn cancelling_idle_hook_reaps_its_owned_process_group() {
        let owner = format!("idle-hook-{}", std::process::id());
        let cancellation = tokio_util::sync::CancellationToken::new();
        let observer = Arc::new(HookPidObserver {
            bytes: std::sync::Mutex::new(Vec::new()),
            ready: tokio::sync::Notify::new(),
        });
        let command = sh("sleep 30 & echo $!; wait").with_process_owner(Some(owner));
        let task = tokio::spawn({
            let cancellation = cancellation.clone();
            let observer: Arc<dyn lingxi_core::host::HookOutputObserver> = observer.clone();
            async move {
                PosixProcess::new()
                    .run_hook_with_async_detection_observed_cancellable(
                        &command,
                        Duration::from_secs(10),
                        Some(observer),
                        Some(cancellation),
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), observer.ready.notified())
            .await
            .expect("hook emits the descendant PID before blocking");
        let child_pid = String::from_utf8(observer.bytes.lock().unwrap().clone())
            .expect("PID output is UTF-8")
            .trim()
            .to_string();
        assert!(!child_pid.is_empty(), "hook emitted a descendant PID");

        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("cancellation kills and reaps the hook group")
            .expect("hook runner task does not panic");
        assert!(
            matches!(result, Err(ProcessError::Io(message)) if message == "hook execution cancelled")
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut alive = true;
        while std::time::Instant::now() < deadline {
            alive = std::process::Command::new("/bin/kill")
                .args(["-0", &child_pid])
                .status()
                .expect("kill -0 runs")
                .success();
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(!alive, "idle hook descendant {child_pid} must not survive");
    }
}

#[cfg(test)]
mod hook_env_tests {
    use super::*;
    use lingxi_core::host::ProcessCommand;
    use std::collections::HashMap;

    /// A sentinel pre-seeded into the caller env for `AI_AGENT` / `GIT_EDITOR`.
    /// The runner OVERWRITES these for a non-hook (Bash-spawn) command but
    /// LEAVES them for a hook command — so the child observing the sentinel vs.
    /// the runner's value tells us exactly which path ran, independently of the
    /// (claude-code-spawned) test process's own inherited env.
    const SENTINEL: &str = "__sentinel_caller_value__";

    /// Spawn `/usr/bin/env` through the runner with the given audit `reason`,
    /// pre-seeding `AI_AGENT` / `GIT_EDITOR` with [`SENTINEL`]. Returns the
    /// child's env dump.
    async fn run_env_dump(reason: &str) -> String {
        let mut env = HashMap::new();
        env.insert("AI_AGENT".to_string(), SENTINEL.to_string());
        env.insert("GIT_EDITOR".to_string(), SENTINEL.to_string());
        let pcmd = ProcessCommand {
            command: "/usr/bin/env".to_string(),
            args: vec![],
            cwd: None,
            env,
            timeout: None,
            stdin: None,
        };
        let cmd = SandboxedCommand::__new_sandboxed(
            pcmd,
            SandboxedTag::BypassAuditedWithReason {
                reason: reason.to_string(),
            },
        );
        let out = PosixProcess::new().run(&cmd).await.expect("env runs");
        out.stdout
    }

    /// A hook command (`source:"harness"`) gets neither `AI_AGENT` nor
    /// `GIT_EDITOR` from the runner — matching claude-code's `{...WO(),
    /// ...Uot(harness), LINGXI_PROJECT_DIR}` hook env, which carries neither.
    /// The pre-seeded sentinel therefore survives untouched.
    #[tokio::test]
    async fn hook_command_omits_ai_agent_and_git_editor() {
        let dump = run_env_dump(HOOK_COMMAND_AUDIT_REASON).await;
        let lines: Vec<&str> = dump.lines().collect();
        assert!(
            lines.iter().any(|l| *l == format!("AI_AGENT={SENTINEL}")),
            "runner must NOT overwrite AI_AGENT on a hook child; env was:\n{dump}",
        );
        assert!(
            lines.iter().any(|l| *l == format!("GIT_EDITOR={SENTINEL}")),
            "runner must NOT overwrite GIT_EDITOR on a hook child; env was:\n{dump}",
        );
        // The always-present `Uot` markers are still set on a hook child.
        assert!(lines.contains(&"LINGXI=1"));
        assert!(lines.contains(&"LINGXI_CHILD_SESSION=1"));
    }

    /// A non-hook command (Bash/REPL/PowerShell tool call, `source:"agent"`)
    /// keeps the full Bash-spawn contract: the runner OVERWRITES the sentinel
    /// with `AI_AGENT=<Mer("agent")>` + `GIT_EDITOR=true`.
    #[tokio::test]
    async fn tool_command_overwrites_ai_agent_and_git_editor() {
        let dump = run_env_dump("bash_tool_call").await;
        let lines: Vec<&str> = dump.lines().collect();
        assert!(
            !lines.iter().any(|l| *l == format!("AI_AGENT={SENTINEL}")),
            "runner overwrites AI_AGENT on a non-hook child; env was:\n{dump}",
        );
        assert!(
            lines
                .iter()
                .any(|l| *l == format!("AI_AGENT={}", ai_agent_value())),
            "non-hook child carries the runner's AI_AGENT value; env was:\n{dump}",
        );
        assert!(
            lines.contains(&"GIT_EDITOR=true"),
            "non-hook child carries GIT_EDITOR=true; env was:\n{dump}",
        );
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use lingxi_core::host::ProcessCommand;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::Notify;

    #[derive(Default)]
    struct RecordingSink {
        lines: Mutex<Vec<String>>,
        stderr: Mutex<Vec<u8>>,
        line_ready: Notify,
    }

    #[async_trait]
    impl ProcessStreamSink for RecordingSink {
        async fn stdout_line(&self, line: String) -> Result<(), ProcessError> {
            self.lines.lock().expect("lines lock").push(line);
            self.line_ready.notify_one();
            Ok(())
        }

        async fn stderr_chunk(&self, chunk: Vec<u8>) -> Result<(), ProcessError> {
            self.stderr.lock().expect("stderr lock").extend(chunk);
            Ok(())
        }
    }

    fn stream_sh(script: &str, timeout: Duration) -> SandboxedCommand {
        SandboxedCommand::__new_sandboxed(
            ProcessCommand {
                command: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), script.to_string()],
                cwd: None,
                env: HashMap::new(),
                timeout: Some(timeout),
                stdin: None,
            },
            SandboxedTag::BypassAuditedWithReason {
                reason: "monitor_task_test".to_string(),
            },
        )
    }

    #[tokio::test]
    async fn owned_streaming_holds_leader_until_descendant_is_stopped() {
        let sink = Arc::new(RecordingSink::default());
        let owner = format!(
            "{}{}",
            lingxi_core::host::process::OWNED_STREAMING_OWNER_PREFIX,
            generate_task_id().unwrap()
        );
        let command = stream_sh(
            "sleep 30 & printf '%s\\n' \"$!\"; exit 0",
            Duration::from_secs(60),
        )
        .with_process_owner(Some(owner.clone()));
        let process = Arc::new(PosixProcess::new());
        let runner = process.clone();
        let observer = sink.clone();
        let task = tokio::spawn(async move { runner.run_streaming(&command, observer).await });
        tokio::time::timeout(Duration::from_secs(10), sink.line_ready.notified())
            .await
            .expect("descendant pid is streamed");
        let leader = lingxi_core::host::agent_processes::snapshot_entries(&owner)
            .first()
            .expect("owner registration")
            .pid;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = tokio::process::Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", &leader.to_string()])
                    .output()
                    .await
                    .unwrap();
                if String::from_utf8_lossy(&status.stdout).contains('Z') {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("exited leader must remain unreaped while descendant holds pipes");
        assert!(!task.is_finished());
        process.kill_owner_processes(&owner).await;
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("owner stop must finish")
            .unwrap()
            .expect("held leader is waited after pipe EOF");
        assert_eq!(result.exit_code, 0);
        assert!(
            lingxi_core::host::live_sessions::process_start_identity(leader).is_none(),
            "leader must be reaped before completion"
        );
    }

    #[tokio::test]
    async fn owned_streaming_sink_failure_is_not_reported_as_verified() {
        struct FailedSink;
        #[async_trait]
        impl ProcessStreamSink for FailedSink {
            async fn stdout_line(&self, _: String) -> Result<(), ProcessError> {
                Err(ProcessError::Io("output consumer failed".into()))
            }

            async fn stderr_chunk(&self, _: Vec<u8>) -> Result<(), ProcessError> {
                Ok(())
            }
        }
        let owner = format!(
            "{}{}",
            lingxi_core::host::process::OWNED_STREAMING_OWNER_PREFIX,
            generate_task_id().unwrap()
        );
        let command = stream_sh("printf 'ready\\n'; sleep 30", Duration::from_secs(60))
            .with_process_owner(Some(owner.clone()));
        let error = tokio::time::timeout(
            Duration::from_secs(15),
            PosixProcess::new().run_streaming(&command, Arc::new(FailedSink)),
        )
        .await
        .expect("failure cleanup must be bounded")
        .unwrap_err();
        assert!(matches!(
            error,
            ProcessError::Io(message)
                if message.starts_with(lingxi_core::host::process::OWNED_STREAMING_CLEANUP_UNVERIFIED)
        ));
        assert!(
            lingxi_core::host::agent_processes::snapshot_entries(&owner).is_empty(),
            "the failed operation must release its owner registration"
        );
    }

    #[tokio::test]
    async fn owned_streaming_cleans_up_redirected_group_children_before_return() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child");
        let release = dir.path().join("release");
        let pid_path = pid_file.display().to_string().replace('\'', "'\\''");
        let release_path = release.display().to_string().replace('\'', "'\\''");
        let script = format!("sleep 60 </dev/null >/dev/null 2>&1 & printf '%s' \"$!\" > '{pid_path}'; while [ ! -f '{release_path}' ]; do sleep 0.02; done; exit 0");
        let owner = format!(
            "{}{}",
            lingxi_core::host::process::OWNED_STREAMING_OWNER_PREFIX,
            generate_task_id().unwrap()
        );
        let command = stream_sh(&script, Duration::from_secs(20)).with_process_owner(Some(owner));
        let task = tokio::spawn(async move {
            PosixProcess::new()
                .run_streaming(&command, Arc::new(RecordingSink::default()))
                .await
        });
        let pid: i32 = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = text.parse() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok());
        let process_id = u32::try_from(pid).expect("child PID must be positive");
        let start = lingxi_core::host::live_sessions::process_start_identity(process_id)
            .expect("child start identity must be observable");
        std::fs::write(release, "go").unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.exit_code, 0);
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
            Err(nix::errno::Errno::ESRCH) => {}
            Ok(()) | Err(nix::errno::Errno::EPERM) => {
                let current = lingxi_core::host::live_sessions::process_start_identity(process_id)
                    .expect("a live PID must have an observable start identity");
                assert_ne!(
                    current, start,
                    "the original redirected descendant survived verified completion"
                );
            }
            Err(error) => panic!("could not inspect descendant PID: {error}"),
        }
    }

    #[tokio::test]
    async fn owned_streaming_drop_signals_redirected_group_children() {
        let sink = Arc::new(RecordingSink::default());
        let owner = format!(
            "{}{}",
            lingxi_core::host::process::OWNED_STREAMING_OWNER_PREFIX,
            generate_task_id().unwrap()
        );
        let command = stream_sh(
            "sleep 60 </dev/null >/dev/null 2>&1 & printf '%s\\n' \"$!\"; wait",
            Duration::from_secs(90),
        )
        .with_process_owner(Some(owner));
        let observer = sink.clone();
        let task =
            tokio::spawn(
                async move { PosixProcess::new().run_streaming(&command, observer).await },
            );
        tokio::time::timeout(Duration::from_secs(10), sink.line_ready.notified())
            .await
            .expect("child PID must be streamed");
        let pid: i32 = sink.lines.lock().unwrap()[0].parse().unwrap();
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = tokio::process::Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", &pid.to_string()])
                    .output()
                    .await
                    .unwrap();
                let state = String::from_utf8_lossy(&status.stdout);
                if !status.status.success() || state.trim().is_empty() || state.contains('Z') {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("dropping the worker must signal the redirected child");
    }

    #[tokio::test]
    async fn streaming_delivers_a_line_before_process_exit() {
        let sink = Arc::new(RecordingSink::default());
        // The child BLOCKS until this test releases it, so `!task.is_finished()`
        // is not a race at all: the process physically cannot have exited while
        // the gate file is absent.
        //
        // This used to be `sleep 0.4`, then `sleep 3`, on the theory that a wide
        // enough window beats scheduler jitter. It does not — the assertion is
        // still "did the child outlive my thread being descheduled", and under
        // full-suite load the answer is occasionally no. Widening a timing window
        // makes a race rarer and slower, never absent. A gate the test owns
        // removes the timing question instead of enlarging it.
        let gate_dir = tempfile::tempdir().expect("gate dir");
        let gate = gate_dir.path().join("release");
        let command = stream_sh(
            &format!(
                "printf 'ready\\n'; while [ ! -f '{}' ]; do sleep 0.01; done; printf 'done\\n'",
                gate.display()
            ),
            Duration::from_secs(60),
        );
        let task_sink: Arc<dyn ProcessStreamSink> = sink.clone();
        let task =
            tokio::spawn(
                async move { PosixProcess::new().run_streaming(&command, task_sink).await },
            );

        tokio::time::timeout(Duration::from_secs(30), sink.line_ready.notified())
            .await
            .expect("first line is delivered live");
        // NOTE which assertion is doing the work. Reaching this line at all is
        // the proof: the child cannot exit before the gate below, so a runner
        // that buffered until exit would never notify, and the timeout above
        // would fire. Verified by planting exactly that — a child that prints
        // nothing until released fails with `first line is delivered live`.
        //
        // The check below is then nearly structural. It is kept as a tripwire on
        // the GATE rather than on streaming: if the gate ever stopped holding the
        // child, this is what would notice.
        assert!(
            !task.is_finished(),
            "streaming must not wait for process exit"
        );
        // Only now can the child finish.
        std::fs::write(&gate, b"go").expect("release the child");
        let output = task
            .await
            .expect("runner task joins")
            .expect("command succeeds");
        assert_eq!(output.stdout, "ready\ndone\n");
        assert_eq!(
            sink.lines.lock().expect("lines lock").as_slice(),
            ["ready", "done"]
        );
    }

    #[tokio::test]
    async fn streaming_drains_large_stderr_without_deadlock() {
        let sink = Arc::new(RecordingSink::default());
        let command = stream_sh(
            "dd if=/dev/zero bs=1024 count=256 1>&2 2>/dev/null; printf 'ok\\n'",
            Duration::from_secs(5),
        );
        let output = PosixProcess::new()
            .run_streaming(&command, sink.clone())
            .await
            .expect("large stderr is drained concurrently");
        assert_eq!(output.stdout, "ok\n");
        assert_eq!(output.stderr.len(), 256 * 1024);
        assert_eq!(sink.stderr.lock().expect("stderr lock").len(), 256 * 1024);
    }

    /// claude-code `I_t` starts by calling `t.background(e)` on the LIVE
    /// `shellCommand`, which is how Ctrl+B and background-all reach a command
    /// that is still running in the foreground. The request lands on the same
    /// footing as the deadline: same move-to-background path, same task id,
    /// same output file.
    #[tokio::test]
    async fn an_on_demand_request_moves_a_running_foreground_child_to_the_background() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output_path = dir.path().join("armed.output");
        std::fs::write(&output_path, b"").expect("seed the bound output file");
        let notify = Arc::new(tokio::sync::Notify::new());

        // A 60 s timeout that will NOT fire: only the request can end this wait,
        // so a pass cannot be the deadline in disguise.
        let command = stream_sh("echo started; sleep 60", Duration::from_secs(60))
            .with_background_task(lingxi_core::host::BackgroundTaskBinding {
                task_id: "btestid01".to_string(),
                output_path: output_path.clone(),
                on_exit: None,
                on_demand: Some(notify.clone()),
            });

        let requester = notify.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            requester.notify_one();
        });

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            PosixProcess::new().run_foreground_with_output_limit(&command, None),
        )
        .await
        .expect("the request must end the foreground wait well before the deadline")
        .expect("the run itself must not error");

        let handle = match result.outcome {
            lingxi_core::host::ForegroundOutcome::MovedToBackground(handle) => handle,
            other => panic!("expected MovedToBackground, got {other:?}"),
        };
        // The BOUND identity is reported, not a runner-minted one: an on-demand
        // background must be addressable by the row that asked for it.
        assert_eq!(handle.task_id, "btestid01");

        // And the child really is still running, still writing to the bound
        // file — otherwise this would be a kill wearing a background's clothes.
        for _ in 0..40 {
            if std::fs::read(&output_path).is_ok_and(|b| b.windows(7).any(|w| w == b"started")) {
                let _ = kill_tree_force(handle.pid);
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = kill_tree_force(handle.pid);
        panic!(
            "the detached child never reached the bound output file: {:?}",
            std::fs::read_to_string(&output_path)
        );
    }

    #[tokio::test]
    async fn streaming_timeout_kills_the_command() {
        let sink: Arc<dyn ProcessStreamSink> = Arc::new(RecordingSink::default());
        let command = stream_sh("sleep 30", Duration::from_millis(50));
        let error = PosixProcess::new()
            .run_streaming(&command, sink)
            .await
            .expect_err("deadline must stop the command");
        assert!(matches!(error, ProcessError::Timeout));
    }

    #[tokio::test]
    async fn dropping_streaming_future_kills_process_group() {
        let sink = Arc::new(RecordingSink::default());
        let command = stream_sh("echo $$; sleep 30", Duration::from_secs(60));
        let task_sink: Arc<dyn ProcessStreamSink> = sink.clone();
        let task =
            tokio::spawn(
                async move { PosixProcess::new().run_streaming(&command, task_sink).await },
            );
        tokio::time::timeout(Duration::from_secs(2), sink.line_ready.notified())
            .await
            .expect("shell pid is emitted");
        let pid = sink.lines.lock().expect("lines lock")[0].clone();

        task.abort();
        let _ = task.await;
        // Poll until the shell is reaped rather than sleeping a fixed 100ms and
        // asserting once. Reaping is the kernel's schedule, not ours: a single
        // fixed wait turns "slower than usual" into "leaked a process", which is
        // a false failure under load. The deadline still fails a real leak — a
        // surviving `sleep 30` is alive for every one of these polls.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut alive = true;
        while std::time::Instant::now() < deadline {
            alive = std::process::Command::new("/bin/kill")
                .args(["-0", &pid])
                .status()
                .expect("kill -0 runs")
                .success();
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(!alive, "cancelled streaming shell {pid} must not survive");
    }
}
