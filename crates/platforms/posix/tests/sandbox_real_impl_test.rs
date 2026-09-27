//! Real-impl sanity checks for [`PosixSandbox`] (Task 17 / M2-04 Phase D).
//!
//! Asserts:
//! - `backend()` matches the host OS.
//! - `bypass_with_audit` records the supplied reason.
//! - When the sandbox is actually available on the host, `prepare()` wraps the
//!   inner command in `/bin/sh -c "<bwrap … | sandbox-exec …>"`.

use platform_api::{
    NetworkPolicy, ProcessCommand, ResourceLimits, Sandbox, SandboxBackend, SandboxPolicy,
    SandboxedTag,
};
use platform_posix::sandbox::PosixSandbox;
use std::collections::HashMap;
use std::path::PathBuf;

fn sample_policy() -> SandboxPolicy {
    SandboxPolicy {
        network: NetworkPolicy::Disabled,
        writable_paths: vec![PathBuf::from("/tmp/work")],
        denied_paths: vec![PathBuf::from("/etc")],
        allow_subprocess: true,
        limits: ResourceLimits {
            max_cpu_seconds: Some(60),
            max_memory_mb: Some(256),
            max_processes: Some(16),
            max_open_files: Some(64),
        },
    }
}

fn sample_command() -> ProcessCommand {
    ProcessCommand {
        command: "ls".into(),
        args: vec!["-la".into()],
        cwd: Some(PathBuf::from("/tmp")),
        env: HashMap::new(),
        timeout: None,
        stdin: None,
    }
}

#[test]
fn backend_matches_target_os() {
    let s = PosixSandbox::new();
    let b = s.backend();
    #[cfg(target_os = "macos")]
    assert!(matches!(b, SandboxBackend::MacOsSandboxExec));
    #[cfg(target_os = "linux")]
    assert!(matches!(
        b,
        SandboxBackend::LinuxNamespaces | SandboxBackend::None
    ));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    assert!(matches!(b, SandboxBackend::None));
    let _ = b;
}

#[test]
fn bypass_with_audit_records_reason() {
    let s = PosixSandbox::new();
    let bypass = s.bypass_with_audit(sample_command(), "user opted out");
    match bypass.tag() {
        SandboxedTag::BypassAuditedWithReason { reason } => {
            assert_eq!(reason, "user opted out");
        }
        other @ SandboxedTag::Wrapped { .. } => {
            panic!("expected BypassAuditedWithReason, got {other:?}")
        }
    }
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn prepare_wraps_command_when_backend_available() {
    let s = PosixSandbox::new();
    // Only assert wrap shape if the host actually has the deps (CI may not).
    if !s.is_available() {
        eprintln!(
            "skip: sandbox not available on host (reason: {:?})",
            futures::executor::block_on(s.probe_capability()).reason
        );
        return;
    }
    let prepared = s
        .prepare(sample_command(), &sample_policy())
        .expect("prepare ok");
    let inner = prepared.inner();
    // The wrapped command should be invoked through /bin/sh -c.
    assert_eq!(inner.command, "/bin/sh", "got: {inner:?}");
    assert_eq!(inner.args.len(), 2, "got: {inner:?}");
    assert_eq!(inner.args[0], "-c", "got: {inner:?}");
    // The second argv element is the wrapped string; on macOS it begins with
    // sandbox-exec, on Linux with bwrap.
    let wrapped = &inner.args[1];
    let starts_ok = wrapped.starts_with("bwrap ") || wrapped.starts_with("sandbox-exec ");
    assert!(
        starts_ok,
        "wrapped argv must start with bwrap/sandbox-exec, got: {wrapped}"
    );
    // The original `ls -la` command should appear at the tail (shell-quoted).
    assert!(
        wrapped.contains("ls"),
        "missing original command: {wrapped}"
    );
}
