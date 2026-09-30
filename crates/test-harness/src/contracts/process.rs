//! [`ProcessRunner`] contract test suite.
//!
//! The runner accepts only [`SandboxedCommand`], so the suite constructs
//! commands via [`Sandbox::bypass_with_audit`] under the canonical "contract
//! test (bypass auditing intentional)" reason. We never call
//! [`Sandbox::prepare`] here — the contract is the runner's, not the
//! sandbox's.
//!
//! Invariants:
//!
//! * `is_available()` answers a `bool` without panicking.
//! * On platforms where the runner is available, a POSIX `echo hello`
//!   pipeline exits with status 0 and emits "hello" on stdout.
//! * `exit 7` is propagated as exit code 7 (and the command itself succeeds
//!   from the runner's POV — non-zero exit is not a `ProcessError`).
//! * A timeout that fires returns [`ProcessError::Timeout`].

use lingxi_core::host::sandbox::ProcessCommand;
use lingxi_core::host::{ProcessRunner, Sandbox};
use mobile_linux_api::ProcessError;

use std::collections::HashMap;
use std::time::Duration;

const TEST_BYPASS_REASON: &str = "contract test (bypass auditing intentional)";

/// Run the standard [`ProcessRunner`] contract against an impl, using the
/// supplied [`Sandbox`] to mint [`SandboxedCommand`] instances.
///
/// # Panics
///
/// Panics on the first invariant violation.
pub async fn process_runner_contract_tests<P, S>(proc: &P, sandbox: &S)
where
    P: ProcessRunner,
    S: Sandbox,
{
    test_is_available_returns_bool(proc);
    test_run_echo_returns_exit_zero_and_stdout(proc, sandbox).await;
    test_run_false_returns_nonzero_exit(proc, sandbox).await;
    test_run_timeout_returns_timeout_error(proc, sandbox).await;
}

fn test_is_available_returns_bool<P: ProcessRunner>(proc: &P) {
    let _ = proc.is_available();
}

async fn test_run_echo_returns_exit_zero_and_stdout<P, S>(proc: &P, sandbox: &S)
where
    P: ProcessRunner,
    S: Sandbox,
{
    if !proc.is_available() || !is_posix_sh_available() {
        return; // platform without /bin/sh; nothing to assert
    }
    let cmd = ProcessCommand {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "echo hello".into()],
        cwd: None,
        env: HashMap::new(),
        timeout: Some(Duration::from_secs(5)),
        stdin: None,
    };
    let sandboxed = sandbox.bypass_with_audit(cmd, TEST_BYPASS_REASON);
    let out = proc.run(&sandboxed).await.expect("run must succeed");
    assert_eq!(out.exit_code, 0, "echo exit code, got {out:?}");
    assert!(
        out.stdout.contains("hello"),
        "stdout must contain 'hello', got {:?}",
        out.stdout
    );
}

async fn test_run_false_returns_nonzero_exit<P, S>(proc: &P, sandbox: &S)
where
    P: ProcessRunner,
    S: Sandbox,
{
    if !proc.is_available() || !is_posix_sh_available() {
        return;
    }
    let cmd = ProcessCommand {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "exit 7".into()],
        cwd: None,
        env: HashMap::new(),
        timeout: Some(Duration::from_secs(5)),
        stdin: None,
    };
    let sandboxed = sandbox.bypass_with_audit(cmd, TEST_BYPASS_REASON);
    let out = proc
        .run(&sandboxed)
        .await
        .expect("run must return Ok with non-zero exit");
    assert_eq!(out.exit_code, 7, "exit 7 must be propagated");
}

async fn test_run_timeout_returns_timeout_error<P, S>(proc: &P, sandbox: &S)
where
    P: ProcessRunner,
    S: Sandbox,
{
    if !proc.is_available() || !is_posix_sh_available() {
        return;
    }
    let cmd = ProcessCommand {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "sleep 5".into()],
        cwd: None,
        env: HashMap::new(),
        timeout: Some(Duration::from_millis(200)),
        stdin: None,
    };
    let sandboxed = sandbox.bypass_with_audit(cmd, TEST_BYPASS_REASON);
    let r = proc.run(&sandboxed).await;
    assert!(
        matches!(r, Err(ProcessError::Timeout)),
        "timeout-fired run must return ProcessError::Timeout, got {r:?}"
    );
}

/// True when the host has `/bin/sh` available — short-circuits the suite on
/// Windows so the trait-level contract still applies, just on a smaller test
/// surface (`is_available()` only).
fn is_posix_sh_available() -> bool {
    std::path::Path::new("/bin/sh").exists()
}
