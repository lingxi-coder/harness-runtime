use super::*;
use async_trait::async_trait;
use base64::Engine as _;
use image::GenericImageView;
use image::{DynamicImage, Rgb, RgbImage};
use media_api::stt::SttTranscript;
use media_api::{
    MediaCapabilities, MediaEvent, MediaMetadata, MediaOperationReadiness, MediaResourceRef,
    MediaSessionState, MediaSessionStatus, MediaSource, MediaSourceId, MediaSourceKind,
    MediaSourceRef, MediaSourceUsage, RecordingId,
};
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use tool_api::context::{MediaAgentHandoffHandle, MediaAgentHandoffRequest};
use tool_api::tool_trait::{Tool, ToolError};
use tool_api::{ToolRegistry, ToolUseContext};

struct FakeMedia {
    capabilities: MediaCapabilities,
    owner_capabilities: Option<MediaCapabilities>,
    failure: Option<MediaErrorKind>,
    stop_failures_remaining: Mutex<usize>,
    operations: Mutex<Vec<(MediaOperationContext, MediaOperation)>>,
    reads: Mutex<Vec<MediaOperationContext>>,
    profile_change_after_start:
        Option<(Arc<tokio::sync::Mutex<lingxi_core::SessionState>>, String)>,
    image: Vec<u8>,
    asset: MediaAsset,
}

impl FakeMedia {
    fn new(operations: &[MediaOperationKind]) -> Self {
        let image = image_png();
        let asset = image_asset(&image);
        Self {
            capabilities: MediaCapabilities {
                service_epoch: 31,
                support_revision: 7,
                supported_operations: operations.to_vec(),
                supported_media_kinds: vec![MediaKind::Audio, MediaKind::Image, MediaKind::Video],
                readiness: Vec::new(),
                max_payload_bytes: 4 * 1024 * 1024,
            },
            owner_capabilities: None,
            failure: None,
            stop_failures_remaining: Mutex::new(0),
            operations: Mutex::new(Vec::new()),
            reads: Mutex::new(Vec::new()),
            profile_change_after_start: None,
            image,
            asset,
        }
    }

    fn with_failure(operations: &[MediaOperationKind], failure: MediaErrorKind) -> Self {
        Self {
            failure: Some(failure),
            ..Self::new(operations)
        }
    }

    fn with_stop_failures(mut self, failures: usize) -> Self {
        self.stop_failures_remaining = Mutex::new(failures);
        self
    }

    fn with_owner_capabilities(mut self, supported: &[MediaOperationKind]) -> Self {
        let mut capabilities = self.capabilities.clone();
        capabilities.supported_operations = supported.to_vec();
        capabilities.support_revision = capabilities.support_revision.saturating_add(1);
        self.owner_capabilities = Some(capabilities);
        self
    }

    fn with_profile_change_after_start(
        mut self,
        session: Arc<tokio::sync::Mutex<lingxi_core::SessionState>>,
        profile: &str,
    ) -> Self {
        self.profile_change_after_start = Some((session, profile.to_owned()));
        self
    }

    fn capabilities_with_readiness(
        supported: Vec<MediaOperationKind>,
        readiness: Vec<MediaOperationReadiness>,
    ) -> Self {
        let mut service = Self::new(&supported);
        service.capabilities.readiness = readiness;
        service
    }

    fn last_operation(&self) -> (MediaOperationContext, MediaOperation) {
        self.operations.lock().unwrap().last().unwrap().clone()
    }
}

#[async_trait]
impl media_api::MediaService for FakeMedia {
    fn capabilities(&self) -> MediaCapabilities {
        self.capabilities.clone()
    }

    fn capabilities_for(&self, _owner: &MediaOwner) -> MediaCapabilities {
        self.owner_capabilities
            .as_ref()
            .unwrap_or(&self.capabilities)
            .clone()
    }

