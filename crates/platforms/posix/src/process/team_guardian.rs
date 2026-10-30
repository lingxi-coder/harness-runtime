//! Private Team shell lifetime, independent of the model worker's process.
//!
//! A fresh trusted guardian owns one sandboxed shell group. Only the worker
//! holds its stdin writer; worker death/future cancellation closes that lease.
//! The guardian has no model credential, owner environment or ambient supervisor.

use lingxi_core::host::{ProcessCommand, SandboxedCommand};
use mobile_linux_api::{ProcessError, ProcessOutput};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const ROLE: &str = "--team-shell-guardian";
const MAX_BOOTSTRAP: usize = 1024 * 1024;
const MAX_OUTPUT: usize = 512 * 1024;
const MAX_RESULT: usize = 4 * 1024 * 1024;

struct Config {
    executable: PathBuf,
    scope_root: PathBuf,
    deadline_unix_ms: u64,
    cancel: CancellationToken,
}
static CONFIG: OnceLock<Arc<Config>> = OnceLock::new();

fn error(message: impl ToString) -> ProcessError {
    ProcessError::Io(message.to_string())
}
fn now_ms() -> Result<u64, ProcessError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(error)?
        .as_millis()
        .try_into()
        .map_err(error)
}
fn remaining(deadline: u64) -> Result<Duration, ProcessError> {
    let millis = deadline
        .checked_sub(now_ms()?)
        .ok_or(ProcessError::Timeout)?;
    if millis == 0 || millis > 24 * 60 * 60 * 1000 {
        return Err(ProcessError::Timeout);
    }
    Ok(Duration::from_millis(millis))
}

/// Enable only in a fresh authenticated Team worker, after its scope validation.
/// This in-process capability is never decoded from tool input or settings.
pub fn enable_team_guardian(
    executable: PathBuf,
    scope_root: PathBuf,
    deadline_unix_ms: u64,
    cancel: CancellationToken,
) -> Result<(), ProcessError> {
    if !cfg!(target_os = "macos")
        || !executable.is_absolute()
        || std::fs::canonicalize(&executable).map_err(error)? != executable
        || !scope_root.is_absolute()
        || std::fs::canonicalize(&scope_root).map_err(error)? != scope_root
        || std::env::current_dir().map_err(error)? != scope_root.join("workspace")
    {
        return Err(ProcessError::Unsupported);
    }
    remaining(deadline_unix_ms)?;
    CONFIG
        .set(Arc::new(Config {
            executable,
            scope_root,
            deadline_unix_ms,
            cancel,
        }))
        .map_err(|_| error("Team guardian already configured"))
}

/// Whether the fresh helper binary was invoked in its private guardian role.
pub fn is_team_guardian_invocation() -> bool {
    let args: Vec<_> = std::env::args_os().collect();
    args.len() == 2 && args[1] == ROLE
}

fn environment(root: &Path) -> HashMap<String, String> {
    HashMap::from([
        ("HOME".into(), root.join("home").to_string_lossy().into()),
        (
            "LINGXI_CONFIG_DIR".into(),
            root.join("home").to_string_lossy().into(),
        ),
        ("TMPDIR".into(), root.join("tmp").to_string_lossy().into()),
        ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
        ("LANG".into(), "C".into()),
        ("LC_ALL".into(), "C".into()),
        (
            "CFFIXED_USER_HOME".into(),
            root.join("home").to_string_lossy().into(),
        ),
        (
            "__CF_USER_TEXT_ENCODING".into(),
            format!("0x{:X}:0x8000100:0x0", super::spawn_unsafe::effective_uid()),
        ),
    ])
}

