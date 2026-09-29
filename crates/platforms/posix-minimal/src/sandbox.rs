//! Stub [`Sandbox`] — wraps every command in a `BypassAuditedWithReason`
//! tag so the engine's command-shape invariants hold without enforcing any
//! actual policy. Real sandbox-exec / namespaces wiring lands in Plan 17.

use async_trait::async_trait;
use lingxi_core::host::{
    ProcessCommand, Sandbox, SandboxBackend, SandboxCapability, SandboxError, SandboxFeatures,
    SandboxPolicy, SandboxedCommand, SandboxedTag,
};

/// No-op sandbox for the M1 demo host.
#[derive(Default)]
pub struct PosixSandbox;

impl PosixSandbox {
    /// Construct.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Sandbox for PosixSandbox {
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
        Err(SandboxError::Unavailable(
            "posix-minimal: sandbox prepare not implemented (Plan 17)".into(),
        ))
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
            reason: Some("posix-minimal stub".into()),
            features: SandboxFeatures::default(),
        }
    }
}