    async fn execute(
        &self,
        context: MediaOperationContext,
        operation: MediaOperation,
    ) -> Result<MediaResult, MediaError> {
        self.operations
            .lock()
            .unwrap()
            .push((context, operation.clone()));
        if let Some(kind) = self.failure {
            return Err(MediaError::new(kind, "controlled test failure"));
        }
        if matches!(&operation, MediaOperation::Stop { .. }) {
            let mut failures_remaining = self.stop_failures_remaining.lock().unwrap();
            if *failures_remaining > 0 {
                *failures_remaining -= 1;
                return Err(MediaError::new(
                    MediaErrorKind::Busy,
                    "controlled stop failure",
                ));
            }
        }
        let is_session_start = matches!(&operation, MediaOperation::SessionStart { .. });
        let result = match operation {
            MediaOperation::Sources { .. } => MediaResult::Sources {
                sources: vec![MediaSource {
                    reference: MediaSourceRef(MediaSourceId::from("camera-device")),
                    kind: MediaSourceKind::Camera,
                    media_kind: Some(MediaKind::Image),
                    label: "Camera".into(),
                    usages: vec![MediaSourceUsage::Capture, MediaSourceUsage::Session],
                    supported_operations: vec![MediaOperationKind::Capture],
                    is_default: true,
                }],
            },
            MediaOperation::Status { .. } => MediaResult::Status { resources: vec![] },
            MediaOperation::Capture { .. } => MediaResult::Captured {
                asset: self.asset.clone(),
            },
            MediaOperation::Record { .. } => MediaResult::RecordingStarted {
                recording: RecordingRef(RecordingId::from("recording-1")),
            },
            MediaOperation::Transcribe { .. } => MediaResult::Transcript {
                transcript: SttTranscript {
                    text: "recognized words".into(),
                    language: Some("en-US".into()),
                    confidence: Some(0.94),
                },
            },
            MediaOperation::Synthesize { .. } => MediaResult::Synthesized {
                asset: self.asset.clone(),
            },
            MediaOperation::Play { .. } => MediaResult::PlaybackCompleted { duration_ms: 120 },
            MediaOperation::Speak { .. } => MediaResult::SpeechCompleted { duration_ms: 120 },
            MediaOperation::SessionStart { request } => MediaResult::SessionStarted {
                session: MediaSessionRef(media_api::MediaSessionId::from("session-1")),
                status: MediaSessionStatus {
                    state: MediaSessionState::Ready,
                    audio_enabled: request.audio_source.is_some(),
                    video_enabled: request.video_source.is_some(),
                },
            },
            MediaOperation::SessionUpdate { session, .. } => MediaResult::SessionUpdated {
                session,
                status: MediaSessionStatus {
                    state: MediaSessionState::Ready,
                    audio_enabled: true,
                    video_enabled: false,
                },
            },
            MediaOperation::Observe { target, .. } | MediaOperation::Wait { target, .. } => {
                MediaResult::Events {
                    batch: MediaEventBatch::bounded(target, Vec::<MediaEvent>::new(), false)
                        .unwrap(),
                }
            }
            MediaOperation::Stop {
                target: MediaResourceRef::Recording(recording),
            } => MediaResult::RecordingStopped {
                recording,
                asset: self.asset.clone(),
            },
            MediaOperation::Stop { target } => MediaResult::Stopped { resource: target },
        };
        if is_session_start {
            if let Some((session, profile)) = &self.profile_change_after_start {
                let mut session = session.lock().await;
                session.model = "model-after-switch".into();
                session.model_profile = Some(profile.clone());
            }
        }
        Ok(result)
    }

    async fn read_media(
        &self,
        context: MediaOperationContext,
        media: MediaRef,
    ) -> Result<MediaContent, MediaError> {
        self.reads.lock().unwrap().push(context);
        if media != self.asset.reference {
            return Err(MediaError::new(
                MediaErrorKind::NotFound,
                "unknown test asset",
            ));
        }
        Ok(MediaContent {
            reference: media,
            metadata: self.asset.metadata.clone(),
            bytes: self.image.clone(),
        })
    }

    async fn cancel(&self, _identity: MediaOperationId) -> Result<(), MediaError> {
        Ok(())
    }
}

struct MutableCapabilityMedia {
    capabilities: Mutex<MediaCapabilities>,
}

impl MutableCapabilityMedia {
    fn new(operations: &[MediaOperationKind]) -> Self {
        Self {
            capabilities: Mutex::new(MediaCapabilities {
                service_epoch: 41,
                support_revision: 1,
                supported_operations: operations.to_vec(),
                supported_media_kinds: vec![MediaKind::Audio, MediaKind::Image, MediaKind::Video],
                readiness: Vec::new(),
                max_payload_bytes: 4 * 1024 * 1024,
            }),
        }
    }

    fn set_operations(&self, operations: &[MediaOperationKind]) {
        let mut capabilities = self.capabilities.lock().unwrap();
        capabilities.supported_operations = operations.to_vec();
        capabilities.support_revision = capabilities.support_revision.saturating_add(1);
    }
}

#[async_trait]
impl media_api::MediaService for MutableCapabilityMedia {
    fn capabilities(&self) -> MediaCapabilities {
        self.capabilities.lock().unwrap().clone()
    }

    fn capabilities_for(&self, _owner: &MediaOwner) -> MediaCapabilities {
        self.capabilities()
    }

    async fn execute(
        &self,
        _context: MediaOperationContext,
        _operation: MediaOperation,
    ) -> Result<MediaResult, MediaError> {
        Err(MediaError::new(
            MediaErrorKind::Unsupported,
            "mutable capability fixture does not execute operations",
        ))
    }

    async fn read_media(
        &self,
        _context: MediaOperationContext,
        _media: MediaRef,
    ) -> Result<MediaContent, MediaError> {
        Err(MediaError::new(
            MediaErrorKind::Unsupported,
            "mutable capability fixture does not read media",
        ))
    }

    async fn cancel(&self, _identity: MediaOperationId) -> Result<(), MediaError> {
        Ok(())
    }
}

#[derive(Default)]
struct FakeHandoff(Mutex<Vec<MediaAgentHandoffRequest>>);

