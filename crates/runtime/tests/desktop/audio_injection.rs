//! The desktop app-scoped media facade reaches the builtin tool registry.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use async_trait::async_trait;
use harness_runtime::desktop::{build, desktop_tool_registry, DesktopConfig};
use media_api::{
    MediaCapabilities, MediaContent, MediaError, MediaErrorKind, MediaKind, MediaOperation,
    MediaOperationContext, MediaOperationKind, MediaOperationReadiness, MediaReadinessState,
    MediaRef, MediaResult, MediaService,
};
use mobile_linux_api::ProcessOutput;
use tool_api::BuiltinToolContext;

struct StubMedia {
    supported: Vec<MediaOperationKind>,
}

#[async_trait]
impl MediaService for StubMedia {
    fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            service_epoch: 9,
            support_revision: 3,
            supported_operations: self.supported.clone(),
            supported_media_kinds: vec![MediaKind::Image, MediaKind::Audio],
            readiness: self
                .supported
                .iter()
                .map(|operation| MediaOperationReadiness {
                    operation: *operation,
                    state: MediaReadinessState::Ready,
                })
                .collect(),
            max_payload_bytes: 1024,
        }
    }

    async fn execute(
        &self,
        _context: MediaOperationContext,
        _operation: MediaOperation,
    ) -> Result<MediaResult, MediaError> {
        Err(MediaError::new(MediaErrorKind::Unavailable, "test stub"))
    }

    async fn read_media(
        &self,
        _context: MediaOperationContext,
        media: MediaRef,
    ) -> Result<MediaContent, MediaError> {
        Err(MediaError::new(
            MediaErrorKind::NotFound,
            format!("test media ref unavailable: {}", media.0),
        ))
    }

    async fn cancel(&self, _identity: media_api::MediaOperationId) -> Result<(), MediaError> {
        Ok(())
    }
}

fn stub_ctx() -> BuiltinToolContext {
    tool_api::test_support::shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    })
}

fn registered_tool_names(ctx: BuiltinToolContext) -> Vec<String> {
    desktop_tool_registry(ctx, None, None)
        .available_tools(&Default::default())
        .into_iter()
        .map(|tool| tool.name().to_owned())
        .collect()
}

#[test]
fn no_media_service_means_no_media_tools() {
    let names = registered_tool_names(stub_ctx());
    assert!(names.iter().all(|name| !name.starts_with("media_")));
    assert!(!names
        .iter()
        .any(|name| matches!(name.as_str(), "voice" | "speech" | "camera")));
}

#[test]
fn media_tools_follow_supported_operations_not_transient_readiness() {
    let service: Arc<dyn MediaService> = Arc::new(StubMedia {
        supported: vec![
            MediaOperationKind::Sources,
            MediaOperationKind::Capture,
            MediaOperationKind::SessionStart,
            MediaOperationKind::Stop,
        ],
    });
    let names = registered_tool_names(BuiltinToolContext {
        media: Some(service),
        ..stub_ctx()
    });
    for required in [
        "media_sources",
        "media_capture",
        "media_session_start",
        "media_stop",
    ] {
        assert!(
            names.iter().any(|name| name == required),
            "{required}: {names:?}"
        );
    }
    for unsupported in ["media_record", "media_play", "media_speak"] {
        assert!(
            !names.iter().any(|name| name == unsupported),
            "{unsupported}: {names:?}"
        );
    }
}

struct NoopPermissionSink;

#[async_trait]
impl client::adapter::PermissionRequestSink for NoopPermissionSink {
    async fn emit_request(&self, _request: client::protocol::permission::PermissionRequest) {}
}

#[tokio::test]
async fn desktop_build_exposes_the_injected_app_media_service() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = DesktopConfig::default();
    cfg.cwd = tmp.path().to_path_buf();
    cfg.lingxi_home = tmp.path().join(".lingxi");
    cfg.isolated_credential_storage = true;
    cfg.session_persistence = false;
    cfg.media_service = Some(Arc::new(StubMedia {
        supported: vec![MediaOperationKind::Sources, MediaOperationKind::Capture],
    }));
    let output: Arc<dyn lingxi_core::host::OutputStream> =
        Arc::new(orchestrator::test_support::MockOutputStream::new());
    let runtime = Box::pin(build(cfg, output, Arc::new(NoopPermissionSink)))
        .await
        .expect("the sandbox config must build");
    let service = runtime.media_service().expect("same injected media facade");
    assert_eq!(service.capabilities().service_epoch, 9);
    let names = runtime.registered_tool_names();
    assert!(names.iter().any(|name| name == "media_sources"));
    assert!(names.iter().any(|name| name == "media_capture"));
    assert!(!names.iter().any(|name| name == "media_record"));
}
