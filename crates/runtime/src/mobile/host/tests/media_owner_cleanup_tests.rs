use super::*;
use async_trait::async_trait;
use media_api::{
    MediaCapabilities, MediaError, MediaErrorKind, MediaKind, MediaOperation,
    MediaOperationContext, MediaOperationId, MediaOperationKind, MediaOwner, MediaResourceRef,
    MediaResourceState, MediaResourceStatus, MediaResult, MediaSessionId, MediaSessionRef,
};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

struct OwnerCleanupMediaService {
    operation_id: MediaOperationId,
    operation_incarnation: media_api::MediaOperationIncarnation,
    stops: StdMutex<Vec<MediaResourceRef>>,
    stopped: StdMutex<HashSet<MediaResourceRef>>,
    owners: StdMutex<Vec<MediaOwner>>,
    stop_count: AtomicUsize,
    fail_stops: std::sync::atomic::AtomicBool,
    block_stops: std::sync::atomic::AtomicBool,
    stop_gate: tokio::sync::Notify,
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
            operation_id: MediaOperationId::new(90, 41),
            operation_incarnation: media_api::MediaOperationIncarnation::new(),
            stops: StdMutex::new(Vec::new()),
            stopped: StdMutex::new(HashSet::new()),
            owners: StdMutex::new(Vec::new()),
            stop_count: AtomicUsize::new(0),
            fail_stops: std::sync::atomic::AtomicBool::new(false),
            block_stops: std::sync::atomic::AtomicBool::new(false),
            stop_gate: tokio::sync::Notify::new(),
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