impl MediaAgentHandoffHandle for FakeHandoff {
    fn request(
        &self,
        _invocation: &ToolUseContext,
        _capability_context: &tool_api::MediaAgentCapabilityContext,
        request: MediaAgentHandoffRequest,
    ) -> Result<(), String> {
        self.0.lock().unwrap().push(request);
        Ok(())
    }
}

struct RejectingHandoff;

impl MediaAgentHandoffHandle for RejectingHandoff {
    fn supported_modes(&self) -> Vec<MediaSessionMode> {
        vec![MediaSessionMode::Audio]
    }

    fn request(
        &self,
        _invocation: &ToolUseContext,
        _capability_context: &tool_api::MediaAgentCapabilityContext,
        _request: MediaAgentHandoffRequest,
    ) -> Result<(), String> {
        Err("controlled handoff rejection".into())
    }
}

#[derive(Default)]
struct RouteAwareHandoff(Mutex<Vec<MediaAgentHandoffRequest>>);

impl RouteAwareHandoff {
    fn modes_for(profile: Option<&str>) -> Vec<MediaSessionMode> {
        match profile {
            Some("profile-av") => vec![MediaSessionMode::Audio, MediaSessionMode::AudioVideo],
            Some("profile-audio") => vec![MediaSessionMode::Audio],
            _ => Vec::new(),
        }
    }
}

impl MediaAgentHandoffHandle for RouteAwareHandoff {
    fn supported_modes(&self) -> Vec<MediaSessionMode> {
        vec![MediaSessionMode::Audio, MediaSessionMode::AudioVideo]
    }

    fn supported_modes_for_context(
        &self,
        context: &tool_api::MediaAgentCapabilityContext,
    ) -> Vec<MediaSessionMode> {
        Self::modes_for(context.profile_name.as_deref())
    }

    fn supports_mode(
        &self,
        context: &tool_api::MediaAgentCapabilityContext,
        mode: MediaSessionMode,
    ) -> bool {
        Self::modes_for(context.profile_name.as_deref()).contains(&mode)
    }

    fn request(
        &self,
        _invocation: &ToolUseContext,
        context: &tool_api::MediaAgentCapabilityContext,
        request: MediaAgentHandoffRequest,
    ) -> Result<(), String> {
        if !self.supports_mode(context, request.mode) {
            return Err("unsupported route mode".into());
        }
        self.0.lock().unwrap().push(request);
        Ok(())
    }
}

fn image_png() -> Vec<u8> {
    let image = RgbImage::from_pixel(12, 8, Rgb([120, 50, 220]));
    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .unwrap();
    cursor.into_inner()
}

fn image_asset(bytes: &[u8]) -> MediaAsset {
    MediaAsset {
        reference: MediaRef("stored-image-1".into()),
        metadata: MediaMetadata {
            kind: MediaKind::Image,
            mime_type: "image/png".into(),
            byte_len: bytes.len() as u64,
            captured_at_unix_ms: Some(1_738_000_123_000),
            duration_ms: None,
            width_px: Some(12),
            height_px: Some(8),
            sample_rate_hz: None,
            channels: None,
        },
    }
}

fn builtin_context(service: Option<Arc<dyn media_api::MediaService>>) -> BuiltinToolContext {
    let mut context = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    context.media = service;
    context
}

fn use_context() -> ToolUseContext {
    let mut context = tool_api::test_support::fresh_ctx();
    context.origin_session_id = Some(lingxi_core::types::SessionId::new());
    context
}

fn main_agent_context(
    profile: &str,
) -> (
    ToolUseContext,
    Arc<tokio::sync::Mutex<lingxi_core::SessionState>>,
) {
    let mut session = lingxi_core::SessionState::empty(
        lingxi_core::types::SessionId::new(),
        "model-before-switch".into(),
    );
    session.model_profile = Some(profile.to_owned());
    let session = Arc::new(tokio::sync::Mutex::new(session));
    let mut context = tool_api::test_support::fresh_ctx();
    context.session = Some(session.clone());
    (context, session)
}

async fn call(
    tool: &dyn Tool,
    input: Value,
    use_context: ToolUseContext,
) -> Result<ToolCallResult, ToolError> {
    tool.call(
        generated_projection(&input),
        use_context,
        tool_api::test_support::fresh_tx(),
    )
    .await
}

async fn prompt_schema(tool: &MediaTool, profile: &str) -> Value {
    let wire = tool_api::wire::tool_to_wire(
        tool,
        &PromptOptions {
            session_id: Some(lingxi_core::types::SessionId::new()),
            is_scheduled_turn: false,
            model: Some("model-before-switch".into()),
            model_profile: Some(profile.into()),
            ..PromptOptions::default()
        },
    )
    .await;
    serde_json::from_str(&wire.input_schema.to_json_string().unwrap()).unwrap()
}

fn result_json(result: &ToolCallResult) -> Value {
    serde_json::from_str(&result.data.to_json_string().unwrap()).unwrap()
}

