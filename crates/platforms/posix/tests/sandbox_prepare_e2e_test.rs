//! End-to-end check of [`PosixSandbox::prepare`] (Task 20 / M2-04 Phase D).
//!
//! Builds a realistic `ls` command + policy, runs it through the new
//! sandbox impl, and inspects the wrapped argv. When the host has the deps
//! (macOS `sandbox-exec`, Linux `bwrap`), the test asserts the bwrap /
//! sandbox-exec prefix; otherwise it asserts the no-op fall-through shape.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use lingxi_core::host::{
    NetworkPolicy, ProcessCommand, ResourceLimits, Sandbox, SandboxPolicy, SandboxedTag,
};
use platform_posix::sandbox::PosixSandbox;
use std::collections::HashMap;
use std::path::PathBuf;

#[test]
fn prepare_emits_bwrap_or_sandbox_exec_invocation() {
    let sandbox = PosixSandbox::new();

    // If sandbox deps are missing on CI, prepare degrades to Wrapped(None).
    // We can still assert the SandboxedCommand wrapper.
    let cmd = ProcessCommand {
        command: "ls".into(),
        args: vec!["-la".into(), "/tmp".into()],
        cwd: Some(PathBuf::from("/tmp")),
        env: HashMap::new(),
        timeout: None,
        stdin: None,
    };
    let policy = SandboxPolicy {
        network: NetworkPolicy::Disabled,
        writable_paths: vec![PathBuf::from("/tmp")],
        denied_paths: vec![PathBuf::from("/etc")],
        allow_subprocess: true,
        limits: ResourceLimits::default(),
    };

    let prepared = sandbox.prepare(cmd, &policy).expect("prepare ok");
    let inner = prepared.inner();
    let tag = prepared.tag();

    match tag {
        SandboxedTag::Wrapped { backend } => {
            eprintln!("backend={backend:?}");
        }
        SandboxedTag::BypassAuditedWithReason { reason } => {
            panic!("unexpected bypass: {reason}");
        }
    }

    if sandbox.is_available() {
        // The argv shape is `/bin/sh -c "<wrapped>"`.
        assert_eq!(inner.command, "/bin/sh");
        assert_eq!(inner.args.len(), 2);
        assert_eq!(inner.args[0], "-c");
        let wrapped = &inner.args[1];
        let ok = wrapped.starts_with("bwrap ") || wrapped.starts_with("sandbox-exec ");
        assert!(ok, "wrapped argv: {wrapped}");
        // /tmp should appear as an allow-write binding (Linux) or as a regex
        // entry in the SBPL profile (macOS — we can't grep the profile from
        // here without re-reading it, so just sanity check substring on Linux).
        // On macOS the profile path is in the wrap output but the writable
        // mount appears inside the profile file, so we only assert the
        // ls survives the wrap.
        assert!(
            wrapped.contains("ls"),
            "missing original command: {wrapped}"
        );
        // On Linux, /tmp should appear in the bwrap argv as a --bind target.
        #[cfg(target_os = "linux")]
        assert!(wrapped.contains("/tmp"), "expected /tmp binding: {wrapped}");
    } else {
        // Stub path: the wrapped command is identical to the original.
        assert_eq!(inner.command, "ls");
    }
}
