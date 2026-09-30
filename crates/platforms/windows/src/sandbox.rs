//! `Sandbox` trait impl — Windows is Unsupported.
//!
//! claude-code refuses sandbox on Windows (its `@anthropic-ai/sandbox-runtime`
//! has no Windows backend). M2 Plan 04 mirrors that: [`Sandbox::is_available`]
//! returns `false`, [`Sandbox::prepare`] returns [`SandboxError::Unsupported`],
//! and `probe_capability().reason` carries the exact claude-code error string.
//!
//! Cross-ref: `docs/superpowers/plans/2026-05-23-m2-04-sandbox-runtime.md` —
//! the parity story for sandbox lives in M2-04; this file is unchanged by
//! M2-04 itself but documents that constraint. No follow-up plan turns this
//! on — `AppContainer` / Job Objects work is not part of claude-code parity.

use async_trait::async_trait;
use lingxi_core::host::{
    ProcessCommand, Sandbox, SandboxCapability, SandboxError, SandboxFeatures, SandboxPolicy,
    SandboxedCommand, SandboxedTag,
};
use mobile_linux_api::SandboxBackend;

/// Windows-side [`Sandbox`] — always reports unsupported.
#[derive(Default)]
pub struct WindowsSandbox;

impl WindowsSandbox {
    /// Construct a new `WindowsSandbox`. Holds no state.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Sandbox for WindowsSandbox {
    fn is_available(&self) -> bool {
        false
    }

    fn backend(&self) -> SandboxBackend {
        SandboxBackend::None
    }

    fn prepare(
        &self,
        _cmd: ProcessCommand,
        _policy: &SandboxPolicy,
    ) -> Result<SandboxedCommand, SandboxError> {
        Err(SandboxError::Unsupported)
    }

    fn bypass_with_audit(&self, cmd: ProcessCommand, reason: &str) -> SandboxedCommand {
        SandboxedCommand::__new_sandboxed(
            cmd,
            SandboxedTag::BypassAuditedWithReason {
                reason: reason.to_string(),
            },
        )
    }

    async fn probe_capability(&self) -> SandboxCapability {
        SandboxCapability {
            available: false,
            reason: Some("claude-code does not support sandbox on Windows".into()),
            features: SandboxFeatures::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::{ProcessCommand, Sandbox, SandboxPolicy};
    use mobile_linux_api::{NetworkPolicy, ResourceLimits, SandboxBackend};
    use std::collections::HashMap;

    fn empty_cmd() -> ProcessCommand {
        ProcessCommand {
            command: "echo".into(),
            args: vec!["hi".into()],
            cwd: None,
            env: HashMap::new(),
            timeout: None,
            stdin: None,
        }
    }

    fn empty_policy() -> SandboxPolicy {
        SandboxPolicy {
            network: NetworkPolicy::Disabled,
            writable_paths: vec![],
            denied_paths: vec![],
            allow_subprocess: false,
            limits: ResourceLimits::default(),
        }
    }

    #[test]
    fn is_available_returns_false() {
        assert!(!WindowsSandbox::new().is_available());
    }

    #[test]
    fn backend_returns_none() {
        assert_eq!(WindowsSandbox::new().backend(), SandboxBackend::None);
    }

    #[test]
    fn prepare_returns_unsupported() {
        let err = WindowsSandbox::new()
            .prepare(empty_cmd(), &empty_policy())
            .unwrap_err();
        assert!(matches!(err, lingxi_core::host::SandboxError::Unsupported));
    }

    #[test]
    fn bypass_with_audit_still_wraps_cmd() {
        // bypass_with_audit returns SandboxedCommand unconditionally — even
        // on unsupported platforms an explicit audit grant must still produce
        // a usable command.
        let wrapped = WindowsSandbox::new().bypass_with_audit(empty_cmd(), "explicit override");
        let _: lingxi_core::host::SandboxedCommand = wrapped;
    }

    #[tokio::test]
    async fn probe_capability_reports_claude_code_string() {
        let cap = WindowsSandbox::new().probe_capability().await;
        assert!(!cap.available);
        assert_eq!(
            cap.reason.as_deref(),
            Some("claude-code does not support sandbox on Windows"),
        );
    }
}