#[test]
fn media_tool_names_are_one_unified_set_without_legacy_aliases() {
    assert_eq!(
        TOOL_NAMES,
        [
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
            "media_stop"
        ]
    );
    let all = all_operation_kinds()
        .into_iter()
        .map(|(_, name)| name)
        .collect::<Vec<_>>();
    for legacy in ["camera", "voice", "speech"] {
        assert!(!all.contains(&legacy));
    }
}

#[test]
fn registration_keeps_all_operations_but_advertises_supported_busy_and_permission_gated_tools() {
    let service = Arc::new(FakeMedia::capabilities_with_readiness(
        vec![MediaOperationKind::Capture, MediaOperationKind::Record],
        vec![
            MediaOperationReadiness {
                operation: MediaOperationKind::Capture,
                state: MediaReadinessState::NeedsPermission,
            },
            MediaOperationReadiness {
                operation: MediaOperationKind::Record,
                state: MediaReadinessState::Busy,
            },
        ],
    ));
    let mut registry = ToolRegistry::new();
    register_all(&mut registry, builtin_context(Some(service)));
    assert_eq!(registry.all_names().len(), TOOL_NAMES.len());
    let available = registry
        .available_tools(&tool_api::tool_trait::ToolStaticContext::default())
        .into_iter()
        .map(|tool| tool.name().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        available,
        ["media_capture".to_owned(), "media_record".to_owned()]
    );
}