/// Must run at the single-threaded binary entry point, before constructing Tokio.
pub fn prepare_team_guardian_entry() -> Result<(), ProcessError> {
    if !cfg!(target_os = "macos")
        || !is_team_guardian_invocation()
        || std::io::stdin().is_terminal()
        || std::io::stdout().is_terminal()
    {
        return Err(ProcessError::Unsupported);
    }
    let allowed = environment(Path::new("/unused"));
    if std::env::vars_os().count() != allowed.len()
        || std::env::vars_os().any(|(key, _)| !allowed.keys().any(|name| key == name.as_str()))
    {
        return Err(error("Invalid Team guardian environment"));
    }
    // Close inherited descriptors before any runtime threads exist. In
    // particular there can be no second copy of the worker's lease writer.
    let descriptors = std::fs::read_dir("/dev/fd")
        .map_err(error)?
        .map(|entry| {
            entry
                .map_err(error)?
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
                .ok_or_else(|| error("Invalid inherited descriptor"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for descriptor in descriptors.into_iter().filter(|fd| *fd > 2) {
        match nix::unistd::close(descriptor) {
            Ok(()) | Err(nix::errno::Errno::EBADF) => {}
            Err(reason) => return Err(error(reason)),
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bootstrap {
    version: u32,
    scope_root: PathBuf,
    deadline_unix_ms: u64,
    output_limit: usize,
    command: ProcessCommand,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultFrame {
    output: ProcessOutput,
    cleanup_complete: bool,
}

/// Once a guardian is admitted, failure to prove cleanup revokes the entire
/// invocation. A recoverable Bash tool error must not hide uncertain lifetime.
struct InvocationFailureGuard {
    cancel: CancellationToken,
    accepted: bool,
}
impl InvocationFailureGuard {
    fn new(cancel: CancellationToken) -> Self {
        Self {
            cancel,
            accepted: false,
        }
    }

    fn finish(
        mut self,
        result: Result<ProcessOutput, ProcessError>,
    ) -> Result<ProcessOutput, ProcessError> {
        if result.is_ok() {
            // Includes ordinary nonzero Bash exit codes, after proven cleanup.
            self.accepted = true;
        } else {
            // Cancel before the error can be converted into model-facing text.
            self.cancel.cancel();
        }
        result
    }
}
impl Drop for InvocationFailureGuard {
    fn drop(&mut self) {
        if !self.accepted {
            // Also covers missing pipes, panic and cancellation of this future.
            self.cancel.cancel();
        }
    }
}

fn decode_result(success: bool, bytes: &[u8]) -> Result<ProcessOutput, ProcessError> {
    if !success {
        return Err(error("Team shell cleanup_incomplete or guardian failure"));
    }
    let frame: ResultFrame = serde_json::from_slice(bytes).map_err(error)?;
    if !frame.cleanup_complete {
        return Err(error("Team shell cleanup_incomplete"));
    }
    Ok(frame.output)
}

pub(super) fn enabled() -> bool {
    // The flag exists only in the dedicated Team worker. Unexpected internal
    // process calls must fail validation, never use the ordinary launcher.
    CONFIG.get().is_some()
}

/// Cancellation drops only the lease writer. Killing this helper would remove
/// the independent owner needed to clean up a SIGKILLed worker's Bash group.
pub(super) async fn execute(
    command: &SandboxedCommand,
    output_limit: Option<usize>,
) -> Result<ProcessOutput, ProcessError> {
    let config = CONFIG.get().ok_or(ProcessError::Unsupported)?.clone();
    if !command.clear_environment() {
        return Err(error("Team process must clear its inherited environment"));
    }
    let timeout = remaining(config.deadline_unix_ms)?;
    if config.cancel.is_cancelled() {
        return Err(error("Team shell cancelled"));
    }
    let bootstrap = Bootstrap {
        version: 1,
        scope_root: config.scope_root.clone(),
        deadline_unix_ms: config.deadline_unix_ms,
        output_limit: output_limit.unwrap_or(MAX_OUTPUT).min(MAX_OUTPUT),
        command: command.inner().clone(),
    };
    validate(&bootstrap)?;
    let bytes = serde_json::to_vec(&bootstrap).map_err(error)?;
    if bytes.len() > MAX_BOOTSTRAP {
        return Err(error("Team shell bootstrap too large"));
    }
    let mut process = tokio::process::Command::new(&config.executable);
    process
        .arg(ROLE)
        .env_clear()
        .envs(environment(&config.scope_root))
        .current_dir(config.scope_root.join("workspace"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    super::spawn_unsafe::attach_setsid(&mut process);
    let mut child = process.spawn().map_err(error)?;
    let invocation = InvocationFailureGuard::new(config.cancel.clone());
    let mut lease = child
        .stdin
        .take()
        .ok_or_else(|| error("Missing guardian lease"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| error("Missing guardian output"))?;
    let exchange = async {
        lease
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .map_err(error)?;
        lease.write_all(&bytes).await.map_err(error)?;
        lease.flush().await.map_err(error)?;
        let mut bytes = Vec::new();
        output
            .take((MAX_RESULT + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(error)?;
        if bytes.len() > MAX_RESULT {
            return Err(error("Team guardian result too large"));
        }
        let status = child.wait().await.map_err(error)?;
        decode_result(status.success(), &bytes)
    };
    let result = tokio::select! {
        biased;
        _ = config.cancel.cancelled() => Err(error("Team shell cancelled")),
        _ = tokio::time::sleep(timeout) => Err(ProcessError::Timeout),
        result = exchange => result,
    };
    // `lease` is deliberately held until this scope ends, including when the
    // execution future is dropped. Only the worker ever holds a writer.
    invocation.finish(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_guardian_exit_and_malformed_response_revoke_invocation() {
        // Real short-lived fault processes exercise the same exit/decoder
        // boundary as production without global worker configuration.
        for command in ["exit 70", "printf '{'"] {
            let cancel = CancellationToken::new();
            let guard = InvocationFailureGuard::new(cancel.clone());
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", command])
                .output()
                .unwrap();
            let result = guard.finish(decode_result(output.status.success(), &output.stdout));
            assert!(result.is_err());
            assert!(
                cancel.is_cancelled(),
                "failure must revoke before returning"
            );
        }
    }

    #[test]
    fn unproven_cleanup_revokes_but_cleaned_nonzero_tool_exit_does_not() {
        for cleanup_complete in [false, true] {
            let cancel = CancellationToken::new();
            let guard = InvocationFailureGuard::new(cancel.clone());
            let bytes = serde_json::to_vec(&ResultFrame {
                cleanup_complete,
                output: ProcessOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 7,
                    timed_out: false,
                },
            })
            .unwrap();
            let result = guard.finish(decode_result(true, &bytes));
            assert_eq!(cancel.is_cancelled(), !cleanup_complete);
            if cleanup_complete {
                assert_eq!(result.unwrap().exit_code, 7);
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[test]
    fn dropped_admitted_exchange_revokes_invocation() {
        let cancel = CancellationToken::new();
        drop(InvocationFailureGuard::new(cancel.clone()));
        assert!(cancel.is_cancelled());
    }
}

fn validate(bootstrap: &Bootstrap) -> Result<(), ProcessError> {
    let root = &bootstrap.scope_root;
    let command = &bootstrap.command;
    let expected_env = HashMap::from([
        (
            "HOME".into(),
            root.join("shell-home").to_string_lossy().into_owned(),
        ),
        (
            "TMPDIR".into(),
            root.join("tmp").to_string_lossy().into_owned(),
        ),
        ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
        ("LANG".into(), "C".into()),
        ("LC_ALL".into(), "C".into()),
    ]);
    if bootstrap.version != 1
        || bootstrap.output_limit > MAX_OUTPUT
        || !root.is_absolute()
        || std::fs::canonicalize(root).map_err(error)? != *root
        || command.cwd.as_deref() != Some(root.join("workspace").as_path())
        || command.command != "/bin/bash"
        || command.args.len() != 4
        || command.args[..3] != ["--noprofile", "--norc", "-c"]
        || !command.args[3].starts_with("exec /usr/bin/sandbox-exec -p '")
        || !command.args[3]
            .contains("(deny syscall-unix (syscall-number SYS_setsid SYS_setpgid SYS_posix_spawn))")
        || command.env != expected_env
        || command.stdin.is_some()
    {
        return Err(error("Invalid private Team shell capability"));
    }
    remaining(bootstrap.deadline_unix_ms)?;
    Ok(())
}

struct GroupGuard(Option<u32>);
impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = super::kill_tree::kill_tree_force(pid);
        }
    }
}

async fn cleanup_group(pid: u32) -> Result<(), ProcessError> {
    // The unreaped direct child pins this PID/PGID for every observation and
    // signal. Membership is kernel-locked by the private syscall policy; this
    // is group verification, never a polling reconstruction of a process tree.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        super::kill_tree::kill_tree_force(pid)?;
        if !super::spawn_unsafe::team_group_has_live_members(pid).map_err(error)? {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(error("Team shell cleanup_incomplete"));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Serve exactly one bounded bootstrap and one worker-only stdin lease.
pub async fn run_team_guardian() -> Result<(), ProcessError> {
    if !cfg!(target_os = "macos") || !is_team_guardian_invocation() {
        return Err(ProcessError::Unsupported);
    }
    let parent_gone = CancellationToken::new();
    let lease_cancel = parent_gone.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // OS thread, not tokio::stdin: a blocked lease read must never hold up
    // Tokio runtime shutdown after the guardian has cleaned its child group.
    std::thread::Builder::new()
        .name("team-shell-lease".into())
        .spawn(move || {
            let mut input = std::io::stdin().lock();
            let parsed = (|| {
                let mut prefix = [0u8; 4];
                input.read_exact(&mut prefix).map_err(error)?;
                let length = u32::from_be_bytes(prefix) as usize;
                if length == 0 || length > MAX_BOOTSTRAP {
                    return Err(error("Invalid guardian bootstrap length"));
                }
                let mut body = vec![0; length];
                input.read_exact(&mut body).map_err(error)?;
                serde_json::from_slice::<Bootstrap>(&body).map_err(error)
            })();
            let _ = sender.send(parsed);
            let _ = input.read(&mut [0u8; 1]);
            lease_cancel.cancel();
        })
        .map_err(error)?;
    let bootstrap = tokio::time::timeout(Duration::from_secs(5), receiver)
        .await
        .map_err(|_| ProcessError::Timeout)?
        .map_err(error)??;
    validate(&bootstrap)?;
    if std::env::current_dir().map_err(error)? != bootstrap.scope_root.join("workspace")
        || environment(&bootstrap.scope_root)
            .into_iter()
            .any(|(key, value)| std::env::var(key).ok().as_ref() != Some(&value))
    {
        return Err(error("Team guardian scope/environment mismatch"));
    }
    if parent_gone.is_cancelled() {
        return Err(error("Team shell lease closed"));
    }
    let command = &bootstrap.command;
    let duration = remaining(bootstrap.deadline_unix_ms)?
        .min(command.timeout.unwrap_or(Duration::from_secs(24 * 60 * 60)));
    let shell_deadline = tokio::time::Instant::now() + duration;
    let mut shell = tokio::process::Command::new(&command.command);
    shell
        .args(&command.args)
        .env_clear()
        .envs(&command.env)
        .current_dir(bootstrap.scope_root.join("workspace"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    super::spawn_unsafe::attach_setsid(&mut shell);
    let mut child = shell.spawn().map_err(error)?;
    let pid = child.id().ok_or_else(|| error("Missing Team shell pid"))?;
    let mut guard = GroupGuard(Some(pid));
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| error("Missing shell stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| error("Missing shell stderr"))?;
    let timer = tokio::time::sleep_until(shell_deadline);
    tokio::pin!(timer);
    let mut poll = tokio::time::interval(Duration::from_millis(10));
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_done = false;
    let mut err_done = false;
    let mut out_chunk = [0u8; 8192];
    let mut err_chunk = [0u8; 8192];
    let mut timed_out = false;
    let mut execution_error = None;
    loop {
        tokio::select! {
            biased;
            _ = parent_gone.cancelled() => break,
            _ = &mut timer => { timed_out = true; break; },
            _ = poll.tick() => {
                match super::spawn_unsafe::team_child_exited_unreaped(pid) {
                    Ok(true) => break,
                    Ok(false) => {},
                    Err(reason) => { execution_error = Some(error(reason)); break; },
                }
            },
            read = stdout.read(&mut out_chunk), if !out_done => {
                let count = match read {
                    Ok(count) => count,
                    Err(reason) => { execution_error = Some(error(reason)); break; },
                };
                out_done = count == 0;
                let available = bootstrap.output_limit.saturating_sub(out.len() + err.len());
                out.extend_from_slice(&out_chunk[..count.min(available)]);
            },
            read = stderr.read(&mut err_chunk), if !err_done => {
                let count = match read {
                    Ok(count) => count,
                    Err(reason) => { execution_error = Some(error(reason)); break; },
                };
                err_done = count == 0;
                let available = bootstrap.output_limit.saturating_sub(out.len() + err.len());
                err.extend_from_slice(&err_chunk[..count.min(available)]);
            },
        }
    }
    cleanup_group(pid).await?;
    // No live group members remain. Disarm BEFORE reaping the PID anchor;
    // neither this guard nor a later drop may signal a reused group number.
    guard.0 = None;
    let status = child.wait().await.map_err(error)?;
    if let Some(reason) = execution_error {
        return Err(reason);
    }
    // Drain only the finite pipe tail after every process writer is dead.
    let tail = async {
        let available = bootstrap.output_limit.saturating_sub(out.len() + err.len());
        stdout
            .take(available as u64)
            .read_to_end(&mut out)
            .await
            .map_err(error)?;
        let available = bootstrap.output_limit.saturating_sub(out.len() + err.len());
        stderr
            .take(available as u64)
            .read_to_end(&mut err)
            .await
            .map_err(error)?;
        Ok::<_, ProcessError>(())
    };
    tokio::time::timeout(Duration::from_millis(100), tail)
        .await
        .map_err(|_| error("Team shell pipe cleanup_incomplete"))??;
    let frame = ResultFrame {
        output: ProcessOutput {
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            exit_code: if timed_out {
                143
            } else {
                status.code().unwrap_or(-1)
            },
            timed_out,
        },
        cleanup_complete: true,
    };
    let bytes = serde_json::to_vec(&frame).map_err(error)?;
    if bytes.len() > MAX_RESULT {
        return Err(error("Team guardian result too large"));
    }
    std::io::stdout().write_all(&bytes).map_err(error)?;
    std::io::stdout().flush().map_err(error)?;
    Ok(())
}
