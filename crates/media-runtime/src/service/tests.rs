use super::*;
use crate::audio::{AudioCapabilitySnapshot, AudioOperationReadiness, AudioStatus};
use media_api::audio::{AudioError, AudioErrorKind, AudioReadinessState, AudioService};
use media_api::camera::{CameraControl, CapturedImage};
use media_api::{
    MediaAudioChannel, MediaConnectorId, MediaError, MediaOperationContext, MediaPcmFrame,
    MediaRenderReceipt, MediaResourceStore, MediaService, MediaSessionId, MediaSessionStatus,
    MediaSessionUpdate, RecordingId,
};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tokio::sync::{Notify, Semaphore};

const EPOCH: u64 = 9;

fn owner(id: &str) -> MediaOwner {
    MediaOwner::Session {
        session_id: id.to_owned(),
    }
}

fn context(owner: MediaOwner, generation: u64) -> MediaOperationContext {
    MediaOperationContext::new(MediaOperationId::new(generation, EPOCH), owner)
        .with_timeout_budget_ms(2_000)
        .with_max_payload_bytes(1024)
}

// Distinct calls may share the public identity but must have distinct admissions.
fn fresh_context_same_identity(original: &MediaOperationContext) -> MediaOperationContext {
    let mut fresh = MediaOperationContext::new(original.identity.clone(), original.owner.clone());
    fresh.initiator = original.initiator.clone();
    fresh.timeout_budget_ms = original.timeout_budget_ms;
    fresh.max_payload_bytes = original.max_payload_bytes;
    fresh
}

async fn observe_session(
    runtime: &Arc<UnifiedMediaService>,
    owner: MediaOwner,
    target: MediaResourceRef,
    generation: u64,
) -> Result<MediaResult, MediaError> {
    runtime
        .execute(
            context(owner, generation),
            MediaOperation::Observe {
                target,
                after: None,
                limit: 16,
            },
        )
        .await
}

fn microphone() -> MediaSourceRef {
    MediaSourceRef(MediaSourceId::from("media-runtime:microphone"))
}

fn camera_source() -> MediaSourceRef {
    MediaSourceRef(MediaSourceId::from("media-runtime:camera"))
}

fn media_store_ref(id: &str) -> MediaRef {
    MediaRef::from(id)
}

#[derive(Default)]
struct MemoryStore {
    next_id: AtomicUsize,
    fail_next_put: AtomicBool,
    items: std::sync::Mutex<HashMap<MediaRef, (MediaOwner, MediaPayload)>>,
}

#[async_trait]
impl MediaResourceStore for MemoryStore {
    async fn put(
        &self,
        context: &MediaOperationContext,
        payload: MediaPayload,
    ) -> Result<MediaAsset, MediaError> {
        if self.fail_next_put.swap(false, Ordering::SeqCst) {
            return Err(MediaError::new(
                MediaErrorKind::StoreFailure,
                "test store is unavailable",
            ));
        }
        payload.validate(context.max_payload_bytes)?;
        let reference = media_store_ref(&format!(
            "media-{}",
            self.next_id.fetch_add(1, Ordering::SeqCst)
        ));
        let asset = MediaAsset {
            reference: reference.clone(),
            metadata: payload.metadata.clone(),
        };
        self.items
            .lock()
            .unwrap()
            .insert(reference, (context.owner.clone(), payload));
        Ok(asset)
    }

    async fn get(
        &self,
        context: &MediaOperationContext,
        media: &MediaRef,
        max_bytes: u64,
    ) -> Result<MediaPayload, MediaError> {
        let items = self.items.lock().unwrap();
        let (owner, payload) = items
            .get(media)
            .ok_or_else(|| MediaError::new(MediaErrorKind::NotFound, "missing test media"))?;
        if owner != &context.owner {
            return Err(MediaError::new(
                MediaErrorKind::NotFound,
                "missing test media",
            ));
        }
        payload.validate(max_bytes)?;
        Ok(payload.clone())
    }

    async fn delete(
        &self,
        context: &MediaOperationContext,
        media: &MediaRef,
    ) -> Result<(), MediaError> {
        let mut items = self.items.lock().unwrap();
        let Some((owner, _)) = items.get(media) else {
            return Err(MediaError::new(
                MediaErrorKind::NotFound,
                "missing test media",
            ));
        };
        if owner != &context.owner {
            return Err(MediaError::new(
                MediaErrorKind::NotFound,
                "missing test media",
            ));
        }
        items.remove(media);
        Ok(())
    }
}

struct FakeAudio {
    native_epoch: AtomicU64,
    contexts: std::sync::Mutex<Vec<MediaOperationContext>>,
    calls: std::sync::Mutex<Vec<Option<AudioOperationKind>>>,
    cancellations: AtomicUsize,
    cancellation_contexts: std::sync::Mutex<Vec<MediaOperationContext>>,
    records: std::sync::Mutex<HashMap<RecordingRef, MediaOwner>>,
    status: std::sync::Mutex<AudioStatus>,
    readiness_overrides: std::sync::Mutex<HashMap<AudioOperationKind, AudioReadinessState>>,
    supported_override: std::sync::Mutex<Option<Vec<AudioOperationKind>>>,
    pending_listen: AtomicBool,
    listen_started: Notify,
    pending_recording_start: AtomicBool,
    recording_started: Semaphore,
    fail_next_recording_start: AtomicBool,
    fail_next_native_call: AtomicBool,
    fail_cancel: AtomicBool,
    fail_status: AtomicBool,
    hold_cancel: AtomicBool,
    cancel_entered: Semaphore,
    cancel_release: Semaphore,
    fixed_recording_handle: AtomicBool,
    hold_stop: AtomicBool,
    stop_entered: Semaphore,
    stop_release: Semaphore,
    owner_restriction: Option<String>,
}

impl FakeAudio {
    fn new() -> Arc<Self> {
        Self::with_owner_restriction(None)
    }

    fn with_backend_epoch(native_epoch: u64) -> Arc<Self> {
        Self::with_owner_restriction_and_epoch(None, native_epoch)
    }

    fn with_owner_restriction(owner_restriction: Option<String>) -> Arc<Self> {
        Self::with_owner_restriction_and_epoch(owner_restriction, EPOCH)
    }

    fn with_owner_restriction_and_epoch(
        owner_restriction: Option<String>,
        native_epoch: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            native_epoch: AtomicU64::new(native_epoch),
            contexts: std::sync::Mutex::new(Vec::new()),
            calls: std::sync::Mutex::new(Vec::new()),
            cancellations: AtomicUsize::new(0),
            cancellation_contexts: std::sync::Mutex::new(Vec::new()),
            records: std::sync::Mutex::new(HashMap::new()),
            status: std::sync::Mutex::new(AudioStatus::default()),
            readiness_overrides: std::sync::Mutex::new(HashMap::new()),
            supported_override: std::sync::Mutex::new(None),
            pending_listen: AtomicBool::new(false),
            listen_started: Notify::new(),
            pending_recording_start: AtomicBool::new(false),
            recording_started: Semaphore::new(0),
            fail_next_recording_start: AtomicBool::new(false),
            fail_next_native_call: AtomicBool::new(false),
            fail_cancel: AtomicBool::new(false),
            fail_status: AtomicBool::new(false),
            hold_cancel: AtomicBool::new(false),
            cancel_entered: Semaphore::new(0),
            cancel_release: Semaphore::new(0),
            fixed_recording_handle: AtomicBool::new(false),
            hold_stop: AtomicBool::new(false),
            stop_entered: Semaphore::new(0),
            stop_release: Semaphore::new(0),
            owner_restriction,
        })
    }

    fn operations(&self) -> Vec<Option<AudioOperationKind>> {
        self.calls.lock().unwrap().clone()
    }

    fn set_readiness(&self, operation: AudioOperationKind, state: AudioReadinessState) {
        self.readiness_overrides
            .lock()
            .unwrap()
            .insert(operation, state);
    }

    fn set_supported_operations(&self, operations: Vec<AudioOperationKind>) {
        *self.supported_override.lock().unwrap() = Some(operations);
    }

    fn snapshot(&self, operations: Vec<AudioOperationKind>) -> AudioCapabilitySnapshot {
        let readiness_overrides = self.readiness_overrides.lock().unwrap();
        AudioCapabilitySnapshot {
            service_epoch: self.native_epoch.load(Ordering::SeqCst),
            support_revision: 1,
            readiness: operations
                .iter()
                .map(|operation| AudioOperationReadiness {
                    operation: *operation,
                    state: readiness_overrides
                        .get(operation)
                        .copied()
                        .unwrap_or(AudioReadinessState::Ready),
                })
                .collect(),
            supported_operations: operations,
            max_payload_bytes: 1024,
        }
    }

    fn all_capabilities(&self) -> AudioCapabilitySnapshot {
        self.snapshot(
            self.supported_override
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| {
                    vec![
                        AudioOperationKind::Capture,
                        AudioOperationKind::Play,
                        AudioOperationKind::Transcribe,
                        AudioOperationKind::Record,
                        AudioOperationKind::Listen,
                        AudioOperationKind::Synthesize,
                        AudioOperationKind::Speak,
                    ]
                }),
        )
    }
}

#[async_trait]
impl AudioService for FakeAudio {
    fn capabilities(&self) -> AudioCapabilitySnapshot {
        self.all_capabilities()
    }

    fn capabilities_for(&self, owner: &MediaOwner) -> AudioCapabilitySnapshot {
        if let Some(only_owner) = &self.owner_restriction {
            if !matches!(owner, MediaOwner::Session { session_id } if session_id == only_owner) {
                return self.snapshot(Vec::new());
            }
        }
        self.all_capabilities()
    }

    async fn execute(
        &self,
        context: MediaOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        self.contexts.lock().unwrap().push(context.clone());
        self.calls.lock().unwrap().push(operation.kind());
        if matches!(operation, AudioOperation::Status { .. })
            && self.fail_status.load(Ordering::SeqCst)
        {
            return Err(AudioError::new(
                AudioErrorKind::Unavailable,
                "native audio is disconnected",
            ));
        }
        if matches!(
            operation,
            AudioOperation::StartRecording { .. } | AudioOperation::Play { .. }
        ) && self.fail_next_native_call.swap(false, Ordering::SeqCst)
        {
            return Err(AudioError::new(
                AudioErrorKind::NativeFailure,
                "native result unknown",
            ));
        }
        if matches!(&operation, AudioOperation::StartRecording { .. }) {
            if self.fail_next_recording_start.swap(false, Ordering::SeqCst) {
                return Err(AudioError::new(
                    AudioErrorKind::Unavailable,
                    "injected recording start failure",
                ));
            }
            if self.pending_recording_start.load(Ordering::SeqCst) {
                self.recording_started.add_permits(1);
                std::future::pending::<()>().await;
            }
        }
        if matches!(operation, AudioOperation::Listen { .. })
            && self.pending_listen.load(Ordering::SeqCst)
        {
            self.listen_started.notify_one();
            std::future::pending::<()>().await;
        }
        if matches!(operation, AudioOperation::StopRecording { .. })
            && self.hold_stop.load(Ordering::SeqCst)
        {
            self.stop_entered.add_permits(1);
            self.stop_release.acquire().await.unwrap().forget();
        }
        let result = match operation {
            AudioOperation::Capture { .. } => AudioOperationSuccess::Recording {
                recording: VoiceRecording {
                    audio_bytes: vec![4, 5, 6],
                    mime_type: "audio/wav".to_owned(),
                },
            },
            AudioOperation::StartRecording { .. } => {
                let handle = RecordingRef(RecordingId::from(
                    if self.fixed_recording_handle.load(Ordering::SeqCst) {
                        "same-recording".to_owned()
                    } else {
                        format!("recording-{}", context.identity.id)
                    },
                ));
                self.records
                    .lock()
                    .unwrap()
                    .insert(handle.clone(), context.owner);
                self.status.lock().unwrap().recording = true;
                AudioOperationSuccess::RecordingStarted { handle }
            }
            AudioOperation::StopRecording { handle } => {
                self.records
                    .lock()
                    .unwrap()
                    .remove(&handle)
                    .ok_or_else(|| {
                        AudioError::new(AudioErrorKind::NotRecording, "recording missing")
                    })?;
                self.status.lock().unwrap().recording = false;
                AudioOperationSuccess::Recording {
                    recording: VoiceRecording {
                        audio_bytes: vec![9, 8, 7],
                        mime_type: "audio/wav".to_owned(),
                    },
                }
            }
            AudioOperation::Listen { .. } | AudioOperation::Transcribe { .. } => {
                AudioOperationSuccess::Transcript {
                    transcript: media_api::stt::SttTranscript {
                        text: "recognized words".to_owned(),
                        language: Some("en-US".to_owned()),
                        confidence: Some(0.9),
                    },
                }
            }
            AudioOperation::Synthesize { .. } => AudioOperationSuccess::Synthesized {
                audio: TtsAudio {
                    pcm: vec![1, 2, 3, 4],
                    sample_rate_hz: 24_000,
                },
            },
            AudioOperation::Play { .. } => {
                AudioOperationSuccess::PlaybackCompleted { duration_ms: 41 }
            }
            AudioOperation::Speak { .. } => {
                AudioOperationSuccess::PlaybackCompleted { duration_ms: 42 }
            }
            AudioOperation::Status { .. } => AudioOperationSuccess::Status {
                status: *self.status.lock().unwrap(),
            },
            AudioOperation::EndOwner => AudioOperationSuccess::OwnerEnded,
        };
        Ok(result)
    }

    async fn cancel(&self, context: MediaOperationContext) -> Result<(), AudioError> {
        self.cancellation_contexts.lock().unwrap().push(context);
        self.cancellations.fetch_add(1, Ordering::SeqCst);
        if self.hold_cancel.load(Ordering::SeqCst) {
            self.cancel_entered.add_permits(1);
            self.cancel_release.acquire().await.unwrap().forget();
        }
        if self.fail_cancel.load(Ordering::SeqCst) {
            Err(AudioError::new(
                AudioErrorKind::NativeFailure,
                "no exact close receipt",
            ))
        } else {
            Ok(())
        }
    }
}

struct FakeCamera;

#[async_trait]
impl CameraControl for FakeCamera {
    async fn capture_photo(&self, _opts: CapturePhotoOpts) -> Result<CapturedImage, CameraError> {
        Ok(CapturedImage {
            jpeg_bytes: vec![1, 2, 3, 4],
            width: 2,
            height: 2,
        })
    }

    async fn pick_from_library(&self) -> Result<CapturedImage, CameraError> {
        Ok(CapturedImage {
            jpeg_bytes: vec![5, 6],
            width: 1,
            height: 1,
        })
    }
}

#[derive(Default)]
struct LoopbackChannel {
    read_frame: std::sync::Mutex<Option<MediaFrame>>,
    last_accepted: AtomicUsize,
}

#[async_trait]
impl MediaAudioChannel for LoopbackChannel {
    async fn read(&self) -> Result<Option<MediaFrame>, MediaError> {
        Ok(self.read_frame.lock().unwrap().take())
    }

    async fn write(&self, frame: MediaPcmFrame) -> Result<MediaRenderReceipt, MediaError> {
        frame.validate(1024)?;
        let sample_frames = frame.pcm16le.len() / (usize::from(frame.channels) * 2);
        self.last_accepted.store(sample_frames, Ordering::SeqCst);
        Ok(MediaRenderReceipt {
            accepted_sample_frames: sample_frames as u32,
            played_sample_frames: sample_frames as u32,
            rendered_at_unix_ms: Some(now_ms()),
        })
    }

