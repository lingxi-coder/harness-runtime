use super::*;
use media_api::{MediaAsset, MediaRef};
use std::sync::atomic::{AtomicU32, AtomicUsize};
use tokio::sync::mpsc;

const EPOCH: u64 = 7;

fn owner() -> MediaOwner {
    MediaOwner::Session {
        session_id: "local-test-session".to_owned(),
    }
}

fn context(generation: u64) -> MediaOperationContext {
    MediaOperationContext::new(MediaOperationId::new(generation, EPOCH), owner())
        .with_timeout_budget_ms(5_000)
        .with_max_payload_bytes(1_024)
}

fn microphone() -> MediaSourceRef {
    MediaSourceRef(MediaSourceId::from(LOCAL_MICROPHONE_ID))
}

fn screen() -> MediaSourceRef {
    MediaSourceRef(MediaSourceId::from("media-runtime:screen:primary"))
}

fn screen_descriptor() -> MediaSource {
    MediaSource {
        reference: screen(),
        kind: MediaSourceKind::Screen,
        media_kind: Some(MediaKind::Video),
        label: "Primary screen".to_owned(),
        usages: vec![MediaSourceUsage::Session],
        supported_operations: vec![MediaOperationKind::SessionStart],
        is_default: true,
    }
}

fn audio_frame(sequence: u64) -> MediaFrame {
    MediaFrame {
        source: microphone(),
        sequence,
        captured_at_unix_ms: now_ms(),
        payload: MediaFramePayload::Audio(MediaPcmFrame {
            sample_rate_hz: 24_000,
            channels: 1,
            pcm16le: vec![0; 4],
        }),
    }
}

fn rgba_frame(source: MediaSourceRef, sequence: u64, captured_at_unix_ms: u64) -> MediaFrame {
    MediaFrame {
        source,
        sequence,
        captured_at_unix_ms,
        payload: MediaFramePayload::Video(media_api::MediaVideoFrame::Rgba8 {
            width_px: 2,
            height_px: 2,
            stride_bytes: 8,
            bytes: vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
        }),
    }
}

#[derive(Default)]
struct FakePorts {
    opens: AtomicUsize,
    device: std::sync::Mutex<Option<Arc<FakeDevice>>>,
}

impl FakePorts {
    fn device(&self) -> Arc<FakeDevice> {
        self.device.lock().unwrap().as_ref().unwrap().clone()
    }
}

#[async_trait]
impl MediaDevicePorts for FakePorts {
    async fn open(
        &self,
        _context: MediaOperationContext,
        _request: MediaSessionRequest,
    ) -> Result<Arc<dyn MediaDeviceSession>, MediaError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel(8);
        let device = Arc::new(FakeDevice {
            frames: AsyncMutex::new(receiver),
            sender,
            stopped: CancellationToken::new(),
            stop_calls: AtomicUsize::new(0),
            stop_failures: AtomicUsize::new(0),
            active_readers: AtomicUsize::new(0),
            max_active_readers: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            block_update: AtomicBool::new(false),
            update_started: Notify::new(),
            release_update: Notify::new(),
            pending_playback: AtomicU32::new(0),
            status: std::sync::Mutex::new(MediaSessionStatus {
                state: MediaSessionState::Ready,
                audio_enabled: true,
                video_enabled: true,
            }),
        });
        *self.device.lock().unwrap() = Some(device.clone());
        Ok(device)
    }
}

struct FakeDevice {
    frames: AsyncMutex<mpsc::Receiver<MediaFrame>>,
    sender: mpsc::Sender<MediaFrame>,
    stopped: CancellationToken,
    stop_calls: AtomicUsize,
    stop_failures: AtomicUsize,
    active_readers: AtomicUsize,
    max_active_readers: AtomicUsize,
    writes: AtomicUsize,
    block_update: AtomicBool,
    update_started: Notify,
    release_update: Notify,
    pending_playback: AtomicU32,
    status: std::sync::Mutex<MediaSessionStatus>,
}

impl FakeDevice {
    async fn send(&self, frame: MediaFrame) {
        self.sender.send(frame).await.unwrap();
    }
}

struct ActiveReader<'a>(&'a AtomicUsize);