#[test]
fn capability_changes_update_enabled_media_tools_without_registry_rebuild() {
    let service = Arc::new(MutableCapabilityMedia::new(&[
        MediaOperationKind::Sources,
        MediaOperationKind::Status,
    ]));
    let mut registry = ToolRegistry::new();
    register_all(&mut registry, builtin_context(Some(service.clone())));

    assert_eq!(registry.all_names().len(), TOOL_NAMES.len());
    let enabled_names = || {
        registry
            .available_tools(&tool_api::tool_trait::ToolStaticContext::default())
            .into_iter()
            .map(|tool| tool.name().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        enabled_names(),
        ["media_sources".to_owned(), "media_status".to_owned()]
    );

    service.set_operations(&[
        MediaOperationKind::Sources,
        MediaOperationKind::Status,
        MediaOperationKind::SessionStart,
    ]);
    assert!(enabled_names().contains(&"media_session_start".to_owned()));

    service.set_operations(&[MediaOperationKind::Sources, MediaOperationKind::Status]);
    assert!(!enabled_names().contains(&"media_session_start".to_owned()));
}

#[test]
fn realtime_catalog_metadata_preserves_camera_and_filters_exclusive_resources() {
    let realtime_resources = [MediaResource::Microphone, MediaResource::Speaker];
    let capture = MediaTool::new(
        builtin_context(None),
        MediaOperationKind::Capture,
        "media_capture",
    );
    let record = MediaTool::new(
        builtin_context(None),
        MediaOperationKind::Record,
        "media_record",
    );
    let play = MediaTool::new(
        builtin_context(None),
        MediaOperationKind::Play,
        "media_play",
    );
    assert!(!capture
        .catalog_execution_metadata()
        .conflicts_with(&realtime_resources));
    assert!(!record
        .catalog_execution_metadata()
        .conflicts_with(&realtime_resources));
    assert!(record
        .execution_metadata(&generated_projection(
            &json!({"source_ref": "unknown-source"})
        ))
        .conflicts_with(&realtime_resources));
    assert!(play
        .catalog_execution_metadata()
        .conflicts_with(&realtime_resources));

    let transcribe = MediaTool::new(
        builtin_context(None),
        MediaOperationKind::Transcribe,
        "media_transcribe",
    );
    let stored = generated_projection(&json!({
        "source":{"kind":"stored","media_ref":"audio-1"}
    }));
    let live = generated_projection(&json!({
        "source":{"kind":"live","source_ref":"mic-1"}
    }));
    assert!(!transcribe
        .catalog_execution_metadata()
        .conflicts_with(&realtime_resources));
    assert!(!transcribe
        .execution_metadata(&stored)
        .conflicts_with(&realtime_resources));
    assert!(transcribe
        .execution_metadata(&live)
        .conflicts_with(&realtime_resources));

    let malformed_record = generated_projection(&json!({}));
    assert!(record
        .execution_metadata(&malformed_record)
        .conflicts_with(&realtime_resources));

    let source_kinds = Arc::new(StdMutex::new(HashMap::from([
        (
            (31, "camera-1".to_owned()),
            Some(SourceKindInfo {
                source_kind: MediaSourceKind::Camera,
                media_kind: Some(MediaKind::Image),
            }),
        ),
        (
            (31, "microphone-1".to_owned()),
            Some(SourceKindInfo {
                source_kind: MediaSourceKind::Microphone,
                media_kind: Some(MediaKind::Audio),
            }),
        ),
    ])));
    let capture_service = Arc::new(FakeMedia::new(&[MediaOperationKind::Capture]));
    let capture = MediaTool::with_source_kinds(
        builtin_context(Some(capture_service)),
        MediaOperationKind::Capture,
        "media_capture",
        source_kinds.clone(),
    );
    let camera = generated_projection(&json!({"source_ref":"camera-1"}));
    let microphone = generated_projection(&json!({"source_ref":"microphone-1"}));
    assert_eq!(
        capture
            .execution_metadata(&camera)
            .exclusive_media_resources,
        [MediaResource::Camera]
    );
    assert_eq!(
        capture
            .execution_metadata(&microphone)
            .exclusive_media_resources,
        [MediaResource::Microphone]
    );
    assert!(capture
        .execution_metadata(&microphone)
        .conflicts_with(&realtime_resources));
    let record = MediaTool::with_source_kinds(
        builtin_context(Some(Arc::new(FakeMedia::new(&[
            MediaOperationKind::Record,
        ])))),
        MediaOperationKind::Record,
        "media_record",
        source_kinds.clone(),
    );
    assert!(
        record
            .execution_metadata(&microphone)
            .conflicts_with(&realtime_resources),
        "a second recording from the active microphone conflicts"
    );
    let observe = MediaTool::with_source_kinds(
        builtin_context(Some(Arc::new(FakeMedia::new(&[
            MediaOperationKind::Observe,
        ])))),
        MediaOperationKind::Observe,
        "media_observe",
        source_kinds.clone(),
    );
    let camera_observation = generated_projection(&json!({
        "target":{"kind":"source","ref":"camera-1"}
    }));
    assert_eq!(
        observe
            .execution_metadata(&camera_observation)
            .exclusive_media_resources,
        [MediaResource::Camera]
    );
    assert!(
        !observe
            .execution_metadata(&camera_observation)
            .conflicts_with(&realtime_resources),
        "camera observations stay available when native realtime owns audio"
    );
    let session_observation = generated_projection(&json!({
        "target":{"kind":"session","ref":"active-session"}
    }));
    assert!(observe
        .execution_metadata(&session_observation)
        .exclusive_media_resources
        .is_empty());
    assert!(
        !observe
            .execution_metadata(&session_observation)
            .conflicts_with(&realtime_resources),
        "observing buffered session events does not acquire another device lease"
    );
    let microphone_observation = generated_projection(&json!({
        "target":{"kind":"source","ref":"microphone-1"}
    }));
    assert!(observe
        .execution_metadata(&microphone_observation)
        .conflicts_with(&realtime_resources));
    let unknown = generated_projection(&json!({"source_ref":"unknown-source"}));
    assert!(capture
        .execution_metadata(&unknown)
        .conflicts_with(&realtime_resources));

    let ambiguous = Arc::new(StdMutex::new(HashMap::from([(
        (31, "shared-ref".to_owned()),
        None,
    )])));
    let capture_service = Arc::new(FakeMedia::new(&[MediaOperationKind::Capture]));
    let capture = MediaTool::with_source_kinds(
        builtin_context(Some(capture_service)),
        MediaOperationKind::Capture,
        "media_capture",
        ambiguous,
    );
    let shared_ref = generated_projection(&json!({"source_ref":"shared-ref"}));
    assert_eq!(
        capture
            .execution_metadata(&shared_ref)
            .exclusive_media_resources,
        [MediaResource::Microphone, MediaResource::Camera]
    );
}

#[test]
fn no_backend_registers_no_media_tool() {
    let mut registry = ToolRegistry::new();
    register_all(&mut registry, builtin_context(None));
    assert!(registry.all_names().is_empty());
}

#[tokio::test]
async fn source_inventory_narrows_capture_resource_needs_for_shared_tools() {
    let service = Arc::new(FakeMedia::new(&[
        MediaOperationKind::Sources,
        MediaOperationKind::Capture,
    ]));
    let service_trait: Arc<dyn media_api::MediaService> = service;
    let mut registry = ToolRegistry::new();
    register_all(&mut registry, builtin_context(Some(service_trait)));
    let sources = registry.find_registered("media_sources").unwrap();
    let capture = registry.find_registered("media_capture").unwrap();
    let _ = call(&*sources, json!({}), use_context()).await.unwrap();
    let input = generated_projection(&json!({"source_ref":"camera-device"}));
    assert_eq!(
        capture.execution_metadata(&input).exclusive_media_resources,
        [MediaResource::Camera]
    );
}

#[tokio::test]
async fn media_failures_preserve_unavailable_busy_and_permission_distinctions() {
    for (kind, expected) in [
        (MediaErrorKind::Unavailable, "unavailable"),
        (MediaErrorKind::Busy, "busy"),
    ] {
        let service = Arc::new(FakeMedia::with_failure(
            &[MediaOperationKind::Capture],
            kind,
        ));
        let tool = MediaTool::new(
            builtin_context(Some(service)),
            MediaOperationKind::Capture,
            "media_capture",
        );
        let error = call(&tool, json!({"source_ref":"camera-device"}), use_context())
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Internal(message) if message.contains(expected)));
    }

    let service = Arc::new(FakeMedia::with_failure(
        &[MediaOperationKind::Capture],
        MediaErrorKind::PermissionDenied,
    ));
    let tool = MediaTool::new(
        builtin_context(Some(service)),
        MediaOperationKind::Capture,
        "media_capture",
    );
    assert!(matches!(
        call(&tool, json!({"source_ref":"camera-device"}), use_context()).await,
        Err(ToolError::PermissionDenied(_))
    ));
}

