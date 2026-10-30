//! Shell and the production runner against a controlled native SDK boundary.
//! This is not a physical-device test.

use async_trait::async_trait;
use mobile_linux_api::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tool_api::tool_trait::{Tool, ToolError};

struct RawSdk {
    base: UnavailableMobileLinuxRuntime,
    first_read: AtomicBool,
    fail_close: bool,
    close_calls: AtomicUsize,
}

#[async_trait]
impl MobileLinuxRuntime for RawSdk {
    fn backend(&self) -> SandboxBackend {
        SandboxBackend::AndroidProot
    }
    fn mode(&self) -> MobileLinuxRuntimeMode {
        MobileLinuxRuntimeMode::MobileLinux
    }
    async fn probe_capability(&self) -> MobileLinuxCapability {
        let mut capability = self.base.probe_capability().await;
        capability.available = true;
        capability.reason = None;
        capability
    }
    async fn boot(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn shutdown(&self) -> Result<(), MobileLinuxError> {
        Ok(())
    }
    async fn run(&self, _: LinuxCommandRequest) -> Result<LinuxCommandResult, MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn spawn_background(
        &self,
        _: LinuxCommandRequest,
    ) -> Result<LinuxProcessHandle, MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn kill(&self, _: &LinuxProcessHandle) -> Result<(), MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn open_pty(&self, _: PtyOpenRequest) -> Result<PtySessionHandle, MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn write_pty(&self, _: &PtySessionHandle, _: Vec<u8>) -> Result<(), MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn resize_pty(&self, _: &PtySessionHandle, _: PtySize) -> Result<(), MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn close_pty(&self, _: &PtySessionHandle) -> Result<(), MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn rootfs_status(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn verify_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn repair_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn reset_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }
    async fn configure_mounts(&self, _: Vec<MountSpec>) -> Result<(), MobileLinuxError> {
        Ok(())
    }

    async fn open_raw_stdio(
        &self,
        _: RawStdioOpenRequest,
    ) -> Result<RawStdioSessionHandle, MobileLinuxError> {
        Ok(RawStdioSessionHandle {
            id: "cross-layer-session".into(),
            enforcement: LinuxEnforcementReceipt::default(),
        })
    }
    async fn read_raw_stdio(
        &self,
        _: &RawStdioSessionHandle,
        _: usize,
    ) -> Result<RawStdioReadResult, MobileLinuxError> {
        if self.first_read.swap(false, Ordering::AcqRel) {
            Ok(RawStdioReadResult {
                stdout: b"partial stdout".to_vec(),
                stderr: b"partial stderr".to_vec(),
                closed: false,
                exit_code: None,
            })
        } else {
            std::future::pending().await
        }
    }
    async fn close_raw_stdio(&self, _: &RawStdioSessionHandle) -> Result<(), MobileLinuxError> {
        self.close_calls.fetch_add(1, Ordering::AcqRel);
        if self.fail_close {
            Err(MobileLinuxError::Io("injected reap failure".into()))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn shell_consumes_real_runner_timeout_only_after_verified_close() {
    for fail_close in [false, true] {
        let runtime = Arc::new(RawSdk {
            base: UnavailableMobileLinuxRuntime::unavailable(
                SandboxBackend::AndroidProot,
                MobileLinuxRuntimeMode::MobileLinux,
                "test",
                "test",
                "controlled SDK",
            ),
            first_read: AtomicBool::new(true),
            fail_close,
            close_calls: AtomicUsize::new(0),
        });
        let runtime_trait: Arc<dyn MobileLinuxRuntime> = runtime.clone();
        let mut context = tool_api::test_support::shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        context.mobile_shell = Some(tool_api::MobileShellToolCtx::mobile_linux_guest(
            true,
            vec![],
            None,
        ));
        context.sandbox = Arc::new(
            platform_common::MobileLinuxSandbox::new(
                runtime_trait.clone(),
                vec![],
                branding::LINGXI_RUNTIME_PROFILE,
            )
            .unwrap(),
        );
        context.process = Arc::new(platform_common::MobileLinuxProcessRunner::new(
            runtime_trait,
        ));
        let marker = Arc::new(());
        let weak = Arc::downgrade(&marker);
        let lease: Arc<dyn Send + Sync> = marker.clone();
        drop(marker);
        let input = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
            &serde_json::json!({"command":"printf test", "timeout":50}),
        )
        .unwrap();
        let tool = tool_shell_mobile::ShellMobileTool::new(context);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tool_api::context::with_process_execution_lease(
                Some(lease),
                tool.call(
                    input,
                    tool_api::test_support::fresh_ctx(),
                    tool_api::test_support::fresh_tx(),
                ),
            ),
        )
        .await
        .expect("runner returns after timeout and close");
        assert_eq!(runtime.close_calls.load(Ordering::Acquire), 1);
        if fail_close {
            assert!(
                matches!(result, Err(ToolError::CleanupUnverified(detail)) if detail.contains("injected reap failure"))
            );
            assert!(
                weak.upgrade().is_some(),
                "unverified owner remains quarantined"
            );
        } else {
            let result = result.expect("verified timeout is a structured result");
            assert!(result.is_error);
            assert_eq!(result.data.string_at("/stdout"), Some("partial stdout"));
            assert_eq!(
                result.data.string_at("/stderr"),
                Some("partial stderr\nShell command timed out after 50ms")
            );
            assert_eq!(
                result
                    .data
                    .at_pointer("/timed_out")
                    .and_then(|value| value.as_bool()),
                Some(true)
            );
            assert!(
                weak.upgrade().is_none(),
                "verified close releases the owner"
            );
        }
    }
}