    async fn wait_while_stops_blocked(&self) {
        loop {
            let released = self.stop_gate.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if !self.block_stops.load(Ordering::SeqCst) {
                return;
            }
            released.await;
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
        context: MediaOperationContext,
        operation: MediaOperation,
    ) -> Result<MediaResult, MediaError> {
        self.owners.lock().unwrap().push(context.owner.clone());
        match operation {
            MediaOperation::Status { target: None } => {
                let mut resources = vec![
                    MediaResourceStatus {
                        resource: MediaResourceRef::Operation {
                            identity: self.operation_id.clone(),
                            incarnation: self.operation_incarnation,
                        },
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
                ];
                let stopped = self.stopped.lock().unwrap();
                resources.retain(|status| !stopped.contains(&status.resource));
                Ok(MediaResult::Status { resources })
            }
            MediaOperation::Stop { target } => {
                self.stops.lock().unwrap().push(target.clone());
                self.stop_count.fetch_add(1, Ordering::SeqCst);
                self.changed.notify_waiters();
                if self.block_stops.load(Ordering::SeqCst) {
                    self.wait_while_stops_blocked().await;
                }
                if self.fail_stops.load(Ordering::SeqCst) {
                    Err(MediaError::new(
                        MediaErrorKind::Unavailable,
                        "fixture cannot confirm native media stop",
                    ))
                } else {
                    self.stopped.lock().unwrap().insert(target.clone());
                    Ok(MediaResult::Stopped { resource: target })
                }
            }
            _ => Err(MediaError::new(
                MediaErrorKind::Unsupported,
                "cleanup fixture supports status and stop only",
            )),
        }
    }

    async fn read_media(
        &self,
        context: MediaOperationContext,
        _media: media_api::MediaRef,
    ) -> Result<media_api::MediaContent, MediaError> {
        self.owners.lock().unwrap().push(context.owner);
        Err(MediaError::new(
            MediaErrorKind::Unsupported,
            "not used by owner cleanup fixture",
        ))
    }

    async fn cancel(&self, _context: MediaOperationContext) -> Result<(), MediaError> {
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
        attachment_id: None,
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
            MediaResourceRef::Operation { identity, .. }
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

async fn wait_for_attachment_phase(
    attachment: &media_runtime::MediaAttachmentHandle,
    expected: media_runtime::MediaAttachmentPhase,
) {
    let waited = tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            if attachment.phase() == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    if waited.is_err() {
        let outcome = attachment
            .revoke(std::time::Duration::from_millis(100))
            .await;
        panic!(
            "attachment did not reach {expected:?}; current phase: {:?}; outcome: {outcome:?}",
            attachment.phase(),
        )
    }
}

async fn wait_for_route_release(
    host: &media_runtime::MediaAttachmentHost,
    attachment_id: &media_api::MediaAttachmentId,
    route_retained: Option<&std::sync::atomic::AtomicBool>,
) {
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        while host.attachment(attachment_id).is_some()
            || route_retained.is_some_and(|route| route.load(Ordering::SeqCst))
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("confirmed attachment and route must be released");
}

struct BlockingSideChatEndListener {
    started: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: tokio::sync::Notify,
}

#[async_trait]
impl client::adapter::ClientEventListener for BlockingSideChatEndListener {
    async fn on_event(&self, event: client::protocol::events::ClientEvent) {
        if matches!(
            event,
            client::protocol::events::ClientEvent::SideChatEnded { .. }
        ) {
            let released = self.release.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if let Some(started) = self.started.lock().unwrap().take() {
                let _ = started.send(());
            }
            released.await;
        }
    }

    async fn on_workflow_progress(
        &self,
        _origin_session_id: String,
        _task_id: String,
        _run_id: String,
        _progress: client::protocol::listings::WorkflowProgressDto,
    ) {
    }
}

#[test]
fn engine_uses_its_host_attachment_and_releases_routes_only_after_confirmed_drain() {
    use crate::mobile::test_support::{
        new_engine_with_streaming, test_config, CollectingPermissionSink, FakeListener,
        HostFakePlatform,
    };
    use orchestrator::test_support_stream::MockStreamingApiClient;

    let root = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(OwnerCleanupMediaService::new());
    let host = media_runtime::MediaAttachmentHost::new();
    let attachment = host
        .attach(service.clone())
        .expect("trusted host issues one engine attachment");
    let attachment_id = attachment.id().clone();
    let route_retained = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let route_retained_for_callback = route_retained.clone();
    let callback_count = Arc::new(AtomicUsize::new(0));
    let callback_count_for_callback = callback_count.clone();
    let mut config = test_config(root.path());
    config.media_service = Some(service.clone());
    config.media_attachment_handle = Some(attachment.clone());
    config.media_attachment_confirmed_callback = Some(Arc::new(move || {
        callback_count_for_callback.fetch_add(1, Ordering::SeqCst);
        route_retained_for_callback.store(false, Ordering::SeqCst);
    }));
    config.media_conversation_attachment = Some(Arc::new(CleanupMediaAttachment));
    let engine = new_engine_with_streaming(
        config,
        Arc::new(HostFakePlatform::new(root.path().to_path_buf())),
        Arc::new(FakeListener::default()),
        Arc::new(CollectingPermissionSink::default()),
        Some(Arc::new(MockStreamingApiClient::empty())),
    )
    .expect("build mobile engine");

    let engine_service = engine
        .media_service()
        .expect("engine exposes media service");
    assert_eq!(
        engine_service.attachment_id(),
        Some(attachment_id.clone()),
        "all engine media calls must use the host-issued scoped facade"
    );
    let raw_owner = MediaOwner::Session {
        session_id: "engine-session-for-scope-test".into(),
    };
    engine.runtime().block_on(async {
        engine_service
            .execute(
                MediaOperationContext::new(MediaOperationId::new(91, 41), raw_owner.clone()),
                MediaOperation::Status { target: None },
            )
            .await
            .expect("scoped status request succeeds");
        let _ = engine_service
            .read_media(
                MediaOperationContext::new(MediaOperationId::new(92, 41), raw_owner.clone()),
                media_api::MediaRef("fixture-media".into()),
            )
            .await;
    });
    let admitted_owner = MediaOwner::Session {
        session_id: "engine-session-for-scope-test".into(),
    }
    .in_attachment(attachment_id.clone());
    assert!(
        service
            .owners
            .lock()
            .unwrap()
            .iter()
            .filter(|owner| *owner == &admitted_owner)
            .count()
            >= 2,
        "execute and read must both reach the host service with the same attachment"
    );
    drop(engine);

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build assertion runtime")
        .block_on(async {
            wait_for_attachment_phase(&attachment, media_runtime::MediaAttachmentPhase::Revoked)
                .await;
            wait_for_route_release(&host, &attachment_id, Some(route_retained.as_ref())).await;
        });
    assert!(host.attachment(&attachment_id).is_none());
    assert!(!route_retained.load(Ordering::SeqCst));
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bare_mobile_runtime_drop_revokes_attachment_once() {
    use crate::mobile::test_support::{
        test_config, CollectingPermissionSink, FakeListener, HostFakePlatform,
    };
    use orchestrator::test_support_stream::MockStreamingApiClient;

    let root = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(OwnerCleanupMediaService::new());
    let host = media_runtime::MediaAttachmentHost::new();
    let attachment = host
        .attach(service)
        .expect("trusted host issues attachment");
    let attachment_id = attachment.id().clone();
    let route_retained = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let route_retained_for_callback = route_retained.clone();
    let callback_count = Arc::new(AtomicUsize::new(0));
    let callback_count_for_callback = callback_count.clone();
    let mut config = test_config(root.path());
    config.media_attachment_handle = Some(attachment.clone());
    config.media_attachment_confirmed_callback = Some(Arc::new(move || {
        callback_count_for_callback.fetch_add(1, Ordering::SeqCst);
        route_retained_for_callback.store(false, Ordering::SeqCst);
    }));
    let runtime = build_mobile_inner(
        config,
        Arc::new(HostFakePlatform::new(root.path().to_path_buf())),
        Arc::new(FakeListener::default()),
        Arc::new(CollectingPermissionSink::default()),
        Some(Arc::new(MockStreamingApiClient::empty())),
    )
    .await
    .expect("build mobile runtime");
    runtime
        .media_service
        .as_ref()
        .expect("runtime exposes scoped media service")
        .execute(
            MediaOperationContext::new(
                MediaOperationId::new(93, 41),
                MediaOwner::Session {
                    session_id: "bare-runtime-session".into(),
                },
            ),
            MediaOperation::Status { target: None },
        )
        .await
        .expect("register attachment owner");

    drop(runtime);
    assert_ne!(
        attachment.phase(),
        media_runtime::MediaAttachmentPhase::Active
    );
    wait_for_attachment_phase(&attachment, media_runtime::MediaAttachmentPhase::Revoked).await;
    wait_for_route_release(&host, &attachment_id, Some(route_retained.as_ref())).await;
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);
}

#[test]
fn engine_drop_keeps_unknown_attachment_and_host_routes_isolated() {
    use crate::mobile::test_support::{
        new_engine_with_streaming, test_config, CollectingPermissionSink, FakeListener,
        HostFakePlatform,
    };
    use orchestrator::test_support_stream::MockStreamingApiClient;

    let root = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(OwnerCleanupMediaService::new());
    service.fail_stops.store(true, Ordering::SeqCst);
    let host = media_runtime::MediaAttachmentHost::new();
    let attachment = host
        .attach(service.clone())
        .expect("trusted host issues one engine attachment");
    let attachment_id = attachment.id().clone();
    let route_retained = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let route_retained_for_callback = route_retained.clone();
    let mut config = test_config(root.path());
    config.media_service = Some(service);
    config.media_attachment_handle = Some(attachment.clone());
    config.media_attachment_confirmed_callback = Some(Arc::new(move || {
        route_retained_for_callback.store(false, Ordering::SeqCst);
    }));
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
        engine
            .media_service()
            .expect("engine media service")
            .execute(
                MediaOperationContext::new(
                    MediaOperationId::new(95, 41),
                    MediaOwner::Session {
                        session_id: "unknown-drain-session".into(),
                    },
                ),
                MediaOperation::Status { target: None },
            )
            .await
            .expect("register an owner whose native stop will fail");
    });
    drop(engine);

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build assertion runtime")
        .block_on(async {
            wait_for_attachment_phase(&attachment, media_runtime::MediaAttachmentPhase::Unknown)
                .await;
        });
    assert!(host.attachment(&attachment_id).is_some());
    assert!(route_retained.load(Ordering::SeqCst));
    assert!(host
        .attach(Arc::new(OwnerCleanupMediaService::new()))
        .is_err());
}

#[test]
fn failed_engine_build_cleanup_survives_timed_out_observation() {
    let service = Arc::new(OwnerCleanupMediaService::new());
    service.block_stops.store(true, Ordering::SeqCst);
    let host = media_runtime::MediaAttachmentHost::new();
    let attachment = host
        .attach(service.clone())
        .expect("trusted host issues an attachment before the engine build");
    let scoped = attachment.service();
    let setup_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build setup runtime");
    setup_runtime.block_on(async {
        scoped
            .execute(
                MediaOperationContext::new(
                    MediaOperationId::new(94, 41),
                    MediaOwner::Session {
                        session_id: "failed-build-drain-session".into(),
                    },
                ),
                MediaOperation::Status { target: None },
            )
            .await
            .expect("record an owner before simulating engine build failure");
    });
    drop(setup_runtime);

    // This is the FFI failed-build cleanup shape: synchronously close
    // admission, then repeatedly observe the same drain from the process
    // executor. An observation timeout must not destroy its runtime.
    attachment.begin_revoke();
    let cleanup_attachment = attachment.clone();
    let (timed_out, timed_out_rx) = tokio::sync::oneshot::channel();
    let mut timed_out = Some(timed_out);
    crate::mobile::spawn_mobile_lifecycle_task(async move {
        match cleanup_attachment
            .revoke(std::time::Duration::from_millis(40))
            .await
        {
            media_runtime::MediaAttachmentRevocation::Confirmed => {}
            media_runtime::MediaAttachmentRevocation::Unknown { .. }
                if cleanup_attachment.phase() == media_runtime::MediaAttachmentPhase::Revoking =>
            {
                if let Some(timed_out) = timed_out.take() {
                    let _ = timed_out.send(());
                }
                if cleanup_attachment.wait_for_drain_completion().await
                    == media_runtime::MediaAttachmentPhase::Revoked
                {
                    let _ = cleanup_attachment
                        .revoke(std::time::Duration::from_millis(40))
                        .await;
                }
            }
            media_runtime::MediaAttachmentRevocation::Unknown { .. } => {}
        }
    })
    .expect("process-lifetime executor accepts failed-build cleanup");

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build assertion runtime")
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(8), async {
                let (timed_out_result, ()) = tokio::join!(timed_out_rx, service.wait_for_stops(1));
                timed_out_result.expect("bounded observation completed with Unknown");
            })
            .await
            .expect("the first bounded wait timed out while native stop was blocked");
            assert_eq!(
                attachment.phase(),
                media_runtime::MediaAttachmentPhase::Revoking,
                "a timeout while the drain is running must not report stable Unknown"
            );
            service.block_stops.store(false, Ordering::SeqCst);
            service.stop_gate.notify_waiters();
            wait_for_attachment_phase(&attachment, media_runtime::MediaAttachmentPhase::Revoked)
                .await;
            wait_for_route_release(&host, attachment.id(), None).await;
        });
    assert!(host.attachment(attachment.id()).is_none());
}

