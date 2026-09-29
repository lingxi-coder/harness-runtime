//! [`Sandbox`] contract.
//!
//! Verifies the invariants every `Sandbox` impl must honour:
//!
//! * `is_available()` answers a `bool` without panicking.
//! * `bypass_with_audit` preserves the inner [`ProcessCommand`] verbatim.
//! * `bypass_with_audit` records the supplied reason in the
//!   [`SandboxedTag::BypassAuditedWithReason`] tag.
//! * `prepare` with a default policy either succeeds (when the backend is
//!   available) or returns one of the documented platform error variants
//!   (`Unsupported`, `Unavailable`).
//! * `probe_capability()` agrees with `is_available()` on the headline
//!   availability bit.
//!
//! Per-backend nuances (the bwrap argv on Linux, sandbox-exec profile shape
//! on macOS, the Windows `Unsupported` branch) belong in platform-local
//! tests; this contract is the trait-level surface only.

use lingxi_core::host::sandbox::{
    NetworkPolicy, ProcessCommand, ResourceLimits, Sandbox, SandboxError, SandboxPolicy,
    SandboxedTag,
};
use std::collections::HashMap;

const REASON: &str = "sandbox contract bypass (audit intentional)";

/// Run the standard [`Sandbox`] contract against an impl.
///
/// # Panics
///
/// Panics on the first invariant violation.
pub async fn sandbox_contract_tests<S: Sandbox>(s: &S) {
    test_is_available_returns_bool(s);
    test_bypass_preserves_inner_command(s);
    test_bypass_tag_carries_reason(s);
    test_prepare_with_default_policy(s);
    test_probe_capability_agrees_with_is_available(s).await;
}

fn test_is_available_returns_bool<S: Sandbox>(s: &S) {
    let _ = s.is_available();
}

fn test_bypass_preserves_inner_command<S: Sandbox>(s: &S) {
    let cmd = ProcessCommand {
        command: "/usr/bin/echo".into(),
        args: vec!["hi".into()],
        cwd: None,
        env: HashMap::new(),
        timeout: None,
        stdin: None,
    };
    let sandboxed = s.bypass_with_audit(cmd, REASON);
    assert_eq!(
        sandboxed.inner().command,
        "/usr/bin/echo",
        "inner.command preserved by bypass_with_audit"
    );
    assert_eq!(
        sandboxed.inner().args,
        vec!["hi".to_string()],
        "inner.args preserved by bypass_with_audit"
    );
}

fn test_bypass_tag_carries_reason<S: Sandbox>(s: &S) {
    let cmd = ProcessCommand {
        command: "/bin/true".into(),
        args: vec![],
        cwd: None,
        env: HashMap::new(),
        timeout: None,
        stdin: None,
    };
    let sandboxed = s.bypass_with_audit(cmd, REASON);
    match sandboxed.tag() {
        SandboxedTag::BypassAuditedWithReason { reason } => {
            assert_eq!(reason, REASON, "bypass reason must round-trip");
        }
        other @ SandboxedTag::Wrapped { .. } => {
            panic!("expected BypassAuditedWithReason tag, got {other:?}");
        }
    }
}

fn test_prepare_with_default_policy<S: Sandbox>(s: &S) {
    let cmd = ProcessCommand {
        command: "/bin/true".into(),
        args: vec![],
        cwd: None,
        env: HashMap::new(),
        timeout: None,
        stdin: None,
    };
    let policy = SandboxPolicy {
        network: NetworkPolicy::Disabled,
        writable_paths: vec![],
        denied_paths: vec![],
        allow_subprocess: false,
        limits: ResourceLimits::default(),
    };
    let r = s.prepare(cmd, &policy);
    // Three documented branches are all fine here:
    //   1. Ok(_)                         — backend is available, wrapped or no-op
    //   2. Err(SandboxError::Unsupported) — platform without a backend (Windows)
    //   3. Err(SandboxError::Unavailable(_)) — backend present but deps missing
    // Anything else (e.g. PathCanonicalize on the empty policy) is a contract
    // violation.
    match r {
        Ok(_) | Err(SandboxError::Unsupported | SandboxError::Unavailable(_)) => {}
        other => {
            panic!("prepare with default policy must be Ok/Unsupported/Unavailable, got {other:?}")
        }
    }
}

async fn test_probe_capability_agrees_with_is_available<S: Sandbox>(s: &S) {
    let cap = s.probe_capability().await;
    assert_eq!(
        cap.available,
        s.is_available(),
        "probe_capability().available must agree with is_available()"
    );
}
