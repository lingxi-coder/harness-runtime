use std::sync::Arc;

/// Runs typed Bash under a distinct immutable process registration.
pub struct TypedBashProcess {
    pub(super) inner: Arc<dyn lingxi_core::host::ProcessRunner>,
    pub(super) owner: String,
    pub(super) cancel: tokio_util::sync::CancellationToken,
}

impl TypedBashProcess {
    /// Construct the real wrapper for native integration tests.
    #[cfg(feature = "test-support")]
    pub fn for_test(
        inner: Arc<dyn lingxi_core::host::ProcessRunner>,
        owner: String,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            inner,
            owner,
            cancel,
        }
    }

    async fn execute(
        &self,
        command: &lingxi_core::host::SandboxedCommand,
        max_output_bytes: Option<usize>,
    ) -> Result<lingxi_core::host::ForegroundRunResult, mobile_linux_api::ProcessError> {
        if self.cancel.is_cancelled() {
            return Err(mobile_linux_api::ProcessError::Io(
                "Bash command cancelled".into(),
            ));
        }
        let command = command
            .clone()
            .with_process_owner(Some(self.owner.clone()))
            .with_auto_background_on_timeout(false);
        // Use the existing sandboxed capture path: it never transfers this
        // typed invocation into auto-background work. Preserve and poll the
        // SAME runner through cancellation so it owns the child wait/reap.
        let mut running = Box::pin(
            self.inner
                .run_foreground_with_output_limit(&command, max_output_bytes),
        );
        let result = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                loop {
                    // The launch itself may have yielded before registering a
                    // child. Recheck the immutable owner until the held future
                    // has finished; every platform rechecks registration tokens
                    // before signalling, rather than acting on stale PIDs.
                    self.inner.kill_owner_processes(&self.owner).await;
                    tokio::select! {
                        biased;
                        result = &mut running => break result,
                        _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {},
                    }
                }
            }
            result = &mut running => result,
        };
        drop(running);
        // Failure remains failure. Never turn an uncertain termination into a
        // successful shell response just because cancellation was requested.
        let output = result?;
        if self.cancel.is_cancelled() {
            return Err(mobile_linux_api::ProcessError::Io(
                "Bash command cancelled".into(),
            ));
        }
        Ok(output)
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::ProcessRunner for TypedBashProcess {
    async fn run(
        &self,
        command: &lingxi_core::host::SandboxedCommand,
    ) -> Result<mobile_linux_api::ProcessOutput, mobile_linux_api::ProcessError> {
        match self.execute(command, None).await?.outcome {
            lingxi_core::host::ForegroundOutcome::Completed(output) => Ok(output),
            lingxi_core::host::ForegroundOutcome::MovedToBackground(_) => {
                Err(mobile_linux_api::ProcessError::Unsupported)
            }
        }
    }
    async fn run_foreground_with_output_limit(
        &self,
        command: &lingxi_core::host::SandboxedCommand,
        max_output_bytes: Option<usize>,
    ) -> Result<lingxi_core::host::ForegroundRunResult, mobile_linux_api::ProcessError> {
        self.execute(command, max_output_bytes).await
    }
    async fn spawn_background(
        &self,
        _command: &lingxi_core::host::SandboxedCommand,
    ) -> Result<lingxi_core::host::ProcessHandle, mobile_linux_api::ProcessError> {
        Err(mobile_linux_api::ProcessError::Unsupported)
    }
    async fn kill(
        &self,
        handle: &lingxi_core::host::ProcessHandle,
    ) -> Result<(), mobile_linux_api::ProcessError> {
        self.inner.kill(handle).await
    }
    fn background_stop_requires_verified_cleanup(
        &self,
        handle: &lingxi_core::host::ProcessHandle,
    ) -> bool {
        self.inner.background_stop_requires_verified_cleanup(handle)
    }
    fn is_available(&self) -> bool {
        self.inner.is_available()
    }
    // Default supports_foreground_backgrounding=false is deliberate. The
    // invocation has no on-demand binding; timeout backgrounding is disabled.
}
