use super::*;
use async_trait::async_trait;
use media_api::{
    MediaCapabilities, MediaError, MediaErrorKind, MediaKind, MediaOperation,
    MediaOperationContext, MediaOperationId, MediaOperationKind, MediaResourceRef,
    MediaResourceState, MediaResourceStatus, MediaResult, MediaSessionId, MediaSessionRef,
};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

struct OwnerCleanupMediaService {
    stops: StdMutex<Vec<MediaResourceRef>>,
    stop_count: AtomicUsize,
    changed: tokio::sync::Notify,
}

struct CleanupMediaAttachment;

#[async_trait]
impl orchestrator::MediaConversationAttachment for CleanupMediaAttachment {
    async fn open(
        &self,
        _request: tool_api::MediaAgentHandoffRequest,
        _prepared: orchestrator::RealtimeAgentContext,
        _audio: Arc<dyn media_api::MediaAudioChannel>,
    ) -> Result<RealtimeAgentSessionIo, String> {
        Err("cleanup fixture never opens realtime".into())
    }
}

impl OwnerCleanupMediaService {
    fn new() -> Self {
        Self {
            stops: StdMutex::new(Vec::new()),
            stop_count: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn stop_targets(&self) -> Vec<MediaResourceRef> {
        self.stops.lock().unwrap().clone()
    }

    async fn wait_for_stops(&self, expected: usize) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.stop_count.load(Ordering::SeqCst) >= expected {
                return;
            }
            changed.await;
        }
    }
}

#[async_trait]
impl media_api::MediaService for OwnerCleanupMediaService {
    fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            service_epoch: 41,
            support_revision: 1,
            supported_operations: vec![MediaOperationKind::Status, MediaOperationKind::Stop],
            supported_media_kinds: vec![MediaKind::Audio],
            readiness: Vec::new(),
            max_payload_bytes: 1024,
        }
    }

    async fn execute(
        &self,
        _context: MediaOperationContext,
        operation: MediaOperation,
    ) -> Result<MediaResult, MediaError> {
        match operation {
            MediaOperation::Status { target: None } => Ok(MediaResult::Status {
                resources: vec![
                    MediaResourceStatus {
                        resource: MediaResourceRef::Operation(MediaOperationId::new(90, 41)),
                        state: MediaResourceState::Active,
                        started_at_unix_ms: None,
                        updated_at_unix_ms: None,
                    },
                    MediaResourceStatus {
                        resource: MediaResourceRef::Session(MediaSessionRef(MediaSessionId::from(
                            "active-observation-session",
                        ))),
                        state: MediaResourceState::Active,
                        started_at_unix_ms: None,
                        updated_at_unix_ms: None,
                    },
                    MediaResourceStatus {
                        resource: MediaResourceRef::Media(media_api::MediaRef(
                            "completed-asset".into(),
                        )),
                        state: MediaResourceState::Completed,
                        started_at_unix_ms: None,
                        updated_at_unix_ms: None,
                    },
                ],
            }),
            MediaOperation::Stop { target } => {
                self.stops.lock().unwrap().push(target.clone());
                self.stop_count.fetch_add(1, Ordering::SeqCst);
                self.changed.notify_waiters();
                Ok(MediaResult::Stopped { resource: target })
            }
            _ => Err(MediaError::new(
                MediaErrorKind::Unsupported,
                "cleanup fixture supports status and stop only",
            )),
        }
    }

    async fn read_media(
        &self,
        _context: MediaOperationContext,
        _media: media_api::MediaRef,
    ) -> Result<media_api::MediaContent, MediaError> {
        Err(MediaError::new(
            MediaErrorKind::Unsupported,
            "not used by owner cleanup fixture",
        ))
    }

    async fn cancel(&self, _identity: MediaOperationId) -> Result<(), MediaError> {
        Ok(())
    }
}

fn queue_pending_conversation(
    queue: &MobileMediaAgentHandoffQueue,
    session_id: lingxi_core::types::SessionId,
) {
    use tool_api::MediaAgentHandoffHandle as _;

    let session_key = session_id.as_uuid().to_string();
    queue.set_attachment(Some(Arc::new(CleanupMediaAttachment)));
    let context = {
        let mut context = tool_api::test_support::fresh_ctx();
        context.session = Some(Arc::new(tokio::sync::Mutex::new(
            lingxi_core::SessionState::empty(session_id, "test-model".into()),
        )));
        let assistant_message_id = lingxi_core::types::MessageId::new();
        let tool_use_id = lingxi_core::types::ToolUseId::new();
        context.assistant_message_id = Some(assistant_message_id.clone());
        context.tool_use_id = Some(tool_use_id.clone());
        context.assistant_message = Some(lingxi_core::types::ConversationMessage::Assistant {
            id: assistant_message_id,
            content: vec![lingxi_core::types::ContentBlock::ToolUse {
                id: tool_use_id,
                name: "media_session_start".into(),
                input: Utf16JsonProjection::from_serde_value(&serde_json::json!({
                    "intent":"conversation",
                    "mode":"audio"
                }))
                .expect("fixture input is valid JSON"),
                provider_id: None,
            }],
            stop_reason: Some("tool_use".into()),
            per_turn_effort: None,
            resumable: None,
        });
        context
    };
    let capability_context = orchestrator::MediaAgentCapabilityContext {
        session_id: session_key,
        model: "test-model".into(),
        profile_name: None,
        account_scope: None,
    };
    queue
        .request(
            &context,
            &capability_context,
            tool_api::MediaAgentHandoffRequest {
                operation_id: MediaOperationId::new(1, 41),
                session: MediaSessionRef(MediaSessionId::from("media-session-1")),
                mode: media_api::MediaSessionMode::Audio,
            },
        )
        .expect("fixture queues a current-session conversation handoff");
}