#[test]
fn scheduled_child_runtime_does_not_reuse_parent_engine_attachment() {
    use crate::mobile::test_support::{test_config, HostFakePlatform};

    let root = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(OwnerCleanupMediaService::new());
    let host = media_runtime::MediaAttachmentHost::new();
    let attachment = host
        .attach(service.clone())
        .expect("trusted host issues parent engine attachment");
    let mut config = test_config(root.path());
    config.media_service = Some(service);
    config.media_attachment_handle = Some(attachment.clone());
    config.media_attachment_confirmed_callback = Some(Arc::new(|| {}));
    config.media_conversation_attachment = Some(Arc::new(CleanupMediaAttachment));
    let firer = MobileTurnFirer {
        cfg: config,
        platform: Arc::new(HostFakePlatform::new(root.path().to_path_buf())),
    };

    let (child_config, _child_cleanup) = firer
        .scheduled_runtime_config()
        .expect("issue child attachment");
    let child = child_config
        .media_attachment_handle
        .as_ref()
        .expect("child has an independent attachment");
    assert_ne!(child.id(), attachment.id());
    assert_eq!(
        child_config.media_service.as_ref().unwrap().attachment_id(),
        Some(child.id().clone()),
        "the scheduled service uses the issued child attachment"
    );
    assert!(child_config.media_attachment_confirmed_callback.is_none());
    assert!(child_config.media_conversation_attachment.is_none());
    assert!(firer.cfg.media_attachment_handle.is_some());
    assert_eq!(
        attachment.phase(),
        media_runtime::MediaAttachmentPhase::Active,
        "building child runtime config must not revoke the parent engine"
    );

    let mut raw_config = test_config(root.path());
    raw_config.media_service = Some(Arc::new(OwnerCleanupMediaService::new()));
    raw_config.media_conversation_attachment = Some(Arc::new(CleanupMediaAttachment));
    let (no_issuer, _child_cleanup) = MobileTurnFirer {
        cfg: raw_config,
        platform: Arc::new(HostFakePlatform::new(root.path().to_path_buf())),
    }
    .scheduled_runtime_config()
    .expect("a raw unscoped service is ignored");
    assert!(no_issuer.media_service.is_none());
    assert!(no_issuer.media_conversation_attachment.is_none());
}

