//! Local native media sessions over one host-opened device lease.
//!
//! The host owns device discovery, permissions, modes, directions, and the
//! [`MediaDevicePorts`] implementation. This connector adds only host-provided
//! local session sources that are not already exposed by `UnifiedMediaService`
//! (for example, a screen source).

use async_trait::async_trait;
use image::ImageEncoder;
use media_api::{
    MediaAudioChannel, MediaConnector, MediaConnectorId, MediaDevicePorts, MediaDeviceSession,
    MediaError, MediaErrorKind, MediaEvent, MediaEventBatch, MediaEventCursor, MediaEventKind,
    MediaFrame, MediaFramePayload, MediaKind, MediaMetadata, MediaOperationContext,
    MediaOperationId, MediaOperationKind, MediaOwner, MediaPayload, MediaPcmFrame,
    MediaRenderReceipt, MediaResourceRef, MediaResourceState, MediaResourceStore, MediaSession,
    MediaSessionId, MediaSessionIntent, MediaSessionMode, MediaSessionRef, MediaSessionRequest,
    MediaSessionState, MediaSessionStatus, MediaSessionUpdate, MediaSource, MediaSourceId,
    MediaSourceKind, MediaSourceRef, MediaSourceUsage, MAX_MEDIA_EVENT_BATCH_SIZE,
    MAX_MEDIA_SESSION_SOURCE_REFS,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{watch, Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const LOCAL_MICROPHONE_ID: &str = "media-runtime:microphone";
const LOCAL_CAMERA_ID: &str = "media-runtime:camera";
const LOCAL_SPEAKER_ID: &str = "media-runtime:speaker";
const LOCAL_LIBRARY_ID: &str = "media-runtime:library";
const MAX_ADDITIONAL_LOCAL_SOURCES: usize = MAX_MEDIA_SESSION_SOURCE_REFS - 2;
const AUDIO_INPUT_QUEUE_CAPACITY: usize = 16;
const MAX_LOCAL_AUDIO_FRAME_BYTES: u64 = 64 * 1024;
const MAX_LOCAL_IMAGE_PIXELS: u64 = 2_000_000;
const LOCAL_EVENT_AGE_MS: u64 = 60_000;
const LOCAL_FUTURE_SKEW_MS: u64 = 5_000;
const OBSERVATION_FRAME_INTERVAL: Duration = Duration::from_secs(1);

/// Local connector backed by exactly one host-opened native device lease per session.
pub struct LocalMediaConnector {
    ports: Arc<dyn MediaDevicePorts>,
    store: Arc<dyn MediaResourceStore>,
    additional_sources: Vec<MediaSource>,
    supported_modes: RwLock<Vec<MediaSessionMode>>,
}

impl LocalMediaConnector {
    /// Create a local connector from host-owned ports and the actual local source/mode inventory.
    ///
    /// `additional_sources` contains session-only sources not already synthesized by
    /// `UnifiedMediaService`, normally screen capture. Microphone and camera sources retain
    /// the runtime's canonical `media-runtime:*` identities.
    pub fn new(
        ports: Arc<dyn MediaDevicePorts>,
        store: Arc<dyn MediaResourceStore>,
        additional_sources: Vec<MediaSource>,
        supported_modes: Vec<MediaSessionMode>,
    ) -> Result<Self, MediaError> {
        let modes = validate_modes(supported_modes)?;
        if additional_sources.len() > MAX_ADDITIONAL_LOCAL_SOURCES {
            return Err(MediaError::new(
                MediaErrorKind::MediaTooLarge,
                "local media source catalog exceeds its limit",
            ));
        }
        let mut source_ids = HashSet::new();
        for source in &additional_sources {
            let id = &source.reference.0 .0;
            if id.is_empty()
                || id.len() > 256
                || source.label.is_empty()
                || source.label.len() > 256
            {
                return Err(invalid("local media source identity or label is invalid"));
            }
            if matches!(
                id.as_str(),
                LOCAL_MICROPHONE_ID | LOCAL_CAMERA_ID | LOCAL_SPEAKER_ID | LOCAL_LIBRARY_ID
            ) {
                return Err(invalid(
                    "additional local sources cannot reuse canonical runtime identities",
                ));
            }
            if source.kind != MediaSourceKind::Screen || source.media_kind != Some(MediaKind::Video)
            {
                return Err(invalid(
                    "additional local session sources must be video screens",
                ));
            }
            if !source.usages.contains(&MediaSourceUsage::Session)
                || !source
                    .supported_operations
                    .contains(&MediaOperationKind::SessionStart)
            {
                return Err(invalid(
                    "additional local sources must advertise session use",
                ));
            }
            if !source_ids.insert(source.reference.clone()) {
                return Err(invalid("additional local source identities must be unique"));
            }
        }
        if !additional_sources.is_empty()
            && !modes
                .iter()
                .any(|mode| matches!(mode, MediaSessionMode::Video | MediaSessionMode::AudioVideo))
        {
            return Err(invalid(
                "local screen sources require an advertised video mode",
            ));
        }
        Ok(Self {
            ports,
            store,
            additional_sources,
            supported_modes: RwLock::new(modes),
        })
    }

    /// Replace the explicit native mode inventory after the platform helper
    /// reports its real duplex capabilities.
    pub fn update_supported_modes(
        &self,
        supported_modes: Vec<MediaSessionMode>,
    ) -> Result<(), MediaError> {
        let modes = validate_modes(supported_modes)?;
        if !self.additional_sources.is_empty()
            && !modes
                .iter()
                .any(|mode| matches!(mode, MediaSessionMode::Video | MediaSessionMode::AudioVideo))
        {
            return Err(invalid(
                "local screen sources require an advertised video mode",
            ));
        }
        *self
            .supported_modes
            .write()
            .unwrap_or_else(|error| error.into_inner()) = modes;
        Ok(())
    }

    fn current_modes(&self) -> Vec<MediaSessionMode> {
        self.supported_modes
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn supports_video(modes: &[MediaSessionMode]) -> bool {
        modes
            .iter()
            .any(|mode| matches!(mode, MediaSessionMode::Video | MediaSessionMode::AudioVideo))
    }

    fn is_available_source(&self, source: &MediaSourceRef, kind: MediaKind) -> bool {
        match kind {
            MediaKind::Audio => source.0 .0 == LOCAL_MICROPHONE_ID,
            MediaKind::Image => source.0 .0 == LOCAL_CAMERA_ID,
            MediaKind::Video => {
                source.0 .0 == LOCAL_CAMERA_ID
                    || self
                        .additional_sources
                        .iter()
                        .any(|candidate| candidate.reference == *source)
            }
        }
    }
}

fn validate_modes(modes: Vec<MediaSessionMode>) -> Result<Vec<MediaSessionMode>, MediaError> {
    if modes.len() > 3 {
        return Err(invalid(
            "local media connector supports at most three modes",
        ));
    }
    let mut unique = Vec::with_capacity(modes.len());
    for mode in modes {
        if unique.contains(&mode) {
            return Err(invalid("local media connector modes must be unique"));
        }
        unique.push(mode);
    }
    Ok(unique)
}

#[async_trait]
impl MediaConnector for LocalMediaConnector {
    fn id(&self) -> MediaConnectorId {
        MediaConnectorId::from("media-runtime:local")
    }

    async fn sources(
        &self,
        _context: MediaOperationContext,
    ) -> Result<Vec<MediaSource>, MediaError> {
        let modes = self.current_modes();
        if Self::supports_video(&modes) {
            Ok(self.additional_sources.clone())
        } else {
            Ok(Vec::new())
        }
    }

    async fn start_session(
        &self,
        context: MediaOperationContext,
        request: MediaSessionRequest,
    ) -> Result<Box<dyn MediaSession>, MediaError> {
        let supported_modes = self.current_modes();
        if !supported_modes.contains(&request.mode) {
            return Err(unsupported("local media mode is unavailable on this host"));
        }
        let supports_video = Self::supports_video(&supported_modes);
        let needs_audio = matches!(
            request.mode,
            MediaSessionMode::Audio | MediaSessionMode::AudioVideo
        );
        let needs_video = matches!(
            request.mode,
            MediaSessionMode::Video | MediaSessionMode::AudioVideo
        );
        match (&request.audio_source, needs_audio) {
            (Some(source), true) if self.is_available_source(source, MediaKind::Audio) => {}
            (Some(_), true) => return Err(not_found_source()),
            (None, true) => {
                return Err(invalid("local audio session requires a microphone source"));
            }
            (Some(_), false) => {
                return Err(invalid("audio source supplied to a non-audio session"));
            }
            (None, false) => {}
        }
        match (&request.video_source, needs_video) {
            (Some(source), true)
                if self.is_available_source(source, MediaKind::Video)
                    && (source.0 .0 != LOCAL_CAMERA_ID || supports_video) => {}
            (Some(_), true) => return Err(not_found_source()),
            (None, true) => return Err(invalid("local video session requires a video source")),
            (Some(_), false) => {
                return Err(invalid("video source supplied to a non-video session"));
            }
            (None, false) => {}
        }
        let device = self.ports.open(context.clone(), request.clone()).await?;
        let reference = MediaSessionRef(MediaSessionId::from(format!(
            "local-session:{}",
            context.identity.id
        )));
        if reference.0 .0.len() > 256 {
            let _ = device.stop().await;
            return Err(invalid("local media session reference exceeds its bound"));
        }
        let target = MediaResourceRef::Session(reference.clone());
        let selected_sources = selected_sources(&request);
        let mut allowed_sources = HashMap::<MediaKind, HashSet<MediaSourceRef>>::new();
        if matches!(
            request.mode,
            MediaSessionMode::Audio | MediaSessionMode::AudioVideo
        ) {
            allowed_sources
                .entry(MediaKind::Audio)
                .or_default()
                .insert(MediaSourceRef(MediaSourceId::from(LOCAL_MICROPHONE_ID)));
        }
        if matches!(
            request.mode,
            MediaSessionMode::Video | MediaSessionMode::AudioVideo
        ) {
            let video_sources = allowed_sources.entry(MediaKind::Video).or_default();
            video_sources.insert(MediaSourceRef(MediaSourceId::from(LOCAL_CAMERA_ID)));
            video_sources.extend(
                self.additional_sources
                    .iter()
                    .map(|source| source.reference.clone()),
            );
        }
        let status = MediaSessionStatus {
            state: MediaSessionState::Ready,
            audio_enabled: request.audio_source.is_some(),
            video_enabled: request.video_source.is_some(),
        };
        let event_stream = Arc::new(LocalEventStream::new(target.clone()));
        let audio_input = Arc::new(AudioInput::new());
        let stop_token = CancellationToken::new();
        let state = Arc::new(LocalSessionState {
            owner: context.owner.clone(),
            service_epoch: context.identity.service_epoch,
            reference,
            target,
            request: request.clone(),
            selected_sources: Arc::new(RwLock::new(selected_sources)),
            allowed_sources,
            status: Arc::new(RwLock::new(status)),
            device: device.clone(),
            max_payload_bytes: context.max_payload_bytes,
            event_stream: event_stream.clone(),
            audio_input: audio_input.clone(),
            stop_token: stop_token.clone(),
            stopped: AtomicBool::new(false),
            stop_acknowledged: AtomicBool::new(false),
            pump: AsyncMutex::new(None),
            lifecycle_gate: AsyncMutex::new(()),
        });
        event_stream
            .push(
                MediaEventKind::StateChanged {
                    resource: state.target.clone(),
                    state: MediaResourceState::Active,
                },
                now_ms(),
                Some(context.identity.clone()),
            )
            .await?;
        let mut background_context = context.clone();
        background_context.timeout_budget_ms = None;
        let pump = LocalPump {
            request,
            selected_sources: state.selected_sources.clone(),
            status: state.status.clone(),
            device,
            store: self.store.clone(),
            context: background_context,
            max_payload_bytes: state.max_payload_bytes,
            event_stream,
            audio_input,
            stop_token,
        };
        *state.pump.lock().await = Some(tokio::spawn(run_local_pump(pump)));
        Ok(Box::new(LocalMediaSession { state }))
    }
}

struct LocalSessionState {
    owner: MediaOwner,
    service_epoch: u64,
    reference: MediaSessionRef,
    target: MediaResourceRef,
    request: MediaSessionRequest,
    selected_sources: Arc<RwLock<HashMap<MediaKind, MediaSourceRef>>>,
    allowed_sources: HashMap<MediaKind, HashSet<MediaSourceRef>>,
    status: Arc<RwLock<MediaSessionStatus>>,
    device: Arc<dyn MediaDeviceSession>,
    max_payload_bytes: u64,
    event_stream: Arc<LocalEventStream>,
    audio_input: Arc<AudioInput>,
    stop_token: CancellationToken,
    stopped: AtomicBool,
    stop_acknowledged: AtomicBool,
    pump: AsyncMutex<Option<JoinHandle<()>>>,
    lifecycle_gate: AsyncMutex<()>,
}

impl LocalSessionState {
    fn validate_context(&self, context: &MediaOperationContext) -> Result<(), MediaError> {
        if context.owner != self.owner || context.identity.service_epoch != self.service_epoch {
            return Err(not_found_session());
        }
        if context.timeout_budget_ms == Some(0) {
            return Err(MediaError::new(
                MediaErrorKind::Timeout,
                "media deadline has elapsed",
            ));
        }
        Ok(())
    }

    fn is_active(&self) -> bool {
        !self.stopped.load(Ordering::Acquire)
            && !matches!(
                self.status.read().unwrap().state,
                MediaSessionState::Ended | MediaSessionState::Failed
            )
    }
}

struct LocalMediaSession {
    state: Arc<LocalSessionState>,
}

#[async_trait]
impl MediaSession for LocalMediaSession {
    fn reference(&self) -> &MediaSessionRef {
        &self.state.reference
    }

    fn status(&self) -> MediaSessionStatus {
        self.state.status.read().unwrap().clone()
    }

    fn authorized_sources(&self) -> Vec<MediaSourceRef> {
        self.state
            .allowed_sources
            .values()
            .flat_map(|sources| sources.iter().cloned())
            .collect()
    }

    async fn audio_channel(&self) -> Result<Arc<dyn MediaAudioChannel>, MediaError> {
        if self.state.request.intent != MediaSessionIntent::Conversation
            || self.state.request.audio_source.is_none()
        {
            return Err(unsupported(
                "local session does not provide a conversation audio channel",
            ));
        }
        Ok(Arc::new(LocalAudioChannel {
            state: self.state.clone(),
        }))
    }

    async fn update(
        &self,
        context: MediaOperationContext,
        update: MediaSessionUpdate,
    ) -> Result<MediaSessionStatus, MediaError> {
        self.state.validate_context(&context)?;
        // Serialize the native update and its state/event commit with stop so
        // an in-flight update cannot restore a nonterminal state afterward.
        let _guard = self.state.lifecycle_gate.lock().await;
        if !self.state.is_active() {
            return Err(not_found_session());
        }
        if let MediaSessionUpdate::SetPlaybackGain { gain } = &update {
            if !gain.is_finite() || !(0.0..=1.0).contains(gain) {
                return Err(invalid("playback gain must be finite and within [0, 1]"));
            }
        }
        if let MediaSessionUpdate::SelectSource { kind, source } = &update {
            let valid = *kind != MediaKind::Image
                && self
                    .state
                    .allowed_sources
                    .get(kind)
                    .is_some_and(|sources| sources.contains(source));
            if !valid {
                return Err(not_found_source());
            }
        }
        let status = self.state.device.update(update.clone()).await?;
        if let MediaSessionUpdate::SelectSource { kind, source } = update {
            self.state
                .selected_sources
                .write()
                .unwrap()
                .insert(kind, source);
        }
        *self.state.status.write().unwrap() = status.clone();
        if !status.audio_enabled {
            self.state.audio_input.clear().await;
        }
        self.state
            .event_stream
            .push(
                MediaEventKind::StateChanged {
                    resource: self.state.target.clone(),
                    state: resource_state(status.state),
                },
                now_ms(),
                Some(context.identity),
            )
            .await?;
        if matches!(
            status.state,
            MediaSessionState::Ended | MediaSessionState::Failed
        ) {
            self.state.stop_token.cancel();
            self.state.audio_input.finish();
        }
        Ok(status)
    }

    async fn observe(
        &self,
        context: MediaOperationContext,
        after: Option<MediaEventCursor>,
        limit: u16,
        wait_ms: Option<u64>,
    ) -> Result<MediaEventBatch, MediaError> {
        self.state.validate_context(&context)?;
        self.state.event_stream.observe(after, limit, wait_ms).await
    }

    async fn stop(&self, context: MediaOperationContext) -> Result<MediaSessionStatus, MediaError> {
        self.state.validate_context(&context)?;
        let _guard = self.state.lifecycle_gate.lock().await;
        if self.state.stop_acknowledged.load(Ordering::Acquire) {
            return Ok(self.status());
        }
        self.state.stopped.store(true, Ordering::Release);
        self.state.stop_token.cancel();
        self.state.audio_input.finish();
        let device_result = self.state.device.stop().await;
        // Keep ownership in shared state while awaiting. A caller timeout must
        // not detach the pump and let a later stop acknowledge unfinished work.
        let mut pump_slot = self.state.pump.lock().await;
        if let Some(pump) = pump_slot.as_mut() {
            let joined = pump.await;
            pump_slot.take();
            joined.map_err(|_| {
                self.state.status.write().unwrap().state = MediaSessionState::Failed;
                MediaError::new(
                    MediaErrorKind::NativeFailure,
                    "local media pump failed to join",
                )
            })?;
        }
        drop(pump_slot);
        let result = {
            let mut status = self.state.status.write().unwrap();
            status.state = if device_result.is_err() || status.state == MediaSessionState::Failed {
                MediaSessionState::Failed
            } else {
                MediaSessionState::Ended
            };
            status.clone()
        };
        if let Err(error) = device_result {
            let _ = self
                .state
                .event_stream
                .push(
                    MediaEventKind::StateChanged {
                        resource: self.state.target.clone(),
                        state: MediaResourceState::Failed,
                    },
                    now_ms(),
                    Some(context.identity),
                )
                .await;
            return Err(error);
        }
        self.state.stop_acknowledged.store(true, Ordering::Release);
        self.state
            .event_stream
            .push(
                MediaEventKind::StateChanged {
                    resource: self.state.target.clone(),
                    state: resource_state(result.state),
                },
                now_ms(),
                Some(context.identity),
            )
            .await?;
        Ok(result)
    }
}

struct LocalAudioChannel {
    state: Arc<LocalSessionState>,
}

#[async_trait]
impl MediaAudioChannel for LocalAudioChannel {
    async fn read(&self) -> Result<Option<MediaFrame>, MediaError> {
        if self.state.request.intent != MediaSessionIntent::Conversation {
            return Err(unsupported(
                "local observation sessions have no audio channel",
            ));
        }
        self.state.audio_input.read().await
    }

    async fn write(&self, frame: MediaPcmFrame) -> Result<MediaRenderReceipt, MediaError> {
        if !self.state.is_active() {
            return Err(not_found_session());
        }
        if !self.state.status.read().unwrap().audio_enabled {
            return Err(unsupported("local session audio is disabled"));
        }
        frame.validate(
            self.state
                .max_payload_bytes
                .min(MAX_LOCAL_AUDIO_FRAME_BYTES),
        )?;
        let requested_frames = frame.pcm16le.len() / (usize::from(frame.channels) * 2);
        let receipt = self.state.device.write_audio(frame).await?;
        if usize::try_from(receipt.accepted_sample_frames).unwrap_or(usize::MAX) > requested_frames
        {
            return Err(MediaError::new(
                MediaErrorKind::NativeFailure,
                "native audio device returned an invalid render receipt",
            ));
        }
        Ok(receipt)
    }

    async fn flush(&self) -> Result<MediaRenderReceipt, MediaError> {
        if !self.state.is_active() {
            return Err(not_found_session());
        }
        validate_barrier_receipt(self.state.device.flush_audio().await?)
    }

    async fn interrupt(&self) -> Result<MediaRenderReceipt, MediaError> {
        if !self.state.is_active() {
            return Err(not_found_session());
        }
        validate_barrier_receipt(self.state.device.interrupt_audio().await?)
    }
}

fn validate_barrier_receipt(receipt: MediaRenderReceipt) -> Result<MediaRenderReceipt, MediaError> {
    if receipt.accepted_sample_frames != 0 {
        return Err(MediaError::new(
            MediaErrorKind::NativeFailure,
            "native audio barrier accepted new samples",
        ));
    }
    Ok(receipt)
}

struct AudioInput {
    frames: AsyncMutex<VecDeque<MediaFrame>>,
    changed: Notify,
    ended: AtomicBool,
}

impl AudioInput {
    fn new() -> Self {
        Self {
            frames: AsyncMutex::new(VecDeque::with_capacity(AUDIO_INPUT_QUEUE_CAPACITY)),
            changed: Notify::new(),
            ended: AtomicBool::new(false),
        }
    }

    async fn push(&self, frame: MediaFrame) {
        let mut frames = self.frames.lock().await;
        if frames.len() >= AUDIO_INPUT_QUEUE_CAPACITY {
            frames.pop_front();
        }
        frames.push_back(frame);
        drop(frames);
        self.changed.notify_one();
    }

    async fn read(&self) -> Result<Option<MediaFrame>, MediaError> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(frame) = self.frames.lock().await.pop_front() {
                return Ok(Some(frame));
            }
            if self.ended.load(Ordering::Acquire) {
                return Ok(None);
            }
            notified.await;
        }
    }

    fn finish(&self) {
        self.ended.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    async fn clear(&self) {
        self.frames.lock().await.clear();
    }
}

struct LocalEventStream {
    target: MediaResourceRef,
    events: AsyncMutex<LocalEventBuffer>,
    revision: watch::Sender<u64>,
}

struct LocalEventBuffer {
    events: VecDeque<MediaEvent>,
    next_sequence: u64,
    dropped_through: u64,
    dropped_any: bool,
}

impl LocalEventStream {
    fn new(target: MediaResourceRef) -> Self {
        let (revision, _) = watch::channel(0);
        Self {
            target,
            events: AsyncMutex::new(LocalEventBuffer {
                events: VecDeque::new(),
                next_sequence: 1,
                dropped_through: 0,
                dropped_any: false,
            }),
            revision,
        }
    }

    async fn push(
        &self,
        kind: MediaEventKind,
        occurred_at_unix_ms: u64,
        operation_id: Option<MediaOperationId>,
    ) -> Result<(), MediaError> {
        let mut state = self.events.lock().await;
        let sequence = state.next_sequence;
        state.next_sequence = sequence.checked_add(1).ok_or_else(|| {
            MediaError::new(
                MediaErrorKind::NativeFailure,
                "local event sequence exhausted",
            )
        })?;
        if state.events.len() >= usize::from(MAX_MEDIA_EVENT_BATCH_SIZE) {
            if let Some(expired) = state.events.pop_front() {
                state.dropped_through = state.dropped_through.max(expired.sequence);
                state.dropped_any = true;
            }
        }
        state.events.push_back(MediaEvent {
            sequence,
            occurred_at_unix_ms,
            target: self.target.clone(),
            operation_id,
            kind,
        });
        drop(state);
        self.revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        Ok(())
    }

    async fn snapshot(
        &self,
        after: Option<MediaEventCursor>,
        limit: u16,
    ) -> Result<MediaEventBatch, MediaError> {
        let state = self.events.lock().await;
        let after_sequence = after.map_or(0, |cursor| cursor.sequence);
        let dropped = state.dropped_any && after_sequence < state.dropped_through;
        let events = state
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .take(usize::from(limit.min(MAX_MEDIA_EVENT_BATCH_SIZE)))
            .cloned()
            .collect();
        MediaEventBatch::bounded(self.target.clone(), events, dropped)
    }

    async fn observe(
        &self,
        after: Option<MediaEventCursor>,
        limit: u16,
        wait_ms: Option<u64>,
    ) -> Result<MediaEventBatch, MediaError> {
        let mut revision = self.revision.subscribe();
        let Some(wait_ms) = wait_ms.filter(|ms| *ms > 0) else {
            return self.snapshot(after, limit).await;
        };
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
        loop {
            let batch = self.snapshot(after, limit).await?;
            if !batch.events.is_empty() || tokio::time::Instant::now() >= deadline {
                return Ok(batch);
            }
            tokio::select! {
                changed = revision.changed() => {
                    if changed.is_err() {
                        return Ok(batch);
                    }
                }
                _ = tokio::time::sleep_until(deadline) => return self.snapshot(after, limit).await,
            }
        }
    }
}

struct LocalPump {
    request: MediaSessionRequest,
    selected_sources: Arc<RwLock<HashMap<MediaKind, MediaSourceRef>>>,
    status: Arc<RwLock<MediaSessionStatus>>,
    device: Arc<dyn MediaDeviceSession>,
    store: Arc<dyn MediaResourceStore>,
    context: MediaOperationContext,
    max_payload_bytes: u64,
    event_stream: Arc<LocalEventStream>,
    audio_input: Arc<AudioInput>,
    stop_token: CancellationToken,
}

async fn run_local_pump(pump: LocalPump) {
    let mut frame_sequences = HashMap::<MediaSourceRef, u64>::new();
    let mut last_observation_sample = HashMap::<MediaSourceRef, std::time::Instant>::new();
    let terminal_state = loop {
        let next = tokio::select! {
            biased;
            _ = pump.stop_token.cancelled() => break MediaSessionState::Ended,
            next = pump.device.next_input() => next,
        };
        let frame = match next {
            Ok(Some(frame)) => frame,
            Ok(None) => break MediaSessionState::Ended,
            Err(error) => {
                let _ = pump
                    .event_stream
                    .push(
                        MediaEventKind::Error {
                            kind: error.kind,
                            message: "native local media input failed".to_owned(),
                        },
                        now_ms(),
                        Some(pump.context.identity.clone()),
                    )
                    .await;
                break MediaSessionState::Failed;
            }
        };
        if let Err(error) = frame.validate(pump.max_payload_bytes) {
            let _ = pump
                .event_stream
                .push(
                    MediaEventKind::Error {
                        kind: error.kind,
                        message: "native local frame exceeded its bounds".to_owned(),
                    },
                    now_ms(),
                    Some(pump.context.identity.clone()),
                )
                .await;
            continue;
        }
        if frame.captured_at_unix_ms > now_ms().saturating_add(LOCAL_FUTURE_SKEW_MS)
            || frame.captured_at_unix_ms.saturating_add(LOCAL_EVENT_AGE_MS) < now_ms()
        {
            continue;
        }
        let media_kind = match &frame.payload {
            MediaFramePayload::Audio(_) => MediaKind::Audio,
            MediaFramePayload::Video(_) => MediaKind::Video,
        };
        let authorized_source = pump
            .selected_sources
            .read()
            .unwrap()
            .get(&media_kind)
            .is_some_and(|source| source == &frame.source);
        if !authorized_source {
            let _ = pump
                .event_stream
                .push(
                    MediaEventKind::Error {
                        kind: MediaErrorKind::InvalidRequest,
                        message: "native media frame source did not match the selected source"
                            .into(),
                    },
                    now_ms(),
                    Some(pump.context.identity.clone()),
                )
                .await;
            continue;
        }
        if frame_sequences
            .get(&frame.source)
            .is_some_and(|previous| frame.sequence <= *previous)
        {
            continue;
        }
        frame_sequences.insert(frame.source.clone(), frame.sequence);
        match &frame.payload {
            MediaFramePayload::Audio(audio)
                if pump.request.intent == MediaSessionIntent::Conversation
                    && pump.status.read().unwrap().audio_enabled
                    && audio.pcm16le.len() as u64 <= MAX_LOCAL_AUDIO_FRAME_BYTES =>
            {
                pump.audio_input.push(frame).await;
            }
            MediaFramePayload::Audio(audio)
                if pump.request.intent == MediaSessionIntent::Conversation
                    && pump.status.read().unwrap().audio_enabled
                    && audio.pcm16le.len() as u64 > MAX_LOCAL_AUDIO_FRAME_BYTES =>
            {
                push_observation_error(&pump, MediaErrorKind::MediaTooLarge).await;
            }
            MediaFramePayload::Video(video) if pump.status.read().unwrap().video_enabled => {
                let now = std::time::Instant::now();
                if last_observation_sample
                    .get(&frame.source)
                    .is_some_and(|last| now.duration_since(*last) < OBSERVATION_FRAME_INTERVAL)
                {
                    continue;
                }
                last_observation_sample.insert(frame.source.clone(), now);
                match encode_video_payload(video, frame.captured_at_unix_ms, pump.max_payload_bytes)
                {
                    Ok(payload) => match pump.store.put(&pump.context, payload.clone()).await {
                        Ok(asset) if asset.metadata == payload.metadata => {
                            let _ = pump
                                .event_stream
                                .push(
                                    MediaEventKind::Frame {
                                        source: frame.source,
                                        asset,
                                    },
                                    frame.captured_at_unix_ms,
                                    Some(pump.context.identity.clone()),
                                )
                                .await;
                        }
                        Ok(asset) => {
                            let _ = pump.store.delete(&pump.context, &asset.reference).await;
                            push_observation_error(&pump, MediaErrorKind::StoreFailure).await;
                        }
                        Err(_) => push_observation_error(&pump, MediaErrorKind::StoreFailure).await,
                    },
                    Err(error) => push_observation_error(&pump, error.kind).await,
                }
            }
            _ => {}
        }
    };
    pump.audio_input.finish();
    if !pump.stop_token.is_cancelled() {
        pump.status.write().unwrap().state = terminal_state;
        let _ = pump
            .event_stream
            .push(
                MediaEventKind::StateChanged {
                    resource: pump.event_stream.target.clone(),
                    state: resource_state(terminal_state),
                },
                now_ms(),
                Some(pump.context.identity.clone()),
            )
            .await;
    }
}

async fn push_observation_error(pump: &LocalPump, kind: MediaErrorKind) {
    let _ = pump
        .event_stream
        .push(
            MediaEventKind::Error {
                kind,
                message: "local video observation frame was skipped".into(),
            },
            now_ms(),
            Some(pump.context.identity.clone()),
        )
        .await;
}

/// Encode a bounded raw video frame as an owner-store-ready PNG image asset.
///
/// Connectors can use this shared projection for observed camera, screen, or
/// decoded remote frames so all model-visible images have consistent bounds,
/// timestamps, metadata, and encoding.
pub fn encode_video_payload(
    frame: &media_api::MediaVideoFrame,
    captured_at_unix_ms: u64,
    max_payload_bytes: u64,
) -> Result<MediaPayload, MediaError> {
    frame.validate(max_payload_bytes)?;
    let (width, height, rgba) = frame_to_rgba(frame, max_payload_bytes)?;
    let mut writer = LimitedWriter::new(max_payload_bytes);
    image::codecs::png::PngEncoder::new(&mut writer)
        .write_image(&rgba, width, height, image::ColorType::Rgba8)
        .map_err(|_| {
            MediaError::new(
                MediaErrorKind::ProcessingFailed,
                "local video frame could not be encoded",
            )
        })?;
    let byte_len = u64::try_from(writer.bytes.len()).unwrap_or(u64::MAX);
    let payload = MediaPayload {
        metadata: MediaMetadata {
            kind: MediaKind::Image,
            mime_type: "image/png".to_owned(),
            byte_len,
            captured_at_unix_ms: Some(captured_at_unix_ms),
            duration_ms: None,
            width_px: Some(width),
            height_px: Some(height),
            sample_rate_hz: None,
            channels: None,
        },
        bytes: writer.bytes,
    };
    payload.validate(max_payload_bytes)?;
    Ok(payload)
}

fn frame_to_rgba(
    frame: &media_api::MediaVideoFrame,
    max_payload_bytes: u64,
) -> Result<(u32, u32, Vec<u8>), MediaError> {
    let (width, height) = match frame {
        media_api::MediaVideoFrame::Rgba8 {
            width_px,
            height_px,
            ..
        }
        | media_api::MediaVideoFrame::I420 {
            width_px,
            height_px,
            ..
        } => (*width_px, *height_px),
    };
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| {
            MediaError::new(MediaErrorKind::InvalidRequest, "image dimensions overflow")
        })?;
    if pixels > MAX_LOCAL_IMAGE_PIXELS || pixels.saturating_mul(4) > max_payload_bytes {
        return Err(MediaError::new(
            MediaErrorKind::MediaTooLarge,
            "local observation image exceeds its pixel bound",
        ));
    }
    let rgba_len = usize::try_from(pixels.saturating_mul(4)).map_err(|_| {
        MediaError::new(
            MediaErrorKind::MediaTooLarge,
            "local observation image is too large",
        )
    })?;
    let mut rgba = Vec::with_capacity(rgba_len);
    match frame {
        media_api::MediaVideoFrame::Rgba8 {
            width_px,
            height_px,
            stride_bytes,
            bytes,
        } => {
            let row_len = usize::try_from(*width_px)
                .ok()
                .and_then(|width| width.checked_mul(4))
                .ok_or_else(|| {
                    MediaError::new(MediaErrorKind::InvalidRequest, "RGBA row overflows")
                })?;
            let stride = usize::try_from(*stride_bytes).map_err(|_| {
                MediaError::new(MediaErrorKind::InvalidRequest, "RGBA stride overflows")
            })?;
            for row in 0..usize::try_from(*height_px).unwrap_or_default() {
                let start = row.checked_mul(stride).ok_or_else(|| {
                    MediaError::new(MediaErrorKind::InvalidRequest, "RGBA row offset overflows")
                })?;
                rgba.extend_from_slice(&bytes[start..start + row_len]);
            }
        }
        media_api::MediaVideoFrame::I420 {
            width_px,
            height_px,
            y_stride_bytes,
            u_stride_bytes,
            v_stride_bytes,
            y,
            u,
            v,
        } => {
            let y_stride = *y_stride_bytes as usize;
            let u_stride = *u_stride_bytes as usize;
            let v_stride = *v_stride_bytes as usize;
            for row in 0..*height_px as usize {
                for column in 0..*width_px as usize {
                    let luma = i32::from(y[row * y_stride + column]) - 16;
                    let chroma_row = row / 2;
                    let chroma_column = column / 2;
                    let cb = i32::from(u[chroma_row * u_stride + chroma_column]) - 128;
                    let cr = i32::from(v[chroma_row * v_stride + chroma_column]) - 128;
                    let red = ((298 * luma + 409 * cr + 128) >> 8).clamp(0, 255) as u8;
                    let green = ((298 * luma - 100 * cb - 208 * cr + 128) >> 8).clamp(0, 255) as u8;
                    let blue = ((298 * luma + 516 * cb + 128) >> 8).clamp(0, 255) as u8;
                    rgba.extend_from_slice(&[red, green, blue, 255]);
                }
            }
        }
    }
    Ok((width, height, rgba))
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl LimitedWriter {
    fn new(limit: u64) -> Self {
        Self {
            bytes: Vec::new(),
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        }
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            return Err(io::Error::other("encoded image exceeds its payload limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn selected_sources(request: &MediaSessionRequest) -> HashMap<MediaKind, MediaSourceRef> {
    let mut sources = HashMap::new();
    if let Some(source) = &request.audio_source {
        sources.insert(MediaKind::Audio, source.clone());
    }
    if let Some(source) = &request.video_source {
        sources.insert(MediaKind::Video, source.clone());
    }
    sources
}

fn resource_state(state: MediaSessionState) -> MediaResourceState {
    match state {
        MediaSessionState::Preparing
        | MediaSessionState::Ready
        | MediaSessionState::Reconnecting => MediaResourceState::Active,
        MediaSessionState::Ended => MediaResourceState::Completed,
        MediaSessionState::Failed => MediaResourceState::Failed,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn invalid(message: &str) -> MediaError {
    MediaError::new(MediaErrorKind::InvalidRequest, message)
}

fn unsupported(message: &str) -> MediaError {
    MediaError::new(MediaErrorKind::Unsupported, message)
}

fn not_found_source() -> MediaError {
    MediaError::new(MediaErrorKind::NotFound, "local media source was not found")
}

fn not_found_session() -> MediaError {
    MediaError::new(
        MediaErrorKind::NotFound,
        "local media session was not found",
    )
}

#[cfg(test)]
mod tests;