#[tokio::test]
async fn tool_operations_use_the_same_uuid_owner_as_host_session_channels() {
    let service: Arc<dyn media_api::MediaService> =
        Arc::new(FakeMedia::new(&[MediaOperationKind::Capture]));
    let (mut invocation, session) = main_agent_context("test-profile");
    let main_id = session.lock().await.session_id;
    let main = operation_context(&service, &invocation, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(
        main.owner,
        MediaOwner::Session {
            session_id: main_id.as_uuid().to_string()
        }
    );

    let origin_id = lingxi_core::types::SessionId::new();
    invocation.origin_session_id = Some(origin_id);
    let delegated = operation_context(&service, &invocation, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(
        delegated.owner,
        MediaOwner::Session {
            session_id: origin_id.as_uuid().to_string()
        }
    );
    assert_ne!(main.owner, delegated.owner);
}

#[tokio::test]
async fn owner_supported_operation_is_not_rejected_by_narrow_catalog_capabilities() {
    let service =
        Arc::new(FakeMedia::new(&[]).with_owner_capabilities(&[MediaOperationKind::Capture]));
    let tool = MediaTool::new(
        builtin_context(Some(service.clone())),
        MediaOperationKind::Capture,
        "media_capture",
    );
    let result = call(&tool, json!({"source_ref":"camera-device"}), use_context())
        .await
        .expect("owner-specific capability should permit this operation");
    assert_eq!(result_json(&result)["type"], "image");
    assert!(matches!(
        service.last_operation().0.owner,
        MediaOwner::Session { .. }
    ));
}

#[tokio::test]
async fn captured_pixels_use_the_shared_budget_and_model_image_projection() {
    let service = Arc::new(FakeMedia::new(&[MediaOperationKind::Capture]));
    let tool = MediaTool::new(
        builtin_context(Some(service.clone())),
        MediaOperationKind::Capture,
        "media_capture",
    );
    let result = call(&tool, json!({"source_ref":"camera-device"}), use_context())
        .await
        .unwrap();
    let result_data = result_json(&result);
    assert_eq!(result_data["type"], "image");
    assert_eq!(result_data["captured_at_unix_ms"], 1_738_000_123_000_u64);
    assert_eq!(result_data["operation_ref"]["kind"], "operation");
    assert!(result.model_content.is_some());

    let blocks = tool_api::tool_result_media::media_content_blocks(&result.data)
        .expect("captured image must become a model-visible image block");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].string_at("/type"), Some("image"));
    assert_eq!(blocks[0].string_at("/source/media_type"), Some("image/png"));
    let pixel_bytes = base64::engine::general_purpose::STANDARD
        .decode(blocks[0].string_at("/source/data").unwrap())
        .unwrap();
    assert_eq!(
        image::load_from_memory(&pixel_bytes).unwrap().dimensions(),
        (12, 8)
    );

    let (operation_context, operation) = service.last_operation();
    let MediaOperation::Capture { source, .. } = operation else {
        panic!("expected capture request")
    };
    assert_eq!(source.0 .0, "camera-device");
    assert!(matches!(
        operation_context.owner,
        MediaOwner::Session { .. }
    ));
    let read_context = service.reads.lock().unwrap().last().unwrap().clone();
    assert_eq!(read_context.owner, operation_context.owner);
}

#[tokio::test]
async fn transcription_discriminates_live_and_stored_inputs_and_returns_text() {
    let service = Arc::new(FakeMedia::new(&[MediaOperationKind::Transcribe]));
    let tool = MediaTool::new(
        builtin_context(Some(service.clone())),
        MediaOperationKind::Transcribe,
        "media_transcribe",
    );
    let result = call(
        &tool,
        json!({"source":{"kind":"stored","media_ref":"stored-audio-1"},"language":"en-US"}),
        use_context(),
    )
    .await
    .unwrap();
    let data = result_json(&result);
    assert_eq!(data["text"], "recognized words");
    assert_eq!(data["language"], "en-US");
    assert!((data["confidence"].as_f64().unwrap() - 0.94).abs() < 0.000_001);
    let (_, operation) = service.last_operation();
    assert!(matches!(
        operation,
        MediaOperation::Transcribe {
            input: TranscriptionInput::StoredMedia(MediaRef(reference)),
            ..
        } if reference == "stored-audio-1"
    ));

    let live = generated_projection(&json!({
        "source":{"kind":"live","source_ref":"microphone-1"}
    }));
    let metadata = tool.execution_metadata(&live);
    assert_eq!(
        metadata.exclusive_media_resources,
        [MediaResource::Microphone]
    );
    let invalid =
        generated_projection(&json!({"source":{"kind":"stored","source_ref":"wrong-field"}}));
    assert!(parse_operation(MediaOperationKind::Transcribe, &invalid).is_err());
}

