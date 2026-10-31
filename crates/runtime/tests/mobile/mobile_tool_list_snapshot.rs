//! M8-P11 — lock the mobile tool set assembled by the composition root, and
//! assert it omits the desktop-only + device-control tools (the composition
//! roots must not drift into registering the same set).

#![allow(clippy::unwrap_used)]

use harness_runtime::mobile::mobile_tool_registry;
use media_api::{
    MediaCapabilities, MediaContent, MediaError, MediaErrorKind, MediaKind, MediaOperation,
    MediaOperationContext, MediaOperationKind, MediaOperationReadiness, MediaReadinessState,
    MediaRef, MediaResult, MediaService,
};
use std::sync::Arc;

struct SnapshotMedia;

#[async_trait::async_trait]
impl MediaService for SnapshotMedia {
    fn capabilities(&self) -> MediaCapabilities {
        let supported_operations = vec![
            MediaOperationKind::Sources,
            MediaOperationKind::Status,
            MediaOperationKind::Capture,
            MediaOperationKind::Record,
            MediaOperationKind::Transcribe,
            MediaOperationKind::Synthesize,
            MediaOperationKind::Play,
            MediaOperationKind::Speak,
            MediaOperationKind::SessionStart,
            MediaOperationKind::SessionUpdate,
            MediaOperationKind::Observe,
            MediaOperationKind::Wait,
            MediaOperationKind::Stop,
        ];
        MediaCapabilities {
            service_epoch: 1,
            support_revision: 1,
            supported_media_kinds: vec![MediaKind::Image, MediaKind::Audio, MediaKind::Video],
            readiness: supported_operations
                .iter()
                .map(|operation| MediaOperationReadiness {
                    operation: *operation,
                    state: MediaReadinessState::Ready,
                })
                .collect(),
            supported_operations,
            max_payload_bytes: 1024,
        }
    }

    async fn execute(
        &self,
        _: MediaOperationContext,
        _: MediaOperation,
    ) -> Result<MediaResult, MediaError> {
        Err(MediaError::new(
            MediaErrorKind::Unavailable,
            "snapshot fixture",
        ))
    }

    async fn read_media(
        &self,
        _: MediaOperationContext,
        _: MediaRef,
    ) -> Result<MediaContent, MediaError> {
        Err(MediaError::new(
            MediaErrorKind::NotFound,
            "snapshot fixture",
        ))
    }

    async fn cancel(&self, _: media_api::MediaOperationContext) -> Result<(), MediaError> {
        Ok(())
    }
}

use mobile_linux_api::ProcessOutput;

#[test]
fn mobile_tool_list_snapshot() {
    let mut ctx = tool_api::test_support::shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    // The locked mobile tool set represents a host with the complete media catalog.
    ctx.media = Some(Arc::new(SnapshotMedia));
    let reg = mobile_tool_registry(ctx);

    let mut names = reg.all_names();
    names.sort();

    // Removed tools must be absent on every host, including mobile.
    for removed in ["TeamCreate", "TeamDelete", "camera", "voice", "speech"] {
        assert!(!names.iter().any(|name| name == removed));
    }

    // Desktop-only + device-control tools must NOT be in the mobile set.
    for forbidden in [
        "Bash",
        "PowerShell",
        "REPL",
        "MCP",
        "Agent",
        "EnterWorktree",
        "ExitWorktree",
        "computer",
        "android_use",
        "ios_use",
        // Monitor's Bash/process-substitution contract is not portable to the
        // restricted mobile Shell runtime.
        "Monitor",
    ] {
        assert!(
            !names.iter().any(|n| n == forbidden),
            "mobile tool set must not include `{forbidden}`"
        );
    }
    // The production mobile profile registers the complete media catalog.
    // Live capabilities filter which registered tools are advertised.
    let available = reg.available_tools(&Default::default());
    for required in [
        "media_sources",
        "media_status",
        "media_capture",
        "media_record",
        "media_transcribe",
        "media_synthesize",
        "media_play",
        "media_speak",
        "media_session_start",
        "media_session_update",
        "media_observe",
        "media_wait",
        "media_stop",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "mobile tool set must include `{required}`"
        );
        assert!(
            available.iter().any(|tool| tool.name() == required),
            "supported mobile media operation must be enabled: `{required}`"
        );
    }
    for unavailable in [
        "notification",
        "clipboard",
        "share",
        "location",
        "device_status",
        "haptics",
        "open_url",
        "calendar",
        "contacts",
    ] {
        assert!(
            !names.iter().any(|name| name == unavailable),
            "unwired {unavailable} must be absent"
        );
    }
    assert!(
        names.iter().any(|name| name == "LSP"),
        "the production mobile tool set must include `LSP`"
    );

    insta::assert_yaml_snapshot!("mobile_tool_list", names);
}
