//! Stub [`ProcessRunner`] — full `tokio::process` impl deferred to
//! `platforms/posix` (Plan 17). M1.22 returns
//! [`ProcessError::Unsupported`] for every entry point so any engine-side
//! call surfaces loudly during the demo.

use async_trait::async_trait;
use lingxi_core::host::{
    ProcessError, ProcessHandle, ProcessOutput, ProcessRunner, SandboxedCommand,
};

/// Stub process runner.
#[derive(Default)]
pub struct PosixProcess;

impl PosixProcess {
    /// Construct.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ProcessRunner for PosixProcess {
    async fn run(&self, _cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
        Err(ProcessError::Unsupported)
    }

    async fn spawn_background(
        &self,
        _cmd: &SandboxedCommand,
    ) -> Result<ProcessHandle, ProcessError> {
        Err(ProcessError::Unsupported)
    }

    async fn kill(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
        Ok(())
    }

    fn is_available(&self) -> bool {
        false
    }
}