    async fn flush(&self) -> Result<MediaRenderReceipt, MediaError> {
        Ok(MediaRenderReceipt {
            accepted_sample_frames: 0,
            played_sample_frames: self.last_accepted.swap(0, Ordering::SeqCst) as u32,
            rendered_at_unix_ms: Some(now_ms()),
        })
    }

    async fn interrupt(&self) -> Result<MediaRenderReceipt, MediaError> {
        self.flush().await
    }
}

struct FakeSession {
    reference: MediaSessionRef,
    status: Arc<std::sync::Mutex<MediaSessionStatus>>,
    authorized_sources: Arc<std::sync::Mutex<Vec<MediaSourceRef>>>,
    stop_count: Arc<AtomicUsize>,
    registration_count: Arc<AtomicUsize>,
    stop_completed: Arc<Notify>,
    event_batches: Arc<std::sync::Mutex<VecDeque<MediaEventBatch>>>,
    block_observe: Arc<AtomicBool>,
    observe_started: Arc<Notify>,
    channel: Arc<LoopbackChannel>,
    nonterminal_stop: Arc<AtomicBool>,
    pending_stop: Arc<AtomicBool>,
    stop_started: Arc<Semaphore>,
    stop_release: Arc<Semaphore>,
    nested_stop: Arc<
        std::sync::Mutex<Option<(std::sync::Weak<UnifiedMediaService>, MediaOperationContext)>>,
    >,
    replacement_on_stop: Arc<std::sync::Mutex<Option<Box<dyn MediaSession>>>>,
}

impl Clone for FakeSession {
    fn clone(&self) -> Self {
        Self {
            reference: self.reference.clone(),
            status: self.status.clone(),
            authorized_sources: self.authorized_sources.clone(),
            stop_count: self.stop_count.clone(),
            registration_count: self.registration_count.clone(),
            stop_completed: self.stop_completed.clone(),
            event_batches: self.event_batches.clone(),
            block_observe: self.block_observe.clone(),
            observe_started: self.observe_started.clone(),
            channel: self.channel.clone(),
            nonterminal_stop: self.nonterminal_stop.clone(),
            pending_stop: self.pending_stop.clone(),
            stop_started: self.stop_started.clone(),
            stop_release: self.stop_release.clone(),
            nested_stop: self.nested_stop.clone(),
            replacement_on_stop: self.replacement_on_stop.clone(),
        }
    }
}

impl FakeSession {
    fn new(reference: MediaSessionRef) -> Self {
        Self {
            reference,
            status: Arc::new(std::sync::Mutex::new(MediaSessionStatus {
                state: MediaSessionState::Preparing,
                audio_enabled: true,
                video_enabled: false,
            })),
            authorized_sources: Arc::new(std::sync::Mutex::new(Vec::new())),
            stop_count: Arc::new(AtomicUsize::new(0)),
            registration_count: Arc::new(AtomicUsize::new(0)),
            stop_completed: Arc::new(Notify::new()),
            event_batches: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            block_observe: Arc::new(AtomicBool::new(false)),
            observe_started: Arc::new(Notify::new()),
            channel: Arc::new(LoopbackChannel::default()),
            nonterminal_stop: Arc::new(AtomicBool::new(false)),
            pending_stop: Arc::new(AtomicBool::new(false)),
            stop_started: Arc::new(Semaphore::new(0)),
            stop_release: Arc::new(Semaphore::new(0)),
            nested_stop: Arc::new(std::sync::Mutex::new(None)),
            replacement_on_stop: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn enqueue(&self, batch: MediaEventBatch) {
        self.event_batches.lock().unwrap().push_back(batch);
    }

    fn set_authorized_sources(&self, sources: Vec<MediaSourceRef>) {
        *self.authorized_sources.lock().unwrap() = sources;
    }

    fn nest_service_stop(
        &self,
        service: &Arc<UnifiedMediaService>,
        context: MediaOperationContext,
    ) {
        *self.nested_stop.lock().unwrap() = Some((Arc::downgrade(service), context));
    }

    fn replace_after_nested_stop(&self, replacement: Box<dyn MediaSession>) {
        *self.replacement_on_stop.lock().unwrap() = Some(replacement);
    }
}

#[async_trait]
impl MediaSession for FakeSession {
    fn reference(&self) -> &MediaSessionRef {
        &self.reference
    }
    fn on_registered(&self) {
        self.registration_count.fetch_add(1, Ordering::SeqCst);
    }
    fn status(&self) -> MediaSessionStatus {
        self.status.lock().unwrap().clone()
    }
    fn authorized_sources(&self) -> Vec<MediaSourceRef> {
        self.authorized_sources.lock().unwrap().clone()
    }
    async fn audio_channel(&self) -> Result<Arc<dyn MediaAudioChannel>, MediaError> {
        Ok(self.channel.clone())
    }

    async fn update(
        &self,
        _context: MediaOperationContext,
        update: MediaSessionUpdate,
    ) -> Result<MediaSessionStatus, MediaError> {
        let mut status = self.status.lock().unwrap();
        match update {
            MediaSessionUpdate::SetEnabled {
                kind: MediaKind::Audio,
                enabled,
            } => status.audio_enabled = enabled,
            MediaSessionUpdate::SetEnabled {
                kind: MediaKind::Video,
                enabled,
            } => status.video_enabled = enabled,
            MediaSessionUpdate::SetPlaybackGain { .. }
            | MediaSessionUpdate::SelectSource { .. }
            | MediaSessionUpdate::SetEnabled {
                kind: MediaKind::Image,
                ..
            } => {}
        }
        Ok(status.clone())
    }

    async fn observe(
        &self,
        _context: MediaOperationContext,
        _after: Option<MediaEventCursor>,
        _limit: u16,
        _wait_ms: Option<u64>,
    ) -> Result<MediaEventBatch, MediaError> {
        self.observe_started.notify_one();
        if self.block_observe.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(self
            .event_batches
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                MediaEventBatch::bounded(
                    MediaResourceRef::Session(self.reference.clone()),
                    Vec::new(),
                    false,
                )
                .expect("empty bounded batch")
            }))
    }

    async fn stop(
        &self,
        _context: MediaOperationContext,
    ) -> Result<MediaSessionStatus, MediaError> {
        self.stop_count.fetch_add(1, Ordering::SeqCst);
        if self.pending_stop.load(Ordering::SeqCst) {
            self.stop_started.add_permits(1);
            let permit = self.stop_release.acquire().await.map_err(|_| {
                MediaError::new(MediaErrorKind::NativeFailure, "stop gate was closed")
            })?;
            permit.forget();
        }
        let status = {
            let mut status = self.status.lock().unwrap();
            if !self.nonterminal_stop.load(Ordering::SeqCst) {
                status.state = MediaSessionState::Ended;
            }
            status.clone()
        };
        let nested = self.nested_stop.lock().unwrap().take();
        if let Some((service, context)) = nested {
            if let Some(service) = service.upgrade() {
                service
                    .execute(
                        context.clone(),
                        MediaOperation::Stop {
                            target: MediaResourceRef::Session(self.reference.clone()),
                        },
                    )
                    .await?;
                let replacement = self.replacement_on_stop.lock().unwrap().take();
                if let Some(replacement) = replacement {
                    service
                        .attach_session(fresh_context_same_identity(&context), replacement)
                        .await?;
                }
            }
        }
        self.stop_completed.notify_one();
        Ok(status)
    }
}

struct FakeConnector {
    name: String,
    next_id: AtomicUsize,
    sources: Vec<MediaSource>,
    sessions: std::sync::Mutex<Vec<Arc<FakeSession>>>,
    pending_start: AtomicBool,
    started_sessions: Semaphore,
    fail_next_start: AtomicBool,
    pending_sources: AtomicBool,
    sources_started: Semaphore,
    pending_session_stop: AtomicBool,
    next_authorized_sources: std::sync::Mutex<Option<Vec<MediaSourceRef>>>,
}

impl FakeConnector {
    fn new(name: &str, sources: Vec<MediaSource>) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            next_id: AtomicUsize::new(1),
            sources,
            sessions: std::sync::Mutex::new(Vec::new()),
            pending_start: AtomicBool::new(false),
            started_sessions: Semaphore::new(0),
            fail_next_start: AtomicBool::new(false),
            pending_sources: AtomicBool::new(false),
            sources_started: Semaphore::new(0),
            pending_session_stop: AtomicBool::new(false),
            next_authorized_sources: std::sync::Mutex::new(None),
        })
    }

    fn latest_session(&self) -> Arc<FakeSession> {
        self.sessions
            .lock()
            .unwrap()
            .last()
            .expect("session started")
            .clone()
    }

    fn set_next_authorized_sources(&self, sources: Vec<MediaSourceRef>) {
        *self.next_authorized_sources.lock().unwrap() = Some(sources);
    }
}

#[async_trait]
impl MediaConnector for FakeConnector {
    fn id(&self) -> MediaConnectorId {
        MediaConnectorId::from(self.name.clone())
    }

    async fn sources(
        &self,
        _context: MediaOperationContext,
    ) -> Result<Vec<MediaSource>, MediaError> {
        if self.pending_sources.load(Ordering::SeqCst) {
            self.sources_started.add_permits(1);
            std::future::pending::<()>().await;
        }
        Ok(self.sources.clone())
    }

    async fn start_session(
        &self,
        _context: MediaOperationContext,
        _request: MediaSessionRequest,
    ) -> Result<Box<dyn MediaSession>, MediaError> {
        if self.fail_next_start.swap(false, Ordering::SeqCst) {
            return Err(MediaError::new(
                MediaErrorKind::NativeFailure,
                "injected session start failure",
            ));
        }
        if self.pending_start.load(Ordering::SeqCst) {
            self.started_sessions.add_permits(1);
            std::future::pending::<()>().await;
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let session = Arc::new(FakeSession::new(MediaSessionRef(MediaSessionId::from(
            format!("{}-{id}", self.name),
        ))));
        if self.pending_session_stop.load(Ordering::SeqCst) {
            session.pending_stop.store(true, Ordering::SeqCst);
        }
        if let Some(sources) = self.next_authorized_sources.lock().unwrap().take() {
            session.set_authorized_sources(sources);
        }
        self.sessions.lock().unwrap().push(session.clone());
        Ok(Box::new(session.as_ref().clone()))
    }
}

/// Like a native adapter, returns the same physical session for a repeated
/// open key while that lease is alive.
#[derive(Default)]
struct IdempotentConnector {
    source_calls: AtomicUsize,
    open_calls: AtomicUsize,
    sessions: std::sync::Mutex<HashMap<MediaOperationId, FakeSession>>,
}

#[async_trait]
impl MediaConnector for IdempotentConnector {
    fn id(&self) -> MediaConnectorId {
        MediaConnectorId::from("idempotent")
    }

    async fn sources(
        &self,
        _context: MediaOperationContext,
    ) -> Result<Vec<MediaSource>, MediaError> {
        self.source_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }

