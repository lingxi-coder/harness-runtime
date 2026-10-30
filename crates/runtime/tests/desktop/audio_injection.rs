//! The desktop app-scoped media facade reaches the builtin tool registry.

#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
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
    source_calls: AtomicUsize,
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
        operation: MediaOperation,
    ) -> Result<MediaResult, MediaError> {
        if matches!(operation, MediaOperation::Sources { .. }) {
            self.source_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(MediaResult::Sources {
                sources: Vec::new(),
            });
        }
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

    async fn cancel(&self, _context: media_api::MediaOperationContext) -> Result<(), MediaError> {
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
        source_calls: AtomicUsize::new(0),
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
    // Exercise the real policy gate without depending on an Auto-mode classifier.
    let settings_dir = tmp.path().join(".lingxi");
    std::fs::create_dir_all(&settings_dir).unwrap();
    std::fs::write(
        settings_dir.join("settings.local.json"),
        r#"{"permissions":{"defaultMode":"default","allow":["media_sources"]}}"#,
    )
    .unwrap();
    let mut cfg = DesktopConfig::default();
    cfg.cwd = tmp.path().to_path_buf();
    cfg.lingxi_home = settings_dir;
    cfg.isolated_credential_storage = true;
    cfg.session_persistence = false;
    cfg.use_noop_permission_gate = true;
    let stub = Arc::new(StubMedia {
        supported: vec![MediaOperationKind::Sources, MediaOperationKind::Capture],
        source_calls: AtomicUsize::new(0),
    });
    let injected: Arc<dyn MediaService> = stub.clone();
    cfg.media_service = Some(injected.clone());
    let output: Arc<dyn lingxi_core::host::OutputStream> =
        Arc::new(orchestrator::test_support::MockOutputStream::new());
    let runtime = Box::pin(build(cfg, output, Arc::new(NoopPermissionSink)))
        .await
        .expect("the sandbox config must build");
    let service = runtime.media_service().expect("same injected media facade");
    assert!(Arc::ptr_eq(&service, &injected));
    let capabilities = service.capabilities();
    assert_eq!(capabilities.service_epoch, 9);
    assert_eq!(
        capabilities.supported_operations,
        [MediaOperationKind::Sources, MediaOperationKind::Capture]
    );
    // Registration is deliberately broader than the live availability view:
    // capability changes must be reflected without rebuilding the registry.
    // The preceding test checks the filtered view for unsupported operations.
    let names = runtime.registered_tool_names();
    for name in tool_media::TOOL_NAMES {
        assert!(
            names.iter().any(|registered| registered == name),
            "{name}: {names:?}"
        );
    }

    let advertised = runtime.orchestrator.advertised_tool_names().await;
    for supported in ["media_sources", "media_capture"] {
        assert!(advertised.iter().any(|name| name == supported));
    }
    for unsupported in ["media_record", "media_play", "media_speak"] {
        assert!(!advertised.iter().any(|name| name == unsupported));
    }

    // Dispatch through the assembled orchestrator and its actual registry,
    // not a separately constructed test registry.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Box::pin(runtime.orchestrator.call_tool_from_host(
            lingxi_core::types::ToolUseId::new(),
            "media_sources".into(),
            tool_api::tool_trait::generated_builtin_result(&serde_json::json!({})),
            None,
        )),
    )
    .await
    .expect("assembled media dispatch must finish")
    .expect("assembled media dispatch must succeed")
    .expect("media_sources must be dispatchable");
    match result {
        lingxi_core::types::ContentBlock::ToolResult {
            is_error, content, ..
        } => assert_ne!(is_error, Some(true), "{content:?}"),
        other => panic!("expected a media tool result, got {other:?}"),
    }
    assert_eq!(stub.source_calls.load(Ordering::SeqCst), 1);
}