#[test]
fn drop_closes_attachment_before_blocked_side_chat_shutdown() {
    use crate::mobile::test_support::{
        new_engine_with_streaming, test_config, CollectingPermissionSink, FakeListener,
        HostFakePlatform,
    };
    use orchestrator::test_support_stream::MockStreamingApiClient;

    let root = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(OwnerCleanupMediaService::new());
    service.block_stops.store(true, Ordering::SeqCst);
    let host = media_runtime::MediaAttachmentHost::new();
    let attachment = host
        .attach(service.clone())
        .expect("trusted host issues parent engine attachment");
    let attachment_id = attachment.id().clone();
    let route_retained = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let route_retained_for_callback = route_retained.clone();
    let mut config = test_config(root.path());
    config.media_service = Some(service.clone());
    config.media_attachment_handle = Some(attachment.clone());
    config.media_attachment_confirmed_callback = Some(Arc::new(move || {
        route_retained_for_callback.store(false, Ordering::SeqCst);
    }));
    config.media_conversation_attachment = Some(Arc::new(CleanupMediaAttachment));
    let engine = new_engine_with_streaming(
        config,
        Arc::new(HostFakePlatform::new(root.path().to_path_buf())),
        Arc::new(FakeListener::default()),
        Arc::new(CollectingPermissionSink::default()),
        Some(Arc::new(MockStreamingApiClient::empty())),
    )
    .expect("build mobile engine");
    let media = engine.media_service().expect("engine media service");
    engine.runtime().block_on(async {
        media
            .execute(
                MediaOperationContext::new(
                    MediaOperationId::new(93, 41),
                    MediaOwner::Session {
                        session_id: "drop-gate-session".into(),
                    },
                ),
                MediaOperation::Status { target: None },
            )
            .await
            .expect("seed an active owner for the attachment drain");
    });

    let (side_chat_ended, side_chat_ended_rx) = tokio::sync::oneshot::channel();
    let side_chat_listener = Arc::new(BlockingSideChatEndListener {
        started: StdMutex::new(Some(side_chat_ended)),
        release: tokio::sync::Notify::new(),
    });
    engine
        .runtime()
        .block_on(engine.create_side_chat(
            "drop-gate-side-chat".into(),
            None,
            side_chat_listener.clone(),
            Arc::new(CollectingPermissionSink::default()),
        ))
        .expect("create side chat whose close callback blocks");
    drop(engine);

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build assertion runtime")
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(8), async {
                let (side_chat_ended, ()) =
                    tokio::join!(side_chat_ended_rx, service.wait_for_stops(1),);
                side_chat_ended.expect("shutdown reached the blocking child listener");
            })
            .await
            .expect("side-chat and attachment cleanup reached their gates");
            assert_eq!(
                attachment.phase(),
                media_runtime::MediaAttachmentPhase::Revoking
            );
            assert!(host
                .attach(Arc::new(OwnerCleanupMediaService::new()))
                .is_err());

            service.block_stops.store(false, Ordering::SeqCst);
            service.stop_gate.notify_waiters();
            side_chat_listener.release.notify_waiters();
            wait_for_attachment_phase(&attachment, media_runtime::MediaAttachmentPhase::Revoked)
                .await;
            wait_for_route_release(&host, &attachment_id, Some(route_retained.as_ref())).await;
        });
    assert!(host.attachment(&attachment_id).is_none());
    assert!(!route_retained.load(Ordering::SeqCst));
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