#[tokio::test]
async fn session_handoff_is_requested_only_for_conversation_intent() {
    let service = Arc::new(FakeMedia::new(&[
        MediaOperationKind::SessionStart,
        MediaOperationKind::Stop,
    ]));
    let handoff = Arc::new(FakeHandoff::default());
    let mut context = builtin_context(Some(service.clone()));
    context.media_agent_handoff = Some(handoff.clone());
    let tool = MediaTool::new(
        context,
        MediaOperationKind::SessionStart,
        "media_session_start",
    );
    let (conversation_context, _) = main_agent_context("profile-a");
    let conversation = call(
        &tool,
        json!({
            "intent":"conversation",
            "mode":"audio",
            "audio_source_ref":"microphone-1"
        }),
        conversation_context,
    )
    .await
    .unwrap();
    let conversation_data = result_json(&conversation);
    assert_eq!(conversation_data["session_ref"], "session-1");
    assert_eq!(handoff.0.lock().unwrap().len(), 1);
    let request = handoff.0.lock().unwrap()[0].clone();
    assert_eq!(request.session.0 .0, "session-1");
    assert_eq!(request.mode, MediaSessionMode::Audio);
    assert_eq!(
        conversation_data["operation_ref"]["ref"],
        request.operation_id.id
    );

    let _ = call(
        &tool,
        json!({
            "intent":"observation",
            "mode":"video",
            "video_source_ref":"camera-device"
        }),
        use_context(),
    )
    .await
    .unwrap();
    assert_eq!(handoff.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn conversation_schema_and_dispatch_use_the_exact_session_route() {
    let service = Arc::new(FakeMedia::new(&[
        MediaOperationKind::SessionStart,
        MediaOperationKind::Stop,
    ]));
    let handoff = Arc::new(RouteAwareHandoff::default());
    let (audio_invocation, _) = main_agent_context("profile-audio");
    let (av_invocation, _) = main_agent_context("profile-av");
    let session_id_for = |invocation: &ToolUseContext| {
        invocation
            .session
            .as_ref()
            .expect("main-agent invocation has a session")
            .try_lock()
            .expect("test session is not contended")
            .session_id
    };
    let tool_for = |invocation: &ToolUseContext| {
        let mut context = builtin_context(Some(service.clone()));
        context.session_id = Some(session_id_for(invocation));
        context.media_agent_handoff = Some(handoff.clone());
        MediaTool::new(
            context,
            MediaOperationKind::SessionStart,
            "media_session_start",
        )
    };
    let audio_tool = tool_for(&audio_invocation);
    let av_tool = tool_for(&av_invocation);

    let audio_schema = prompt_schema(&audio_tool, "profile-audio").await;
    let av_schema = prompt_schema(&av_tool, "profile-av").await;
    assert_eq!(
        audio_schema["allOf"][0]["then"]["properties"]["mode"]["enum"],
        json!(["audio"])
    );
    assert_eq!(
        av_schema["allOf"][0]["then"]["properties"]["mode"]["enum"],
        json!(["audio", "audio_video"])
    );

    let error = call(
        &audio_tool,
        json!({
            "intent":"conversation",
            "mode":"audio_video",
            "audio_source_ref":"microphone-1",
            "video_source_ref":"camera-device"
        }),
        audio_invocation,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ToolError::InteractionRequired(_)));
    assert!(service.operations.lock().unwrap().is_empty());

    let result = call(
        &av_tool,
        json!({
            "intent":"conversation",
            "mode":"audio_video",
            "audio_source_ref":"microphone-1",
            "video_source_ref":"camera-device"
        }),
        av_invocation,
    )
    .await
    .expect("the exact AV route supports the requested mode");
    assert_eq!(result_json(&result)["session_ref"], "session-1");
    assert_eq!(handoff.0.lock().unwrap().len(), 1);
    assert_eq!(
        handoff.0.lock().unwrap()[0].mode,
        MediaSessionMode::AudioVideo
    );
}

#[tokio::test]
async fn session_handoff_rejects_profile_change_and_stops_opened_session() {
    let (use_context, session) = main_agent_context("profile-before");
    let service = Arc::new(
        FakeMedia::new(&[MediaOperationKind::SessionStart, MediaOperationKind::Stop])
            .with_profile_change_after_start(session, "profile-after"),
    );
    let handoff = Arc::new(FakeHandoff::default());
    let mut context = builtin_context(Some(service.clone()));
    context.media_agent_handoff = Some(handoff.clone());
    let tool = MediaTool::new(
        context,
        MediaOperationKind::SessionStart,
        "media_session_start",
    );

    let error = call(
        &tool,
        json!({
            "intent":"conversation",
            "mode":"audio",
            "audio_source_ref":"microphone-1"
        }),
        use_context,
    )
    .await
    .unwrap_err();

    assert!(matches!(error, ToolError::InteractionRequired(_)));
    assert!(handoff.0.lock().unwrap().is_empty());
    let operations = service.operations.lock().unwrap();
    assert!(matches!(
        operations[0].1,
        MediaOperation::SessionStart { .. }
    ));
    assert!(matches!(
        operations[1].1,
        MediaOperation::Stop {
            target: MediaResourceRef::Session(_)
        }
    ));
}

#[tokio::test]
async fn rejected_handoff_with_failed_cleanup_returns_session_ref_for_stop_retry() {
    let service = Arc::new(
        FakeMedia::new(&[MediaOperationKind::SessionStart, MediaOperationKind::Stop])
            .with_stop_failures(1),
    );
    let mut context = builtin_context(Some(service.clone()));
    context.media_agent_handoff = Some(Arc::new(RejectingHandoff));
    let start_tool = MediaTool::new(
        context,
        MediaOperationKind::SessionStart,
        "media_session_start",
    );
    let (use_context, _) = main_agent_context("profile-a");

    let result = call(
        &start_tool,
        json!({
            "intent":"conversation",
            "mode":"audio",
            "audio_source_ref":"microphone-1"
        }),
        use_context.clone(),
    )
    .await
    .expect("a failed cleanup returns a recoverable tool error result");
    let data = result_json(&result);
    assert!(result.is_error);
    assert_eq!(data["session_ref"], "session-1");
    assert_eq!(data["handoff_error"], "controlled handoff rejection");
    assert!(data["cleanup_error"]
        .as_str()
        .is_some_and(|message| message.contains("controlled stop failure")));
    assert_eq!(
        data["recovery"],
        json!({
            "tool":"media_stop",
            "target":{"kind":"session","ref":"session-1"}
        })
    );
    assert!(result
        .model_content
        .as_ref()
        .and_then(|content| content.string_at(""))
        .is_some_and(|content| content.contains("media_stop") && content.contains("session-1")));

    let stop_tool = MediaTool::new(
        builtin_context(Some(service.clone())),
        MediaOperationKind::Stop,
        "media_stop",
    );
    let cleanup_target = data["recovery"]["target"].clone();
    let retry = call(&stop_tool, json!({"target":cleanup_target}), use_context)
        .await
        .expect("the surfaced typed session reference can be stopped on retry");
    assert!(!retry.is_error);
    assert_eq!(service.operations.lock().unwrap().len(), 3);
    assert!(matches!(
        service.last_operation().1,
        MediaOperation::Stop {
            target: MediaResourceRef::Session(_)
        }
    ));
}

#[tokio::test]
async fn video_conversation_is_rejected_before_media_session_open() {
    let service = Arc::new(FakeMedia::new(&[MediaOperationKind::SessionStart]));
    let handoff = Arc::new(FakeHandoff::default());
    let mut context = builtin_context(Some(service.clone()));
    context.media_agent_handoff = Some(handoff);
    let tool = MediaTool::new(
        context,
        MediaOperationKind::SessionStart,
        "media_session_start",
    );

    let schema: Value =
        serde_json::from_str(&tool.input_schema().to_json_string().unwrap()).unwrap();
    assert_eq!(
        schema["allOf"][0]["then"]["properties"]["mode"]["enum"],
        json!(["audio"])
    );
    let error = call(
        &tool,
        json!({
            "intent":"conversation",
            "mode":"video",
            "video_source_ref":"camera-device"
        }),
        use_context(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ToolError::InteractionRequired(_)));
    assert!(service.operations.lock().unwrap().is_empty());

    let observation = call(
        &tool,
        json!({
            "intent":"observation",
            "mode":"video",
            "video_source_ref":"camera-device"
        }),
        use_context(),
    )
    .await
    .expect("observation video does not need an Agent audio lane");
    assert_eq!(result_json(&observation)["session_ref"], "session-1");
}

#[tokio::test]
async fn conversation_intent_is_not_advertised_or_started_without_handoff_support() {
    let service = Arc::new(FakeMedia::new(&[MediaOperationKind::SessionStart]));
    let tool = MediaTool::new(
        builtin_context(Some(service.clone())),
        MediaOperationKind::SessionStart,
        "media_session_start",
    );
    let schema: Value =
        serde_json::from_str(&tool.input_schema().to_json_string().unwrap()).unwrap();
    assert_eq!(
        schema["properties"]["intent"]["enum"],
        json!(["observation"])
    );
    let error = call(
        &tool,
        json!({
            "intent":"conversation",
            "mode":"audio",
            "audio_source_ref":"mic-1"
        }),
        use_context(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ToolError::InteractionRequired(_)));
    assert!(service.operations.lock().unwrap().is_empty());
}

#[test]
fn malformed_operation_target_requires_host_minted_generation_and_epoch() {
    let missing_identity = generated_projection(&json!({
        "target":{"kind":"operation","ref":"operation-id"}
    }));
    assert!(parse_operation(MediaOperationKind::Stop, &missing_identity).is_err());

    let typed_target = generated_projection(&json!({
        "target":{
            "kind":"operation",
            "ref":"operation-id",
            "generation":42,
            "service_epoch":7
        }
    }));
    let MediaOperation::Stop {
        target: MediaResourceRef::Operation(identity),
    } = parse_operation(MediaOperationKind::Stop, &typed_target).unwrap()
    else {
        panic!("expected operation stop target")
    };
    assert_eq!(identity.generation, 42);
    assert_eq!(identity.service_epoch, 7);
}