impl Drop for ActiveReader<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl MediaDeviceSession for FakeDevice {
    async fn next_input(&self) -> Result<Option<MediaFrame>, MediaError> {
        let active = self.active_readers.fetch_add(1, Ordering::SeqCst) + 1;
        let _reader = ActiveReader(&self.active_readers);
        self.max_active_readers.fetch_max(active, Ordering::SeqCst);
        let mut frames = self.frames.lock().await;
        let next = tokio::select! {
            biased;
            _ = self.stopped.cancelled() => None,
            frame = frames.recv() => frame,
        };
        Ok(next)
    }

    async fn write_audio(&self, frame: MediaPcmFrame) -> Result<MediaRenderReceipt, MediaError> {
        frame.validate(64 * 1024)?;
        self.writes.fetch_add(1, Ordering::SeqCst);
        let accepted =
            u32::try_from(frame.pcm16le.len() / (usize::from(frame.channels) * 2)).unwrap();
        self.pending_playback.fetch_add(accepted, Ordering::SeqCst);
        Ok(MediaRenderReceipt {
            accepted_sample_frames: accepted,
            played_sample_frames: 0,
            rendered_at_unix_ms: None,
        })
    }

    async fn flush_audio(&self) -> Result<MediaRenderReceipt, MediaError> {
        Ok(MediaRenderReceipt {
            accepted_sample_frames: 0,
            played_sample_frames: self.pending_playback.swap(0, Ordering::SeqCst),
            rendered_at_unix_ms: Some(now_ms()),
        })
    }

    async fn interrupt_audio(&self) -> Result<MediaRenderReceipt, MediaError> {
        self.pending_playback.store(0, Ordering::SeqCst);
        Ok(MediaRenderReceipt {
            accepted_sample_frames: 0,
            played_sample_frames: 0,
            rendered_at_unix_ms: Some(now_ms()),
        })
    }

    async fn write_video(&self, frame: media_api::MediaVideoFrame) -> Result<(), MediaError> {
        frame.validate(1_024)
    }

    async fn update(&self, update: MediaSessionUpdate) -> Result<MediaSessionStatus, MediaError> {
        if self.block_update.load(Ordering::Acquire) {
            self.update_started.notify_one();
            self.release_update.notified().await;
        }
        let mut status = self.status.lock().unwrap();
        match update {
            MediaSessionUpdate::SetEnabled {
                kind: MediaKind::Audio,
                enabled,
            } => status.audio_enabled = enabled,
            MediaSessionUpdate::SetEnabled {
                kind: MediaKind::Video | MediaKind::Image,
                enabled,
            } => status.video_enabled = enabled,
            MediaSessionUpdate::SelectSource { .. }
            | MediaSessionUpdate::SetPlaybackGain { .. } => {}
        }
        Ok(status.clone())
    }

    async fn stop(&self) -> Result<(), MediaError> {
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
        self.stopped.cancel();
        if self
            .stop_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(MediaError::new(
                MediaErrorKind::NativeFailure,
                "device cleanup failed",
            ));
        }
        Ok(())
    }
}

#[derive(Default)]
struct FakeStore {
    next_id: AtomicUsize,
    assets: AsyncMutex<HashMap<MediaRef, (MediaOwner, MediaPayload)>>,
    block_put: AtomicBool,
    panic_put: AtomicBool,
    put_started: Notify,
    release_put: Notify,
}

#[async_trait]
impl MediaResourceStore for FakeStore {
    async fn put(
        &self,
        context: &MediaOperationContext,
        payload: MediaPayload,
    ) -> Result<MediaAsset, MediaError> {
        if self.panic_put.load(Ordering::Acquire) {
            self.put_started.notify_one();
            panic!("injected resource store panic");
        }
        if self.block_put.load(Ordering::Acquire) {
            self.put_started.notify_one();
            self.release_put.notified().await;
        }
        payload.validate(context.max_payload_bytes)?;
        let reference = MediaRef::from(format!(
            "local-test-asset-{}",
            self.next_id.fetch_add(1, Ordering::SeqCst)
        ));
        let asset = MediaAsset {
            reference: reference.clone(),
            metadata: payload.metadata.clone(),
        };
        self.assets
            .lock()
            .await
            .insert(reference, (context.owner.clone(), payload));
        Ok(asset)
    }

    async fn get(
        &self,
        context: &MediaOperationContext,
        media: &MediaRef,
        max_bytes: u64,
    ) -> Result<MediaPayload, MediaError> {
        let assets = self.assets.lock().await;
        let (owner, payload) = assets.get(media).ok_or_else(not_found_source)?;
        if owner != &context.owner {
            return Err(not_found_source());
        }
        payload.validate(max_bytes)?;
        Ok(payload.clone())
    }