fn stopped_observation_operation(stopped: &[MediaResourceRef]) -> bool {
    stopped.iter().any(|resource| {
        matches!(
            resource,
            MediaResourceRef::Operation(identity)
                if identity.generation == 90 && identity.service_epoch == 41
        )
    })
}

fn cleanup_config(root: &Path, service: Arc<OwnerCleanupMediaService>) -> MobileConfig {
    use crate::mobile::test_support::test_config;

    let mut config = test_config(root);
    config.media_service = Some(service);
    config.media_conversation_attachment = Some(Arc::new(CleanupMediaAttachment));
    config
}

#[test]
fn session_retarget_stops_pending_handoff_and_active_owner_resources() {
    use crate::mobile::test_support::{
        new_engine_with_streaming, test_config, CollectingPermissionSink, FakeListener,
        HostFakePlatform,
    };
    use orchestrator::test_support_stream::MockStreamingApiClient;

    let root = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(OwnerCleanupMediaService::new());
    let mut config = test_config(root.path());
    config.media_service = Some(service.clone());
    config.media_conversation_attachment = Some(Arc::new(CleanupMediaAttachment));
    let engine = new_engine_with_streaming(
        config,
        Arc::new(HostFakePlatform::new(root.path().to_path_buf())),
        Arc::new(FakeListener::default()),
        Arc::new(CollectingPermissionSink::default()),
        Some(Arc::new(MockStreamingApiClient::empty())),
    )
    .expect("build mobile engine");

    engine.runtime().block_on(async {
        let old_session = engine.inner.orchestrator.current_session_id().await;
        let old_key = old_session.as_uuid().to_string();
        queue_pending_conversation(&engine.inner.media_agent_handoffs, old_session);
        assert!(engine
            .inner
            .media_agent_handoffs
            .has_pending_for_session(&old_key));

        engine
            .submit(client::protocol::commands::ClientCommand::NewSession {
                cwd: None,
                model: None,
            })
            .await
            .expect("retarget to a new session");

        let new_key = engine
            .inner
            .orchestrator
            .current_session_id()
            .await
            .as_uuid()
            .to_string();
        assert_ne!(old_key, new_key);
        assert!(!engine
            .inner
            .media_agent_handoffs
            .has_pending_for_session(&old_key));
        let stopped = service.stop_targets();
        assert!(stopped.contains(&MediaResourceRef::Session(MediaSessionRef(
            MediaSessionId::from("media-session-1"),
        ))));
        assert!(stopped_observation_operation(&stopped), "{stopped:?}");
        assert!(stopped.contains(&MediaResourceRef::Session(MediaSessionRef(
            MediaSessionId::from("active-observation-session"),
        ))));
    });
}

#[test]
fn dropping_engine_cancels_turn_then_stops_pending_and_active_media_resources() {
    use crate::mobile::test_support::{
        new_engine_with_streaming, CollectingPermissionSink, FakeListener, HostFakePlatform,
    };
    use orchestrator::test_support_stream::MockStreamingApiClient;

    let root = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(OwnerCleanupMediaService::new());
    let engine = new_engine_with_streaming(
        cleanup_config(root.path(), service.clone()),
        Arc::new(HostFakePlatform::new(root.path().to_path_buf())),
        Arc::new(FakeListener::default()),
        Arc::new(CollectingPermissionSink::default()),
        Some(Arc::new(MockStreamingApiClient::empty())),
    )
    .expect("build mobile engine");
    let session_id = engine
        .runtime()
        .block_on(engine.inner.orchestrator.current_session_id());
    queue_pending_conversation(&engine.inner.media_agent_handoffs, session_id);

    drop(engine);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build assertion runtime")
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(8), service.wait_for_stops(3))
                .await
                .expect("Drop performs bounded pending and owner resource cleanup");
        });
    let stopped = service.stop_targets();
    assert!(stopped.contains(&MediaResourceRef::Session(MediaSessionRef(
        MediaSessionId::from("media-session-1"),
    ))));
    assert!(stopped_observation_operation(&stopped), "{stopped:?}");
    assert!(stopped.contains(&MediaResourceRef::Session(MediaSessionRef(
        MediaSessionId::from("active-observation-session"),
    ))));
}