    async fn start_session(
        &self,
        context: MediaOperationContext,
        _request: MediaSessionRequest,
    ) -> Result<Box<dyn MediaSession>, MediaError> {
        self.open_calls.fetch_add(1, Ordering::SeqCst);
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.entry(context.identity.clone()).or_insert_with(|| {
            FakeSession::new(MediaSessionRef(MediaSessionId::from(format!(
                "idempotent-{}",
                context.identity.id
            ))))
        });
        Ok(Box::new(session.clone()))
    }
}

fn video_remote_source() -> MediaSource {
    MediaSource {
        reference: MediaSourceRef(MediaSourceId::from("remote:camera")),
        kind: MediaSourceKind::Remote,
        media_kind: Some(MediaKind::Video),
        label: "Remote camera".to_owned(),
        usages: vec![MediaSourceUsage::Session],
        supported_operations: vec![MediaOperationKind::SessionStart],
        is_default: false,
    }
}

fn audio_remote_source(id: &str) -> MediaSource {
    MediaSource {
        reference: MediaSourceRef(MediaSourceId::from(id)),
        kind: MediaSourceKind::Remote,
        media_kind: Some(MediaKind::Audio),
        label: "Remote microphone".to_owned(),
        usages: vec![MediaSourceUsage::Session],
        supported_operations: vec![MediaOperationKind::SessionStart],
        is_default: false,
    }
}

fn audio_frame(source: MediaSourceRef, sequence: u64) -> MediaFrame {
    MediaFrame {
        source,
        sequence,
        captured_at_unix_ms: now_ms(),
        payload: MediaFramePayload::Audio(MediaPcmFrame {
            sample_rate_hz: 24_000,
            channels: 1,
            pcm16le: vec![0, 0, 1, 0],
        }),
    }
}

fn service(
    audio: Arc<dyn AudioService>,
    camera: bool,
    connector: Option<Arc<dyn MediaConnector>>,
) -> Arc<UnifiedMediaService> {
    let mut builder = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
        .audio(audio)
        .service_epoch(EPOCH);
    if camera {
        builder = builder.camera(Arc::new(FakeCamera));
    }
    if let Some(connector) = connector {
        builder = builder.connector(
            connector,
            [
                MediaSessionMode::Audio,
                MediaSessionMode::Video,
                MediaSessionMode::AudioVideo,
            ],
        );
    }
    builder.build()
}

#[tokio::test]
async fn unified_status_records_itself_active_but_attachment_drain_filters_that_exact_id() {
    let runtime = service(FakeAudio::new(), false, None);
    let host = crate::attachment::MediaAttachmentHost::new();
    let attachment = host.attach(runtime.clone()).unwrap();
    let scoped = attachment.service();
    let status_context = context(owner("status-owner"), 7_701);
    let status_identity = status_context.identity.clone();
    let status_incarnation = status_context.operation_incarnation();
    let result = scoped
        .execute(status_context, MediaOperation::Status { target: None })
        .await
        .unwrap();
    let MediaResult::Status { resources } = result else {
        panic!("status operation should return a status snapshot");
    };
    assert!(resources.iter().any(|resource| {
        matches!(&resource.resource, MediaResourceRef::Operation { identity, incarnation } if identity == &status_identity && *incarnation == status_incarnation)
            && resource.state == MediaResourceState::Active
    }));

    assert_eq!(
        attachment.revoke(Duration::from_secs(1)).await,
        crate::attachment::MediaAttachmentRevocation::Confirmed
    );
}

#[tokio::test]
async fn cancel_does_not_cross_an_operation_owner_even_when_identity_is_known() {
    let audio = FakeAudio::new();
    audio.pending_listen.store(true, Ordering::SeqCst);
    let runtime = service(audio.clone(), false, None);
    let active_context = context(owner("owner-b"), 7_702);
    let task_runtime = runtime.clone();
    let task_context = active_context.clone();
    let task = tokio::spawn(async move {
        task_runtime
            .execute(
                task_context,
                MediaOperation::Transcribe {
                    input: TranscriptionInput::LiveSource(microphone()),
                    language: None,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), audio.listen_started.notified())
        .await
        .expect("owner-b's audio operation must start");

    let mut wrong_owner = active_context.clone();
    wrong_owner.owner = owner("owner-a");
    runtime.cancel(wrong_owner).await.unwrap();
    assert!(!task.is_finished());
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 0);

    runtime.cancel(active_context).await.unwrap();
    let cancelled = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("owner-b's cancelled operation must exit")
        .unwrap()
        .unwrap_err();
    assert_eq!(cancelled.kind, MediaErrorKind::Cancelled);
    assert!(audio.cancellations.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn stop_reference_handle_does_not_bypass_operation_owner_check() {
    let audio = FakeAudio::new();
    audio.pending_listen.store(true, Ordering::SeqCst);
    let runtime = service(audio.clone(), false, None);
    let active_context = context(owner("operation-owner"), 7_705);
    let operation_ref = MediaResourceRef::Operation {
        identity: active_context.identity.clone(),
        incarnation: active_context.operation_incarnation(),
    };
    let task_runtime = runtime.clone();
    let task_context = active_context.clone();
    let task = tokio::spawn(async move {
        task_runtime
            .execute(
                task_context,
                MediaOperation::Transcribe {
                    input: TranscriptionInput::LiveSource(microphone()),
                    language: None,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), audio.listen_started.notified())
        .await
        .unwrap();

    let wrong_owner_stop = runtime
        .execute(
            context(owner("other-owner"), 7_706),
            MediaOperation::Stop {
                target: operation_ref,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(wrong_owner_stop.kind, MediaErrorKind::NotFound);
    assert!(!task.is_finished());
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 0);

    runtime.cancel(active_context).await.unwrap();
    assert_eq!(
        task.await.unwrap().unwrap_err().kind,
        MediaErrorKind::Cancelled
    );
}

#[tokio::test]
async fn attachment_drain_cancels_audio_with_the_original_operation_handle() {
    let audio = FakeAudio::new();
    audio.pending_listen.store(true, Ordering::SeqCst);
    let runtime = service(audio.clone(), false, None);
    let host = crate::attachment::MediaAttachmentHost::new();
    let attachment = host.attach(runtime).unwrap();
    let service = attachment.service();
    let operation_context = context(owner("drain-audio"), 7_704);
    let task_service = service.clone();
    let operation = tokio::spawn(async move {
        task_service
            .execute(
                operation_context,
                MediaOperation::Transcribe {
                    input: TranscriptionInput::LiveSource(microphone()),
                    language: None,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), audio.listen_started.notified())
        .await
        .expect("the attached audio operation should be pending before drain");

    assert_eq!(
        attachment.revoke(Duration::from_secs(1)).await,
        crate::attachment::MediaAttachmentRevocation::Confirmed
    );
    let result = operation.await.unwrap().unwrap_err();
    assert_eq!(result.kind, MediaErrorKind::Cancelled);
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 1);
}

#[test]
fn media_builder_keeps_a_logical_audio_epoch_separate_from_native_epoch() {
    let runtime = service(FakeAudio::with_backend_epoch(77), false, None);
    assert_eq!(
        runtime.capabilities_for(&owner("alpha")).service_epoch,
        EPOCH
    );
    assert_eq!(
        runtime
            .audio_service()
            .capabilities_for(&owner("alpha"))
            .service_epoch,
        EPOCH
    );
}

#[tokio::test]
async fn builder_normalizes_zero_epoch_before_native_backend_becomes_ready() {
    let native = FakeAudio::with_backend_epoch(0);
    let runtime = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
        .audio(native.clone())
        .build();
    let owner = owner("deferred-audio");
    assert_eq!(runtime.capabilities_for(&owner).service_epoch, 1);
    let audio = runtime.audio_service();
    assert_eq!(audio.capabilities_for(&owner).service_epoch, 1);

    native.native_epoch.store(77, Ordering::SeqCst);
    audio
        .execute(
            MediaOperationContext::new(MediaOperationId::new(5, 1), owner.clone()),
            AudioOperation::StartRecording {
                sample_rate_hz: 24_000,
                format: "audio/wav".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        native.contexts.lock().unwrap()[0].identity.service_epoch,
        77
    );
    assert_eq!(runtime.capabilities_for(&owner).service_epoch, 1);
    assert_eq!(audio.capabilities_for(&owner).service_epoch, 1);
}

fn session_request(mode: MediaSessionMode, intent: MediaSessionIntent) -> MediaSessionRequest {
    MediaSessionRequest {
        intent,
        mode,
        audio_source: None,
        video_source: None,
    }
}

#[tokio::test]
async fn audio_capture_transcription_synthesis_playback_and_speech_use_the_shared_store() {
    let audio = FakeAudio::new();
    let runtime = service(audio.clone(), true, None);
    let capture_context = context(owner("alpha"), 1);
    let result = runtime
        .execute(
            capture_context.clone(),
            MediaOperation::Capture {
                source: microphone(),
                options: CaptureOptions::default(),
            },
        )
        .await
        .unwrap();
    let MediaResult::Captured { asset } = result else {
        panic!("capture result")
    };
    assert_eq!(asset.metadata.kind, MediaKind::Audio);
    let content = runtime
        .read_media(context(owner("alpha"), 2), asset.reference.clone())
        .await
        .unwrap();
    assert_eq!(content.bytes, vec![4, 5, 6]);

    let transcribed = runtime
        .execute(
            context(owner("alpha"), 3),
            MediaOperation::Transcribe {
                input: TranscriptionInput::StoredMedia(asset.reference),
                language: Some("en-US".to_owned()),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(transcribed, MediaResult::Transcript { transcript } if transcript.text == "recognized words")
    );

    let synthesized = runtime
        .execute(
            context(owner("alpha"), 4),
            MediaOperation::Synthesize {
                text: "hello".to_owned(),
                language: None,
                rate: None,
                voice: None,
            },
        )
        .await
        .unwrap();
    let MediaResult::Synthesized { asset } = synthesized else {
        panic!("synthesis result")
    };
    assert_eq!(asset.metadata.sample_rate_hz, Some(24_000));
    let played = runtime
        .execute(
            context(owner("alpha"), 5),
            MediaOperation::Play {
                media: asset.reference,
                source: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        played,
        MediaResult::PlaybackCompleted { duration_ms: 41 }
    ));
    let spoken = runtime
        .execute(
            context(owner("alpha"), 6),
            MediaOperation::Speak {
                text: "hello".to_owned(),
                language: None,
                rate: None,
                voice: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        spoken,
        MediaResult::SpeechCompleted { duration_ms: 42 }
    ));
    assert!(audio.operations().contains(&Some(AudioOperationKind::Play)));
    assert!(audio
        .operations()
        .contains(&Some(AudioOperationKind::Speak)));

    let image = runtime
        .execute(
            context(owner("alpha"), 7),
            MediaOperation::Capture {
                source: camera_source(),
                options: CaptureOptions::default(),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(image, MediaResult::Captured { asset } if asset.metadata.kind == MediaKind::Image)
    );
}

#[tokio::test]
async fn camera_capture_readiness_is_not_blocked_by_busy_audio_capture() {
    let audio = FakeAudio::new();
    audio.set_readiness(AudioOperationKind::Capture, AudioReadinessState::Busy);
    let runtime = service(audio, true, None);
    let owner = owner("camera-while-audio-busy");
    let capabilities = runtime.capabilities_for(&owner);
    assert!(capabilities.supports(MediaOperationKind::Capture));
    assert!(!capabilities
        .readiness
        .iter()
        .any(|entry| entry.operation == MediaOperationKind::Capture));

    let image = runtime
        .execute(
            context(owner.clone(), 91),
            MediaOperation::Capture {
                source: camera_source(),
                options: CaptureOptions::default(),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(image, MediaResult::Captured { asset } if asset.metadata.kind == MediaKind::Image)
    );

    let microphone = runtime
        .execute(
            context(owner, 92),
            MediaOperation::Capture {
                source: microphone(),
                options: CaptureOptions::default(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(microphone.kind, MediaErrorKind::Busy);
}

#[tokio::test]
async fn microphone_source_is_available_for_audio_sessions_without_legacy_audio_ops() {
    let audio = FakeAudio::new();
    audio.set_supported_operations(Vec::new());
    let connector = FakeConnector::new("session-only-audio", vec![]);
    let runtime = service(audio, false, Some(connector));
    let listed = runtime
        .execute(
            context(owner("session-only"), 93),
            MediaOperation::Sources {
                kind: Some(MediaKind::Audio),
                usage: Some(MediaSourceUsage::Session),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(listed, MediaResult::Sources { sources } if sources.iter().any(|source| {
            source.reference == microphone()
                && source.kind == MediaSourceKind::Microphone
                && source.supported_operations == vec![MediaOperationKind::SessionStart]
        }))
    );

    let started = runtime
        .execute(
            context(owner("session-only"), 94),
            MediaOperation::SessionStart {
                request: MediaSessionRequest {
                    intent: MediaSessionIntent::Conversation,
                    mode: MediaSessionMode::Audio,
                    audio_source: Some(microphone()),
                    video_source: None,
                },
            },
        )
        .await
        .unwrap();
    assert!(matches!(started, MediaResult::SessionStarted { .. }));
}

#[tokio::test]
async fn transcription_readiness_defers_to_the_selected_input_route() {
    for (live_readiness, stored_readiness) in [
        (AudioReadinessState::Unavailable, AudioReadinessState::Ready),
        (AudioReadinessState::Ready, AudioReadinessState::Unavailable),
    ] {
        let audio = FakeAudio::new();
        audio.set_readiness(AudioOperationKind::Listen, live_readiness);
        audio.set_readiness(AudioOperationKind::Transcribe, stored_readiness);
        let store = Arc::new(MemoryStore::default());
        let runtime = MediaRuntimeBuilder::new(store.clone())
            .audio(audio.clone())
            .service_epoch(EPOCH)
            .build();
        let owner = owner("transcription-routes");
        let stored_asset = store
            .put(
                &context(owner.clone(), 800),
                MediaPayload {
                    metadata: MediaMetadata {
                        kind: MediaKind::Audio,
                        mime_type: "audio/wav".to_owned(),
                        byte_len: 4,
                        duration_ms: Some(1),
                        width_px: None,
                        height_px: None,
                        sample_rate_hz: Some(24_000),
                        channels: Some(1),
                        captured_at_unix_ms: None,
                    },
                    bytes: vec![0, 0, 0, 0],
                },
            )
            .await
            .unwrap();

        let capabilities = runtime.capabilities_for(&owner);
        assert!(capabilities.supports(MediaOperationKind::Transcribe));
        assert!(!capabilities
            .readiness
            .iter()
            .any(|entry| entry.operation == MediaOperationKind::Transcribe));

        let live = runtime
            .execute(
                context(owner.clone(), 801),
                MediaOperation::Transcribe {
                    input: TranscriptionInput::LiveSource(microphone()),
                    language: None,
                },
            )
            .await;
        if live_readiness == AudioReadinessState::Unavailable {
            assert_eq!(live.unwrap_err().kind, MediaErrorKind::Unavailable);
            assert!(!audio
                .operations()
                .contains(&Some(AudioOperationKind::Listen)));
        } else {
            assert!(matches!(live.unwrap(), MediaResult::Transcript { .. }));
            assert!(audio
                .operations()
                .contains(&Some(AudioOperationKind::Listen)));
        }

        let stored = runtime
            .execute(
                context(owner, 802),
                MediaOperation::Transcribe {
                    input: TranscriptionInput::StoredMedia(stored_asset.reference),
                    language: None,
                },
            )
            .await;
        if stored_readiness == AudioReadinessState::Unavailable {
            assert_eq!(stored.unwrap_err().kind, MediaErrorKind::Unavailable);
            assert!(!audio
                .operations()
                .contains(&Some(AudioOperationKind::Transcribe)));
        } else {
            assert!(matches!(stored.unwrap(), MediaResult::Transcript { .. }));
            assert!(audio
                .operations()
                .contains(&Some(AudioOperationKind::Transcribe)));
        }
    }
}

#[tokio::test]
async fn scoped_asset_status_does_not_require_disconnected_audio_to_revoke() {
    for found in [false, true] {
        let audio = FakeAudio::new();
        audio.fail_status.store(true, Ordering::SeqCst);
        let runtime = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
            .audio(audio.clone())
            .service_epoch(EPOCH)
            .build();
        let host = crate::MediaAttachmentHost::new();
        let attachment = host.attach(runtime.clone()).unwrap();
        let reference = media_store_ref("test-asset");
        if found {
            let existing_owner = owner("alpha").in_attachment(attachment.id().clone());
            runtime
                .remember_asset(
                    &context(existing_owner, 149),
                    &MediaAsset {
                        reference: reference.clone(),
                        metadata: MediaMetadata {
                            kind: MediaKind::Audio,
                            mime_type: "audio/wav".into(),
                            byte_len: 4,
                            captured_at_unix_ms: None,
                            duration_ms: Some(1),
                            width_px: None,
                            height_px: None,
                            sample_rate_hz: Some(24_000),
                            channels: Some(1),
                        },
                    },
                )
                .unwrap();
        }
        let result = attachment
            .service()
            .execute(
                context(owner("alpha"), 150),
                MediaOperation::Status {
                    target: Some(MediaResourceRef::Media(reference)),
                },
            )
            .await;
        if found {
            assert!(
                matches!(result, Ok(MediaResult::Status { resources }) if resources.len() == 1 && resources[0].state == MediaResourceState::Completed)
            );
        } else {
            assert_eq!(result.unwrap_err().kind, MediaErrorKind::NotFound);
        }
        assert!(
            audio.operations().is_empty(),
            "the asset lookup does not dispatch native audio"
        );
        assert_eq!(
            attachment.revoke(Duration::from_secs(1)).await,
            crate::MediaAttachmentRevocation::Confirmed
        );
        assert!(
            audio.operations().is_empty(),
            "read-only asset status must not require a later audio status"
        );
    }
}

#[tokio::test]
async fn missing_asset_status_does_not_erase_a_prior_unknown_native_origin() {
    let audio = FakeAudio::new();
    audio.fail_status.store(true, Ordering::SeqCst);
    audio.fail_next_native_call.store(true, Ordering::SeqCst);
    audio.fail_cancel.store(true, Ordering::SeqCst);
    let runtime = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
        .audio(audio.clone())
        .service_epoch(EPOCH)
        .build();
    let host = crate::MediaAttachmentHost::new();
    let attachment = host.attach(runtime).unwrap();
    let scoped = attachment.service();
    let recording = context(owner("alpha"), 151);
    assert!(scoped
        .execute(
            recording.clone(),
            MediaOperation::Record {
                source: microphone(),
                options: RecordOptions::default(),
            }
        )
        .await
        .is_err());
    let error = scoped
        .execute(
            context(owner("alpha"), 152),
            MediaOperation::Status {
                target: Some(MediaResourceRef::Media(media_store_ref("missing-asset"))),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, MediaErrorKind::NotFound);
    assert!(audio
        .operations()
        .contains(&Some(AudioOperationKind::Record)));
    assert_ne!(
        attachment.revoke(Duration::from_secs(1)).await,
        crate::MediaAttachmentRevocation::Confirmed
    );
    assert_ne!(
        attachment.operation_cleanup(&recording),
        crate::MediaOperationCleanup::Confirmed
    );
}

#[tokio::test]
async fn scoped_media_audio_failure_requires_its_exact_close_receipt() {
    for play in [false, true] {
        for cancel_confirmed in [false, true] {
            let audio = FakeAudio::new();
            let runtime = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
                .audio(audio.clone())
                .service_epoch(EPOCH)
                .build();
            let host = crate::MediaAttachmentHost::new();
            let attachment = host.attach(runtime).unwrap();
            let scoped = attachment.service();
            let operation = if play {
                let MediaResult::Synthesized { asset } = scoped
                    .execute(
                        context(owner("alpha"), 160),
                        MediaOperation::Synthesize {
                            text: "hello".to_owned(),
                            language: None,
                            rate: None,
                            voice: None,
                        },
                    )
                    .await
                    .unwrap()
                else {
                    panic!("synthesized asset");
                };
                MediaOperation::Play {
                    media: asset.reference,
                    source: None,
                }
            } else {
                MediaOperation::Record {
                    source: microphone(),
                    options: RecordOptions::default(),
                }
            };
            audio.fail_next_native_call.store(true, Ordering::SeqCst);
            audio.fail_cancel.store(!cancel_confirmed, Ordering::SeqCst);
            let call = context(owner("alpha"), 161);
            let error = scoped.execute(call.clone(), operation).await.unwrap_err();
            assert_eq!(error.kind, MediaErrorKind::NativeFailure);
            tokio::time::timeout(Duration::from_secs(1), async {
                while audio.cancellations.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("exact original audio cancellation was dispatched");
            let result = attachment.revoke(Duration::from_secs(1)).await;
            if cancel_confirmed {
                assert_eq!(result, crate::MediaAttachmentRevocation::Confirmed);
                assert_eq!(
                    attachment.operation_cleanup(&call),
                    crate::MediaOperationCleanup::Confirmed
                );
            } else {
                assert_ne!(result, crate::MediaAttachmentRevocation::Confirmed);
                assert_ne!(
                    attachment.operation_cleanup(&call),
                    crate::MediaOperationCleanup::Confirmed
                );
            }
        }
    }
}

#[tokio::test]
async fn scoped_media_audio_cleanup_waits_for_inflight_exact_cancel() {
    let audio = FakeAudio::new();
    audio.fail_next_native_call.store(true, Ordering::SeqCst);
    audio.hold_cancel.store(true, Ordering::SeqCst);
    let runtime = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
        .audio(audio.clone())
        .service_epoch(EPOCH)
        .build();
    let host = crate::MediaAttachmentHost::new();
    let attachment = host.attach(runtime).unwrap();
    let call = context(owner("alpha"), 165);
    assert!(attachment
        .service()
        .execute(
            call.clone(),
            MediaOperation::Record {
                source: microphone(),
                options: RecordOptions::default(),
            },
        )
        .await
        .is_err());
    tokio::time::timeout(Duration::from_secs(1), audio.cancel_entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert_eq!(
        attachment.operation_cleanup(&call),
        crate::MediaOperationCleanup::Pending
    );
    let draining = attachment.clone();
    let mut drain = tokio::spawn(async move { draining.revoke(Duration::from_secs(2)).await });
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut drain)
        .await
        .is_err());
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 1);
    audio.cancel_release.add_permits(1);
    assert_eq!(
        drain.await.unwrap(),
        crate::MediaAttachmentRevocation::Confirmed
    );
    assert_eq!(
        attachment.operation_cleanup(&call),
        crate::MediaOperationCleanup::Confirmed
    );
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn scoped_media_audio_late_exact_receipt_can_be_rechecked_before_revoke() {
    let audio = FakeAudio::new();
    audio.fail_next_native_call.store(true, Ordering::SeqCst);
    audio.fail_cancel.store(true, Ordering::SeqCst);
    let runtime = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
        .audio(audio.clone())
        .service_epoch(EPOCH)
        .build();
    let host = crate::MediaAttachmentHost::new();
    let attachment = host.attach(runtime).unwrap();
    let scoped = attachment.service();
    let call = context(owner("alpha"), 166);
    assert!(scoped
        .execute(
            call.clone(),
            MediaOperation::Record {
                source: microphone(),
                options: RecordOptions::default(),
            }
        )
        .await
        .is_err());
    tokio::time::timeout(Duration::from_secs(1), async {
        while audio.cancellations.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(scoped.cancel(call.clone()).await.is_err());
    let count = audio.cancellations.load(Ordering::SeqCst);
    audio.fail_cancel.store(false, Ordering::SeqCst); // exact late no-resource proof is now available
    let mut wrong_owner = call.clone();
    wrong_owner.owner = owner("other");
    scoped.cancel(wrong_owner).await.unwrap();
    scoped
        .cancel(fresh_context_same_identity(&call))
        .await
        .unwrap();
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), count);
    assert_eq!(
        attachment.operation_cleanup(&call),
        crate::MediaOperationCleanup::Pending
    );
    scoped.cancel(call.clone()).await.unwrap();
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), count + 1);
    assert_eq!(
        attachment.operation_cleanup(&call),
        crate::MediaOperationCleanup::Confirmed
    );
    assert_eq!(
        attachment.revoke(Duration::from_secs(1)).await,
        crate::MediaAttachmentRevocation::Confirmed
    );
}

#[tokio::test]
async fn scoped_direct_audio_late_receipt_releases_only_its_original_lease() {
    let audio = FakeAudio::new();
    audio.fail_next_native_call.store(true, Ordering::SeqCst);
    audio.fail_cancel.store(true, Ordering::SeqCst);
    let runtime = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
        .audio(audio.clone())
        .service_epoch(EPOCH)
        .build();
    let router = runtime.audio_router.clone();
    let host = crate::MediaAttachmentHost::new();
    let attachment = host.attach(runtime).unwrap();
    let scoped = attachment.audio_service(router).unwrap();
    let call = context(owner("alpha"), 168);
    assert!(scoped
        .execute(
            call.clone(),
            AudioOperation::StartRecording {
                sample_rate_hz: 24_000,
                format: "wav".into(),
            }
        )
        .await
        .is_err());
    while audio.cancellations.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    assert!(scoped.cancel(call.clone()).await.is_err());
    let count = audio.cancellations.load(Ordering::SeqCst);
    audio.fail_cancel.store(false, Ordering::SeqCst);
    let mut wrong_owner = call.clone();
    wrong_owner.owner = owner("other");
    scoped.cancel(wrong_owner).await.unwrap();
    scoped
        .cancel(fresh_context_same_identity(&call))
        .await
        .unwrap();
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), count);
    scoped.cancel(call.clone()).await.unwrap();
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), count + 1);
    let cancellations = audio.cancellation_contexts.lock().unwrap();
    let last = cancellations.last().unwrap();
    assert_eq!(last.identity, call.identity);
    assert_eq!(last.operation_incarnation(), call.operation_incarnation());
    assert_eq!(
        last.owner,
        call.owner.clone().in_attachment(attachment.id().clone())
    );
    drop(cancellations);
    assert_eq!(
        attachment.revoke(Duration::from_secs(1)).await,
        crate::MediaAttachmentRevocation::Confirmed
    );
}

#[tokio::test(start_paused = true)]
async fn late_audio_receipt_does_not_reopen_a_terminal_unknown_attachment() {
    let audio = FakeAudio::new();
    audio.fail_next_native_call.store(true, Ordering::SeqCst);
    audio.fail_cancel.store(true, Ordering::SeqCst);
    let runtime = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
        .audio(audio.clone())
        .service_epoch(EPOCH)
        .build();
    let host = crate::MediaAttachmentHost::new();
    let attachment = host.attach(runtime.clone()).unwrap();
    let call = context(owner("alpha"), 167);
    assert!(attachment
        .service()
        .execute(
            call.clone(),
            MediaOperation::Record {
                source: microphone(),
                options: RecordOptions::default(),
            }
        )
        .await
        .is_err());
    while audio.cancellations.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    let handle = attachment.clone();
    let drain = tokio::spawn(async move { handle.revoke(Duration::from_secs(40)).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(31)).await;
    assert!(matches!(
        drain.await.unwrap(),
        crate::MediaAttachmentRevocation::Unknown { .. }
    ));
    assert_eq!(attachment.phase(), crate::MediaAttachmentPhase::Unknown);
    audio.fail_cancel.store(false, Ordering::SeqCst);
    runtime
        .audio_router
        .cancel_for_incarnation(
            &call.identity,
            &call.owner.clone().in_attachment(attachment.id().clone()),
            &call.operation_incarnation(),
        )
        .await
        .unwrap();
    assert_eq!(
        attachment.operation_cleanup(&call),
        crate::MediaOperationCleanup::Confirmed
    );
    assert!(matches!(
        attachment.revoke(Duration::from_secs(1)).await,
        crate::MediaAttachmentRevocation::Unknown { .. }
    ));
}

#[tokio::test]
async fn scoped_media_recording_origin_requires_its_own_close_receipt_on_revoke() {
    for cancel_confirmed in [false, true] {
        let audio = FakeAudio::new();
        let runtime = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
            .audio(audio.clone())
            .service_epoch(EPOCH)
            .build();
        let host = crate::MediaAttachmentHost::new();
        let attachment = host.attach(runtime).unwrap();
        let call = context(owner("alpha"), 169);
        assert!(matches!(
            attachment
                .service()
                .execute(
                    call.clone(),
                    MediaOperation::Record {
                        source: microphone(),
                        options: RecordOptions::default(),
                    }
                )
                .await
                .unwrap(),
            MediaResult::RecordingStarted { .. }
        ));
        assert_eq!(
            attachment.operation_cleanup(&call),
            crate::MediaOperationCleanup::Pending
        );
        assert_eq!(audio.cancellations.load(Ordering::SeqCst), 0);
        audio.fail_cancel.store(!cancel_confirmed, Ordering::SeqCst);
        let outcome = attachment.revoke(Duration::from_secs(1)).await;
        let cancellations = audio.cancellation_contexts.lock().unwrap();
        assert!(!cancellations.is_empty());
        if cancel_confirmed {
            assert_eq!(cancellations.len(), 1);
        }
        assert!(cancellations.iter().all(|cancel| {
            cancel.identity == call.identity
                && cancel.operation_incarnation() == call.operation_incarnation()
                && cancel.owner == call.owner.clone().in_attachment(attachment.id().clone())
        }));
        drop(cancellations);
        if cancel_confirmed {
            assert_eq!(outcome, crate::MediaAttachmentRevocation::Confirmed);
            assert_eq!(
                attachment.operation_cleanup(&call),
                crate::MediaOperationCleanup::Confirmed
            );
        } else {
            assert_ne!(outcome, crate::MediaAttachmentRevocation::Confirmed);
            assert_ne!(
                attachment.operation_cleanup(&call),
                crate::MediaOperationCleanup::Confirmed
            );
        }
    }
}

#[tokio::test]
async fn scoped_audio_stop_does_not_cancel_a_later_start_with_the_same_handle() {
    let audio = FakeAudio::new();
    audio.fixed_recording_handle.store(true, Ordering::SeqCst);
    audio.hold_stop.store(true, Ordering::SeqCst);
    let runtime = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
        .audio(audio.clone())
        .service_epoch(EPOCH)
        .build();
    let router = runtime.audio_router.clone();
    let host = crate::MediaAttachmentHost::new();
    let attachment = host.attach(runtime).unwrap();
    let scoped = attachment.audio_service(router).unwrap();
    let start_a = context(owner("alpha"), 180);
    let AudioOperationSuccess::RecordingStarted { handle } = scoped
        .execute(
            start_a.clone(),
            AudioOperation::StartRecording {
                sample_rate_hz: 24_000,
                format: "wav".into(),
            },
        )
        .await
        .unwrap()
    else {
        panic!("first recording")
    };
    let stopping = scoped.clone();
    let stop = tokio::spawn(async move {
        stopping
            .execute(
                context(owner("alpha"), 181),
                AudioOperation::StopRecording { handle },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), audio.stop_entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let start_b = context(owner("alpha"), 182);
    assert!(matches!(
        scoped
            .execute(
                start_b.clone(),
                AudioOperation::StartRecording {
                    sample_rate_hz: 24_000,
                    format: "wav".into(),
                }
            )
            .await
            .unwrap(),
        AudioOperationSuccess::RecordingStarted { .. }
    ));
    audio.stop_release.add_permits(1);
    assert!(matches!(
        stop.await.unwrap().unwrap(),
        AudioOperationSuccess::Recording { .. }
    ));
    let cancels = audio.cancellation_contexts.lock().unwrap().clone();
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0].identity, start_a.identity);
    assert_eq!(
        cancels[0].operation_incarnation(),
        start_a.operation_incarnation()
    );
    scoped.cancel(start_b.clone()).await.unwrap();
    let cancels = audio.cancellation_contexts.lock().unwrap().clone();
    assert_eq!(cancels.len(), 2);
    assert_eq!(cancels[1].identity, start_b.identity);
    assert_eq!(
        cancels[1].operation_incarnation(),
        start_b.operation_incarnation()
    );
    assert_eq!(
        attachment.revoke(Duration::from_secs(1)).await,
        crate::MediaAttachmentRevocation::Confirmed
    );
}

#[tokio::test]
async fn scoped_media_record_stop_retires_its_exact_audio_origin() {
    let audio = FakeAudio::new();
    let runtime = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
        .audio(audio.clone())
        .service_epoch(EPOCH)
        .build();
    let host = crate::MediaAttachmentHost::new();
    let attachment = host.attach(runtime).unwrap();
    let scoped = attachment.service();
    for index in 0..2 {
        let started = scoped
            .execute(
                context(owner("alpha"), 170 + index * 2),
                MediaOperation::Record {
                    source: microphone(),
                    options: RecordOptions::default(),
                },
            )
            .await
            .unwrap();
        let MediaResult::RecordingStarted { recording } = started else {
            panic!("recording start");
        };
        assert_eq!(audio.cancellations.load(Ordering::SeqCst), index as usize);
        assert!(matches!(
            scoped
                .execute(
                    context(owner("alpha"), 171 + index * 2),
                    MediaOperation::Stop {
                        target: MediaResourceRef::Recording(recording),
                    },
                )
                .await
                .unwrap(),
            MediaResult::RecordingStopped { .. }
        ));
        assert_eq!(
            audio.cancellations.load(Ordering::SeqCst),
            (index + 1) as usize
        );
    }
    assert_eq!(
        attachment.revoke(Duration::from_secs(1)).await,
        crate::MediaAttachmentRevocation::Confirmed
    );
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn recording_stop_is_owner_scoped_and_retries_storage_after_native_finalization() {
    let audio = FakeAudio::new();
    let store = Arc::new(MemoryStore::default());
    let runtime = MediaRuntimeBuilder::new(store.clone())
        .audio(audio.clone())
        .service_epoch(EPOCH)
        .build();
    let started = runtime
        .execute(
            context(owner("alpha"), 10),
            MediaOperation::Record {
                source: microphone(),
                options: RecordOptions::default(),
            },
        )
        .await
        .unwrap();
    let MediaResult::RecordingStarted { recording } = started else {
        panic!("recording start")
    };

    let wrong_owner = runtime
        .execute(
            context(owner("beta"), 11),
            MediaOperation::Stop {
                target: MediaResourceRef::Recording(recording.clone()),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(wrong_owner.kind, MediaErrorKind::NotFound);
    assert_eq!(
        audio
            .operations()
            .iter()
            .filter(|operation| **operation == Some(AudioOperationKind::Record))
            .count(),
        1
    );
    assert_eq!(audio.records.lock().unwrap().len(), 1);

    store.fail_next_put.store(true, Ordering::SeqCst);
    let first_stop = runtime
        .execute(
            context(owner("alpha"), 12),
            MediaOperation::Stop {
                target: MediaResourceRef::Recording(recording.clone()),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(first_stop.kind, MediaErrorKind::StoreFailure);
    assert_eq!(
        audio
            .operations()
            .iter()
            .filter(|operation| **operation == Some(AudioOperationKind::Record))
            .count(),
        2
    );

    // The native stop is retried only if it failed. A storage-only failure keeps finalized bytes.
    let finalized = runtime
        .execute(
            context(owner("alpha"), 13),
            MediaOperation::Stop {
                target: MediaResourceRef::Recording(recording.clone()),
            },
        )
        .await
        .unwrap();
    let MediaResult::RecordingStopped {
        recording: returned,
        asset,
    } = finalized
    else {
        panic!("recording stop result")
    };
    assert_eq!(returned, recording);
    assert_eq!(asset.metadata.byte_len, 3);
    assert_eq!(
        audio
            .operations()
            .iter()
            .filter(|operation| **operation == Some(AudioOperationKind::Record))
            .count(),
        2
    );
    assert_eq!(
        audio
            .operations()
            .iter()
            .filter(|operation| **operation == Some(AudioOperationKind::Transcribe))
            .count(),
        0
    );
    assert_eq!(
        audio
            .operations()
            .iter()
            .filter(|operation| **operation == Some(AudioOperationKind::Capture))
            .count(),
        0
    );
    assert_eq!(
        audio
            .operations()
            .iter()
            .filter(|operation| **operation == Some(AudioOperationKind::Synthesize))
            .count(),
        0
    );
    assert_eq!(
        audio
            .operations()
            .iter()
            .filter(|operation| **operation == Some(AudioOperationKind::Play))
            .count(),
        0
    );
    assert_eq!(audio.records.lock().unwrap().len(), 0);
    let status = runtime
        .execute(
            context(owner("alpha"), 14),
            MediaOperation::Status {
                target: Some(MediaResourceRef::Recording(recording.clone())),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(status, MediaResult::Status { resources } if resources[0].state == MediaResourceState::Completed)
    );
    let repeated = runtime
        .execute(
            context(owner("alpha"), 15),
            MediaOperation::Stop {
                target: MediaResourceRef::Recording(recording),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(repeated, MediaResult::RecordingStopped { asset: repeated_asset, .. } if repeated_asset == asset)
    );
}

#[tokio::test]
async fn concurrent_recording_starts_reserve_capacity_and_release_failed_slots() {
    let audio = FakeAudio::new();
    audio.pending_recording_start.store(true, Ordering::SeqCst);
    let runtime = service(audio.clone(), false, None);
    let starts: Vec<_> = (0..MAX_ACTIVE_RECORDINGS)
        .map(|generation| {
            let runtime = runtime.clone();
            tokio::spawn(async move {
                runtime
                    .execute(
                        context(owner("alpha"), generation as u64 + 100),
                        MediaOperation::Record {
                            source: microphone(),
                            options: RecordOptions::default(),
                        },
                    )
                    .await
            })
        })
        .collect();
    let started = tokio::time::timeout(
        Duration::from_secs(2),
        audio
            .recording_started
            .acquire_many(MAX_ACTIVE_RECORDINGS as u32),
    )
    .await
    .expect("all available recording slots were entered")
    .expect("recording start semaphore remains open");
    started.forget();

    let full = runtime
        .execute(
            context(owner("alpha"), 500),
            MediaOperation::Record {
                source: microphone(),
                options: RecordOptions::default(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(full.kind, MediaErrorKind::Busy);
    assert_eq!(
        runtime.inner.lock().unwrap().recording_reservations,
        MAX_ACTIVE_RECORDINGS
    );

    for start in starts {
        start.abort();
        let _ = start.await;
    }
    assert_eq!(runtime.inner.lock().unwrap().recording_reservations, 0);

    audio.pending_recording_start.store(false, Ordering::SeqCst);
    audio
        .fail_next_recording_start
        .store(true, Ordering::SeqCst);
    let failed = runtime
        .execute(
            context(owner("alpha"), 501),
            MediaOperation::Record {
                source: microphone(),
                options: RecordOptions::default(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(failed.kind, MediaErrorKind::Unavailable);
    assert_eq!(runtime.inner.lock().unwrap().recording_reservations, 0);

    let recovered = runtime
        .execute(
            context(owner("alpha"), 502),
            MediaOperation::Record {
                source: microphone(),
                options: RecordOptions::default(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(recovered, MediaResult::RecordingStarted { .. }));
}

#[tokio::test]
async fn session_connector_swap_is_future_only_and_audio_bridge_is_owner_bound() {
    let audio = FakeAudio::new();
    let first = FakeConnector::new("rtc-a", vec![video_remote_source()]);
    let second = FakeConnector::new("rtc-b", vec![]);
    let runtime = service(audio, false, Some(first.clone()));
    let started = runtime
        .execute(
            context(owner("alpha"), 20),
            MediaOperation::SessionStart {
                request: session_request(MediaSessionMode::Audio, MediaSessionIntent::Conversation),
            },
        )
        .await
        .unwrap();
    let MediaResult::SessionStarted {
        session: first_ref, ..
    } = started
    else {
        panic!("session start")
    };
    let bridge = runtime
        .session_audio(context(owner("alpha"), 21), first_ref.clone())
        .await
        .unwrap();
    assert_eq!(
        first
            .latest_session()
            .registration_count
            .load(Ordering::SeqCst),
        1
    );
    let receipt = bridge
        .write(MediaPcmFrame {
            sample_rate_hz: 24_000,
            channels: 1,
            pcm16le: vec![0; 8],
        })
        .await
        .unwrap();
    assert_eq!(receipt.accepted_sample_frames, 4);
    assert_eq!(bridge.flush().await.unwrap().played_sample_frames, 4);
    let denied = match runtime
        .session_audio(context(owner("beta"), 22), first_ref.clone())
        .await
    {
        Ok(_) => panic!("another owner must not obtain this session audio bridge"),
        Err(error) => error,
    };
    assert_eq!(denied.kind, MediaErrorKind::NotFound);

    runtime.update_connector(Some(second.clone()), [MediaSessionMode::Audio]);
    let started = runtime
        .execute(
            context(owner("beta"), 23),
            MediaOperation::SessionStart {
                request: session_request(MediaSessionMode::Audio, MediaSessionIntent::Observation),
            },
        )
        .await
        .unwrap();
    let MediaResult::SessionStarted {
        session: second_ref,
        ..
    } = started
    else {
        panic!("second session start")
    };
    assert!(second_ref.0 .0.starts_with("rtc-b-"));
    let observation_bridge = match runtime
        .session_audio(context(owner("beta"), 26), second_ref.clone())
        .await
    {
        Ok(_) => panic!("observation session must not expose a conversation bridge"),
        Err(error) => error,
    };
    assert_eq!(observation_bridge.kind, MediaErrorKind::Unsupported);

    let stopped = runtime
        .execute(
            context(owner("alpha"), 24),
            MediaOperation::Stop {
                target: MediaResourceRef::Session(first_ref.clone()),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(stopped, MediaResult::Stopped { resource: MediaResourceRef::Session(ref session) } if session == &first_ref)
    );
    assert_eq!(first.latest_session().stop_count.load(Ordering::SeqCst), 1);
    let status = runtime
        .execute(
            context(owner("beta"), 25),
            MediaOperation::Status {
                target: Some(MediaResourceRef::Session(second_ref.clone())),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(status, MediaResult::Status { resources } if resources[0].state == MediaResourceState::Active)
    );
    let second_session = second.latest_session();
    second_session
        .nonterminal_stop
        .store(true, Ordering::SeqCst);
    let still_stopping = runtime
        .execute(
            context(owner("beta"), 27),
            MediaOperation::Stop {
                target: MediaResourceRef::Session(second_ref.clone()),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(still_stopping.kind, MediaErrorKind::Busy);
    second_session
        .nonterminal_stop
        .store(false, Ordering::SeqCst);
    let terminal = runtime
        .execute(
            context(owner("beta"), 28),
            MediaOperation::Stop {
                target: MediaResourceRef::Session(second_ref),
            },
        )
        .await
        .unwrap();
    assert!(matches!(terminal, MediaResult::Stopped { .. }));
}

#[tokio::test]
async fn nested_terminal_stop_preserves_reused_session_and_emits_one_terminal_event() {
    let runtime = service(FakeAudio::new(), false, None);
    let owner = owner("nested-stop");
    let fake_session = FakeSession::new(MediaSessionRef(MediaSessionId::from("nested-stop")));
    let reference = runtime
        .attach_session(context(owner.clone(), 900), Box::new(fake_session.clone()))
        .await
        .unwrap();
    fake_session.nest_service_stop(&runtime, context(owner.clone(), 902));
    fake_session.replace_after_nested_stop(Box::new(FakeSession::new(reference.clone())));

    let stopped = runtime
        .execute(
            context(owner.clone(), 901),
            MediaOperation::Stop {
                target: MediaResourceRef::Session(reference.clone()),
            },
        )
        .await
        .unwrap();
    assert!(matches!(stopped, MediaResult::Stopped { .. }));
    assert_eq!(fake_session.stop_count.load(Ordering::SeqCst), 2);

    let inner = runtime.inner.lock().unwrap();
    let replacement = inner
        .sessions
        .get(&reference)
        .expect("replacement session remains registered");
    assert_eq!(
        replacement.session.status().state,
        MediaSessionState::Preparing
    );
    let key = EventKey {
        owner,
        target: MediaResourceRef::Session(reference),
    };
    let terminal_events = inner.events[&key]
        .buffer
        .events
        .iter()
        .filter(|event| {
            matches!(
                &event.kind,
                MediaEventKind::StateChanged {
                    state: MediaResourceState::Completed,
                    ..
                }
            )
        })
        .count();
    assert_eq!(terminal_events, 1);
}

#[tokio::test]
async fn session_audio_bridge_rejects_frames_from_unselected_sources() {
    let audio = FakeAudio::new();
    let source_a = audio_remote_source("remote:microphone-a");
    let source_b = audio_remote_source("remote:microphone-b");
    let connector = FakeConnector::new("rtc-audio", vec![source_a.clone(), source_b.clone()]);
    let runtime = service(audio, false, Some(connector.clone()));
    let started = runtime
        .execute(
            context(owner("alpha"), 25),
            MediaOperation::SessionStart {
                request: MediaSessionRequest {
                    intent: MediaSessionIntent::Conversation,
                    mode: MediaSessionMode::Audio,
                    audio_source: Some(source_a.reference.clone()),
                    video_source: None,
                },
            },
        )
        .await
        .unwrap();
    let MediaResult::SessionStarted { session, .. } = started else {
        panic!("session start")
    };
    let fake_session = connector.latest_session();
    let bridge = runtime
        .session_audio(context(owner("alpha"), 26), session)
        .await
        .unwrap();

    *fake_session.channel.read_frame.lock().unwrap() =
        Some(audio_frame(source_b.reference.clone(), 1));
    let rejected = bridge.read().await.unwrap_err();
    assert_eq!(rejected.kind, MediaErrorKind::InvalidRequest);

    *fake_session.channel.read_frame.lock().unwrap() =
        Some(audio_frame(source_a.reference.clone(), 1));
    let accepted = bridge.read().await.unwrap().expect("selected source frame");
    assert_eq!(accepted.source, source_a.reference);
}

#[tokio::test]
async fn attached_session_audio_bridge_projects_declared_input_audio_sources() {
    let audio = FakeAudio::new();
    let microphone = audio_remote_source("remote:incoming-microphone");
    let camera = video_remote_source();
    let connector = FakeConnector::new("incoming", vec![microphone.clone(), camera.clone()]);
    let runtime = service(audio, false, Some(connector));
    let fake_session = FakeSession::new(MediaSessionRef(MediaSessionId::from("incoming-audio")));
    fake_session
        .set_authorized_sources(vec![microphone.reference.clone(), camera.reference.clone()]);
    let session = runtime
        .attach_session(context(owner("alpha"), 27), Box::new(fake_session.clone()))
        .await
        .unwrap();
    assert_eq!(fake_session.registration_count.load(Ordering::SeqCst), 1);
    let bridge = runtime
        .session_audio(context(owner("alpha"), 28), session)
        .await
        .unwrap();

    *fake_session.channel.read_frame.lock().unwrap() =
        Some(audio_frame(camera.reference.clone(), 1));
    assert_eq!(
        bridge.read().await.unwrap_err().kind,
        MediaErrorKind::InvalidRequest
    );
    *fake_session.channel.read_frame.lock().unwrap() =
        Some(audio_frame(microphone.reference.clone(), 1));
    assert_eq!(
        bridge
            .read()
            .await
            .unwrap()
            .expect("declared audio source")
            .source,
        microphone.reference
    );
}

#[tokio::test]
async fn attach_session_and_session_audio_consume_the_context_admission_once() {
    let runtime = service(FakeAudio::new(), false, None);
    let logical_owner = owner("one-shot-session-context");
    let attached = FakeSession::new(MediaSessionRef(MediaSessionId::from("one-shot-attached")));
    let attach_context = context(logical_owner.clone(), 29);
    let reference = runtime
        .attach_session(attach_context.clone(), Box::new(attached))
        .await
        .unwrap();

    let attach_replay = runtime
        .execute(
            attach_context,
            MediaOperation::Status {
                target: Some(MediaResourceRef::Session(reference.clone())),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(attach_replay.kind, MediaErrorKind::InvalidRequest);

    let audio_context = context(logical_owner, 30);
    runtime
        .session_audio(audio_context.clone(), reference.clone())
        .await
        .expect("a fresh context may open the attached session's audio bridge");
    let audio_replay = runtime
        .execute(
            audio_context,
            MediaOperation::Status {
                target: Some(MediaResourceRef::Session(reference)),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(audio_replay.kind, MediaErrorKind::InvalidRequest);
}

#[tokio::test]
async fn parent_revoke_waits_for_child_session_and_audio_channel_to_close() {
    let runtime = service(FakeAudio::new(), false, None);
    let host = crate::attachment::MediaAttachmentHost::new();
    let parent = host.attach(runtime).unwrap();
    let child = parent.fork_child().unwrap();
    let child_service = child.service();
    let session = FakeSession::new(MediaSessionRef(MediaSessionId::from(
        "scheduled-child-session",
    )));
    session.pending_stop.store(true, Ordering::SeqCst);
    let reference = child_service
        .attach_session(
            context(owner("scheduled-child"), 31),
            Box::new(session.clone()),
        )
        .await
        .unwrap();
    let channel = child_service
        .session_audio(context(owner("scheduled-child"), 32), reference)
        .await
        .unwrap();

    let first = parent.revoke(Duration::from_millis(10)).await;
    assert!(matches!(
        first,
        crate::attachment::MediaAttachmentRevocation::Unknown { .. }
    ));
    let stop_gate = tokio::time::timeout(Duration::from_secs(1), session.stop_started.acquire())
        .await
        .expect("parent drain reaches the real child session stop")
        .expect("session stop gate remains open");
    stop_gate.forget();
    assert_eq!(
        parent.phase(),
        crate::attachment::MediaAttachmentPhase::Revoking
    );
    assert_eq!(
        child.phase(),
        crate::attachment::MediaAttachmentPhase::Revoking
    );
    assert_eq!(host.attachments().len(), 2);
    assert_eq!(
        child_service
            .execute(
                context(owner("scheduled-child"), 33),
                MediaOperation::Status { target: None },
            )
            .await
            .unwrap_err()
            .kind,
        MediaErrorKind::Cancelled
    );

    session.stop_release.add_permits(1);
    assert_eq!(
        parent.revoke(Duration::from_secs(1)).await,
        crate::attachment::MediaAttachmentRevocation::Confirmed
    );
    assert_eq!(
        parent.phase(),
        crate::attachment::MediaAttachmentPhase::Revoked
    );
    assert_eq!(
        child.phase(),
        crate::attachment::MediaAttachmentPhase::Revoked
    );
    assert!(host.attachments().is_empty());
    assert_eq!(
        channel.read().await.unwrap_err().kind,
        MediaErrorKind::Cancelled
    );
    assert_eq!(session.stop_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unknown_child_session_keeps_parent_attachment_fenced_and_retained() {
    let runtime = service(FakeAudio::new(), false, None);
    let host = crate::attachment::MediaAttachmentHost::new();
    let parent = host.attach(runtime).unwrap();
    let child = parent.fork_child().unwrap();
    let child_service = child.service();
    let session = FakeSession::new(MediaSessionRef(MediaSessionId::from(
        "scheduled-child-unknown-session",
    )));
    session.nonterminal_stop.store(true, Ordering::SeqCst);
    let reference = child_service
        .attach_session(
            context(owner("scheduled-child-unknown"), 34),
            Box::new(session.clone()),
        )
        .await
        .unwrap();
    let channel = child_service
        .session_audio(context(owner("scheduled-child-unknown"), 35), reference)
        .await
        .unwrap();

    assert!(matches!(
        parent.revoke(Duration::from_secs(1)).await,
        crate::attachment::MediaAttachmentRevocation::Unknown { .. }
    ));
    assert_eq!(
        parent.phase(),
        crate::attachment::MediaAttachmentPhase::Unknown
    );
    assert_eq!(
        child.phase(),
        crate::attachment::MediaAttachmentPhase::Unknown
    );
    assert_eq!(host.attachments().len(), 2);
    assert_eq!(session.stop_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        channel.read().await.unwrap_err().kind,
        MediaErrorKind::Cancelled
    );

    // Unknown is a retained fence: a later revoke observation does not retry
    // an unconfirmed native session stop or claim successful cleanup.
    assert!(matches!(
        parent.revoke(Duration::from_secs(1)).await,
        crate::attachment::MediaAttachmentRevocation::Unknown { .. }
    ));
    assert_eq!(session.stop_count.load(Ordering::SeqCst), 1);
    assert_eq!(host.attachments().len(), 2);
}

#[tokio::test]
async fn replayed_start_preserves_the_live_idempotent_lease_after_history_eviction() {
    let connector = Arc::new(IdempotentConnector::default());
    let runtime = service(FakeAudio::new(), false, Some(connector.clone()));
    let session_owner = owner("idempotent-start");
    let start_context = context(session_owner.clone(), 500);
    let request = session_request(MediaSessionMode::Audio, MediaSessionIntent::Conversation);
    let result = runtime
        .execute(
            start_context.clone(),
            MediaOperation::SessionStart {
                request: request.clone(),
            },
        )
        .await
        .unwrap();
    let MediaResult::SessionStarted { session, .. } = result else {
        panic!("first session start")
    };
    let original = connector.sessions.lock().unwrap()[&start_context.identity].clone();

    let replay = runtime
        .execute(
            MediaOperationContext::new(start_context.identity.clone(), session_owner.clone()),
            MediaOperation::SessionStart {
                request: request.clone(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(original.stop_count.load(Ordering::SeqCst), 0);
    assert_eq!(replay.kind, MediaErrorKind::InvalidRequest);
    assert_eq!(connector.source_calls.load(Ordering::SeqCst), 1);
    assert_eq!(connector.open_calls.load(Ordering::SeqCst), 1);

    // The start identity is still valid as the context of an unrelated call.
    let status = runtime
        .execute(
            MediaOperationContext::new(start_context.identity.clone(), session_owner.clone()),
            MediaOperation::Status {
                target: Some(MediaResourceRef::Session(session.clone())),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(status, MediaResult::Status { resources } if resources[0].state == MediaResourceState::Active)
    );

    // The same generation with a fresh UUID is a different operation.
    let fresh_context = context(session_owner.clone(), 500);
    assert_ne!(fresh_context.identity, start_context.identity);
    let fresh = runtime
        .execute(
            fresh_context,
            MediaOperation::SessionStart {
                request: request.clone(),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(fresh, MediaResult::SessionStarted { session: fresh_session, .. } if fresh_session != session)
    );
    assert_eq!(connector.source_calls.load(Ordering::SeqCst), 2);
    assert_eq!(connector.open_calls.load(Ordering::SeqCst), 2);

    for generation in 0..=MAX_OPERATION_HISTORY {
        runtime
            .execute(
                context(session_owner.clone(), generation as u64 + 1_000),
                MediaOperation::Status {
                    target: Some(MediaResourceRef::Session(session.clone())),
                },
            )
            .await
            .unwrap();
    }
    assert!(!runtime
        .inner
        .lock()
        .unwrap()
        .operation_status
        .contains_key(&start_context.identity));

    let replay_context =
        MediaOperationContext::new(start_context.identity.clone(), owner("other-owner"));
    let replay = runtime
        .execute(replay_context, MediaOperation::SessionStart { request })
        .await
        .unwrap_err();
    assert_eq!(replay.kind, MediaErrorKind::InvalidRequest);
    assert_eq!(connector.source_calls.load(Ordering::SeqCst), 2);
    assert_eq!(connector.open_calls.load(Ordering::SeqCst), 2);
    assert_eq!(original.stop_count.load(Ordering::SeqCst), 0);
    assert_eq!(original.status().state, MediaSessionState::Preparing);
}

#[tokio::test]
async fn start_rejects_an_attached_sessions_origin_identity_before_open() {
    let connector = Arc::new(IdempotentConnector::default());
    let runtime = service(FakeAudio::new(), false, Some(connector.clone()));
    let attach_context = context(owner("attached-origin"), 501);
    let incoming = FakeSession::new(MediaSessionRef(MediaSessionId::from("incoming-origin")));
    runtime
        .attach_session(attach_context.clone(), Box::new(incoming.clone()))
        .await
        .unwrap();
    let source_calls = connector.source_calls.load(Ordering::SeqCst);

    let replay = runtime
        .execute(
            fresh_context_same_identity(&attach_context),
            MediaOperation::SessionStart {
                request: session_request(MediaSessionMode::Audio, MediaSessionIntent::Conversation),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(replay.kind, MediaErrorKind::InvalidRequest);
    assert_eq!(connector.source_calls.load(Ordering::SeqCst), source_calls);
    assert_eq!(connector.open_calls.load(Ordering::SeqCst), 0);
    assert_eq!(incoming.stop_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stopped_session_releases_its_origin_identity_for_a_later_start() {
    let connector = FakeConnector::new("stopped-origin", vec![]);
    let runtime = service(FakeAudio::new(), false, Some(connector.clone()));
    let start_context = context(owner("stopped-origin"), 502);
    let request = session_request(MediaSessionMode::Audio, MediaSessionIntent::Conversation);
    let first = runtime
        .execute(
            start_context.clone(),
            MediaOperation::SessionStart {
                request: request.clone(),
            },
        )
        .await
        .unwrap();
    let MediaResult::SessionStarted { session, .. } = first else {
        panic!("first session start")
    };
    let original = connector.latest_session();

    // Reusing the identity for Stop is valid, and removes the live session.
    runtime
        .execute(
            MediaOperationContext::new(start_context.identity.clone(), start_context.owner.clone()),
            MediaOperation::Stop {
                target: MediaResourceRef::Session(session.clone()),
            },
        )
        .await
        .unwrap();
    assert_eq!(original.stop_count.load(Ordering::SeqCst), 1);
    let restarted = runtime
        .execute(
            MediaOperationContext::new(start_context.identity.clone(), start_context.owner.clone()),
            MediaOperation::SessionStart { request },
        )
        .await
        .unwrap();
    assert!(
        matches!(restarted, MediaResult::SessionStarted { session: next, .. } if next != session)
    );
}

#[tokio::test]
async fn overlap_attach_first_blocks_same_identity_start_until_cleanup() {
    let connector = FakeConnector::new("attach-first", vec![]);
    connector.pending_sources.store(true, Ordering::SeqCst);
    let runtime = service(FakeAudio::new(), false, Some(connector.clone()));
    let attach_context = context(owner("attach-first"), 503);
    let incoming = FakeSession::new(MediaSessionRef(MediaSessionId::from("attach-first")));
    incoming.pending_stop.store(true, Ordering::SeqCst);
    let task_runtime = runtime.clone();
    let task_context = attach_context.clone();
    let task_session = incoming.clone();
    let attach = tokio::spawn(async move {
        task_runtime
            .attach_session(task_context, Box::new(task_session))
            .await
    });
    let source_gate =
        tokio::time::timeout(Duration::from_secs(1), connector.sources_started.acquire())
            .await
            .expect("attach reached the source catalog")
            .expect("source semaphore remains open");
    source_gate.forget();

    let request = session_request(MediaSessionMode::Audio, MediaSessionIntent::Conversation);
    let duplicate = tokio::time::timeout(
        Duration::from_secs(1),
        runtime.execute(
            fresh_context_same_identity(&attach_context),
            MediaOperation::SessionStart {
                request: request.clone(),
            },
        ),
    )
    .await
    .expect("duplicate start is rejected before querying the connector")
    .unwrap_err();
    assert_eq!(duplicate.kind, MediaErrorKind::InvalidRequest);
    assert_eq!(connector.next_id.load(Ordering::SeqCst), 1);
    let second_incoming =
        FakeSession::new(MediaSessionRef(MediaSessionId::from("attach-duplicate")));
    let duplicate_attach = runtime
        .attach_session(
            fresh_context_same_identity(&attach_context),
            Box::new(second_incoming.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(duplicate_attach.kind, MediaErrorKind::InvalidRequest);
    assert_eq!(second_incoming.stop_count.load(Ordering::SeqCst), 1);
    assert_eq!(incoming.stop_count.load(Ordering::SeqCst), 0);

    connector.pending_sources.store(false, Ordering::SeqCst);
    let fresh = runtime
        .execute(
            context(owner("attach-first"), 504),
            MediaOperation::SessionStart {
                request: request.clone(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(fresh, MediaResult::SessionStarted { .. }));

    attach.abort();
    let _ = attach.await;
    let stop_gate = tokio::time::timeout(Duration::from_secs(1), incoming.stop_started.acquire())
        .await
        .expect("cancelled attach began cleanup")
        .expect("stop semaphore remains open");
    stop_gate.forget();
    let duplicate = runtime
        .execute(
            fresh_context_same_identity(&attach_context),
            MediaOperation::SessionStart {
                request: request.clone(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(duplicate.kind, MediaErrorKind::InvalidRequest);
    assert_eq!(connector.next_id.load(Ordering::SeqCst), 2);

    incoming.stop_release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(1), incoming.stop_completed.notified())
        .await
        .expect("attach cleanup finishes");
    tokio::time::timeout(Duration::from_secs(1), async {
        while runtime.inner.lock().unwrap().session_reservations.len() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("attach reservation is released after cleanup");
    assert!(matches!(
        runtime
            .execute(
                fresh_context_same_identity(&attach_context),
                MediaOperation::SessionStart { request }
            )
            .await
            .unwrap(),
        MediaResult::SessionStarted { .. }
    ));
}

#[tokio::test]
async fn attach_rejects_the_origin_identity_of_an_existing_session() {
    let connector = FakeConnector::new("attach-registered", vec![]);
    let runtime = service(FakeAudio::new(), false, Some(connector));
    let attach_context = context(owner("attach-registered"), 507);
    let original = FakeSession::new(MediaSessionRef(MediaSessionId::from("registered-original")));
    runtime
        .attach_session(attach_context.clone(), Box::new(original.clone()))
        .await
        .unwrap();
    assert!(runtime
        .inner
        .lock()
        .unwrap()
        .session_reservations
        .is_empty());

    let duplicate = FakeSession::new(MediaSessionRef(MediaSessionId::from(
        "registered-duplicate",
    )));
    let error = runtime
        .attach_session(
            fresh_context_same_identity(&attach_context),
            Box::new(duplicate.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, MediaErrorKind::InvalidRequest);
    assert_eq!(duplicate.stop_count.load(Ordering::SeqCst), 1);
    assert_eq!(original.stop_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn overlap_start_first_blocks_same_identity_attach_before_reservation() {
    let connector = FakeConnector::new("start-first", vec![]);
    connector.pending_sources.store(true, Ordering::SeqCst);
    let runtime = service(FakeAudio::new(), false, Some(connector.clone()));
    let start_context = context(owner("start-first"), 505);
    let task_runtime = runtime.clone();
    let task_context = start_context.clone();
    let start = tokio::spawn(async move {
        task_runtime
            .execute(
                task_context,
                MediaOperation::SessionStart {
                    request: session_request(
                        MediaSessionMode::Audio,
                        MediaSessionIntent::Conversation,
                    ),
                },
            )
            .await
    });
    let source_gate =
        tokio::time::timeout(Duration::from_secs(1), connector.sources_started.acquire())
            .await
            .expect("start reached the source catalog before reserving a slot")
            .expect("source semaphore remains open");
    source_gate.forget();

    let incoming = FakeSession::new(MediaSessionRef(MediaSessionId::from("duplicate-incoming")));
    let duplicate = tokio::time::timeout(
        Duration::from_secs(1),
        runtime.attach_session(
            fresh_context_same_identity(&start_context),
            Box::new(incoming.clone()),
        ),
    )
    .await
    .expect("duplicate attach is rejected before querying the connector")
    .unwrap_err();
    assert_eq!(duplicate.kind, MediaErrorKind::InvalidRequest);
    assert_eq!(incoming.stop_count.load(Ordering::SeqCst), 1);
    assert_eq!(incoming.registration_count.load(Ordering::SeqCst), 0);

    connector.pending_sources.store(false, Ordering::SeqCst);
    let fresh = FakeSession::new(MediaSessionRef(MediaSessionId::from("fresh-incoming")));
    runtime
        .attach_session(context(owner("start-first"), 506), Box::new(fresh.clone()))
        .await
        .unwrap();
    assert_eq!(fresh.registration_count.load(Ordering::SeqCst), 1);
    start.abort();
    let _ = start.await;
    assert!(matches!(
        runtime
            .execute(
                MediaOperationContext::new(
                    start_context.identity.clone(),
                    start_context.owner.clone()
                ),
                MediaOperation::SessionStart {
                    request: session_request(
                        MediaSessionMode::Audio,
                        MediaSessionIntent::Conversation
                    ),
                },
            )
            .await
            .unwrap(),
        MediaResult::SessionStarted { .. }
    ));
}

#[tokio::test]
async fn concurrent_started_and_attached_sessions_share_reserved_capacity() {
    let audio = FakeAudio::new();
    let connector = FakeConnector::new("rtc-capacity", vec![]);
    connector.pending_start.store(true, Ordering::SeqCst);
    let runtime = service(audio, false, Some(connector.clone()));
    let starts: Vec<_> = (0..MAX_ACTIVE_SESSIONS)
        .map(|generation| {
            let runtime = runtime.clone();
            tokio::spawn(async move {
                runtime
                    .execute(
                        context(owner("alpha"), generation as u64 + 600),
                        MediaOperation::SessionStart {
                            request: session_request(
                                MediaSessionMode::Audio,
                                MediaSessionIntent::Conversation,
                            ),
                        },
                    )
                    .await
            })
        })
        .collect();
    let started = tokio::time::timeout(
        Duration::from_secs(2),
        connector
            .started_sessions
            .acquire_many(MAX_ACTIVE_SESSIONS as u32),
    )
    .await
    .expect("all available session slots were entered")
    .expect("session start semaphore remains open");
    started.forget();

    let incoming = FakeSession::new(MediaSessionRef(MediaSessionId::from("incoming-at-cap")));
    let attached = runtime
        .attach_session(context(owner("alpha"), 700), Box::new(incoming.clone()))
        .await
        .unwrap_err();
    assert_eq!(attached.kind, MediaErrorKind::Busy);
    assert_eq!(incoming.stop_count.load(Ordering::SeqCst), 1);
    assert_eq!(incoming.registration_count.load(Ordering::SeqCst), 0);

    let full = runtime
        .execute(
            context(owner("alpha"), 701),
            MediaOperation::SessionStart {
                request: session_request(MediaSessionMode::Audio, MediaSessionIntent::Conversation),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(full.kind, MediaErrorKind::Busy);
    assert_eq!(
        runtime.inner.lock().unwrap().session_reservations.len(),
        MAX_ACTIVE_SESSIONS
    );

    for start in starts {
        start.abort();
        let _ = start.await;
    }
    assert_eq!(runtime.inner.lock().unwrap().session_reservations.len(), 0);

    connector.pending_start.store(false, Ordering::SeqCst);
    connector.fail_next_start.store(true, Ordering::SeqCst);
    let failed = runtime
        .execute(
            context(owner("alpha"), 702),
            MediaOperation::SessionStart {
                request: session_request(MediaSessionMode::Audio, MediaSessionIntent::Conversation),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(failed.kind, MediaErrorKind::NativeFailure);
    assert_eq!(runtime.inner.lock().unwrap().session_reservations.len(), 0);

    let recovered = runtime
        .execute(
            context(owner("alpha"), 703),
            MediaOperation::SessionStart {
                request: session_request(MediaSessionMode::Audio, MediaSessionIntent::Conversation),
            },
        )
        .await
        .unwrap();
    assert!(matches!(recovered, MediaResult::SessionStarted { .. }));
    assert_eq!(runtime.inner.lock().unwrap().session_reservations.len(), 0);
}

#[tokio::test]
async fn cancelled_attach_stops_incoming_session_and_releases_reservation() {
    let connector = FakeConnector::new("pending-sources", vec![]);
    connector.pending_sources.store(true, Ordering::SeqCst);
    let runtime = service(FakeAudio::new(), false, Some(connector.clone()));
    let incoming = FakeSession::new(MediaSessionRef(MediaSessionId::from("incoming-cancel")));
    let task_runtime = runtime.clone();
    let task_session = incoming.clone();
    let attach = tokio::spawn(async move {
        task_runtime
            .attach_session(context(owner("alpha"), 710), Box::new(task_session))
            .await
    });
    let started = tokio::time::timeout(Duration::from_secs(1), connector.sources_started.acquire())
        .await
        .expect("attach reached the source catalog await")
        .expect("source semaphore remains open");
    started.forget();
    assert_eq!(runtime.inner.lock().unwrap().session_reservations.len(), 1);

    attach.abort();
    let _ = attach.await;
    tokio::time::timeout(Duration::from_secs(1), incoming.stop_completed.notified())
        .await
        .expect("dropped attach schedules incoming-session cleanup");
    assert_eq!(incoming.stop_count.load(Ordering::SeqCst), 1);
    assert_eq!(incoming.registration_count.load(Ordering::SeqCst), 0);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if runtime.inner.lock().unwrap().session_reservations.len() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cleanup releases the held session reservation");
    assert!(!runtime
        .inner
        .lock()
        .unwrap()
        .sessions
        .contains_key(&incoming.reference));
}

#[tokio::test]
async fn cancelled_started_session_validation_keeps_slot_until_cleanup_finishes() {
    let connector = FakeConnector::new("invalid-authorized-sources", vec![]);
    connector.pending_session_stop.store(true, Ordering::SeqCst);
    connector.set_next_authorized_sources(
        (0..=MAX_MEDIA_SESSION_SOURCE_REFS)
            .map(|index| MediaSourceRef(MediaSourceId::from(format!("incoming-source-{index}"))))
            .collect(),
    );
    let runtime = service(FakeAudio::new(), false, Some(connector.clone()));
    let start_context = context(owner("session-cleanup"), 711);
    let task_runtime = runtime.clone();
    let task_context = start_context.clone();
    let start = tokio::spawn(async move {
        task_runtime
            .execute(
                task_context,
                MediaOperation::SessionStart {
                    request: session_request(
                        MediaSessionMode::Audio,
                        MediaSessionIntent::Conversation,
                    ),
                },
            )
            .await
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !connector.sessions.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connector returned its invalid session");
    let session = connector.latest_session();
    let first_stop = tokio::time::timeout(Duration::from_secs(1), session.stop_started.acquire())
        .await
        .expect("validation cleanup reached session stop")
        .expect("stop semaphore remains open");
    first_stop.forget();

    start.abort();
    let _ = start.await;
    let retry = tokio::time::timeout(Duration::from_secs(1), session.stop_started.acquire())
        .await
        .expect("dropped start schedules session cleanup")
        .expect("stop semaphore remains open");
    retry.forget();
    assert_eq!(runtime.inner.lock().unwrap().session_reservations.len(), 1);
    let duplicate_start = runtime
        .execute(
            fresh_context_same_identity(&start_context),
            MediaOperation::SessionStart {
                request: session_request(MediaSessionMode::Audio, MediaSessionIntent::Conversation),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(duplicate_start.kind, MediaErrorKind::InvalidRequest);
    assert_eq!(connector.sessions.lock().unwrap().len(), 1);
    let duplicate_session =
        FakeSession::new(MediaSessionRef(MediaSessionId::from("cleanup-duplicate")));
    let duplicate_attach = runtime
        .attach_session(
            fresh_context_same_identity(&start_context),
            Box::new(duplicate_session.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(duplicate_attach.kind, MediaErrorKind::InvalidRequest);
    assert_eq!(duplicate_session.stop_count.load(Ordering::SeqCst), 1);

    session.stop_release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(1), session.stop_completed.notified())
        .await
        .expect("session cleanup completes");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if runtime.inner.lock().unwrap().session_reservations.len() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cleanup releases the session slot");
    assert!(runtime.inner.lock().unwrap().sessions.is_empty());
    assert_eq!(session.registration_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancel_waits_for_operation_exit_and_deadlines_cancel_audio_work() {
    let audio = FakeAudio::new();
    audio.pending_listen.store(true, Ordering::SeqCst);
    let connector = FakeConnector::new("rtc", vec![]);
    let runtime = service(audio.clone(), false, Some(connector));
    let operation_context = context(owner("alpha"), 30);
    let identity = operation_context.identity.clone();
    let incarnation = operation_context.operation_incarnation();
    let task_runtime = runtime.clone();
    let task = tokio::spawn(async move {
        task_runtime
            .execute(
                operation_context,
                MediaOperation::Transcribe {
                    input: TranscriptionInput::LiveSource(microphone()),
                    language: None,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), audio.listen_started.notified())
        .await
        .unwrap();
    let stopped = runtime
        .execute(
            context(owner("alpha"), 31),
            MediaOperation::Stop {
                target: MediaResourceRef::Operation {
                    identity: identity.clone(),
                    incarnation,
                },
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(stopped, MediaResult::Stopped { resource: MediaResourceRef::Operation { identity: id, incarnation: stopped_incarnation } } if id == identity && stopped_incarnation == incarnation)
    );
    let cancelled = task.await.unwrap().unwrap_err();
    assert_eq!(cancelled.kind, MediaErrorKind::Cancelled);
    assert!(audio.cancellations.load(Ordering::SeqCst) >= 1);

    let deadline_context = context(owner("alpha"), 32).with_timeout_budget_ms(5);
    let timed_out = runtime
        .execute(
            deadline_context,
            MediaOperation::Transcribe {
                input: TranscriptionInput::LiveSource(microphone()),
                language: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(timed_out.kind, MediaErrorKind::Timeout);
}

#[tokio::test]
async fn session_event_buffers_bound_batches_report_overflow_and_drop_stale_frames() {
    let audio = FakeAudio::new();
    let connector = FakeConnector::new("loopback", vec![video_remote_source()]);
    let runtime = service(audio, false, Some(connector.clone()));
    let started = runtime
        .execute(
            context(owner("alpha"), 40),
            MediaOperation::SessionStart {
                request: session_request(MediaSessionMode::Video, MediaSessionIntent::Observation),
            },
        )
        .await
        .unwrap();
    let MediaResult::SessionStarted { session, .. } = started else {
        panic!("session start")
    };
    let fake_session = connector.latest_session();
    let target = MediaResourceRef::Session(session.clone());
    let now = now_ms();
    for start in [1_u64, 129, 257] {
        let end = start + 127;
        let events = (start..=end)
            .map(|sequence| MediaEvent {
                sequence,
                occurred_at_unix_ms: now,
                target: target.clone(),
                operation_id: None,
                kind: MediaEventKind::StateChanged {
                    resource: target.clone(),
                    state: MediaResourceState::Active,
                },
            })
            .collect();
        fake_session.enqueue(MediaEventBatch {
            target: target.clone(),
            events,
            next_cursor: Some(MediaEventCursor { sequence: end }),
            dropped_before_cursor: false,
        });
    }

    let first = runtime
        .execute(
            context(owner("alpha"), 41),
            MediaOperation::Observe {
                target: target.clone(),
                after: None,
                limit: 128,
            },
        )
        .await
        .unwrap();
    let MediaResult::Events { batch: first } = first else {
        panic!("first event batch")
    };
    assert_eq!(first.events.len(), 128);
    assert_eq!(first.events[0].sequence, 1);
    assert_eq!(first.events[127].sequence, 128);
    assert!(!first.dropped_before_cursor);

    let second = runtime
        .execute(
            context(owner("alpha"), 42),
            MediaOperation::Observe {
                target: target.clone(),
                after: None,
                limit: 128,
            },
        )
        .await
        .unwrap();
    let MediaResult::Events { batch: second } = second else {
        panic!("second event batch")
    };
    assert_eq!(second.events.first().map(|event| event.sequence), Some(129));
    assert_eq!(second.events.last().map(|event| event.sequence), Some(256));
    assert!(second.dropped_before_cursor);

    let third = runtime
        .execute(
            context(owner("alpha"), 43),
            MediaOperation::Observe {
                target: target.clone(),
                after: None,
                limit: 128,
            },
        )
        .await
        .unwrap();
    let MediaResult::Events { batch: third } = third else {
        panic!("third event batch")
    };
    assert_eq!(third.events.first().map(|event| event.sequence), Some(257));
    assert_eq!(third.events.last().map(|event| event.sequence), Some(384));
    assert!(third.dropped_before_cursor);

    let after_overflow = runtime
        .execute(
            context(owner("alpha"), 44),
            MediaOperation::Wait {
                target: target.clone(),
                after: Some(MediaEventCursor { sequence: 256 }),
                timeout_ms: 10,
                limit: 128,
            },
        )
        .await
        .unwrap();
    let MediaResult::Events {
        batch: after_overflow,
    } = after_overflow
    else {
        panic!("post-overflow batch")
    };
    assert_eq!(
        after_overflow.events.first().map(|event| event.sequence),
        Some(257)
    );
    assert!(!after_overflow.dropped_before_cursor);

    let stale_target = MediaResourceRef::Session(session.clone());
    fake_session.enqueue(MediaEventBatch {
        target: stale_target.clone(),
        events: vec![MediaEvent {
            sequence: 385,
            occurred_at_unix_ms: now.saturating_sub(MAX_EVENT_AGE_MS + 1),
            target: stale_target.clone(),
            operation_id: None,
            kind: MediaEventKind::StateChanged {
                resource: stale_target.clone(),
                state: MediaResourceState::Active,
            },
        }],
        next_cursor: Some(MediaEventCursor { sequence: 385 }),
        dropped_before_cursor: false,
    });
    let stale = runtime
        .execute(
            context(owner("alpha"), 45),
            MediaOperation::Observe {
                target: stale_target,
                after: Some(MediaEventCursor { sequence: 384 }),
                limit: 16,
            },
        )
        .await
        .unwrap();
    let MediaResult::Events { batch: stale } = stale else {
        panic!("stale batch")
    };
    assert!(stale.events.is_empty());
    assert!(stale.dropped_before_cursor);

    let oversized_events = (386..=514)
        .map(|sequence| MediaEvent {
            sequence,
            occurred_at_unix_ms: now_ms(),
            target: target.clone(),
            operation_id: None,
            kind: MediaEventKind::StateChanged {
                resource: target.clone(),
                state: MediaResourceState::Active,
            },
        })
        .collect();
    fake_session.enqueue(MediaEventBatch {
        target: target.clone(),
        events: oversized_events,
        next_cursor: Some(MediaEventCursor { sequence: 514 }),
        dropped_before_cursor: false,
    });
    let oversized = runtime
        .execute(
            context(owner("alpha"), 46),
            MediaOperation::Observe {
                target: target.clone(),
                after: Some(MediaEventCursor { sequence: 385 }),
                limit: 128,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(oversized.kind, MediaErrorKind::MediaTooLarge);

    let video_sources = runtime
        .execute(
            context(owner("alpha"), 47),
            MediaOperation::Sources {
                kind: Some(MediaKind::Video),
                usage: Some(MediaSourceUsage::Session),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(video_sources, MediaResult::Sources { sources } if sources.iter().any(|source| source.reference == MediaSourceRef(MediaSourceId::from("remote:camera"))))
    );
    let audio_sources = runtime
        .execute(
            context(owner("alpha"), 48),
            MediaOperation::Sources {
                kind: Some(MediaKind::Audio),
                usage: Some(MediaSourceUsage::Session),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(audio_sources, MediaResult::Sources { sources } if !sources.iter().any(|source| source.kind == MediaSourceKind::Remote && source.media_kind == Some(MediaKind::Video)))
    );
}

#[tokio::test]
async fn video_source_filter_requires_a_live_camera_session_and_declared_remote_video() {
    let still_only = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
        .audio(FakeAudio::new())
        .camera(Arc::new(FakeCamera))
        .connector(
            FakeConnector::new("audio-only", Vec::new()),
            [MediaSessionMode::Audio],
        )
        .service_epoch(EPOCH)
        .build();
    let still_only_video = still_only
        .execute(
            context(owner("alpha"), 50),
            MediaOperation::Sources {
                kind: Some(MediaKind::Video),
                usage: Some(MediaSourceUsage::Session),
            },
        )
        .await
        .unwrap();
    assert!(matches!(still_only_video, MediaResult::Sources { sources } if sources.is_empty()));

    let mut remote_audio = video_remote_source();
    remote_audio.reference = MediaSourceRef(MediaSourceId::from("remote:audio"));
    remote_audio.media_kind = Some(MediaKind::Audio);
    remote_audio.label = "Remote microphone".to_owned();
    let runtime = MediaRuntimeBuilder::new(Arc::new(MemoryStore::default()))
        .audio(FakeAudio::new())
        .camera(Arc::new(FakeCamera))
        .connector(
            FakeConnector::new("video", vec![video_remote_source(), remote_audio]),
            [MediaSessionMode::Video],
        )
        .service_epoch(EPOCH)
        .build();
    let video_result = runtime
        .execute(
            context(owner("alpha"), 51),
            MediaOperation::Sources {
                kind: Some(MediaKind::Video),
                usage: Some(MediaSourceUsage::Session),
            },
        )
        .await
        .unwrap();
    let MediaResult::Sources {
        sources: video_sources,
    } = video_result
    else {
        panic!("video source listing")
    };
    let camera = video_sources
        .iter()
        .find(|source| source.reference == camera_source())
        .expect("live camera is listed as a video session source");
    assert_eq!(camera.media_kind, Some(MediaKind::Image));
    assert!(camera.usages.contains(&MediaSourceUsage::Session));
    assert!(camera
        .supported_operations
        .contains(&MediaOperationKind::SessionStart));
    assert!(video_sources
        .iter()
        .any(|source| source.reference == MediaSourceRef(MediaSourceId::from("remote:camera"))));
    assert!(!video_sources.iter().any(|source| {
        source.reference == MediaSourceRef(MediaSourceId::from("media-runtime:library"))
            || source.reference == MediaSourceRef(MediaSourceId::from("remote:audio"))
    }));

    let still_result = runtime
        .execute(
            context(owner("alpha"), 52),
            MediaOperation::Sources {
                kind: Some(MediaKind::Video),
                usage: Some(MediaSourceUsage::Capture),
            },
        )
        .await
        .unwrap();
    assert!(matches!(still_result, MediaResult::Sources { sources } if sources.is_empty()));

    let image_result = runtime
        .execute(
            context(owner("alpha"), 53),
            MediaOperation::Sources {
                kind: Some(MediaKind::Image),
                usage: Some(MediaSourceUsage::Capture),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(image_result, MediaResult::Sources { sources } if sources.iter().any(|source| source.reference == camera_source()) && sources.iter().any(|source| source.reference == MediaSourceRef(MediaSourceId::from("media-runtime:library"))))
    );
}

#[tokio::test]
async fn session_observation_rejects_unselected_frame_sources_and_foreign_state_resources() {
    let audio = FakeAudio::new();
    let connector = FakeConnector::new("events", vec![video_remote_source()]);
    let runtime = service(audio, false, Some(connector.clone()));
    let selected_source = video_remote_source().reference;
    let started = runtime
        .execute(
            context(owner("alpha"), 60),
            MediaOperation::SessionStart {
                request: session_request(MediaSessionMode::Video, MediaSessionIntent::Observation),
            },
        )
        .await
        .unwrap();
    let MediaResult::SessionStarted { session, .. } = started else {
        panic!("session start")
    };
    let fake_session = connector.latest_session();
    let target = MediaResourceRef::Session(session.clone());
    let now = now_ms();
    let selected = runtime
        .execute(
            context(owner("alpha"), 61),
            MediaOperation::SessionUpdate {
                session,
                update: MediaSessionUpdate::SelectSource {
                    kind: MediaKind::Video,
                    source: selected_source.clone(),
                },
            },
        )
        .await
        .unwrap();
    assert!(matches!(selected, MediaResult::SessionUpdated { .. }));
    let image_asset = |reference: &str| MediaAsset {
        reference: MediaRef::from(reference),
        metadata: MediaMetadata {
            kind: MediaKind::Image,
            mime_type: "image/jpeg".to_owned(),
            byte_len: 4,
            duration_ms: None,
            width_px: Some(1),
            height_px: Some(1),
            sample_rate_hz: None,
            channels: None,
            captured_at_unix_ms: Some(now),
        },
    };
    let frame_event = |sequence, source: MediaSourceRef, asset: MediaAsset| MediaEvent {
        sequence,
        occurred_at_unix_ms: now,
        target: target.clone(),
        operation_id: None,
        kind: MediaEventKind::Frame { source, asset },
    };
    let enqueue = |session: &FakeSession, event: MediaEvent| {
        let target = target.clone();
        let sequence = event.sequence;
        session.enqueue(MediaEventBatch {
            target,
            events: vec![event],
            next_cursor: Some(MediaEventCursor { sequence }),
            dropped_before_cursor: false,
        });
    };
    let wrong_source_asset = image_asset("frame-wrong-source");
    enqueue(
        &fake_session,
        frame_event(
            1,
            MediaSourceRef(MediaSourceId::from("remote:other")),
            wrong_source_asset.clone(),
        ),
    );
    let error = observe_session(&runtime, owner("alpha"), target.clone(), 62)
        .await
        .unwrap_err();
    assert_eq!(error.kind, MediaErrorKind::InvalidRequest);
    let hidden = observe_session(&runtime, owner("alpha"), target.clone(), 63)
        .await
        .unwrap();
    assert!(
        matches!(hidden, MediaResult::Events { batch } if batch.events.iter().all(|event| {
            matches!(&event.kind, MediaEventKind::StateChanged { resource, .. } if resource == &target)
        }))
    );
    let asset_status = runtime
        .execute(
            context(owner("alpha"), 64),
            MediaOperation::Status {
                target: Some(MediaResourceRef::Media(wrong_source_asset.reference)),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(asset_status.kind, MediaErrorKind::NotFound);

    enqueue(
        &fake_session,
        MediaEvent {
            sequence: 2,
            occurred_at_unix_ms: now,
            target: target.clone(),
            operation_id: None,
            kind: MediaEventKind::StateChanged {
                resource: MediaResourceRef::Session(MediaSessionRef(MediaSessionId::from(
                    "unrelated-session",
                ))),
                state: MediaResourceState::Active,
            },
        },
    );
    let error = observe_session(&runtime, owner("alpha"), target.clone(), 65)
        .await
        .unwrap_err();
    assert_eq!(error.kind, MediaErrorKind::InvalidRequest);
    let hidden = observe_session(&runtime, owner("alpha"), target.clone(), 66)
        .await
        .unwrap();
    assert!(
        matches!(hidden, MediaResult::Events { batch } if batch.events.iter().all(|event| {
            matches!(&event.kind, MediaEventKind::StateChanged { resource, .. } if resource == &target)
        }))
    );

    enqueue(
        &fake_session,
        frame_event(3, selected_source.clone(), image_asset("frame-authorized")),
    );
    let visible = observe_session(&runtime, owner("alpha"), target.clone(), 67)
        .await
        .unwrap();
    assert!(
        matches!(visible, MediaResult::Events { batch } if batch.events.iter().filter(|event| {
        matches!(&event.kind, MediaEventKind::Frame { source, .. } if source == &selected_source)
    }).count() == 1)
    );
}

#[tokio::test]
async fn attached_session_may_observe_only_its_declared_frame_sources() {
    let runtime = service(FakeAudio::new(), false, None);
    let authorized_source = MediaSourceRef(MediaSourceId::from("native:camera"));
    let fake_session = FakeSession::new(MediaSessionRef(MediaSessionId::from("incoming-1")));
    fake_session.set_authorized_sources(vec![authorized_source.clone()]);
    let session = runtime
        .attach_session(context(owner("alpha"), 70), Box::new(fake_session.clone()))
        .await
        .unwrap();
    let target = MediaResourceRef::Session(session);
    let now = now_ms();
    let asset = MediaAsset {
        reference: MediaRef::from("attached-frame"),
        metadata: MediaMetadata {
            kind: MediaKind::Image,
            mime_type: "image/jpeg".to_owned(),
            byte_len: 4,
            duration_ms: None,
            width_px: Some(1),
            height_px: Some(1),
            sample_rate_hz: None,
            channels: None,
            captured_at_unix_ms: Some(now),
        },
    };
    fake_session.enqueue(MediaEventBatch {
        target: target.clone(),
        events: vec![MediaEvent {
            sequence: 1,
            occurred_at_unix_ms: now,
            target: target.clone(),
            operation_id: None,
            kind: MediaEventKind::Frame {
                source: authorized_source,
                asset,
            },
        }],
        next_cursor: Some(MediaEventCursor { sequence: 1 }),
        dropped_before_cursor: false,
    });
    let visible = runtime
        .execute(
            context(owner("alpha"), 71),
            MediaOperation::Observe {
                target,
                after: None,
                limit: 16,
            },
        )
        .await
        .unwrap();
    assert!(matches!(visible, MediaResult::Events { batch } if batch.events.len() == 1));
}

#[test]
fn event_buffer_keeps_sequence_high_water_after_pruning_all_events() {
    let target = MediaResourceRef::Session(MediaSessionRef(MediaSessionId::from("event-buffer")));
    let first_time = 1_000_000;
    let later = first_time + MAX_EVENT_AGE_MS + 1;
    let mut buffer = EventBuffer::default();
    let first = MediaEvent {
        sequence: 40,
        occurred_at_unix_ms: first_time,
        target: target.clone(),
        operation_id: None,
        kind: MediaEventKind::StateChanged {
            resource: target.clone(),
            state: MediaResourceState::Active,
        },
    };
    assert!(buffer.push(first, first_time));

    buffer.prune(later);
    assert!(buffer.events.is_empty());
    let out_of_order = MediaEvent {
        sequence: 39,
        occurred_at_unix_ms: later,
        target: target.clone(),
        operation_id: None,
        kind: MediaEventKind::StateChanged {
            resource: target.clone(),
            state: MediaResourceState::Active,
        },
    };
    assert!(!buffer.push(out_of_order, later));

    buffer.push_state(target.clone(), MediaResourceState::Completed, None, later);
    let snapshot = buffer
        .snapshot(target, Some(MediaEventCursor { sequence: 39 }), 16, later)
        .unwrap();
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].sequence, 41);
    assert!(snapshot.dropped_before_cursor);
}

#[tokio::test]
async fn support_revisions_are_stable_per_owner_when_routes_differ() {
    let audio = FakeAudio::with_owner_restriction(Some("alpha".to_owned()));
    let runtime = service(audio, false, None);
    let alpha = owner("alpha");
    let beta = owner("beta");
    let first_alpha = runtime.capabilities_for(&alpha);
    let beta_caps = runtime.capabilities_for(&beta);
    let second_alpha = runtime.capabilities_for(&alpha);
    let catalog = runtime.capabilities();
    assert!(first_alpha.supports(MediaOperationKind::Synthesize));
    assert!(!beta_caps.supports(MediaOperationKind::Synthesize));
    assert!(catalog.supports(MediaOperationKind::Synthesize));
    assert_eq!(first_alpha.support_revision, second_alpha.support_revision);
}

struct DelayedFirstCancelAudio {
    inner: Arc<dyn AudioService>,
    calls: AtomicUsize,
    first_started: Notify,
    first_release: Notify,
}

#[async_trait]
impl AudioService for DelayedFirstCancelAudio {
    fn capabilities(&self) -> AudioCapabilitySnapshot {
        self.inner.capabilities()
    }
    fn capabilities_for(&self, owner: &MediaOwner) -> AudioCapabilitySnapshot {
        self.inner.capabilities_for(owner)
    }
    async fn execute(
        &self,
        context: MediaOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        self.inner.execute(context, operation).await
    }
    async fn cancel(&self, context: MediaOperationContext) -> Result<(), AudioError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_started.notify_one();
            self.first_release.notified().await;
        }
        self.inner.cancel(context).await
    }
}

#[tokio::test]
async fn delayed_cancel_fences_identity_until_receipt_then_same_id_reuse_is_safe() {
    let audio = FakeAudio::new();
    audio.pending_listen.store(true, Ordering::SeqCst);
    let delayed = Arc::new(DelayedFirstCancelAudio {
        inner: audio.clone(),
        calls: AtomicUsize::new(0),
        first_started: Notify::new(),
        first_release: Notify::new(),
    });
    let runtime = service(delayed.clone(), false, None);
    let host = crate::attachment::MediaAttachmentHost::new();
    let attachment_a = host.attach(runtime.clone()).unwrap();
    let attachment_b = host.attach(runtime).unwrap();
    let context_a = context(owner("shared"), 8_001);
    let mut context_b = context(owner("shared"), 8_001);
    context_b.identity = context_a.identity.clone();
    let service_a = attachment_a.service();
    let service_b = attachment_b.service();

    let first_service = service_a.clone();
    let first_context = context_a.clone();
    let first = tokio::spawn(async move {
        first_service
            .execute(
                first_context,
                MediaOperation::Transcribe {
                    input: TranscriptionInput::LiveSource(microphone()),
                    language: None,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), audio.listen_started.notified())
        .await
        .unwrap();
    let cancel = tokio::spawn(async move { service_a.cancel(context_a).await });
    tokio::time::timeout(Duration::from_secs(1), delayed.first_started.notified())
        .await
        .unwrap();

    let duplicate = tokio::time::timeout(
        Duration::from_secs(1),
        service_b.execute(
            context_b.clone(),
            MediaOperation::Transcribe {
                input: TranscriptionInput::LiveSource(microphone()),
                language: None,
            },
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(duplicate.kind, MediaErrorKind::InvalidRequest);
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 0);

    delayed.first_release.notify_one();
    cancel.await.unwrap().unwrap();
    let first_result = tokio::time::timeout(Duration::from_secs(1), first)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_result.unwrap_err().kind, MediaErrorKind::Cancelled);
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 1);

    let second_service = service_b.clone();
    let second_context = context_b.clone();
    let second = tokio::spawn(async move {
        second_service
            .execute(
                second_context,
                MediaOperation::Transcribe {
                    input: TranscriptionInput::LiveSource(microphone()),
                    language: None,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), audio.listen_started.notified())
        .await
        .unwrap();
    assert!(!second.is_finished());
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 1);

    service_b.cancel(context_b.clone()).await.unwrap();
    let second_result = tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(second_result.kind, MediaErrorKind::Cancelled);
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unpolled_cancel_cannot_retarget_a_new_same_owner_operation() {
    let audio = FakeAudio::new();
    let runtime = service(audio.clone(), false, None);
    let original_context = context(owner("same-owner"), 8_003);
    let completed = runtime
        .execute(
            original_context.clone(),
            MediaOperation::Transcribe {
                input: TranscriptionInput::LiveSource(microphone()),
                language: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(completed, MediaResult::Transcript { .. }));

    let stale_cancel = runtime.cancel(original_context.clone());
    let mut reused_context = context(owner("same-owner"), 8_003);
    reused_context.identity = original_context.identity.clone();
    audio.pending_listen.store(true, Ordering::SeqCst);
    let task_runtime = runtime.clone();
    let task_context = reused_context.clone();
    let second = tokio::spawn(async move {
        task_runtime
            .execute(
                task_context,
                MediaOperation::Transcribe {
                    input: TranscriptionInput::LiveSource(microphone()),
                    language: None,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), audio.listen_started.notified())
        .await
        .unwrap();
    stale_cancel.await.unwrap();
    assert!(!second.is_finished());
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 0);

    runtime.cancel(reused_context).await.unwrap();
    let cancelled = tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(cancelled.kind, MediaErrorKind::Cancelled);
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unpolled_stop_reference_cannot_retarget_a_reused_operation_id() {
    let audio = FakeAudio::new();
    let runtime = service(audio.clone(), false, None);
    let original_context = context(owner("same-owner"), 8_004);
    let operation_reference = MediaResourceRef::Operation {
        identity: original_context.identity.clone(),
        incarnation: original_context.operation_incarnation(),
    };
    let completed = runtime
        .execute(
            original_context.clone(),
            MediaOperation::Transcribe {
                input: TranscriptionInput::LiveSource(microphone()),
                language: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(completed, MediaResult::Transcript { .. }));

    let old_stop = runtime.execute(
        context(owner("same-owner"), 8_005),
        MediaOperation::Stop {
            target: operation_reference,
        },
    );
    let mut reused_context = context(owner("same-owner"), 8_004);
    reused_context.identity = original_context.identity.clone();
    audio.pending_listen.store(true, Ordering::SeqCst);
    let task_runtime = runtime.clone();
    let task_context = reused_context.clone();
    let second = tokio::spawn(async move {
        task_runtime
            .execute(
                task_context,
                MediaOperation::Transcribe {
                    input: TranscriptionInput::LiveSource(microphone()),
                    language: None,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), audio.listen_started.notified())
        .await
        .unwrap();

    let stop_error = old_stop.await.unwrap_err();
    assert_eq!(stop_error.kind, MediaErrorKind::NotFound);
    assert!(!second.is_finished());
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 0);

    runtime.cancel(reused_context).await.unwrap();
    let cancelled = tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(cancelled.kind, MediaErrorKind::Cancelled);
    assert_eq!(audio.cancellations.load(Ordering::SeqCst), 1);
}