    async fn delete(
        &self,
        context: &MediaOperationContext,
        media: &MediaRef,
    ) -> Result<(), MediaError> {
        let mut assets = self.assets.lock().await;
        if assets
            .get(media)
            .is_some_and(|(owner, _)| owner == &context.owner)
        {
            assets.remove(media);
            Ok(())
        } else {
            Err(not_found_source())
        }
    }
}

#[tokio::test]
async fn explicit_native_modes_enable_and_remove_local_audio_sessions() {
    let ports = Arc::new(FakePorts::default());
    let connector = LocalMediaConnector::new(
        ports.clone(),
        Arc::new(FakeStore::default()),
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    let context = context(20);
    let request = MediaSessionRequest {
        intent: MediaSessionIntent::Conversation,
        mode: MediaSessionMode::Audio,
        audio_source: Some(microphone()),
        video_source: None,
    };

    assert!(connector
        .start_session(context.clone(), request.clone())
        .await
        .is_err());
    assert_eq!(ports.opens.load(Ordering::SeqCst), 0);

    connector
        .update_supported_modes(vec![MediaSessionMode::Audio])
        .unwrap();
    let session = connector
        .start_session(context.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(ports.opens.load(Ordering::SeqCst), 1);
    session.stop(context.clone()).await.unwrap();

    connector.update_supported_modes(Vec::new()).unwrap();
    assert!(connector.start_session(context, request).await.is_err());
    assert_eq!(ports.opens.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn conversation_session_uses_one_device_lease_and_owner_scope() {
    let ports = Arc::new(FakePorts::default());
    let store = Arc::new(FakeStore::default());
    let connector = LocalMediaConnector::new(
        ports.clone(),
        store,
        Vec::new(),
        vec![MediaSessionMode::Audio],
    )
    .unwrap();
    let context = context(1);
    let session = connector
        .start_session(
            context.clone(),
            MediaSessionRequest {
                intent: MediaSessionIntent::Conversation,
                mode: MediaSessionMode::Audio,
                audio_source: Some(microphone()),
                video_source: None,
            },
        )
        .await
        .unwrap();
    let device = ports.device();
    device.send(audio_frame(1)).await;

    let channel = session.audio_channel().await.unwrap();
    let captured = channel.read().await.unwrap().unwrap();
    assert_eq!(captured.source, microphone());
    assert!(matches!(captured.payload, MediaFramePayload::Audio(_)));
    let receipt = channel
        .write(MediaPcmFrame {
            sample_rate_hz: 24_000,
            channels: 1,
            pcm16le: vec![0; 4],
        })
        .await
        .unwrap();
    assert_eq!(receipt.accepted_sample_frames, 2);
    assert_eq!(channel.flush().await.unwrap().played_sample_frames, 2);
    assert_eq!(channel.interrupt().await.unwrap().accepted_sample_frames, 0);
    assert_eq!(ports.opens.load(Ordering::SeqCst), 1);
    assert_eq!(device.writes.load(Ordering::SeqCst), 1);
    assert_eq!(device.max_active_readers.load(Ordering::SeqCst), 1);

    let foreign = MediaOperationContext::new(
        MediaOperationId::new(2, EPOCH),
        MediaOwner::Session {
            session_id: "another-owner".to_owned(),
        },
    );
    assert!(session.observe(foreign, None, 8, None).await.is_err());

    let stopped = session.stop(context).await.unwrap();
    assert_eq!(stopped.state, MediaSessionState::Ended);
    assert_eq!(device.stop_calls.load(Ordering::SeqCst), 1);
    assert_eq!(device.max_active_readers.load(Ordering::SeqCst), 1);
    assert_eq!(device.active_readers.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_device_stop_is_retried_until_cleanup_is_acknowledged() {
    let ports = Arc::new(FakePorts::default());
    let connector = LocalMediaConnector::new(
        ports.clone(),
        Arc::new(FakeStore::default()),
        Vec::new(),
        vec![MediaSessionMode::Audio],
    )
    .unwrap();
    let context = context(5);
    let session = connector
        .start_session(
            context.clone(),
            MediaSessionRequest {
                intent: MediaSessionIntent::Conversation,
                mode: MediaSessionMode::Audio,
                audio_source: Some(microphone()),
                video_source: None,
            },
        )
        .await
        .unwrap();
    let device = ports.device();
    device.stop_failures.store(1, Ordering::SeqCst);
    assert!(session.stop(context.clone()).await.is_err());
    let channel = session.audio_channel().await.unwrap();
    assert!(channel
        .write(MediaPcmFrame {
            sample_rate_hz: 24_000,
            channels: 1,
            pcm16le: vec![0; 4]
        })
        .await
        .is_err());
    session.stop(context.clone()).await.unwrap();
    session.stop(context).await.unwrap();
    assert_eq!(device.stop_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn screen_observation_rejects_unselected_frames_and_persists_real_png() {
    let ports = Arc::new(FakePorts::default());
    let store = Arc::new(FakeStore::default());
    let connector = LocalMediaConnector::new(
        ports.clone(),
        store.clone(),
        vec![screen_descriptor()],
        vec![MediaSessionMode::Video],
    )
    .unwrap();
    let context = context(3);
    let session = connector
        .start_session(
            context.clone(),
            MediaSessionRequest {
                intent: MediaSessionIntent::Observation,
                mode: MediaSessionMode::Video,
                audio_source: None,
                video_source: Some(screen()),
            },
        )
        .await
        .unwrap();
    let device = ports.device();
    let captured_at = now_ms();
    device
        .send(rgba_frame(
            MediaSourceRef(MediaSourceId::from(LOCAL_CAMERA_ID)),
            1,
            captured_at,
        ))
        .await;
    device
        .send(rgba_frame(
            screen(),
            1,
            captured_at.saturating_sub(LOCAL_EVENT_AGE_MS + 1),
        ))
        .await;
    device.send(rgba_frame(screen(), 2, captured_at)).await;

    let mut cursor = None;
    let mut frame_asset = None;
    for _ in 0..4 {
        let batch = session
            .observe(context.clone(), cursor, 16, Some(500))
            .await
            .unwrap();
        cursor = batch.next_cursor;
        if let Some(MediaEvent {
            kind: MediaEventKind::Frame { source, asset },
            ..
        }) = batch
            .events
            .iter()
            .find(|event| matches!(&event.kind, MediaEventKind::Frame { .. }))
        {
            assert_eq!(source, &screen());
            frame_asset = Some(asset.clone());
            break;
        }
    }

    let asset = frame_asset.expect("a fresh frame from the selected screen is emitted");
    assert_eq!(asset.metadata.kind, MediaKind::Image);
    assert_eq!(asset.metadata.captured_at_unix_ms, Some(captured_at));
    assert_eq!(asset.metadata.width_px, Some(2));
    assert_eq!(asset.metadata.height_px, Some(2));
    let payload = store
        .get(&context, &asset.reference, context.max_payload_bytes)
        .await
        .unwrap();
    assert_eq!(payload.metadata, asset.metadata);
    let decoded = image::load_from_memory_with_format(&payload.bytes, image::ImageFormat::Png)
        .unwrap()
        .to_rgba8();
    assert_eq!(decoded.dimensions(), (2, 2));
    assert_eq!(decoded.get_pixel(0, 0).0, [255, 0, 0, 255]);
    assert_eq!(store.assets.lock().await.len(), 1);
    assert_eq!(ports.opens.load(Ordering::SeqCst), 1);
    session.stop(context).await.unwrap();
}

#[tokio::test]
async fn stop_serializes_with_pending_update_and_rejects_updates_after_stop() {
    let ports = Arc::new(FakePorts::default());
    let connector = LocalMediaConnector::new(
        ports.clone(),
        Arc::new(FakeStore::default()),
        Vec::new(),
        vec![MediaSessionMode::Audio],
    )
    .unwrap();
    let session: Arc<dyn MediaSession> = Arc::from(
        connector
            .start_session(
                context(8),
                MediaSessionRequest {
                    intent: MediaSessionIntent::Observation,
                    mode: MediaSessionMode::Audio,
                    audio_source: Some(microphone()),
                    video_source: None,
                },
            )
            .await
            .unwrap(),
    );
    let device = ports.device();
    device.block_update.store(true, Ordering::Release);

    let updating_session = session.clone();
    let update = tokio::spawn(async move {
        updating_session
            .update(
                context(9),
                MediaSessionUpdate::SetEnabled {
                    kind: MediaKind::Audio,
                    enabled: false,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), device.update_started.notified())
        .await
        .expect("the update reached the controlled device barrier");

    let stopping_session = session.clone();
    let stop = tokio::spawn(async move { stopping_session.stop(context(10)).await });
    tokio::task::yield_now().await;
    assert!(
        !stop.is_finished(),
        "stop waits for the in-flight update to commit before ending the session"
    );
    assert_eq!(device.stop_calls.load(Ordering::SeqCst), 0);

    device.release_update.notify_one();
    let updated = tokio::time::timeout(Duration::from_secs(1), update)
        .await
        .expect("update completed after the device barrier was released")
        .expect("update task did not panic")
        .expect("update succeeds before stop acquires the lifecycle gate");
    assert!(!updated.audio_enabled);

    let stopped = tokio::time::timeout(Duration::from_secs(1), stop)
        .await
        .expect("stop completed after the update committed")
        .expect("stop task did not panic")
        .expect("device stop succeeded");
    assert_eq!(stopped.state, MediaSessionState::Ended);
    assert_eq!(session.status().state, MediaSessionState::Ended);
    assert_eq!(device.stop_calls.load(Ordering::SeqCst), 1);

    let rejected = session
        .update(
            context(11),
            MediaSessionUpdate::SetEnabled {
                kind: MediaKind::Audio,
                enabled: true,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(rejected.kind, MediaErrorKind::NotFound);

    let target = MediaResourceRef::Session(session.reference().clone());
    let events = session.observe(context(12), None, 16, None).await.unwrap();
    let final_state = events
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            MediaEventKind::StateChanged { resource, state } if resource == &target => Some(*state),
            _ => None,
        })
        .next_back();
    assert_eq!(final_state, Some(MediaResourceState::Completed));
}

#[tokio::test]
async fn cancelled_stop_retains_pump_until_inflight_store_finishes() {
    let ports = Arc::new(FakePorts::default());
    let store = Arc::new(FakeStore::default());
    store.block_put.store(true, Ordering::Release);
    let connector = LocalMediaConnector::new(
        ports.clone(),
        store.clone(),
        vec![screen_descriptor()],
        vec![MediaSessionMode::Video],
    )
    .unwrap();
    let context = context(6);
    let session = connector
        .start_session(
            context.clone(),
            MediaSessionRequest {
                intent: MediaSessionIntent::Observation,
                mode: MediaSessionMode::Video,
                audio_source: None,
                video_source: Some(screen()),
            },
        )
        .await
        .unwrap();
    ports.device().send(rgba_frame(screen(), 1, now_ms())).await;
    tokio::time::timeout(Duration::from_secs(1), store.put_started.notified())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), session.stop(context.clone()))
            .await
            .is_err()
    );
    let retry = session.stop(context);
    tokio::pin!(retry);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut retry)
            .await
            .is_err(),
        "a retry must not acknowledge stop while the original pump is writing"
    );
    store.release_put.notify_one();
    let status = tokio::time::timeout(Duration::from_secs(1), retry)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.state, MediaSessionState::Ended);
    assert_eq!(store.assets.lock().await.len(), 1);
}

#[tokio::test]
async fn panicked_pump_is_consumed_and_cleanup_remains_retryable() {
    let ports = Arc::new(FakePorts::default());
    let store = Arc::new(FakeStore::default());
    store.panic_put.store(true, Ordering::Release);
    let connector = LocalMediaConnector::new(
        ports.clone(),
        store.clone(),
        vec![screen_descriptor()],
        vec![MediaSessionMode::Video],
    )
    .unwrap();
    let context = context(7);
    let session = connector
        .start_session(
            context.clone(),
            MediaSessionRequest {
                intent: MediaSessionIntent::Observation,
                mode: MediaSessionMode::Video,
                audio_source: None,
                video_source: Some(screen()),
            },
        )
        .await
        .unwrap();
    ports.device().send(rgba_frame(screen(), 1, now_ms())).await;
    tokio::time::timeout(Duration::from_secs(1), store.put_started.notified())
        .await
        .unwrap();
    assert_eq!(
        session.stop(context.clone()).await.unwrap_err().kind,
        MediaErrorKind::NativeFailure
    );
    assert_eq!(
        session.stop(context).await.unwrap().state,
        MediaSessionState::Failed
    );
    assert_eq!(ports.device().stop_calls.load(Ordering::SeqCst), 2);
}
