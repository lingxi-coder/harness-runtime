//! App-scoped media orchestration over host-owned device, storage, and RTC adapters.

use crate::audio::{
    resolve_service_epoch, AudioOperation, AudioOperationKind, AudioOperationSuccess,
    AudioReadinessState, AudioRoutingSettings, AudioService, CloudAudioHost, OfflineAudioBackend,
    UnifiedAudioService,
};
use async_trait::async_trait;
use media_api::audio::{AudioCapabilitySnapshot, AudioError, AudioErrorKind};
use media_api::camera::{CameraControl, CameraError, CameraPosition, CapturePhotoOpts};
use media_api::tts::TtsAudio;
use media_api::voice::VoiceRecording;
use media_api::MediaOperationIncarnation;
use media_api::{
    CaptureOptions, MediaAsset, MediaAudioChannel, MediaCapabilities, MediaConnector, MediaError,
    MediaErrorKind, MediaEvent, MediaEventBatch, MediaEventCursor, MediaEventKind, MediaFrame,
    MediaFramePayload, MediaKind, MediaMetadata, MediaOperation, MediaOperationContext,
    MediaOperationId, MediaOperationKind, MediaOperationReadiness, MediaOwner, MediaPayload,
    MediaPcmFrame, MediaReadinessState, MediaRef, MediaRenderReceipt, MediaResourceRef,
    MediaResourceState, MediaResourceStatus, MediaResourceStore, MediaResult, MediaSession,
    MediaSessionIntent, MediaSessionMode, MediaSessionRef, MediaSessionRequest, MediaSessionState,
    MediaSessionStatus, MediaSource, MediaSourceId, MediaSourceKind, MediaSourceRef,
    MediaSourceUsage, RecordOptions, RecordingRef, TranscriptionInput, MAX_MEDIA_EVENT_BATCH_SIZE,
    MAX_MEDIA_SESSION_SOURCE_REFS,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{watch, Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;

const DEFAULT_MAX_PAYLOAD_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_OPERATION_TIMEOUT_MS: u64 = 30_000;
const MAX_ACTIVE_RECORDINGS: usize = 32;
const MAX_RECORDING_HISTORY: usize = 256;
const MAX_ACTIVE_SESSIONS: usize = 64;
const MAX_EVENT_TARGETS: usize = 64;
const MAX_EVENTS_PER_TARGET: usize = MAX_MEDIA_EVENT_BATCH_SIZE as usize;
const MAX_OPERATION_HISTORY: usize = 256;
const MAX_EVENT_AGE_MS: u64 = 60_000;
const MAX_FUTURE_EVENT_SKEW_MS: u64 = 5_000;
const MAX_MEDIA_REFERENCE_CHARS: usize = 256;
const MAX_MEDIA_MIME_CHARS: usize = 128;
const MAX_SOURCES: usize = 256;
const MAX_AUTO_RECORDING_MS: u64 = 60 * 60 * 1000;

/// Builds the shared media service around one native audio authority and host storage.
pub struct MediaRuntimeBuilder {
    native_audio: Option<Arc<dyn AudioService>>,
    camera: Option<Arc<dyn CameraControl>>,
    connector: Option<Arc<dyn MediaConnector>>,
    connector_modes: Vec<MediaSessionMode>,
    resources: Arc<dyn MediaResourceStore>,
    offline_audio: Vec<OfflineAudioBackend>,
    cloud_audio_host: Option<Arc<dyn CloudAudioHost>>,
    audio_routing: AudioRoutingSettings,
    service_epoch: u64,
    max_payload_bytes: u64,
}

impl MediaRuntimeBuilder {
    /// Start a builder with the host-owned media store.
    #[must_use]
    pub fn new(resources: Arc<dyn MediaResourceStore>) -> Self {
        Self {
            native_audio: None,
            camera: None,
            connector: None,
            connector_modes: Vec::new(),
            resources,
            offline_audio: Vec::new(),
            cloud_audio_host: None,
            audio_routing: AudioRoutingSettings::default(),
            service_epoch: 0,
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
        }
    }

    /// Inject the native AudioService that owns the platform's audio lease.
    #[must_use]
    pub fn audio(mut self, native_audio: Arc<dyn AudioService>) -> Self {
        self.native_audio = Some(native_audio);
        self
    }

    /// Inject the native camera and photo-library implementation.
    #[must_use]
    pub fn camera(mut self, camera: Arc<dyn CameraControl>) -> Self {
        self.camera = Some(camera);
        self
    }

    /// Inject the configured RTC/media connector and its implemented modes.
    #[must_use]
    pub fn connector(
        mut self,
        connector: Arc<dyn MediaConnector>,
        supported_modes: impl IntoIterator<Item = MediaSessionMode>,
    ) -> Self {
        self.connector = Some(connector);
        self.connector_modes = supported_modes.into_iter().collect();
        self
    }

    /// Add host-installed offline audio backends.
    #[must_use]
    pub fn offline_audio(mut self, backends: Vec<OfflineAudioBackend>) -> Self {
        self.offline_audio = backends;
        self
    }

    /// Inject the host adapter for explicitly configured cloud audio routes.
    #[must_use]
    pub fn cloud_audio_host(mut self, host: Arc<dyn CloudAudioHost>) -> Self {
        self.cloud_audio_host = Some(host);
        self
    }

    /// Configure the single authoritative system/offline/cloud audio policy.
    #[must_use]
    pub fn audio_routing(mut self, settings: AudioRoutingSettings) -> Self {
        self.audio_routing = settings;
        self
    }

    /// Set the host-issued service epoch used by operation contexts.
    #[must_use]
    pub fn service_epoch(mut self, service_epoch: u64) -> Self {
        self.service_epoch = service_epoch;
        self
    }

    /// Set the service-wide encoded media byte ceiling.
    #[must_use]
    pub fn max_payload_bytes(mut self, max_payload_bytes: u64) -> Self {
        self.max_payload_bytes = max_payload_bytes.min(DEFAULT_MAX_PAYLOAD_BYTES);
        self
    }

    /// Build the shared service. The returned handle is suitable for UI, tools, and realtime use.
    #[must_use]
    pub fn build(self) -> Arc<UnifiedMediaService> {
        let requested_service_epoch = self.service_epoch;
        let device_audio: Arc<dyn AudioService> = self.native_audio.unwrap_or_else(|| {
            Arc::new(UnsupportedAudioService {
                service_epoch: if requested_service_epoch == 0 {
                    1
                } else {
                    requested_service_epoch
                },
                max_payload_bytes: self.max_payload_bytes,
            })
        });
        let service_epoch = resolve_service_epoch(
            requested_service_epoch,
            device_audio.capabilities().service_epoch,
        );
        let audio_router = Arc::new(UnifiedAudioService::new_with_service_epoch(
            device_audio,
            self.offline_audio,
            self.cloud_audio_host,
            self.audio_routing,
            service_epoch,
        ));
        let audio: Arc<dyn AudioService> = audio_router.clone();
        Arc::new(UnifiedMediaService {
            audio,
            audio_router,
            camera: self.camera,
            connector: RwLock::new(ConnectorConfig {
                connector: self.connector,
                supported_modes: self.connector_modes,
            }),
            resources: self.resources,
            service_epoch,
            max_payload_bytes: self.max_payload_bytes,
            support_revision: Mutex::new(HashMap::new()),
            inner: Arc::new(Mutex::new(RuntimeState::default())),
        })
    }
}

/// The common owner-scoped media facade used by desktop, mobile, UI, and tools.
pub struct UnifiedMediaService {
    audio: Arc<dyn AudioService>,
    audio_router: Arc<UnifiedAudioService>,
    camera: Option<Arc<dyn CameraControl>>,
    connector: RwLock<ConnectorConfig>,
    resources: Arc<dyn MediaResourceStore>,
    service_epoch: u64,
    max_payload_bytes: u64,
    support_revision: Mutex<HashMap<MediaOwner, (Vec<MediaOperationKind>, Vec<MediaKind>, u64)>>,
    inner: Arc<Mutex<RuntimeState>>,
}

struct ConnectorConfig {
    connector: Option<Arc<dyn MediaConnector>>,
    supported_modes: Vec<MediaSessionMode>,
}

#[derive(Default)]
struct RuntimeState {
    active: HashMap<MediaOperationId, ActiveOperation>,
    recordings: HashMap<RecordingRef, OwnedRecording>,
    recording_order: VecDeque<RecordingRef>,
    recording_reservations: usize,
    sessions: HashMap<MediaSessionRef, OwnedSession>,
    session_reservations: HashMap<MediaOperationId, SessionReservationKind>,
    known_assets: HashMap<MediaRef, OwnedAsset>,
    known_asset_order: VecDeque<MediaRef>,
    events: HashMap<EventKey, OwnedEventBuffer>,
    event_target_order: VecDeque<EventKey>,
    operation_status: HashMap<MediaOperationId, OwnedOperationStatus>,
    operation_order: VecDeque<MediaOperationId>,
}

struct ActiveOperation {
    owner: MediaOwner,
    token: CancellationToken,
    done: Arc<Notify>,
    incarnation: MediaOperationIncarnation,
    cancel_flights: usize,
    finished: bool,
    cancel_unknown: bool,
}

struct MediaCancelFlight {
    inner: Arc<Mutex<RuntimeState>>,
    identity: MediaOperationId,
    incarnation: MediaOperationIncarnation,
    settled: bool,
}

impl MediaCancelFlight {
    fn finish(&mut self, confirmed: bool) {
        if self.settled {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let remove = if let Some(active) = inner.active.get_mut(&self.identity) {
            if active.incarnation != self.incarnation {
                self.settled = true;
                return;
            }
            active.cancel_flights = active.cancel_flights.saturating_sub(1);
            if !confirmed {
                active.cancel_unknown = true;
            }
            active.finished && active.cancel_flights == 0 && !active.cancel_unknown
        } else {
            false
        };
        if remove {
            inner.active.remove(&self.identity);
        }
        self.settled = true;
    }
}

impl Drop for MediaCancelFlight {
    fn drop(&mut self) {
        if !self.settled {
            self.finish(false);
        }
    }
}

async fn wait_for_active_operation(
    inner: &Arc<Mutex<RuntimeState>>,
    identity: &MediaOperationId,
    done: &Arc<Notify>,
    finished_at_admission: bool,
) {
    if finished_at_admission {
        return;
    }
    loop {
        let notified = done.notified();
        let finished = inner
            .lock()
            .unwrap()
            .active
            .get(identity)
            .is_none_or(|active| active.finished);
        if finished {
            return;
        }
        notified.await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EventKey {
    owner: MediaOwner,
    target: MediaResourceRef,
}

struct OwnedRecording {
    owner: MediaOwner,
    started_at_unix_ms: u64,
    timer: Option<CancellationToken>,
    finalize_in_progress: bool,
    lifecycle: RecordingLifecycle,
}

enum RecordingLifecycle {
    Active,
    Stopping,
    Finalized(VoiceRecording),
    Completed(MediaAsset),
    Failed(MediaErrorKind),
}

struct OwnedSession {
    owner: MediaOwner,
    origin_operation_id: MediaOperationId,
    session: Arc<dyn MediaSession>,
    audio_channel: Arc<AsyncMutex<Option<Arc<dyn MediaAudioChannel>>>>,
    intent: Option<MediaSessionIntent>,
    declared_sources: HashSet<MediaSourceRef>,
    selected_sources: HashMap<MediaKind, MediaSourceRef>,
    audio_sources: Arc<RwLock<HashSet<MediaSourceRef>>>,
    started_at_unix_ms: u64,
}

enum CapacityReservationKind {
    Recording,
    Session(MediaOperationId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionReservationKind {
    Start,
    Attach,
}

struct CapacityReservation {
    inner: Arc<Mutex<RuntimeState>>,
    kind: CapacityReservationKind,
    active: bool,
}

impl CapacityReservation {
    fn commit_locked(&mut self, inner: &mut RuntimeState) {
        match &self.kind {
            CapacityReservationKind::Recording => {
                inner.recording_reservations = inner.recording_reservations.saturating_sub(1);
            }
            CapacityReservationKind::Session(identity) => {
                inner.session_reservations.remove(identity);
            }
        }
        self.active = false;
    }
}

impl Drop for CapacityReservation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        match &self.kind {
            CapacityReservationKind::Recording => {
                inner.recording_reservations = inner.recording_reservations.saturating_sub(1);
            }
            CapacityReservationKind::Session(identity) => {
                inner.session_reservations.remove(identity);
            }
        }
        self.active = false;
    }
}

struct UnregisteredSessionLease {
    session: Arc<dyn MediaSession>,
    context: MediaOperationContext,
    reservation: Option<CapacityReservation>,
    armed: bool,
}

impl UnregisteredSessionLease {
    fn new(session: Arc<dyn MediaSession>, context: MediaOperationContext) -> Self {
        Self {
            session,
            context,
            reservation: None,
            armed: true,
        }
    }

    fn hold_reservation(&mut self, reservation: CapacityReservation) {
        self.reservation = Some(reservation);
    }

    fn commit_locked(&mut self, inner: &mut RuntimeState) {
        if let Some(mut reservation) = self.reservation.take() {
            reservation.commit_locked(inner);
        }
        self.armed = false;
    }

    fn release_after_terminal(&mut self) {
        self.armed = false;
        self.reservation.take();
    }

    async fn stop(&mut self) {
        if !self.armed {
            return;
        }
        let result = tokio::time::timeout(
            Duration::from_millis(DEFAULT_OPERATION_TIMEOUT_MS),
            self.session.stop(self.context.clone()),
        )
        .await;
        if matches!(
            result,
            Ok(Ok(MediaSessionStatus {
                state: MediaSessionState::Ended | MediaSessionState::Failed,
                ..
            }))
        ) {
            self.release_after_terminal();
        }
    }
}

impl Drop for UnregisteredSessionLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let session = self.session.clone();
        let context = self.context.clone();
        let reservation = self.reservation.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for attempt in 0..3 {
                    let result = tokio::time::timeout(
                        Duration::from_millis(DEFAULT_OPERATION_TIMEOUT_MS),
                        session.stop(context.clone()),
                    )
                    .await;
                    if matches!(
                        result,
                        Ok(Ok(MediaSessionStatus {
                            state: MediaSessionState::Ended | MediaSessionState::Failed,
                            ..
                        }))
                    ) {
                        break;
                    }
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
                drop(reservation);
            });
        } else {
            drop(reservation);
        }
    }
}

struct OwnedAsset {
    owner: MediaOwner,
    metadata: MediaMetadata,
    created_at_unix_ms: u64,
}

struct OwnedOperationStatus {
    owner: MediaOwner,
    incarnation: MediaOperationIncarnation,
    state: MediaResourceState,
    started_at_unix_ms: u64,
    updated_at_unix_ms: u64,
}

struct OwnedEventBuffer {
    buffer: EventBuffer,
    last_access_unix_ms: u64,
}

struct EventBuffer {
    events: VecDeque<MediaEvent>,
    high_water_sequence: u64,
    dropped_through: u64,
    dropped_any: bool,
    revision_tx: watch::Sender<u64>,
}

impl Default for EventBuffer {
    fn default() -> Self {
        let (revision_tx, _) = watch::channel(0);
        Self {
            events: VecDeque::new(),
            high_water_sequence: 0,
            dropped_through: 0,
            dropped_any: false,
            revision_tx,
        }
    }
}

impl EventBuffer {
    fn prune(&mut self, now: u64) {
        let cutoff = now.saturating_sub(MAX_EVENT_AGE_MS);
        while self
            .events
            .front()
            .is_some_and(|event| event.occurred_at_unix_ms < cutoff)
        {
            if let Some(event) = self.events.pop_front() {
                self.dropped_through = self.dropped_through.max(event.sequence);
                self.dropped_any = true;
            }
        }
    }

    fn push(&mut self, mut event: MediaEvent, now: u64) -> bool {
        self.prune(now);
        event.sequence = event.sequence.max(1);
        if event.occurred_at_unix_ms > now.saturating_add(MAX_FUTURE_EVENT_SKEW_MS) {
            return false;
        }
        if event.sequence <= self.high_water_sequence {
            return false;
        }
        if event.occurred_at_unix_ms.saturating_add(MAX_EVENT_AGE_MS) < now {
            self.high_water_sequence = event.sequence;
            self.dropped_through = self.dropped_through.max(event.sequence);
            self.dropped_any = true;
            return false;
        }
        if event.sequence > self.high_water_sequence.saturating_add(1) {
            self.dropped_through = self.dropped_through.max(event.sequence - 1);
            self.dropped_any = true;
        }
        while self.events.len() >= MAX_EVENTS_PER_TARGET {
            if let Some(removed) = self.events.pop_front() {
                self.dropped_through = self.dropped_through.max(removed.sequence);
                self.dropped_any = true;
            }
        }
        self.high_water_sequence = event.sequence;
        self.events.push_back(event);
        let revision = self.revision_tx.borrow().wrapping_add(1);
        self.revision_tx.send_replace(revision);
        true
    }

    fn push_state(
        &mut self,
        target: MediaResourceRef,
        state: MediaResourceState,
        operation_id: Option<MediaOperationId>,
        now: u64,
    ) {
        let sequence = self.high_water_sequence.saturating_add(1).max(1);
        let resource = target.clone();
        let event = MediaEvent {
            sequence,
            occurred_at_unix_ms: now,
            target,
            operation_id,
            kind: MediaEventKind::StateChanged { resource, state },
        };
        self.push(event, now);
    }

    fn snapshot(
        &mut self,
        target: MediaResourceRef,
        after: Option<MediaEventCursor>,
        limit: u16,
        now: u64,
    ) -> Result<MediaEventBatch, MediaError> {
        self.prune(now);
        let after_sequence = after.map_or(0, |cursor| cursor.sequence);
        let dropped = self.dropped_any && after_sequence < self.dropped_through;
        let events = self
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .take(usize::from(limit.min(MAX_MEDIA_EVENT_BATCH_SIZE)))
            .cloned()
            .collect();
        MediaEventBatch::bounded(target, events, dropped)
    }
}

impl UnifiedMediaService {
    fn claim_context_admission(context: &mut MediaOperationContext) -> Result<(), MediaError> {
        let incarnation = context.operation_incarnation();
        if !context.runtime_admitted() && !context.claim_operation_admission() {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "media operation context has already been admitted",
            ));
        }
        *context = context.clone().with_operation_incarnation(incarnation);
        Ok(())
    }

    fn reserve_recording_slot(&self) -> Result<CapacityReservation, MediaError> {
        let mut inner = self.inner.lock().unwrap();
        let active_recordings = inner
            .recordings
            .values()
            .filter(|recording| {
                !matches!(
                    &recording.lifecycle,
                    RecordingLifecycle::Completed(_) | RecordingLifecycle::Failed(_)
                )
            })
            .count();
        if active_recordings + inner.recording_reservations >= MAX_ACTIVE_RECORDINGS {
            return Err(MediaError::new(
                MediaErrorKind::Busy,
                "too many active recordings",
            ));
        }
        inner.recording_reservations += 1;
        Ok(CapacityReservation {
            inner: self.inner.clone(),
            kind: CapacityReservationKind::Recording,
            active: true,
        })
    }

    fn reserve_session_slot(
        &self,
        identity: &MediaOperationId,
        kind: SessionReservationKind,
    ) -> Result<CapacityReservation, MediaError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.session_reservations.contains_key(identity)
            || inner
                .sessions
                .values()
                .any(|session| &session.origin_operation_id == identity)
            || (kind == SessionReservationKind::Attach && inner.active.contains_key(identity))
        {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "media session operation identity is already in use",
            ));
        }
        if inner.sessions.len() + inner.session_reservations.len() >= MAX_ACTIVE_SESSIONS {
            return Err(MediaError::new(
                MediaErrorKind::Busy,
                "too many active media sessions",
            ));
        }
        inner.session_reservations.insert(identity.clone(), kind);
        Ok(CapacityReservation {
            inner: self.inner.clone(),
            kind: CapacityReservationKind::Session(identity.clone()),
            active: true,
        })
    }

    fn finish_session_registration(
        &self,
        context: &MediaOperationContext,
        reference: &MediaSessionRef,
        session: &Arc<dyn MediaSession>,
    ) -> Result<MediaSessionStatus, MediaError> {
        session.on_registered();
        let status = session.status();
        if matches!(
            status.state,
            MediaSessionState::Ended | MediaSessionState::Failed
        ) {
            if self.remove_owned_session_if_same(&context.owner, reference, session) {
                self.push_resource_event(
                    context,
                    MediaResourceRef::Session(reference.clone()),
                    session_resource_state(status.state),
                );
            }
            return Err(native_failure(
                "media session terminated during registration",
            ));
        }
        Ok(status)
    }

    /// Return the same routed audio service used by this media facade.
    /// Hosts may expose it through legacy audio-only interfaces without creating a second device lease authority.
    #[must_use]
    pub fn audio_service(&self) -> Arc<dyn AudioService> {
        self.audio.clone()
    }

    /// Replace audio routing configuration. In-flight audio calls retain their selected route.
    pub fn update_audio_routing(&self, settings: AudioRoutingSettings) {
        self.audio_router.update_settings(settings);
    }

    /// Replace the connector used for future source discovery and session starts.
    /// Existing sessions retain their connector-owned session handles.
    pub fn update_connector(
        &self,
        connector: Option<Arc<dyn MediaConnector>>,
        supported_modes: impl IntoIterator<Item = MediaSessionMode>,
    ) {
        *self.connector.write().unwrap() = ConnectorConfig {
            connector,
            supported_modes: supported_modes.into_iter().collect(),
        };
    }

    fn connector_config(&self) -> (Option<Arc<dyn MediaConnector>>, Vec<MediaSessionMode>) {
        let config = self.connector.read().unwrap();
        let modes = if config.connector.is_some() {
            config.supported_modes.clone()
        } else {
            Vec::new()
        };
        (config.connector.clone(), modes)
    }

    fn connector_modes(&self) -> Vec<MediaSessionMode> {
        self.connector.read().unwrap().supported_modes.clone()
    }

    fn audio_caps(&self, owner: &MediaOwner) -> AudioCapabilitySnapshot {
        self.audio.capabilities_for(owner)
    }

    fn local_sources(&self, owner: &MediaOwner) -> Vec<MediaSource> {
        self.local_sources_with_modes(owner, &self.connector_modes())
    }

    fn local_sources_with_modes(
        &self,
        owner: &MediaOwner,
        connector_modes: &[MediaSessionMode],
    ) -> Vec<MediaSource> {
        let audio = self.audio_caps(owner);
        let has_audio_session = connector_modes
            .iter()
            .any(|mode| matches!(mode, MediaSessionMode::Audio | MediaSessionMode::AudioVideo));
        let has_video_session = connector_modes
            .iter()
            .any(|mode| matches!(mode, MediaSessionMode::Video | MediaSessionMode::AudioVideo));
        let mut sources = Vec::new();
        let mut microphone_ops = Vec::new();
        let mut microphone_usages = Vec::new();
        if audio
            .supported_operations
            .contains(&AudioOperationKind::Capture)
        {
            microphone_ops.push(MediaOperationKind::Capture);
            microphone_usages.push(MediaSourceUsage::Capture);
        }
        if audio
            .supported_operations
            .contains(&AudioOperationKind::Record)
        {
            microphone_ops.push(MediaOperationKind::Record);
            microphone_usages.push(MediaSourceUsage::Record);
        }
        if audio
            .supported_operations
            .contains(&AudioOperationKind::Listen)
        {
            microphone_ops.push(MediaOperationKind::Transcribe);
            microphone_usages.push(MediaSourceUsage::Transcribe);
        }
        if has_audio_session {
            microphone_ops.push(MediaOperationKind::SessionStart);
            microphone_usages.push(MediaSourceUsage::Session);
        }
        if !microphone_ops.is_empty() {
            sources.push(MediaSource {
                reference: MediaSourceRef(MediaSourceId::from("media-runtime:microphone")),
                kind: MediaSourceKind::Microphone,
                media_kind: Some(MediaKind::Audio),
                label: "Microphone".to_owned(),
                usages: microphone_usages,
                supported_operations: microphone_ops,
                is_default: true,
            });
        }

        let mut speaker_ops = Vec::new();
        let mut speaker_usages = Vec::new();
        if audio
            .supported_operations
            .contains(&AudioOperationKind::Play)
        {
            speaker_ops.push(MediaOperationKind::Play);
            speaker_usages.push(MediaSourceUsage::Play);
        }
        if audio
            .supported_operations
            .contains(&AudioOperationKind::Speak)
        {
            speaker_ops.push(MediaOperationKind::Speak);
            speaker_usages.push(MediaSourceUsage::Speak);
        }
        if !speaker_ops.is_empty() {
            if has_audio_session {
                speaker_ops.push(MediaOperationKind::SessionStart);
                speaker_usages.push(MediaSourceUsage::Session);
            }
            sources.push(MediaSource {
                reference: MediaSourceRef(MediaSourceId::from("media-runtime:speaker")),
                kind: MediaSourceKind::Speaker,
                media_kind: Some(MediaKind::Audio),
                label: "Speaker".to_owned(),
                usages: speaker_usages,
                supported_operations: speaker_ops,
                is_default: true,
            });
        }

        if self.camera.is_some() {
            let mut camera_ops = vec![MediaOperationKind::Capture];
            let mut camera_usages = vec![MediaSourceUsage::Capture];
            if has_video_session {
                camera_ops.push(MediaOperationKind::SessionStart);
                camera_usages.push(MediaSourceUsage::Session);
            }
            sources.push(MediaSource {
                reference: MediaSourceRef(MediaSourceId::from("media-runtime:camera")),
                kind: MediaSourceKind::Camera,
                media_kind: Some(MediaKind::Image),
                label: "Camera".to_owned(),
                usages: camera_usages,
                supported_operations: camera_ops,
                is_default: true,
            });
            sources.push(MediaSource {
                reference: MediaSourceRef(MediaSourceId::from("media-runtime:library")),
                kind: MediaSourceKind::Library,
                media_kind: Some(MediaKind::Image),
                label: "Photo library".to_owned(),
                usages: vec![MediaSourceUsage::Capture, MediaSourceUsage::ReadImage],
                supported_operations: vec![MediaOperationKind::Capture],
                is_default: false,
            });
        }
        sources
    }

    async fn sources_for(
        &self,
        context: &MediaOperationContext,
    ) -> Result<Vec<MediaSource>, MediaError> {
        let (connector, modes) = self.connector_config();
        self.sources_for_config(context, connector, modes).await
    }

    async fn sources_for_config(
        &self,
        context: &MediaOperationContext,
        connector: Option<Arc<dyn MediaConnector>>,
        modes: Vec<MediaSessionMode>,
    ) -> Result<Vec<MediaSource>, MediaError> {
        let mut sources = self.local_sources_with_modes(&context.owner, &modes);
        if let Some(connector) = connector {
            let mut remote_sources = connector.sources(context.clone()).await?;
            if modes.is_empty() {
                remote_sources.clear();
            } else {
                remote_sources.retain_mut(|source| {
                    source
                        .supported_operations
                        .retain(|operation| *operation == MediaOperationKind::SessionStart);
                    source
                        .usages
                        .retain(|usage| *usage == MediaSourceUsage::Session);
                    source
                        .supported_operations
                        .contains(&MediaOperationKind::SessionStart)
                        && source.usages.contains(&MediaSourceUsage::Session)
                });
            }
            sources.extend(remote_sources);
        }
        if sources.len() > MAX_SOURCES {
            return Err(MediaError::new(
                MediaErrorKind::MediaTooLarge,
                "media connector returned too many sources",
            ));
        }
        let mut ids = std::collections::HashSet::new();
        for source in &sources {
            if source.reference.0 .0.len() > MAX_MEDIA_REFERENCE_CHARS || source.label.len() > 256 {
                return Err(MediaError::new(
                    MediaErrorKind::MediaTooLarge,
                    "media source metadata exceeds its bound",
                ));
            }
            if !ids.insert(source.reference.clone()) {
                return Err(MediaError::new(
                    MediaErrorKind::NativeFailure,
                    "media source references are not unique",
                ));
            }
        }
        Ok(sources)
    }

    fn ensure_source(
        sources: &[MediaSource],
        reference: &MediaSourceRef,
        operation: MediaOperationKind,
    ) -> Result<MediaSource, MediaError> {
        let source = sources
            .iter()
            .find(|source| source.reference == *reference)
            .cloned()
            .ok_or_else(|| {
                MediaError::new(MediaErrorKind::NotFound, "media source was not found")
            })?;
        if !source.supported_operations.contains(&operation) {
            return Err(MediaError::new(
                MediaErrorKind::Unsupported,
                "media source does not support this operation",
            ));
        }
        Ok(source)
    }

    fn capabilities_for_owner(&self, owner: &MediaOwner) -> MediaCapabilities {
        self.project_capabilities(owner, self.audio_caps(owner))
    }

    fn capabilities_catalog(&self) -> MediaCapabilities {
        // `AudioService::capabilities` is the structural configured-route union.
        // Owner-specific readiness/support is queried only through capabilities_for.
        self.project_capabilities(
            &MediaOwner::System {
                instance_id: "media-catalog".to_owned(),
            },
            self.audio.capabilities(),
        )
    }

    fn project_capabilities(
        &self,
        owner: &MediaOwner,
        audio: AudioCapabilitySnapshot,
    ) -> MediaCapabilities {
        let (connector, connector_modes) = self.connector_config();
        let mut operations = vec![MediaOperationKind::Sources, MediaOperationKind::Status];
        let has_audio = !audio.supported_operations.is_empty();
        if audio
            .supported_operations
            .contains(&AudioOperationKind::Capture)
            || self.camera.is_some()
        {
            operations.push(MediaOperationKind::Capture);
        }
        if audio
            .supported_operations
            .contains(&AudioOperationKind::Record)
        {
            operations.push(MediaOperationKind::Record);
        }
        if audio
            .supported_operations
            .contains(&AudioOperationKind::Listen)
            || audio
                .supported_operations
                .contains(&AudioOperationKind::Transcribe)
        {
            operations.push(MediaOperationKind::Transcribe);
        }
        if audio
            .supported_operations
            .contains(&AudioOperationKind::Synthesize)
        {
            operations.push(MediaOperationKind::Synthesize);
        }
        if audio
            .supported_operations
            .contains(&AudioOperationKind::Play)
        {
            operations.push(MediaOperationKind::Play);
        }
        if audio
            .supported_operations
            .contains(&AudioOperationKind::Speak)
        {
            operations.push(MediaOperationKind::Speak);
        }
        if connector.is_some() && !connector_modes.is_empty() {
            operations.extend([
                MediaOperationKind::SessionStart,
                MediaOperationKind::SessionUpdate,
            ]);
        }
        // These lifecycle methods are implemented for every resource the service creates.
        operations.extend([
            MediaOperationKind::Observe,
            MediaOperationKind::Wait,
            MediaOperationKind::Stop,
        ]);
        operations.sort_unstable();
        operations.dedup();

        let mut kinds = Vec::new();
        if has_audio
            || connector_modes
                .iter()
                .any(|mode| matches!(mode, MediaSessionMode::Audio | MediaSessionMode::AudioVideo))
        {
            kinds.push(MediaKind::Audio);
        }
        if self.camera.is_some() {
            kinds.push(MediaKind::Image);
        }
        if connector_modes
            .iter()
            .any(|mode| matches!(mode, MediaSessionMode::Video | MediaSessionMode::AudioVideo))
        {
            kinds.push(MediaKind::Video);
        }
        kinds.sort_unstable();
        kinds.dedup();

        let mut readiness = Vec::new();
        for kind in [
            AudioOperationKind::Capture,
            AudioOperationKind::Record,
            AudioOperationKind::Synthesize,
            AudioOperationKind::Play,
            AudioOperationKind::Speak,
        ] {
            if !audio.supported_operations.contains(&kind) {
                continue;
            }
            if kind == AudioOperationKind::Capture && self.camera.is_some() {
                continue;
            }
            let media_kind = media_operation_for_audio(kind);
            let Some(media_kind) = media_kind else {
                continue;
            };
            if !operations.contains(&media_kind) {
                continue;
            }
            if let Some(state) = audio
                .readiness
                .iter()
                .find(|entry| entry.operation == kind)
                .map(|entry| map_audio_readiness(entry.state))
            {
                readiness.push(MediaOperationReadiness {
                    operation: media_kind,
                    state,
                });
            }
        }
        if let Some(state) = transcription_readiness(&audio) {
            readiness.push(MediaOperationReadiness {
                operation: MediaOperationKind::Transcribe,
                state,
            });
        }
        readiness.sort_by_key(|entry| entry.operation);
        readiness.dedup_by_key(|entry| entry.operation);

        let mut revisions = self.support_revision.lock().unwrap();
        let revision = revisions
            .entry(owner.clone())
            .or_insert_with(|| (operations.clone(), kinds.clone(), 1));
        if revision.0 != operations || revision.1 != kinds {
            revision.0 = operations.clone();
            revision.1 = kinds.clone();
            revision.2 = revision.2.saturating_add(1);
        }
        MediaCapabilities {
            service_epoch: self.service_epoch,
            support_revision: revision.2,
            supported_operations: operations,
            supported_media_kinds: kinds,
            readiness,
            max_payload_bytes: self.max_payload_bytes,
        }
    }

    async fn execute_inner(
        &self,
        context: MediaOperationContext,
        operation: MediaOperation,
    ) -> Result<MediaResult, MediaError> {
        match operation {
            MediaOperation::Sources { kind, usage } => {
                let mut sources = self.sources_for(&context).await?;
                sources.retain(|source| {
                    source_matches_kind(source, kind, usage)
                        && usage.is_none_or(|usage| source.usages.contains(&usage))
                });
                Ok(MediaResult::Sources { sources })
            }
            MediaOperation::Status { target } => self.status(&context, target).await,
            MediaOperation::Capture { source, options } => {
                self.capture(context, source, options).await
            }
            MediaOperation::Record { source, options } => {
                self.record(context, source, options).await
            }
            MediaOperation::Transcribe { input, language } => {
                self.transcribe(context, input, language).await
            }
            MediaOperation::Synthesize {
                text,
                language,
                rate,
                voice,
            } => self.synthesize(context, text, language, rate, voice).await,
            MediaOperation::Play { media, source } => self.play(context, media, source).await,
            MediaOperation::Speak {
                text,
                language,
                rate,
                voice,
            } => self.speak(context, text, language, rate, voice).await,
            MediaOperation::SessionStart { request } => self.start_session(context, request).await,
            MediaOperation::SessionUpdate { session, update } => {
                self.update_session(context, session, update).await
            }
            MediaOperation::Observe {
                target,
                after,
                limit,
            } => self.observe(context, target, after, limit, None).await,
            MediaOperation::Wait {
                target,
                after,
                timeout_ms,
                limit,
            } => {
                self.observe(context, target, after, limit, Some(timeout_ms))
                    .await
            }
            MediaOperation::Stop { target } => self.stop(context, target).await,
        }
    }

    fn begin_active_cancel(
        &self,
        identity: &MediaOperationId,
        owner: &MediaOwner,
        expected_incarnation: Option<MediaOperationIncarnation>,
    ) -> Option<(
        CancellationToken,
        Arc<Notify>,
        bool,
        MediaOperationIncarnation,
        MediaCancelFlight,
    )> {
        let shared_inner = self.inner.clone();
        let mut inner = shared_inner.lock().unwrap();
        let active = inner.active.get_mut(identity)?;
        if active.owner != *owner {
            return None;
        }
        if expected_incarnation.is_some_and(|expected| active.incarnation != expected) {
            return None;
        }
        active.cancel_flights = active.cancel_flights.saturating_add(1);
        let incarnation = active.incarnation;
        Some((
            active.token.clone(),
            active.done.clone(),
            active.finished,
            incarnation,
            MediaCancelFlight {
                inner: shared_inner.clone(),
                identity: identity.clone(),
                incarnation,
                settled: false,
            },
        ))
    }

    fn begin_active(
        &self,
        context: &mut MediaOperationContext,
    ) -> Result<CancellationToken, MediaError> {
        if context.identity.service_epoch != self.service_epoch {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "stale media service epoch",
            ));
        }
        if context.timeout_budget_ms == Some(0) {
            return Err(MediaError::new(
                MediaErrorKind::Timeout,
                "media deadline has elapsed",
            ));
        }
        let token = CancellationToken::new();
        let mut inner = self.inner.lock().unwrap();
        if inner.active.contains_key(&context.identity) {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "duplicate active media identity",
            ));
        }
        if inner.session_reservations.get(&context.identity)
            == Some(&SessionReservationKind::Attach)
        {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "media session operation identity is already in use",
            ));
        }
        let incarnation = context.operation_incarnation();
        if !context.claim_operation_admission() {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "media operation context has already been admitted",
            ));
        }
        *context = context.clone().with_operation_incarnation(incarnation);
        inner.active.insert(
            context.identity.clone(),
            ActiveOperation {
                owner: context.owner.clone(),
                token: token.clone(),
                done: Arc::new(Notify::new()),
                incarnation,
                cancel_flights: 0,
                finished: false,
                cancel_unknown: false,
            },
        );
        Ok(token)
    }

    async fn run_operation(
        &self,
        mut context: MediaOperationContext,
        operation: MediaOperation,
    ) -> Result<MediaResult, MediaError> {
        context.max_payload_bytes = context.max_payload_bytes.min(self.max_payload_bytes);
        context
            .timeout_budget_ms
            .get_or_insert(DEFAULT_OPERATION_TIMEOUT_MS);
        let token = self.begin_active(&mut context)?;
        let mut guard = ActiveGuard {
            service: self,
            identity: context.identity.clone(),
            incarnation: context.operation_incarnation(),
            armed: true,
        };
        let started = now_ms();
        self.set_operation_status(
            context.owner.clone(),
            context.identity.clone(),
            context.operation_incarnation(),
            MediaResourceState::Active,
            started,
        );
        self.push_state_event(
            &context,
            MediaResourceRef::Operation {
                identity: context.identity.clone(),
                incarnation: context.operation_incarnation(),
            },
            MediaResourceState::Active,
        );
        let operation_future = self.execute_inner(context.clone(), operation);
        let result = match context.timeout_budget_ms {
            Some(budget) => tokio::select! {
                biased;
                _ = token.cancelled() => Err(MediaError::new(MediaErrorKind::Cancelled, "media operation cancelled")),
                _ = tokio::time::sleep(Duration::from_millis(budget)) => Err(MediaError::new(MediaErrorKind::Timeout, "media deadline has elapsed")),
                result = operation_future => result,
            },
            None => tokio::select! {
                biased;
                _ = token.cancelled() => Err(MediaError::new(MediaErrorKind::Cancelled, "media operation cancelled")),
                result = operation_future => result,
            },
        };
        guard.armed = false;
        if matches!(
            result,
            Err(MediaError {
                kind: MediaErrorKind::Cancelled | MediaErrorKind::Timeout,
                ..
            })
        ) {
            if let Some((_, _, _, incarnation, mut cancel_flight)) = self.begin_active_cancel(
                &context.identity,
                &context.owner,
                Some(context.operation_incarnation()),
            ) {
                let confirmed = self
                    .audio_router
                    .cancel_for_incarnation(&context.identity, &context.owner, &incarnation)
                    .await
                    .is_ok();
                cancel_flight.finish(confirmed);
            }
        }
        let state = if result.is_ok() {
            MediaResourceState::Completed
        } else {
            MediaResourceState::Failed
        };
        self.set_operation_status(
            context.owner.clone(),
            context.identity.clone(),
            context.operation_incarnation(),
            state,
            now_ms(),
        );
        self.push_state_event(
            &context,
            MediaResourceRef::Operation {
                identity: context.identity.clone(),
                incarnation: context.operation_incarnation(),
            },
            state,
        );
        result
    }

    async fn run_read(
        &self,
        mut context: MediaOperationContext,
        media: MediaRef,
    ) -> Result<media_api::MediaContent, MediaError> {
        context.max_payload_bytes = context.max_payload_bytes.min(self.max_payload_bytes);
        context
            .timeout_budget_ms
            .get_or_insert(DEFAULT_OPERATION_TIMEOUT_MS);
        let token = self.begin_active(&mut context)?;
        let mut guard = ActiveGuard {
            service: self,
            identity: context.identity.clone(),
            incarnation: context.operation_incarnation(),
            armed: true,
        };
        let read = self.read_payload(&context, &media);
        let result = match context.timeout_budget_ms {
            Some(budget) => tokio::select! {
                biased;
                _ = token.cancelled() => Err(MediaError::new(MediaErrorKind::Cancelled, "media read cancelled")),
                _ = tokio::time::sleep(Duration::from_millis(budget)) => Err(MediaError::new(MediaErrorKind::Timeout, "media deadline has elapsed")),
                result = read => result.map(|payload| {
                    media_api::MediaContent {
                        reference: media.clone(),
                        metadata: payload.metadata,
                        bytes: payload.bytes,
                    }
                }),
            },
            None => tokio::select! {
                biased;
                _ = token.cancelled() => Err(MediaError::new(MediaErrorKind::Cancelled, "media read cancelled")),
                result = read => result.map(|payload| {
                    media_api::MediaContent {
                        reference: media.clone(),
                        metadata: payload.metadata,
                        bytes: payload.bytes,
                    }
                }),
            },
        };
        guard.armed = false;
        if matches!(
            result,
            Err(MediaError {
                kind: MediaErrorKind::Cancelled | MediaErrorKind::Timeout,
                ..
            })
        ) {
            if let Some((_, _, _, incarnation, mut cancel_flight)) = self.begin_active_cancel(
                &context.identity,
                &context.owner,
                Some(context.operation_incarnation()),
            ) {
                let confirmed = self
                    .audio_router
                    .cancel_for_incarnation(&context.identity, &context.owner, &incarnation)
                    .await
                    .is_ok();
                cancel_flight.finish(confirmed);
            }
        }
        result
    }

    fn set_operation_status(
        &self,
        owner: MediaOwner,
        identity: MediaOperationId,
        incarnation: MediaOperationIncarnation,
        state: MediaResourceState,
        timestamp: u64,
    ) {
        let mut inner = self.inner.lock().unwrap();
        let is_new = !inner.operation_status.contains_key(&identity);
        let status = inner
            .operation_status
            .entry(identity.clone())
            .or_insert_with(|| OwnedOperationStatus {
                owner: owner.clone(),
                incarnation,
                state,
                started_at_unix_ms: timestamp,
                updated_at_unix_ms: timestamp,
            });
        status.state = state;
        status.owner = owner;
        status.incarnation = incarnation;
        status.updated_at_unix_ms = timestamp;
        if is_new {
            inner.operation_order.push_back(identity);
            while inner.operation_order.len() > MAX_OPERATION_HISTORY {
                if let Some(expired) = inner.operation_order.pop_front() {
                    inner.operation_status.remove(&expired);
                }
            }
        }
    }

    fn push_state_event(
        &self,
        context: &MediaOperationContext,
        target: MediaResourceRef,
        state: MediaResourceState,
    ) {
        let now = now_ms();
        let mut inner = self.inner.lock().unwrap();
        let buffer = get_event_buffer(&mut inner, target.clone(), context.owner.clone(), now);
        buffer.push_state(target, state, Some(context.identity.clone()), now);
    }

    fn push_resource_event(
        &self,
        context: &MediaOperationContext,
        target: MediaResourceRef,
        state: MediaResourceState,
    ) {
        self.push_state_event(context, target, state);
    }

    fn remember_asset(
        &self,
        context: &MediaOperationContext,
        asset: &MediaAsset,
    ) -> Result<(), MediaError> {
        validate_asset_metadata(asset, context.max_payload_bytes.min(self.max_payload_bytes))?;
        let mut inner = self.inner.lock().unwrap();
        if let Some(existing) = inner.known_assets.get(&asset.reference) {
            if existing.owner != context.owner || existing.metadata != asset.metadata {
                return Err(MediaError::new(
                    MediaErrorKind::StoreFailure,
                    "media store returned a conflicting asset reference",
                ));
            }
        } else {
            inner.known_assets.insert(
                asset.reference.clone(),
                OwnedAsset {
                    owner: context.owner.clone(),
                    metadata: asset.metadata.clone(),
                    created_at_unix_ms: now_ms(),
                },
            );
            inner.known_asset_order.push_back(asset.reference.clone());
            while inner.known_asset_order.len() > 1024 {
                if let Some(expired) = inner.known_asset_order.pop_front() {
                    inner.known_assets.remove(&expired);
                }
            }
        }
        Ok(())
    }

    async fn store_payload(
        &self,
        context: &MediaOperationContext,
        payload: MediaPayload,
    ) -> Result<MediaAsset, MediaError> {
        let max_bytes = context.max_payload_bytes.min(self.max_payload_bytes);
        payload.validate(max_bytes)?;
        let asset = self.resources.put(context, payload.clone()).await?;
        if asset.metadata != payload.metadata {
            let foreign_reference = self
                .inner
                .lock()
                .unwrap()
                .known_assets
                .get(&asset.reference)
                .is_some_and(|existing| existing.owner != context.owner);
            if !foreign_reference {
                let _ = self.resources.delete(context, &asset.reference).await;
            }
            return Err(MediaError::new(
                MediaErrorKind::StoreFailure,
                "media store returned mismatched asset metadata",
            ));
        }
        self.remember_asset(context, &asset)?;
        Ok(asset)
    }

    async fn capture(
        &self,
        context: MediaOperationContext,
        source: MediaSourceRef,
        options: CaptureOptions,
    ) -> Result<MediaResult, MediaError> {
        validate_capture_options(&options)?;
        let sources = self.sources_for(&context).await?;
        let source = Self::ensure_source(&sources, &source, MediaOperationKind::Capture)?;
        let payload = match source.kind {
            MediaSourceKind::Microphone => {
                if options.max_duration_ms.is_some() {
                    return Err(unsupported(
                        "native one-shot audio capture does not expose a duration limit",
                    ));
                }
                if options.max_dimension_px.is_some() || options.jpeg_quality.is_some() {
                    return Err(MediaError::new(
                        MediaErrorKind::InvalidRequest,
                        "image sizing options cannot be used for audio capture",
                    ));
                }
                let result = self
                    .audio
                    .execute(
                        context.clone(),
                        AudioOperation::Capture {
                            sample_rate_hz: 24_000,
                            format: "wav".to_owned(),
                        },
                    )
                    .await
                    .map_err(media_error_from_audio)?;
                let AudioOperationSuccess::Recording { recording } = result else {
                    return Err(native_failure(
                        "audio capture returned an unexpected result",
                    ));
                };
                voice_payload(
                    recording,
                    None,
                    context.max_payload_bytes.min(self.max_payload_bytes),
                )?
            }
            MediaSourceKind::Camera | MediaSourceKind::Library => {
                let camera = self
                    .camera
                    .as_ref()
                    .ok_or_else(|| unsupported("camera capture is unavailable"))?;
                if options.max_duration_ms.is_some() {
                    return Err(MediaError::new(
                        MediaErrorKind::InvalidRequest,
                        "still-image capture does not use a duration limit",
                    ));
                }
                if options.jpeg_quality.is_some() && options.max_dimension_px.is_none() {
                    return Err(unsupported(
                        "JPEG quality requires a requested maximum image dimension",
                    ));
                }
                let image = match (source.kind, options.max_dimension_px) {
                    (MediaSourceKind::Camera, Some(max_dimension)) => camera
                        .capture_photo_sized(
                            CapturePhotoOpts {
                                position: CameraPosition::Back,
                                allow_editing: false,
                            },
                            max_dimension,
                            options.jpeg_quality.unwrap_or(0.85),
                        )
                        .await
                        .map_err(media_error_from_camera)?,
                    (MediaSourceKind::Library, Some(max_dimension)) => camera
                        .pick_from_library_sized(
                            max_dimension,
                            options.jpeg_quality.unwrap_or(0.85),
                        )
                        .await
                        .map_err(media_error_from_camera)?,
                    (MediaSourceKind::Camera, None) => camera
                        .capture_photo(CapturePhotoOpts {
                            position: CameraPosition::Back,
                            allow_editing: false,
                        })
                        .await
                        .map_err(media_error_from_camera)?,
                    (MediaSourceKind::Library, None) => camera
                        .pick_from_library()
                        .await
                        .map_err(media_error_from_camera)?,
                    _ => unreachable!(),
                };
                let actual = u64::try_from(image.jpeg_bytes.len()).unwrap_or(u64::MAX);
                if actual > context.max_payload_bytes.min(self.max_payload_bytes) {
                    return Err(MediaError::new(
                        MediaErrorKind::MediaTooLarge,
                        "captured image exceeds the payload bound",
                    ));
                }
                if options.max_dimension_px.is_some()
                    && image.width.max(image.height) > options.max_dimension_px.unwrap_or_default()
                {
                    return Err(unsupported(
                        "native camera did not honor the requested image size",
                    ));
                }
                MediaPayload {
                    metadata: MediaMetadata {
                        kind: MediaKind::Image,
                        mime_type: "image/jpeg".to_owned(),
                        byte_len: actual,
                        duration_ms: None,
                        width_px: Some(image.width),
                        height_px: Some(image.height),
                        sample_rate_hz: None,
                        channels: None,
                        captured_at_unix_ms: Some(now_ms()),
                    },
                    bytes: image.jpeg_bytes,
                }
            }
            _ => return Err(unsupported("this source does not provide one-shot capture")),
        };
        let asset = self.store_payload(&context, payload).await?;
        self.push_resource_event(
            &context,
            MediaResourceRef::Media(asset.reference.clone()),
            MediaResourceState::Completed,
        );
        Ok(MediaResult::Captured { asset })
    }

    async fn record(
        &self,
        context: MediaOperationContext,
        source: MediaSourceRef,
        options: RecordOptions,
    ) -> Result<MediaResult, MediaError> {
        if options.max_duration_ms == Some(0)
            || options
                .max_duration_ms
                .is_some_and(|value| value > MAX_AUTO_RECORDING_MS)
        {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "recording duration is outside the supported bound",
            ));
        }
        let sources = self.sources_for(&context).await?;
        let source = Self::ensure_source(&sources, &source, MediaOperationKind::Record)?;
        if source.kind != MediaSourceKind::Microphone {
            return Err(unsupported(
                "continuous recording is currently supported for audio sources only",
            ));
        }
        let format = audio_format(options.mime_type.as_deref())?;
        let mut reservation = self.reserve_recording_slot()?;
        let result = self
            .audio
            .execute(
                context.clone(),
                AudioOperation::StartRecording {
                    sample_rate_hz: 24_000,
                    format,
                },
            )
            .await
            .map_err(media_error_from_audio)?;
        let AudioOperationSuccess::RecordingStarted { handle } = result else {
            return Err(native_failure("audio service did not start a recording"));
        };
        let recording = handle;
        if recording.0 .0.len() > MAX_MEDIA_REFERENCE_CHARS {
            let _ = self
                .audio
                .execute(
                    context.clone(),
                    AudioOperation::StopRecording {
                        handle: recording.clone(),
                    },
                )
                .await;
            return Err(MediaError::new(
                MediaErrorKind::MediaTooLarge,
                "recording reference exceeds its bound",
            ));
        }
        let started = now_ms();
        let timer = options.max_duration_ms.map(|_| CancellationToken::new());
        let duplicate = {
            let mut inner = self.inner.lock().unwrap();
            if inner.recordings.contains_key(&recording) {
                true
            } else {
                reservation.commit_locked(&mut inner);
                inner.recordings.insert(
                    recording.clone(),
                    OwnedRecording {
                        owner: context.owner.clone(),
                        started_at_unix_ms: started,
                        timer: timer.clone(),
                        finalize_in_progress: false,
                        lifecycle: RecordingLifecycle::Active,
                    },
                );
                inner.recording_order.push_back(recording.clone());
                prune_recording_history(&mut inner);
                false
            }
        };
        if duplicate {
            if let Some(timer) = &timer {
                timer.cancel();
            }
            let _ = self
                .audio
                .execute(
                    context.clone(),
                    AudioOperation::StopRecording {
                        handle: recording.clone(),
                    },
                )
                .await;
            return Err(MediaError::new(
                MediaErrorKind::NativeFailure,
                "audio service reused an active recording reference",
            ));
        }
        self.push_resource_event(
            &context,
            MediaResourceRef::Recording(recording.clone()),
            MediaResourceState::Active,
        );
        if let (Some(max_duration), Some(timer)) = (options.max_duration_ms, timer) {
            let audio = self.audio.clone();
            let resources = self.resources.clone();
            let inner = self.inner.clone();
            let mut timer_context = MediaOperationContext::new(
                MediaOperationId::new(
                    context.identity.generation.saturating_add(1),
                    self.service_epoch,
                ),
                context.owner.clone(),
            )
            .with_max_payload_bytes(context.max_payload_bytes.min(self.max_payload_bytes));
            timer_context.initiator = None;
            timer_context.timeout_budget_ms = Some(DEFAULT_OPERATION_TIMEOUT_MS);
            let timer_recording = recording.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = timer.cancelled() => {}
                    _ = tokio::time::sleep(Duration::from_millis(max_duration)) => {
                        let _ = tokio::time::timeout(
                            Duration::from_millis(DEFAULT_OPERATION_TIMEOUT_MS),
                            stop_recording_shared(&inner, &audio, &resources, timer_context, timer_recording),
                        ).await;
                    }
                }
            });
        }
        Ok(MediaResult::RecordingStarted { recording })
    }

    async fn transcribe(
        &self,
        context: MediaOperationContext,
        input: TranscriptionInput,
        language: Option<String>,
    ) -> Result<MediaResult, MediaError> {
        let result = match input {
            TranscriptionInput::LiveSource(source) => {
                let sources = self.sources_for(&context).await?;
                let source =
                    Self::ensure_source(&sources, &source, MediaOperationKind::Transcribe)?;
                if source.kind != MediaSourceKind::Microphone {
                    return Err(unsupported(
                        "live transcription requires a microphone source",
                    ));
                }
                self.audio
                    .execute(context, AudioOperation::Listen { language })
                    .await
                    .map_err(media_error_from_audio)?
            }
            TranscriptionInput::StoredMedia(media) => {
                let payload = self.read_payload(&context, &media).await?;
                if payload.metadata.kind != MediaKind::Audio {
                    return Err(unsupported("transcription requires audio media"));
                }
                let recording = VoiceRecording {
                    audio_bytes: payload.bytes,
                    mime_type: payload.metadata.mime_type,
                };
                self.audio
                    .execute(
                        context,
                        AudioOperation::Transcribe {
                            recording,
                            language,
                        },
                    )
                    .await
                    .map_err(media_error_from_audio)?
            }
        };
        let AudioOperationSuccess::Transcript { transcript } = result else {
            return Err(native_failure(
                "audio service returned a different transcription result",
            ));
        };
        Ok(MediaResult::Transcript { transcript })
    }

    async fn synthesize(
        &self,
        context: MediaOperationContext,
        text: String,
        language: Option<String>,
        rate: Option<f32>,
        voice: Option<String>,
    ) -> Result<MediaResult, MediaError> {
        let result = self
            .audio
            .execute(
                context.clone(),
                AudioOperation::Synthesize {
                    text,
                    language,
                    rate,
                    voice,
                },
            )
            .await
            .map_err(media_error_from_audio)?;
        let AudioOperationSuccess::Synthesized { audio } = result else {
            return Err(native_failure(
                "audio service returned a different synthesis result",
            ));
        };
        let asset = self.store_payload(&context, pcm_payload(audio)?).await?;
        Ok(MediaResult::Synthesized { asset })
    }

    async fn play(
        &self,
        context: MediaOperationContext,
        media: MediaRef,
        source: Option<MediaSourceRef>,
    ) -> Result<MediaResult, MediaError> {
        if let Some(source) = source {
            let sources = self.sources_for(&context).await?;
            let output = Self::ensure_source(&sources, &source, MediaOperationKind::Play)?;
            if output.kind != MediaSourceKind::Speaker {
                return Err(unsupported("playback source is not an audio output"));
            }
        }
        let payload = self.read_payload(&context, &media).await?;
        let audio = pcm_from_payload(payload)?;
        let result = self
            .audio
            .execute(context, AudioOperation::Play { audio })
            .await
            .map_err(media_error_from_audio)?;
        let AudioOperationSuccess::PlaybackCompleted { duration_ms } = result else {
            return Err(native_failure(
                "audio service did not confirm playback completion",
            ));
        };
        Ok(MediaResult::PlaybackCompleted { duration_ms })
    }

    async fn speak(
        &self,
        context: MediaOperationContext,
        text: String,
        language: Option<String>,
        rate: Option<f32>,
        voice: Option<String>,
    ) -> Result<MediaResult, MediaError> {
        let result = self
            .audio
            .execute(
                context,
                AudioOperation::Speak {
                    text,
                    language,
                    rate,
                    voice,
                },
            )
            .await
            .map_err(media_error_from_audio)?;
        let AudioOperationSuccess::PlaybackCompleted { duration_ms } = result else {
            return Err(native_failure(
                "audio service did not confirm speech playback completion",
            ));
        };
        Ok(MediaResult::SpeechCompleted { duration_ms })
    }

    async fn start_session(
        &self,
        context: MediaOperationContext,
        request: MediaSessionRequest,
    ) -> Result<MediaResult, MediaError> {
        // A native connector may return an existing physical lease for the
        // same identity. Reject the replay before querying or opening it, so
        // duplicate-reference cleanup cannot stop the registered session.
        let duplicate_identity = {
            let inner = self.inner.lock().unwrap();
            inner.session_reservations.contains_key(&context.identity)
                || inner
                    .sessions
                    .values()
                    .any(|session| session.origin_operation_id == context.identity)
        };
        if duplicate_identity {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "media session already registered for this operation identity",
            ));
        }
        let (connector, modes) = self.connector_config();
        let connector = connector.ok_or_else(|| unsupported("no media connector is configured"))?;
        if !modes.contains(&request.mode) {
            return Err(unsupported(
                "configured media connector does not support this session mode",
            ));
        }
        let sources = self
            .sources_for_config(&context, Some(connector.clone()), modes.clone())
            .await?;
        if let Some(source) = &request.audio_source {
            let source = Self::ensure_source(&sources, source, MediaOperationKind::SessionStart)?;
            if !matches!(
                source.kind,
                MediaSourceKind::Microphone | MediaSourceKind::Remote
            ) {
                return Err(MediaError::new(
                    MediaErrorKind::InvalidRequest,
                    "session audio source is not an input source",
                ));
            }
            if source
                .media_kind
                .is_some_and(|kind| kind != MediaKind::Audio)
                || (source.kind == MediaSourceKind::Remote && source.media_kind.is_none())
            {
                return Err(MediaError::new(
                    MediaErrorKind::InvalidRequest,
                    "session audio source has no audio modality",
                ));
            }
        }
        if let Some(source) = &request.video_source {
            let source = Self::ensure_source(&sources, source, MediaOperationKind::SessionStart)?;
            if !matches!(
                source.kind,
                MediaSourceKind::Camera | MediaSourceKind::Screen | MediaSourceKind::Remote
            ) {
                return Err(MediaError::new(
                    MediaErrorKind::InvalidRequest,
                    "session video source is not a visual input source",
                ));
            }
            if source
                .media_kind
                .is_some_and(|kind| !matches!(kind, MediaKind::Image | MediaKind::Video))
                || (source.kind == MediaSourceKind::Remote && source.media_kind.is_none())
            {
                return Err(MediaError::new(
                    MediaErrorKind::InvalidRequest,
                    "session video source has no visual modality",
                ));
            }
        }
        let intent = request.intent;
        let selected_sources = selected_session_sources(&request);
        let audio_sources = selected_sources
            .get(&MediaKind::Audio)
            .cloned()
            .into_iter()
            .collect();
        let audio_sources = Arc::new(RwLock::new(audio_sources));
        let reservation =
            self.reserve_session_slot(&context.identity, SessionReservationKind::Start)?;
        let session: Arc<dyn MediaSession> =
            Arc::from(connector.start_session(context.clone(), request).await?);
        let mut cleanup = UnregisteredSessionLease::new(session.clone(), context.clone());
        cleanup.hold_reservation(reservation);
        let declared_sources = match declared_session_sources(session.as_ref()) {
            Ok(sources) => sources,
            Err(error) => {
                cleanup.stop().await;
                return Err(error);
            }
        };
        if let Err(error) = session_authorized_sources(&selected_sources, &declared_sources) {
            cleanup.stop().await;
            return Err(error);
        }
        let reference = session.reference().clone();
        if let Err(error) = validate_session_reference(&reference) {
            cleanup.stop().await;
            return Err(error);
        }
        let status = session.status();
        if matches!(
            status.state,
            MediaSessionState::Ended | MediaSessionState::Failed
        ) {
            cleanup.stop().await;
            return Err(MediaError::new(
                MediaErrorKind::NativeFailure,
                "media connector returned a terminal session",
            ));
        }
        let duplicate = {
            let mut inner = self.inner.lock().unwrap();
            if inner.sessions.contains_key(&reference) {
                true
            } else {
                cleanup.commit_locked(&mut inner);
                inner.sessions.insert(
                    reference.clone(),
                    OwnedSession {
                        owner: context.owner.clone(),
                        origin_operation_id: context.identity.clone(),
                        session: session.clone(),
                        audio_channel: Arc::new(AsyncMutex::new(None)),
                        intent: Some(intent),
                        declared_sources,
                        selected_sources,
                        audio_sources,
                        started_at_unix_ms: now_ms(),
                    },
                );
                false
            }
        };
        if duplicate {
            cleanup.stop().await;
            return Err(MediaError::new(
                MediaErrorKind::NativeFailure,
                "media connector reused an active session reference",
            ));
        }
        self.push_resource_event(
            &context,
            MediaResourceRef::Session(reference.clone()),
            MediaResourceState::Active,
        );
        let status = self.finish_session_registration(&context, &reference, &session)?;
        Ok(MediaResult::SessionStarted {
            session: reference,
            status,
        })
    }

    async fn update_session(
        &self,
        context: MediaOperationContext,
        reference: MediaSessionRef,
        update: media_api::MediaSessionUpdate,
    ) -> Result<MediaResult, MediaError> {
        let session = self.owned_session(&context.owner, &reference)?;
        let source_change = match &update {
            media_api::MediaSessionUpdate::SelectSource { kind, source } => {
                let sources = self.sources_for(&context).await?;
                let selected =
                    Self::ensure_source(&sources, source, MediaOperationKind::SessionStart)?;
                let compatible = match kind {
                    MediaKind::Audio => {
                        matches!(
                            selected.kind,
                            MediaSourceKind::Microphone | MediaSourceKind::Remote
                        ) && selected.media_kind == Some(MediaKind::Audio)
                    }
                    MediaKind::Image => {
                        matches!(
                            selected.kind,
                            MediaSourceKind::Camera
                                | MediaSourceKind::Library
                                | MediaSourceKind::Remote
                        ) && selected.media_kind == Some(MediaKind::Image)
                    }
                    MediaKind::Video => {
                        matches!(
                            selected.kind,
                            MediaSourceKind::Camera
                                | MediaSourceKind::Screen
                                | MediaSourceKind::Remote
                        ) && matches!(
                            selected.media_kind,
                            Some(MediaKind::Image | MediaKind::Video)
                        )
                    }
                };
                if !compatible {
                    return Err(MediaError::new(
                        MediaErrorKind::InvalidRequest,
                        "session source does not match the requested media kind",
                    ));
                }
                let (declared, mut selected_sources) = {
                    let inner = self.inner.lock().unwrap();
                    let owned = inner
                        .sessions
                        .get(&reference)
                        .filter(|owned| owned.owner == context.owner)
                        .ok_or_else(|| not_found("media session was not found"))?;
                    (
                        owned.declared_sources.clone(),
                        owned.selected_sources.clone(),
                    )
                };
                selected_sources.insert(*kind, source.clone());
                session_authorized_sources(&selected_sources, &declared)?;
                Some((*kind, source.clone()))
            }
            _ => None,
        };
        if let media_api::MediaSessionUpdate::SetPlaybackGain { gain } = &update {
            if !gain.is_finite() || !(0.0..=1.0).contains(gain) {
                return Err(MediaError::new(
                    MediaErrorKind::InvalidRequest,
                    "playback gain must be in [0, 1]",
                ));
            }
        }
        let status = session.update(context.clone(), update).await?;
        if let Some((kind, source)) = source_change {
            let mut inner = self.inner.lock().unwrap();
            let owned = inner
                .sessions
                .get_mut(&reference)
                .filter(|owned| owned.owner == context.owner)
                .ok_or_else(|| not_found("media session was not found"))?;
            owned.selected_sources.insert(kind, source);
            if kind == MediaKind::Audio {
                let selected_audio = owned.selected_sources[&MediaKind::Audio].clone();
                *owned.audio_sources.write().unwrap() = HashSet::from([selected_audio]);
            }
        }
        Ok(MediaResult::SessionUpdated {
            session: reference,
            status,
        })
    }

    async fn session_audio(
        &self,
        mut context: MediaOperationContext,
        reference: MediaSessionRef,
    ) -> Result<Arc<dyn MediaAudioChannel>, MediaError> {
        context
            .timeout_budget_ms
            .get_or_insert(DEFAULT_OPERATION_TIMEOUT_MS);
        if context.identity.service_epoch != self.service_epoch {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "stale media service epoch",
            ));
        }
        if context.timeout_budget_ms == Some(0) {
            return Err(MediaError::new(
                MediaErrorKind::Timeout,
                "media deadline has elapsed",
            ));
        }
        let (session, intent, channel_slot, audio_sources) = {
            let inner = self.inner.lock().unwrap();
            let entry = inner
                .sessions
                .get(&reference)
                .filter(|session| session.owner == context.owner)
                .ok_or_else(|| not_found("media session was not found"))?;
            (
                entry.session.clone(),
                entry.intent,
                entry.audio_channel.clone(),
                entry.audio_sources.clone(),
            )
        };
        if intent == Some(MediaSessionIntent::Observation) {
            return Err(unsupported(
                "observation sessions do not expose a conversation audio channel",
            ));
        }
        Self::claim_context_admission(&mut context)?;
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(
                context
                    .timeout_budget_ms
                    .unwrap_or(DEFAULT_OPERATION_TIMEOUT_MS),
            );
        let mut slot = tokio::time::timeout_at(deadline, channel_slot.lock())
            .await
            .map_err(|_| MediaError::new(MediaErrorKind::Timeout, "media deadline has elapsed"))?;
        if slot.is_none() {
            let channel = tokio::time::timeout_at(deadline, session.audio_channel())
                .await
                .map_err(|_| {
                    MediaError::new(MediaErrorKind::Timeout, "media deadline has elapsed")
                })??;
            *slot = Some(Arc::new(BoundedMediaAudioChannel::new(
                channel,
                context.max_payload_bytes.min(self.max_payload_bytes),
                audio_sources,
            )));
        }
        Ok(slot.as_ref().expect("channel initialized").clone())
    }

    async fn observe(
        &self,
        context: MediaOperationContext,
        target: MediaResourceRef,
        after: Option<MediaEventCursor>,
        requested_limit: u16,
        requested_wait_ms: Option<u64>,
    ) -> Result<MediaResult, MediaError> {
        let limit = requested_limit.min(MAX_MEDIA_EVENT_BATCH_SIZE);
        if limit == 0 {
            return Ok(MediaResult::Events {
                batch: MediaEventBatch::bounded(target, Vec::new(), false)?,
            });
        }
        let wait_ms = requested_wait_ms
            .map(|requested| requested.min(context.timeout_budget_ms.unwrap_or(requested)));
        if let MediaResourceRef::Session(reference) = &target {
            let (session, authorized_sources) = {
                let inner = self.inner.lock().unwrap();
                let session = inner
                    .sessions
                    .get(reference)
                    .filter(|session| session.owner == context.owner)
                    .ok_or_else(|| not_found("media session was not found"))?;
                (
                    session.session.clone(),
                    session_authorized_sources(
                        &session.selected_sources,
                        &session.declared_sources,
                    )?,
                )
            };
            let batch = session
                .observe(context.clone(), after, limit, wait_ms)
                .await?;
            batch.validate()?;
            if batch.target != target {
                return Err(native_failure(
                    "media session returned events for a different target",
                ));
            }
            let mut incoming = batch.events;
            let connector_dropped = batch.dropped_before_cursor;
            incoming.sort_by_key(|event| event.sequence);
            let mut inner = self.inner.lock().unwrap();
            let now = now_ms();
            let mut fresh_assets = Vec::new();
            let mut stale_frame_sequences = std::collections::HashSet::new();
            for event in &incoming {
                if event.occurred_at_unix_ms > now.saturating_add(MAX_FUTURE_EVENT_SKEW_MS) {
                    return Err(MediaError::new(
                        MediaErrorKind::InvalidRequest,
                        "media event timestamp is in the future",
                    ));
                }
                event.validate()?;
                validate_observed_event_identity(&target, event, &authorized_sources)?;
                match &event.kind {
                    MediaEventKind::StateChanged { .. } => {}
                    MediaEventKind::Frame { asset, .. } => {
                        validate_asset_metadata(
                            asset,
                            context.max_payload_bytes.min(self.max_payload_bytes),
                        )?;
                        if let Some(captured_at) = asset.metadata.captured_at_unix_ms {
                            if captured_at > now.saturating_add(MAX_FUTURE_EVENT_SKEW_MS) {
                                return Err(MediaError::new(
                                    MediaErrorKind::InvalidRequest,
                                    "media frame capture timestamp is in the future",
                                ));
                            }
                            if captured_at.saturating_add(MAX_EVENT_AGE_MS) < now {
                                stale_frame_sequences.insert(event.sequence);
                                continue;
                            }
                        }
                        fresh_assets.push(asset.clone());
                    }
                    MediaEventKind::Transcript { .. } | MediaEventKind::Error { .. } => {}
                }
            }
            for asset in &fresh_assets {
                remember_asset_locked(
                    &mut inner,
                    &context.owner,
                    asset,
                    context.max_payload_bytes.min(self.max_payload_bytes),
                )?;
            }
            let buffer = get_event_buffer(&mut inner, target.clone(), context.owner.clone(), now);
            if connector_dropped {
                buffer.dropped_any = true;
                buffer.dropped_through = buffer
                    .dropped_through
                    .max(after.map_or(1, |cursor| cursor.sequence.saturating_add(1)));
            }
            for sequence in &stale_frame_sequences {
                buffer.dropped_any = true;
                buffer.dropped_through = buffer.dropped_through.max(*sequence);
            }
            for event in incoming {
                if stale_frame_sequences.contains(&event.sequence) {
                    continue;
                }
                buffer.push(event, now);
            }
            let event_key = EventKey {
                owner: context.owner.clone(),
                target: target.clone(),
            };
            let mut snapshot = inner
                .events
                .get_mut(&event_key)
                .expect("event buffer inserted")
                .buffer
                .snapshot(target, after, limit, now)?;
            snapshot.dropped_before_cursor |= connector_dropped;
            return Ok(MediaResult::Events { batch: snapshot });
        }
        self.observe_local(context, target, after, limit, wait_ms)
            .await
    }

    async fn observe_local(
        &self,
        context: MediaOperationContext,
        target: MediaResourceRef,
        after: Option<MediaEventCursor>,
        limit: u16,
        wait_ms: Option<u64>,
    ) -> Result<MediaResult, MediaError> {
        self.validate_observable_target(&context, &target).await?;
        let mut receiver = self.event_receiver(&context.owner, &target)?;
        let deadline =
            wait_ms.map(|millis| tokio::time::Instant::now() + Duration::from_millis(millis));
        let no_session_sources = HashSet::new();
        loop {
            let batch = self.snapshot_events(&context.owner, &target, after, limit)?;
            batch.validate()?;
            for event in &batch.events {
                validate_observed_event_identity(&target, event, &no_session_sources)?;
            }
            if !batch.events.is_empty() || wait_ms.unwrap_or(0) == 0 {
                return Ok(MediaResult::Events { batch });
            }
            let Some(deadline) = deadline else {
                return Ok(MediaResult::Events { batch });
            };
            if tokio::time::Instant::now() >= deadline {
                return Ok(MediaResult::Events { batch });
            }
            tokio::select! {
                _ = receiver.changed() => {},
                _ = tokio::time::sleep_until(deadline) => return Ok(MediaResult::Events { batch }),
            }
        }
    }

    async fn validate_observable_target(
        &self,
        context: &MediaOperationContext,
        target: &MediaResourceRef,
    ) -> Result<(), MediaError> {
        match target {
            MediaResourceRef::Session(reference) => {
                self.owned_session(&context.owner, reference)?;
            }
            MediaResourceRef::Recording(reference) => {
                self.owned_recording(&context.owner, reference)?;
            }
            MediaResourceRef::Source(reference) => {
                let sources = self.sources_for(context).await?;
                if !sources.iter().any(|source| source.reference == *reference) {
                    return Err(not_found("media source was not found"));
                }
            }
            MediaResourceRef::Media(reference) => {
                let known = self
                    .inner
                    .lock()
                    .unwrap()
                    .known_assets
                    .get(reference)
                    .is_some_and(|asset| asset.owner == context.owner);
                if !known {
                    return Err(not_found("media asset was not found"));
                }
            }
            MediaResourceRef::Operation {
                identity,
                incarnation,
            } => {
                if !self
                    .inner
                    .lock()
                    .unwrap()
                    .operation_status
                    .get(identity)
                    .is_some_and(|status| {
                        status.owner == context.owner && status.incarnation == *incarnation
                    })
                {
                    return Err(not_found("media operation was not found"));
                }
            }
        }
        Ok(())
    }

    fn event_receiver(
        &self,
        owner: &MediaOwner,
        target: &MediaResourceRef,
    ) -> Result<watch::Receiver<u64>, MediaError> {
        let mut inner = self.inner.lock().unwrap();
        let now = now_ms();
        Ok(
            get_event_buffer(&mut inner, target.clone(), owner.clone(), now)
                .revision_tx
                .subscribe(),
        )
    }

    fn snapshot_events(
        &self,
        owner: &MediaOwner,
        target: &MediaResourceRef,
        after: Option<MediaEventCursor>,
        limit: u16,
    ) -> Result<MediaEventBatch, MediaError> {
        let mut inner = self.inner.lock().unwrap();
        let key = EventKey {
            owner: owner.clone(),
            target: target.clone(),
        };
        let entry = inner
            .events
            .get_mut(&key)
            .ok_or_else(|| not_found("media event stream was not found"))?;
        entry.last_access_unix_ms = now_ms();
        entry
            .buffer
            .snapshot(target.clone(), after, limit, now_ms())
    }

    async fn status(
        &self,
        context: &MediaOperationContext,
        target: Option<MediaResourceRef>,
    ) -> Result<MediaResult, MediaError> {
        if let Some(MediaResourceRef::Source(reference)) = target.as_ref() {
            let sources = self.sources_for(context).await?;
            let source = sources
                .iter()
                .find(|source| source.reference == *reference)
                .ok_or_else(|| not_found("media source was not found"))?;
            let state = match source.kind {
                MediaSourceKind::Microphone | MediaSourceKind::Speaker => {
                    let device = self.query_audio_status(context, None).await?;
                    let active = device.is_some_and(|status| match source.kind {
                        MediaSourceKind::Microphone => status.recording,
                        MediaSourceKind::Speaker => status.playing,
                        _ => false,
                    });
                    if active {
                        MediaResourceState::Active
                    } else {
                        MediaResourceState::Ready
                    }
                }
                _ => MediaResourceState::Ready,
            };
            return Ok(MediaResult::Status {
                resources: vec![MediaResourceStatus {
                    resource: MediaResourceRef::Source(reference.clone()),
                    state,
                    started_at_unix_ms: None,
                    updated_at_unix_ms: None,
                }],
            });
        }

        let include_all = target.is_none();
        let mut statuses = Vec::new();
        {
            let inner = self.inner.lock().unwrap();
            match target.as_ref() {
                Some(resource) => {
                    statuses.push(self.status_for_locked(context, &inner, resource.clone())?)
                }
                None => {
                    for (reference, recording) in &inner.recordings {
                        if recording.owner == context.owner {
                            statuses.push(MediaResourceStatus {
                                resource: MediaResourceRef::Recording(reference.clone()),
                                state: recording_resource_state(&recording.lifecycle),
                                started_at_unix_ms: Some(recording.started_at_unix_ms),
                                updated_at_unix_ms: Some(recording.started_at_unix_ms),
                            });
                        }
                    }
                    for (reference, session) in &inner.sessions {
                        if session.owner == context.owner {
                            statuses.push(MediaResourceStatus {
                                resource: MediaResourceRef::Session(reference.clone()),
                                state: session_resource_state(session.session.status().state),
                                started_at_unix_ms: Some(session.started_at_unix_ms),
                                updated_at_unix_ms: Some(now_ms()),
                            });
                        }
                    }
                    for (identity, operation) in &inner.operation_status {
                        if operation.owner == context.owner {
                            statuses.push(MediaResourceStatus {
                                resource: MediaResourceRef::Operation {
                                    identity: identity.clone(),
                                    incarnation: operation.incarnation,
                                },
                                state: active_operation_state(&inner, identity, operation.state),
                                started_at_unix_ms: Some(operation.started_at_unix_ms),
                                updated_at_unix_ms: Some(operation.updated_at_unix_ms),
                            });
                        }
                    }
                    for (reference, asset) in &inner.known_assets {
                        if asset.owner == context.owner {
                            statuses.push(MediaResourceStatus {
                                resource: MediaResourceRef::Media(reference.clone()),
                                state: MediaResourceState::Completed,
                                started_at_unix_ms: Some(asset.created_at_unix_ms),
                                updated_at_unix_ms: Some(asset.created_at_unix_ms),
                            });
                        }
                    }
                }
            }
        }
        if include_all {
            if let Some(device) = self.query_audio_status(context, None).await? {
                let sources = self.local_sources(&context.owner);
                if device.recording {
                    if let Some(source) = sources
                        .iter()
                        .find(|source| source.kind == MediaSourceKind::Microphone)
                    {
                        statuses.push(MediaResourceStatus {
                            resource: MediaResourceRef::Source(source.reference.clone()),
                            state: MediaResourceState::Active,
                            started_at_unix_ms: None,
                            updated_at_unix_ms: Some(now_ms()),
                        });
                    }
                }
                if device.playing {
                    if let Some(source) = sources
                        .iter()
                        .find(|source| source.kind == MediaSourceKind::Speaker)
                    {
                        statuses.push(MediaResourceStatus {
                            resource: MediaResourceRef::Source(source.reference.clone()),
                            state: MediaResourceState::Active,
                            started_at_unix_ms: None,
                            updated_at_unix_ms: Some(now_ms()),
                        });
                    }
                }
            }
        }
        statuses
            .sort_by(|a, b| resource_sort_key(&a.resource).cmp(&resource_sort_key(&b.resource)));
        Ok(MediaResult::Status {
            resources: statuses,
        })
    }

    async fn query_audio_status(
        &self,
        context: &MediaOperationContext,
        handle: Option<RecordingRef>,
    ) -> Result<Option<media_api::audio::AudioStatus>, MediaError> {
        match self
            .audio
            .execute(context.clone(), AudioOperation::Status { handle })
            .await
        {
            Ok(AudioOperationSuccess::Status { status }) => Ok(Some(status)),
            Ok(_) => Err(native_failure(
                "audio service returned an unexpected status result",
            )),
            Err(error) if error.kind == AudioErrorKind::Unsupported => Ok(None),
            Err(error) => Err(media_error_from_audio(error)),
        }
    }

    fn status_for_locked(
        &self,
        context: &MediaOperationContext,
        inner: &RuntimeState,
        resource: MediaResourceRef,
    ) -> Result<MediaResourceStatus, MediaError> {
        match &resource {
            MediaResourceRef::Recording(reference) => {
                let entry = inner
                    .recordings
                    .get(reference)
                    .filter(|entry| entry.owner == context.owner)
                    .ok_or_else(|| not_found("recording was not found"))?;
                Ok(MediaResourceStatus {
                    resource,
                    state: recording_resource_state(&entry.lifecycle),
                    started_at_unix_ms: Some(entry.started_at_unix_ms),
                    updated_at_unix_ms: Some(entry.started_at_unix_ms),
                })
            }
            MediaResourceRef::Session(reference) => {
                let entry = inner
                    .sessions
                    .get(reference)
                    .filter(|entry| entry.owner == context.owner)
                    .ok_or_else(|| not_found("media session was not found"))?;
                Ok(MediaResourceStatus {
                    resource,
                    state: session_resource_state(entry.session.status().state),
                    started_at_unix_ms: Some(entry.started_at_unix_ms),
                    updated_at_unix_ms: Some(now_ms()),
                })
            }
            MediaResourceRef::Operation {
                identity,
                incarnation,
            } => {
                let entry = inner
                    .operation_status
                    .get(identity)
                    .filter(|entry| {
                        entry.owner == context.owner && entry.incarnation == *incarnation
                    })
                    .ok_or_else(|| not_found("media operation was not found"))?;
                let state = active_operation_state(inner, identity, entry.state);
                Ok(MediaResourceStatus {
                    resource,
                    state,
                    started_at_unix_ms: Some(entry.started_at_unix_ms),
                    updated_at_unix_ms: Some(entry.updated_at_unix_ms),
                })
            }
            MediaResourceRef::Media(reference) => {
                let entry = inner
                    .known_assets
                    .get(reference)
                    .filter(|entry| entry.owner == context.owner)
                    .ok_or_else(|| not_found("media asset was not found"))?;
                Ok(MediaResourceStatus {
                    resource,
                    state: MediaResourceState::Completed,
                    started_at_unix_ms: Some(entry.created_at_unix_ms),
                    updated_at_unix_ms: Some(entry.created_at_unix_ms),
                })
            }
            MediaResourceRef::Source(reference) => {
                let _ = reference;
                Err(not_found(
                    "source status is queried through the owner-visible source catalog",
                ))
            }
        }
    }

    async fn stop(
        &self,
        context: MediaOperationContext,
        target: MediaResourceRef,
    ) -> Result<MediaResult, MediaError> {
        match target {
            MediaResourceRef::Recording(reference) => {
                let asset = stop_recording_shared(
                    &self.inner,
                    &self.audio,
                    &self.resources,
                    context.clone(),
                    reference.clone(),
                )
                .await?;
                Ok(MediaResult::RecordingStopped {
                    recording: reference,
                    asset,
                })
            }
            MediaResourceRef::Session(reference) => {
                let session = self.owned_session(&context.owner, &reference)?;
                let status = session.stop(context.clone()).await?;
                let removed = match status.state {
                    MediaSessionState::Ended => {
                        self.remove_owned_session_if_same(&context.owner, &reference, &session)
                    }
                    MediaSessionState::Failed => {
                        if self.remove_owned_session_if_same(&context.owner, &reference, &session) {
                            self.push_resource_event(
                                &context,
                                MediaResourceRef::Session(reference.clone()),
                                MediaResourceState::Failed,
                            );
                        }
                        return Err(native_failure("media session failed while stopping"));
                    }
                    MediaSessionState::Preparing
                    | MediaSessionState::Ready
                    | MediaSessionState::Reconnecting => {
                        return Err(MediaError::new(
                            MediaErrorKind::Busy,
                            "media session is still stopping",
                        ));
                    }
                };
                if removed {
                    self.push_resource_event(
                        &context,
                        MediaResourceRef::Session(reference.clone()),
                        session_resource_state(status.state),
                    );
                }
                Ok(MediaResult::Stopped {
                    resource: MediaResourceRef::Session(reference),
                })
            }
            MediaResourceRef::Operation {
                identity,
                incarnation,
            } => {
                if identity == context.identity && incarnation == context.operation_incarnation() {
                    return Err(MediaError::new(
                        MediaErrorKind::InvalidRequest,
                        "an operation cannot stop itself",
                    ));
                }
                let Some((token, done, finished, incarnation, mut cancel_flight)) =
                    self.begin_active_cancel(&identity, &context.owner, Some(incarnation))
                else {
                    return Err(not_found("active media operation was not found"));
                };
                token.cancel();
                if let Err(error) = self
                    .audio_router
                    .cancel_for_incarnation(&identity, &context.owner, &incarnation)
                    .await
                {
                    cancel_flight.finish(false);
                    return Err(media_error_from_audio(error));
                }
                let deadline = context.timeout_budget_ms.map(Duration::from_millis);
                if let Some(deadline) = deadline {
                    tokio::time::timeout(
                        deadline,
                        wait_for_active_operation(&self.inner, &identity, &done, finished),
                    )
                    .await
                    .map_err(|_| {
                        MediaError::new(
                            MediaErrorKind::Timeout,
                            "cancelled media operation did not stop before its deadline",
                        )
                    })?;
                } else {
                    wait_for_active_operation(&self.inner, &identity, &done, finished).await;
                }
                cancel_flight.finish(true);
                Ok(MediaResult::Stopped {
                    resource: MediaResourceRef::Operation {
                        identity,
                        incarnation,
                    },
                })
            }
            MediaResourceRef::Media(media) => {
                if self
                    .inner
                    .lock()
                    .unwrap()
                    .known_assets
                    .get(&media)
                    .is_some_and(|asset| asset.owner != context.owner)
                {
                    return Err(not_found("media asset was not found"));
                }
                self.resources.delete(&context, &media).await?;
                let mut inner = self.inner.lock().unwrap();
                inner.known_assets.remove(&media);
                inner
                    .known_asset_order
                    .retain(|candidate| candidate != &media);
                for recording in inner.recordings.values_mut() {
                    if recording.owner == context.owner
                        && matches!(&recording.lifecycle, RecordingLifecycle::Completed(asset) if asset.reference == media)
                    {
                        recording.lifecycle = RecordingLifecycle::Failed(MediaErrorKind::NotFound);
                    }
                }
                Ok(MediaResult::Stopped {
                    resource: MediaResourceRef::Media(media),
                })
            }
            MediaResourceRef::Source(_) => Err(unsupported("media sources cannot be stopped")),
        }
    }

    fn owned_recording(
        &self,
        owner: &MediaOwner,
        reference: &RecordingRef,
    ) -> Result<(), MediaError> {
        if self
            .inner
            .lock()
            .unwrap()
            .recordings
            .get(reference)
            .is_some_and(|recording| recording.owner == *owner)
        {
            Ok(())
        } else {
            Err(not_found("recording was not found"))
        }
    }

    fn owned_session(
        &self,
        owner: &MediaOwner,
        reference: &MediaSessionRef,
    ) -> Result<Arc<dyn MediaSession>, MediaError> {
        let inner = self.inner.lock().unwrap();
        inner
            .sessions
            .get(reference)
            .filter(|session| session.owner == *owner)
            .map(|session| session.session.clone())
            .ok_or_else(|| not_found("media session was not found"))
    }

    fn remove_owned_session_if_same(
        &self,
        owner: &MediaOwner,
        reference: &MediaSessionRef,
        expected: &Arc<dyn MediaSession>,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let matches = inner
            .sessions
            .get(reference)
            .is_some_and(|entry| entry.owner == *owner && Arc::ptr_eq(&entry.session, expected));
        if matches {
            inner.sessions.remove(reference);
        }
        matches
    }

    async fn read_payload(
        &self,
        context: &MediaOperationContext,
        media: &MediaRef,
    ) -> Result<MediaPayload, MediaError> {
        if media.0.len() > MAX_MEDIA_REFERENCE_CHARS {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "media reference exceeds its bound",
            ));
        }
        let max_bytes = context.max_payload_bytes.min(self.max_payload_bytes);
        {
            let inner = self.inner.lock().unwrap();
            if inner
                .known_assets
                .get(media)
                .is_some_and(|asset| asset.owner != context.owner)
            {
                return Err(not_found("media asset was not found"));
            }
        }
        let payload = self.resources.get(context, media, max_bytes).await?;
        payload.validate(max_bytes)?;
        if let Some(asset) = self.inner.lock().unwrap().known_assets.get(media) {
            if asset.owner != context.owner {
                return Err(not_found("media asset was not found"));
            }
            if asset.metadata != payload.metadata {
                return Err(MediaError::new(
                    MediaErrorKind::StoreFailure,
                    "media store returned mismatched asset metadata",
                ));
            }
        }
        Ok(payload)
    }
}

struct ActiveGuard<'a> {
    service: &'a UnifiedMediaService,
    identity: MediaOperationId,
    incarnation: MediaOperationIncarnation,
    armed: bool,
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        let flight = {
            let mut inner = self.service.inner.lock().unwrap();
            // A dropped execution future may not reach run_operation's
            // terminal update. Preserve the exact incarnation's failed state
            // once its cancellation flight has actually settled.
            if let Some(status) = inner.operation_status.get_mut(&self.identity) {
                if status.incarnation == self.incarnation
                    && status.state == MediaResourceState::Active
                {
                    status.state = MediaResourceState::Failed;
                    status.updated_at_unix_ms = now_ms();
                }
            }
            let Some(active) = inner.active.get_mut(&self.identity) else {
                return;
            };
            if active.incarnation != self.incarnation {
                return;
            }
            active.finished = true;
            active.done.notify_waiters();
            if self.armed {
                active.cancel_flights = active.cancel_flights.saturating_add(1);
                active.token.cancel();
                Some((
                    active.owner.clone(),
                    active.incarnation,
                    MediaCancelFlight {
                        inner: self.service.inner.clone(),
                        identity: self.identity.clone(),
                        incarnation: active.incarnation,
                        settled: false,
                    },
                ))
            } else {
                let remove = active.cancel_flights == 0 && !active.cancel_unknown;
                if remove {
                    inner.active.remove(&self.identity);
                }
                None
            }
        };
        if let Some((owner, incarnation, mut flight)) = flight {
            let audio = self.service.audio_router.clone();
            let identity = self.identity.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let confirmed = audio
                        .cancel_for_incarnation(&identity, &owner, &incarnation)
                        .await
                        .is_ok();
                    flight.finish(confirmed);
                });
            } else {
                flight.finish(false);
            }
        }
    }
}

/// Match a source's primary media kind, with a live-camera exception.
///
/// A camera's primary kind stays `Image` because one-shot capture produces an
/// image. Include it in video listings only when the host advertises a live
/// session route; still-image capture must not be presented as video input.
fn source_matches_kind(
    source: &MediaSource,
    requested_kind: Option<MediaKind>,
    requested_usage: Option<MediaSourceUsage>,
) -> bool {
    let Some(kind) = requested_kind else {
        return true;
    };
    if source.media_kind == Some(kind) {
        return true;
    }
    kind == MediaKind::Video
        && (requested_usage.is_none() || requested_usage == Some(MediaSourceUsage::Session))
        && source.kind == MediaSourceKind::Camera
        && source.usages.contains(&MediaSourceUsage::Session)
        && source
            .supported_operations
            .contains(&MediaOperationKind::SessionStart)
}

struct BoundedMediaAudioChannel {
    inner: Arc<dyn MediaAudioChannel>,
    max_payload_bytes: u64,
    authorized_sources: Arc<RwLock<HashSet<MediaSourceRef>>>,
    last_sequence_by_source: Mutex<HashMap<MediaSourceRef, u64>>,
}

impl BoundedMediaAudioChannel {
    fn new(
        inner: Arc<dyn MediaAudioChannel>,
        max_payload_bytes: u64,
        authorized_sources: Arc<RwLock<HashSet<MediaSourceRef>>>,
    ) -> Self {
        Self {
            inner,
            max_payload_bytes,
            authorized_sources,
            last_sequence_by_source: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl MediaAudioChannel for BoundedMediaAudioChannel {
    async fn read(&self) -> Result<Option<MediaFrame>, MediaError> {
        let frame = tokio::time::timeout(
            Duration::from_millis(DEFAULT_OPERATION_TIMEOUT_MS),
            self.inner.read(),
        )
        .await
        .map_err(|_| {
            MediaError::new(
                MediaErrorKind::Timeout,
                "media audio channel read timed out",
            )
        })??;
        let Some(frame) = frame else {
            return Ok(None);
        };
        frame.validate(self.max_payload_bytes)?;
        if !matches!(&frame.payload, MediaFramePayload::Audio(_)) {
            return Err(unsupported(
                "conversation audio channel produced a non-audio frame",
            ));
        }
        let now = now_ms();
        if frame.captured_at_unix_ms > now.saturating_add(MAX_FUTURE_EVENT_SKEW_MS) {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "captured audio timestamp is in the future",
            ));
        }
        if frame.captured_at_unix_ms.saturating_add(MAX_EVENT_AGE_MS) < now {
            return Err(MediaError::new(
                MediaErrorKind::NoData,
                "captured audio frame is stale",
            ));
        }
        if frame.source.0 .0.len() > MAX_MEDIA_REFERENCE_CHARS {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "audio source reference exceeds its bound",
            ));
        }
        let authorized_sources = self.authorized_sources.read().unwrap();
        if !authorized_sources.contains(&frame.source) {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "audio frame source is not authorized for this session",
            ));
        }
        let mut sequences = self.last_sequence_by_source.lock().unwrap();
        sequences.retain(|source, _| authorized_sources.contains(source));
        if sequences
            .get(&frame.source)
            .is_some_and(|last| frame.sequence <= *last)
        {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "audio frame sequence is stale",
            ));
        }
        sequences.insert(frame.source.clone(), frame.sequence);
        Ok(Some(frame))
    }

    async fn write(&self, frame: MediaPcmFrame) -> Result<MediaRenderReceipt, MediaError> {
        frame.validate(self.max_payload_bytes)?;
        let requested_sample_frames = frame.pcm16le.len() / (usize::from(frame.channels) * 2);
        let receipt = tokio::time::timeout(
            Duration::from_millis(DEFAULT_OPERATION_TIMEOUT_MS),
            self.inner.write(frame),
        )
        .await
        .map_err(|_| {
            MediaError::new(
                MediaErrorKind::Timeout,
                "media audio channel write timed out",
            )
        })??;
        if usize::try_from(receipt.accepted_sample_frames).unwrap_or(usize::MAX)
            > requested_sample_frames
        {
            return Err(native_failure(
                "media audio channel returned an invalid render receipt",
            ));
        }
        Ok(receipt)
    }

    async fn flush(&self) -> Result<MediaRenderReceipt, MediaError> {
        tokio::time::timeout(
            Duration::from_millis(DEFAULT_OPERATION_TIMEOUT_MS),
            self.inner.flush(),
        )
        .await
        .map_err(|_| {
            MediaError::new(
                MediaErrorKind::Timeout,
                "media audio channel flush timed out",
            )
        })?
    }

    async fn interrupt(&self) -> Result<MediaRenderReceipt, MediaError> {
        tokio::time::timeout(
            Duration::from_millis(DEFAULT_OPERATION_TIMEOUT_MS),
            self.inner.interrupt(),
        )
        .await
        .map_err(|_| {
            MediaError::new(
                MediaErrorKind::Timeout,
                "media audio channel interrupt timed out",
            )
        })?
    }
}

#[async_trait]
impl media_api::MediaService for UnifiedMediaService {
    fn capabilities(&self) -> MediaCapabilities {
        self.capabilities_catalog()
    }

    fn capabilities_for(&self, owner: &MediaOwner) -> MediaCapabilities {
        self.capabilities_for_owner(owner)
    }

    async fn execute(
        &self,
        context: MediaOperationContext,
        operation: MediaOperation,
    ) -> Result<MediaResult, MediaError> {
        let capabilities = self.capabilities_for(&context.owner);
        if !capabilities.supports(operation.kind()) {
            return Err(unsupported("media operation is unsupported"));
        }
        self.run_operation(context, operation).await
    }

    async fn attach_session(
        &self,
        mut context: MediaOperationContext,
        session: Box<dyn MediaSession>,
    ) -> Result<MediaSessionRef, MediaError> {
        let session: Arc<dyn MediaSession> = Arc::from(session);
        let mut cleanup = UnregisteredSessionLease::new(session.clone(), context.clone());
        if context.identity.service_epoch != self.service_epoch {
            cleanup.stop().await;
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "stale media service epoch",
            ));
        }
        match self.reserve_session_slot(&context.identity, SessionReservationKind::Attach) {
            Ok(reservation) => cleanup.hold_reservation(reservation),
            Err(error) => {
                cleanup.stop().await;
                return Err(error);
            }
        }
        if let Err(error) = Self::claim_context_admission(&mut context) {
            cleanup.stop().await;
            return Err(error);
        }
        cleanup.context = context.clone();
        let declared_sources = match declared_session_sources(session.as_ref()) {
            Ok(sources) => sources,
            Err(error) => {
                cleanup.stop().await;
                return Err(error);
            }
        };
        if let Err(error) = session_authorized_sources(&HashMap::new(), &declared_sources) {
            cleanup.stop().await;
            return Err(error);
        }
        let source_catalog = tokio::time::timeout(
            Duration::from_millis(
                context
                    .timeout_budget_ms
                    .unwrap_or(DEFAULT_OPERATION_TIMEOUT_MS),
            ),
            self.sources_for(&context),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
        let audio_sources = Arc::new(RwLock::new(declared_input_audio_sources(
            &declared_sources,
            &source_catalog,
        )));
        let reference = session.reference().clone();
        if let Err(error) = validate_session_reference(&reference) {
            cleanup.stop().await;
            return Err(error);
        }
        let status = session.status();
        if matches!(
            status.state,
            MediaSessionState::Ended | MediaSessionState::Failed
        ) {
            cleanup.stop().await;
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "cannot attach a terminal media session",
            ));
        }
        let duplicate = {
            let mut inner = self.inner.lock().unwrap();
            if inner.sessions.contains_key(&reference) {
                true
            } else {
                cleanup.commit_locked(&mut inner);
                inner.sessions.insert(
                    reference.clone(),
                    OwnedSession {
                        owner: context.owner.clone(),
                        origin_operation_id: context.identity.clone(),
                        session: session.clone(),
                        audio_channel: Arc::new(AsyncMutex::new(None)),
                        intent: None,
                        declared_sources,
                        selected_sources: HashMap::new(),
                        audio_sources,
                        started_at_unix_ms: now_ms(),
                    },
                );
                false
            }
        };
        if duplicate {
            cleanup.stop().await;
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "media session reference is already registered",
            ));
        }
        self.push_resource_event(
            &context,
            MediaResourceRef::Session(reference.clone()),
            MediaResourceState::Active,
        );
        self.finish_session_registration(&context, &reference, &session)?;
        Ok(reference)
    }

    async fn session_audio(
        &self,
        context: MediaOperationContext,
        session: MediaSessionRef,
    ) -> Result<Arc<dyn MediaAudioChannel>, MediaError> {
        UnifiedMediaService::session_audio(self, context, session).await
    }

    async fn read_media(
        &self,
        context: MediaOperationContext,
        media: MediaRef,
    ) -> Result<media_api::MediaContent, MediaError> {
        self.run_read(context, media).await
    }

    async fn cancel(&self, context: MediaOperationContext) -> Result<(), MediaError> {
        if let Some((token, _, _, incarnation, mut cancel_flight)) = self.begin_active_cancel(
            &context.identity,
            &context.owner,
            Some(context.operation_incarnation()),
        ) {
            token.cancel();
            let result = self
                .audio_router
                .cancel_for_incarnation(&context.identity, &context.owner, &incarnation)
                .await
                .map_err(media_error_from_audio);
            cancel_flight.finish(result.is_ok());
            result?;
        } else {
            // Media execution can finish before an audio cancellation receipt.
            // Recheck only the exact audio incarnation retained by the router.
            self.audio_router
                .cancel_for_incarnation(
                    &context.identity,
                    &context.owner,
                    &context.operation_incarnation(),
                )
                .await
                .map_err(media_error_from_audio)?;
        }
        Ok(())
    }
}

async fn stop_recording_shared(
    inner: &Arc<Mutex<RuntimeState>>,
    audio: &Arc<dyn AudioService>,
    resources: &Arc<dyn MediaResourceStore>,
    context: MediaOperationContext,
    recording: RecordingRef,
) -> Result<MediaAsset, MediaError> {
    let (finalized, timer) = {
        let mut inner = inner.lock().unwrap();
        let entry = inner
            .recordings
            .get_mut(&recording)
            .ok_or_else(|| not_found("recording was not found"))?;
        if entry.owner != context.owner {
            return Err(not_found("recording was not found"));
        }
        match &entry.lifecycle {
            RecordingLifecycle::Completed(asset) => return Ok(asset.clone()),
            RecordingLifecycle::Failed(kind) => {
                return Err(MediaError::new(*kind, "recording could not be finalized"));
            }
            RecordingLifecycle::Stopping => {
                return Err(MediaError::new(
                    MediaErrorKind::Busy,
                    "recording is already stopping",
                ));
            }
            RecordingLifecycle::Active => {
                entry.lifecycle = RecordingLifecycle::Stopping;
                entry.finalize_in_progress = true;
                (None, entry.timer.take())
            }
            RecordingLifecycle::Finalized(recording) => {
                if entry.finalize_in_progress {
                    return Err(MediaError::new(
                        MediaErrorKind::Busy,
                        "recording artifact is already being stored",
                    ));
                }
                entry.finalize_in_progress = true;
                (Some(recording.clone()), None)
            }
        }
    };
    if let Some(timer) = timer {
        timer.cancel();
    }
    let mut stop_guard = StopRecordingGuard {
        inner: inner.clone(),
        owner: context.owner.clone(),
        recording: recording.clone(),
        armed: true,
    };
    let recording_data = if let Some(recording_data) = finalized {
        recording_data
    } else {
        let result = audio
            .execute(
                context.clone(),
                AudioOperation::StopRecording {
                    handle: recording.clone(),
                },
            )
            .await;
        let recording_data = match result {
            Ok(AudioOperationSuccess::Recording { recording }) => recording,
            Ok(_) => {
                restore_recording_after_stop_failure(inner, &context.owner, &recording);
                stop_guard.armed = false;
                return Err(native_failure(
                    "audio service did not return finalized recording bytes",
                ));
            }
            Err(error) => {
                restore_recording_after_stop_failure(inner, &context.owner, &recording);
                stop_guard.armed = false;
                return Err(media_error_from_audio(error));
            }
        };
        {
            let mut state = inner.lock().unwrap();
            let entry = state
                .recordings
                .get_mut(&recording)
                .ok_or_else(|| not_found("recording was not found"))?;
            if entry.owner != context.owner {
                return Err(not_found("recording was not found"));
            }
            entry.lifecycle = RecordingLifecycle::Finalized(recording_data.clone());
        }
        recording_data
    };
    let payload = match voice_payload(recording_data, None, context.max_payload_bytes) {
        Ok(payload) => payload,
        Err(error) => {
            set_recording_failed(inner, &context.owner, &recording, error.kind);
            stop_guard.armed = false;
            return Err(error);
        }
    };
    let asset = resources.put(&context, payload.clone()).await?;
    if asset.metadata != payload.metadata {
        let foreign_reference = inner
            .lock()
            .unwrap()
            .known_assets
            .get(&asset.reference)
            .is_some_and(|existing| existing.owner != context.owner);
        if !foreign_reference {
            let _ = resources.delete(&context, &asset.reference).await;
        }
        return Err(MediaError::new(
            MediaErrorKind::StoreFailure,
            "media store returned mismatched recording metadata",
        ));
    }
    let mut inner = inner.lock().unwrap();
    remember_asset_locked(
        &mut inner,
        &context.owner,
        &asset,
        context.max_payload_bytes,
    )?;
    let entry = inner
        .recordings
        .get_mut(&recording)
        .ok_or_else(|| not_found("recording was not found"))?;
    if entry.owner != context.owner {
        return Err(not_found("recording was not found"));
    }
    entry.lifecycle = RecordingLifecycle::Completed(asset.clone());
    entry.timer = None;
    entry.finalize_in_progress = false;
    stop_guard.armed = false;
    prune_recording_history(&mut inner);
    let target = MediaResourceRef::Recording(recording);
    let now = now_ms();
    let buffer = get_event_buffer(&mut inner, target.clone(), context.owner.clone(), now);
    buffer.push_state(
        target,
        MediaResourceState::Completed,
        Some(context.identity),
        now,
    );
    Ok(asset)
}

struct StopRecordingGuard {
    inner: Arc<Mutex<RuntimeState>>,
    owner: MediaOwner,
    recording: RecordingRef,
    armed: bool,
}

impl Drop for StopRecordingGuard {
    fn drop(&mut self) {
        if self.armed {
            restore_recording_after_stop_failure(&self.inner, &self.owner, &self.recording);
        }
    }
}

fn restore_recording_after_stop_failure(
    inner: &Arc<Mutex<RuntimeState>>,
    owner: &MediaOwner,
    recording: &RecordingRef,
) {
    let mut inner = inner.lock().unwrap();
    if let Some(entry) = inner
        .recordings
        .get_mut(recording)
        .filter(|entry| entry.owner == *owner)
    {
        if matches!(&entry.lifecycle, RecordingLifecycle::Stopping) {
            entry.lifecycle = RecordingLifecycle::Active;
            entry.timer = None;
            entry.finalize_in_progress = false;
        } else if matches!(&entry.lifecycle, RecordingLifecycle::Finalized(_)) {
            entry.finalize_in_progress = false;
        }
    }
}

fn set_recording_failed(
    inner: &Arc<Mutex<RuntimeState>>,
    owner: &MediaOwner,
    recording: &RecordingRef,
    kind: MediaErrorKind,
) {
    let mut inner = inner.lock().unwrap();
    if let Some(entry) = inner
        .recordings
        .get_mut(recording)
        .filter(|entry| entry.owner == *owner)
    {
        entry.lifecycle = RecordingLifecycle::Failed(kind);
        entry.timer = None;
        entry.finalize_in_progress = false;
    }
    prune_recording_history(&mut inner);
}

fn prune_recording_history(inner: &mut RuntimeState) {
    let mut attempts = inner.recording_order.len();
    while inner.recordings.len() > MAX_RECORDING_HISTORY && attempts > 0 {
        attempts -= 1;
        let Some(reference) = inner.recording_order.pop_front() else {
            break;
        };
        match inner
            .recordings
            .get(&reference)
            .map(|entry| &entry.lifecycle)
        {
            Some(RecordingLifecycle::Completed(_) | RecordingLifecycle::Failed(_)) => {
                inner.recordings.remove(&reference);
            }
            Some(_) => inner.recording_order.push_back(reference),
            None => {}
        }
    }
}

fn get_event_buffer(
    inner: &mut RuntimeState,
    target: MediaResourceRef,
    owner: MediaOwner,
    now: u64,
) -> &mut EventBuffer {
    let key = EventKey {
        owner: owner.clone(),
        target: target.clone(),
    };
    if !inner.events.contains_key(&key) {
        while inner.events.len() >= MAX_EVENT_TARGETS {
            let Some(removable) = inner.event_target_order.pop_front() else {
                break;
            };
            inner.events.remove(&removable);
        }
        inner.events.insert(
            key.clone(),
            OwnedEventBuffer {
                buffer: EventBuffer::default(),
                last_access_unix_ms: now,
            },
        );
        inner.event_target_order.push_back(key.clone());
    }
    let entry = inner.events.get_mut(&key).expect("event buffer inserted");
    entry.last_access_unix_ms = now;
    &mut entry.buffer
}

fn remember_asset_locked(
    inner: &mut RuntimeState,
    owner: &MediaOwner,
    asset: &MediaAsset,
    max_bytes: u64,
) -> Result<(), MediaError> {
    validate_asset_metadata(asset, max_bytes)?;
    if let Some(existing) = inner.known_assets.get(&asset.reference) {
        if existing.owner != *owner || existing.metadata != asset.metadata {
            return Err(MediaError::new(
                MediaErrorKind::StoreFailure,
                "media asset reference belongs to another resource",
            ));
        }
    } else {
        inner.known_assets.insert(
            asset.reference.clone(),
            OwnedAsset {
                owner: owner.clone(),
                metadata: asset.metadata.clone(),
                created_at_unix_ms: now_ms(),
            },
        );
        inner.known_asset_order.push_back(asset.reference.clone());
        while inner.known_asset_order.len() > 1024 {
            if let Some(expired) = inner.known_asset_order.pop_front() {
                inner.known_assets.remove(&expired);
            }
        }
    }
    Ok(())
}

struct UnsupportedAudioService {
    service_epoch: u64,
    max_payload_bytes: u64,
}

#[async_trait]
impl AudioService for UnsupportedAudioService {
    fn capabilities(&self) -> AudioCapabilitySnapshot {
        AudioCapabilitySnapshot {
            service_epoch: self.service_epoch,
            support_revision: 1,
            supported_operations: Vec::new(),
            readiness: Vec::new(),
            max_payload_bytes: self.max_payload_bytes,
        }
    }

    async fn execute(
        &self,
        _context: MediaOperationContext,
        _operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        Err(AudioError::new(
            AudioErrorKind::Unsupported,
            "native audio is unavailable",
        ))
    }

    async fn cancel(&self, _context: MediaOperationContext) -> Result<(), AudioError> {
        Ok(())
    }
}

fn media_operation_for_audio(kind: AudioOperationKind) -> Option<MediaOperationKind> {
    Some(match kind {
        AudioOperationKind::Capture => MediaOperationKind::Capture,
        AudioOperationKind::Record => MediaOperationKind::Record,
        AudioOperationKind::Listen | AudioOperationKind::Transcribe => {
            MediaOperationKind::Transcribe
        }
        AudioOperationKind::Synthesize => MediaOperationKind::Synthesize,
        AudioOperationKind::Play => MediaOperationKind::Play,
        AudioOperationKind::Speak => MediaOperationKind::Speak,
    })
}

fn map_audio_readiness(state: AudioReadinessState) -> MediaReadinessState {
    match state {
        AudioReadinessState::Ready => MediaReadinessState::Ready,
        AudioReadinessState::NeedsPermission => MediaReadinessState::NeedsPermission,
        AudioReadinessState::Busy => MediaReadinessState::Busy,
        AudioReadinessState::MissingModel => MediaReadinessState::MissingModel,
        AudioReadinessState::Unavailable => MediaReadinessState::Unavailable,
    }
}

fn transcription_readiness(audio: &AudioCapabilitySnapshot) -> Option<MediaReadinessState> {
    let listen_supported = audio
        .supported_operations
        .contains(&AudioOperationKind::Listen);
    let transcribe_supported = audio
        .supported_operations
        .contains(&AudioOperationKind::Transcribe);
    let route_readiness = |operation| {
        audio
            .readiness
            .iter()
            .find(|entry| entry.operation == operation)
            .map(|entry| map_audio_readiness(entry.state))
    };

    match (listen_supported, transcribe_supported) {
        (true, true) => match (
            route_readiness(AudioOperationKind::Listen),
            route_readiness(AudioOperationKind::Transcribe),
        ) {
            (Some(listen), Some(transcribe)) if listen == transcribe => Some(listen),
            _ => None,
        },
        (true, false) => route_readiness(AudioOperationKind::Listen),
        (false, true) => route_readiness(AudioOperationKind::Transcribe),
        (false, false) => None,
    }
}

fn session_resource_state(state: MediaSessionState) -> MediaResourceState {
    match state {
        MediaSessionState::Preparing
        | MediaSessionState::Ready
        | MediaSessionState::Reconnecting => MediaResourceState::Active,
        MediaSessionState::Ended => MediaResourceState::Completed,
        MediaSessionState::Failed => MediaResourceState::Failed,
    }
}

fn recording_resource_state(state: &RecordingLifecycle) -> MediaResourceState {
    match state {
        RecordingLifecycle::Active => MediaResourceState::Active,
        RecordingLifecycle::Stopping | RecordingLifecycle::Finalized(_) => {
            MediaResourceState::Stopping
        }
        RecordingLifecycle::Completed(_) => MediaResourceState::Completed,
        RecordingLifecycle::Failed(_) => MediaResourceState::Failed,
    }
}

fn validate_capture_options(options: &CaptureOptions) -> Result<(), MediaError> {
    if options.max_dimension_px == Some(0) {
        return Err(MediaError::new(
            MediaErrorKind::InvalidRequest,
            "maximum image dimension must be positive",
        ));
    }
    if options
        .jpeg_quality
        .is_some_and(|quality| !quality.is_finite() || !(0.0..=1.0).contains(&quality))
    {
        return Err(MediaError::new(
            MediaErrorKind::InvalidRequest,
            "JPEG quality must be in [0, 1]",
        ));
    }
    Ok(())
}

fn audio_format(mime: Option<&str>) -> Result<String, MediaError> {
    match mime.map(str::trim).filter(|mime| !mime.is_empty()) {
        None | Some("audio/wav") | Some("audio/x-wav") | Some("wav") => Ok("wav".to_owned()),
        Some("audio/m4a") | Some("audio/mp4") | Some("m4a") => Ok("m4a".to_owned()),
        Some(_) => Err(unsupported("requested recording format is unsupported")),
    }
}

fn voice_payload(
    recording: VoiceRecording,
    duration_ms: Option<u64>,
    max_bytes: u64,
) -> Result<MediaPayload, MediaError> {
    if recording.audio_bytes.is_empty() || recording.mime_type.trim().is_empty() {
        return Err(MediaError::new(
            MediaErrorKind::NoData,
            "audio service returned an empty recording",
        ));
    }
    if recording.mime_type.len() > MAX_MEDIA_MIME_CHARS {
        return Err(MediaError::new(
            MediaErrorKind::InvalidRequest,
            "audio MIME type exceeds its bound",
        ));
    }
    let byte_len = u64::try_from(recording.audio_bytes.len()).unwrap_or(u64::MAX);
    if byte_len > max_bytes {
        return Err(MediaError::new(
            MediaErrorKind::MediaTooLarge,
            "recording exceeds the payload bound",
        ));
    }
    Ok(MediaPayload {
        metadata: MediaMetadata {
            kind: MediaKind::Audio,
            mime_type: recording.mime_type,
            byte_len,
            duration_ms,
            width_px: None,
            height_px: None,
            sample_rate_hz: None,
            channels: None,
            captured_at_unix_ms: Some(now_ms()),
        },
        bytes: recording.audio_bytes,
    })
}

fn pcm_payload(audio: TtsAudio) -> Result<MediaPayload, MediaError> {
    if audio.pcm.is_empty() || !audio.pcm.len().is_multiple_of(2) || audio.sample_rate_hz == 0 {
        return Err(MediaError::new(
            MediaErrorKind::ProcessingFailed,
            "synthesis returned invalid PCM16 audio",
        ));
    }
    let byte_len = u64::try_from(audio.pcm.len()).unwrap_or(u64::MAX);
    Ok(MediaPayload {
        metadata: MediaMetadata {
            kind: MediaKind::Audio,
            mime_type: "audio/pcm;format=s16le".to_owned(),
            byte_len,
            duration_ms: Some(
                (audio.pcm.len() as u64).saturating_mul(1000)
                    / (u64::from(audio.sample_rate_hz) * 2),
            ),
            width_px: None,
            height_px: None,
            sample_rate_hz: Some(audio.sample_rate_hz),
            channels: Some(1),
            captured_at_unix_ms: Some(now_ms()),
        },
        bytes: audio.pcm,
    })
}

fn pcm_from_payload(payload: MediaPayload) -> Result<TtsAudio, MediaError> {
    if payload.metadata.kind != MediaKind::Audio
        || payload.metadata.channels != Some(1)
        || !matches!(
            payload.metadata.mime_type.as_str(),
            "audio/pcm;format=s16le" | "audio/L16"
        )
    {
        return Err(unsupported(
            "playback currently requires stored mono PCM16 little-endian audio",
        ));
    }
    let sample_rate_hz = payload
        .metadata
        .sample_rate_hz
        .filter(|rate| *rate > 0)
        .ok_or_else(|| unsupported("PCM sample rate is unknown"))?;
    if payload.bytes.is_empty() || !payload.bytes.len().is_multiple_of(2) {
        return Err(MediaError::new(
            MediaErrorKind::InvalidRequest,
            "stored PCM audio is malformed",
        ));
    }
    Ok(TtsAudio {
        pcm: payload.bytes,
        sample_rate_hz,
    })
}

fn media_error_from_audio(error: AudioError) -> MediaError {
    let kind = match error.kind {
        AudioErrorKind::PermissionDenied => MediaErrorKind::PermissionDenied,
        AudioErrorKind::Busy => MediaErrorKind::Busy,
        AudioErrorKind::Cancelled => MediaErrorKind::Cancelled,
        AudioErrorKind::Timeout => MediaErrorKind::Timeout,
        AudioErrorKind::NoSpeech => MediaErrorKind::NoData,
        AudioErrorKind::NotRecording => MediaErrorKind::NotFound,
        AudioErrorKind::Unavailable => MediaErrorKind::Unavailable,
        AudioErrorKind::Unsupported => MediaErrorKind::Unsupported,
        AudioErrorKind::ModelMissing => MediaErrorKind::ModelMissing,
        AudioErrorKind::VoiceMissing | AudioErrorKind::SynthesisFailed => {
            MediaErrorKind::ProcessingFailed
        }
        AudioErrorKind::InvalidRequest => MediaErrorKind::InvalidRequest,
        AudioErrorKind::NativeFailure => MediaErrorKind::NativeFailure,
        AudioErrorKind::MediaTooLarge => MediaErrorKind::MediaTooLarge,
    };
    MediaError::new(kind, error.message)
}

fn media_error_from_camera(error: CameraError) -> MediaError {
    let kind = match error {
        CameraError::PermissionDenied => MediaErrorKind::PermissionDenied,
        CameraError::Cancelled => MediaErrorKind::Cancelled,
        CameraError::DeviceUnavailable => MediaErrorKind::Unavailable,
        CameraError::Other(_) => MediaErrorKind::NativeFailure,
    };
    MediaError::new(kind, error.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn active_operation_state(
    inner: &RuntimeState,
    identity: &MediaOperationId,
    recorded: MediaResourceState,
) -> MediaResourceState {
    match inner.active.get(identity) {
        Some(operation)
            if operation.finished || operation.cancel_flights > 0 || operation.cancel_unknown =>
        {
            MediaResourceState::Stopping
        }
        Some(_) => MediaResourceState::Active,
        None => recorded,
    }
}

fn resource_sort_key(resource: &MediaResourceRef) -> (u8, String) {
    match resource {
        MediaResourceRef::Operation { identity, .. } => (0, identity.id.clone()),
        MediaResourceRef::Recording(id) => (1, id.0 .0.clone()),
        MediaResourceRef::Session(id) => (2, id.0 .0.clone()),
        MediaResourceRef::Media(id) => (3, id.0.clone()),
        MediaResourceRef::Source(id) => (4, id.0 .0.clone()),
    }
}

fn validate_asset_metadata(asset: &MediaAsset, max_bytes: u64) -> Result<(), MediaError> {
    if asset.reference.0.len() > MAX_MEDIA_REFERENCE_CHARS
        || asset.metadata.mime_type.trim().is_empty()
        || asset.metadata.mime_type.len() > MAX_MEDIA_MIME_CHARS
    {
        return Err(MediaError::new(
            MediaErrorKind::InvalidRequest,
            "media asset metadata exceeds its bound",
        ));
    }
    if asset.metadata.byte_len > max_bytes {
        return Err(MediaError::new(
            MediaErrorKind::MediaTooLarge,
            "media asset exceeds the payload bound",
        ));
    }
    Ok(())
}

fn validate_session_reference(reference: &MediaSessionRef) -> Result<(), MediaError> {
    if reference.0 .0.is_empty() || reference.0 .0.len() > MAX_MEDIA_REFERENCE_CHARS {
        return Err(MediaError::new(
            MediaErrorKind::InvalidRequest,
            "media session reference exceeds its bound",
        ));
    }
    Ok(())
}

fn selected_session_sources(request: &MediaSessionRequest) -> HashMap<MediaKind, MediaSourceRef> {
    let mut sources = HashMap::new();
    if let Some(source) = &request.audio_source {
        sources.insert(MediaKind::Audio, source.clone());
    }
    if let Some(source) = &request.video_source {
        sources.insert(MediaKind::Video, source.clone());
    }
    sources
}

fn declared_input_audio_sources(
    declared: &HashSet<MediaSourceRef>,
    catalog: &[MediaSource],
) -> HashSet<MediaSourceRef> {
    catalog
        .iter()
        .filter(|source| {
            declared.contains(&source.reference)
                && source.media_kind == Some(MediaKind::Audio)
                && matches!(
                    source.kind,
                    MediaSourceKind::Microphone | MediaSourceKind::Remote
                )
                && source.usages.contains(&MediaSourceUsage::Session)
                && source
                    .supported_operations
                    .contains(&MediaOperationKind::SessionStart)
        })
        .map(|source| source.reference.clone())
        .collect()
}

fn session_authorized_sources(
    selected: &HashMap<MediaKind, MediaSourceRef>,
    declared: &HashSet<MediaSourceRef>,
) -> Result<HashSet<MediaSourceRef>, MediaError> {
    let sources: HashSet<_> = selected
        .values()
        .cloned()
        .chain(declared.iter().cloned())
        .collect();
    if sources.len() > MAX_MEDIA_SESSION_SOURCE_REFS {
        return Err(MediaError::new(
            MediaErrorKind::MediaTooLarge,
            "media session exposes too many authorized sources",
        ));
    }
    Ok(sources)
}

fn declared_session_sources(
    session: &dyn MediaSession,
) -> Result<HashSet<MediaSourceRef>, MediaError> {
    let declared = session.authorized_sources();
    if declared.len() > MAX_MEDIA_SESSION_SOURCE_REFS {
        return Err(MediaError::new(
            MediaErrorKind::MediaTooLarge,
            "media session declares too many authorized sources",
        ));
    }
    let mut sources = HashSet::new();
    for source in declared {
        if source.0 .0.is_empty() || source.0 .0.len() > MAX_MEDIA_REFERENCE_CHARS {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "media session source reference exceeds its bound",
            ));
        }
        sources.insert(source);
    }
    Ok(sources)
}

fn validate_resource_reference(reference: &MediaResourceRef) -> Result<(), MediaError> {
    let length = match reference {
        MediaResourceRef::Operation { identity, .. } => identity.id.len(),
        MediaResourceRef::Source(source) => source.0 .0.len(),
        MediaResourceRef::Recording(recording) => recording.0 .0.len(),
        MediaResourceRef::Session(session) => session.0 .0.len(),
        MediaResourceRef::Media(media) => media.0.len(),
    };
    if length == 0 || length > MAX_MEDIA_REFERENCE_CHARS {
        return Err(MediaError::new(
            MediaErrorKind::InvalidRequest,
            "media resource reference exceeds its bound",
        ));
    }
    Ok(())
}

fn validate_observed_event_identity(
    target: &MediaResourceRef,
    event: &MediaEvent,
    authorized_sources: &HashSet<MediaSourceRef>,
) -> Result<(), MediaError> {
    validate_resource_reference(&event.target)?;
    if &event.target != target {
        return Err(native_failure(
            "media session returned an event for a different target",
        ));
    }
    match &event.kind {
        MediaEventKind::StateChanged { resource, .. } => {
            validate_resource_reference(resource)?;
            if resource != &event.target {
                return Err(MediaError::new(
                    MediaErrorKind::InvalidRequest,
                    "media state event names a different resource than its target",
                ));
            }
        }
        MediaEventKind::Frame { source, .. } => {
            if source.0 .0.is_empty() || source.0 .0.len() > MAX_MEDIA_REFERENCE_CHARS {
                return Err(MediaError::new(
                    MediaErrorKind::InvalidRequest,
                    "media event source reference exceeds its bound",
                ));
            }
            let authorized = match target {
                MediaResourceRef::Source(expected) => source == expected,
                MediaResourceRef::Session(_) => authorized_sources.contains(source),
                MediaResourceRef::Operation { .. }
                | MediaResourceRef::Recording(_)
                | MediaResourceRef::Media(_) => false,
            };
            if !authorized {
                return Err(MediaError::new(
                    MediaErrorKind::InvalidRequest,
                    "media frame source is not authorized for the observed target",
                ));
            }
        }
        MediaEventKind::Transcript { .. } | MediaEventKind::Error { .. } => {}
    }
    Ok(())
}

fn unsupported(message: &str) -> MediaError {
    MediaError::new(MediaErrorKind::Unsupported, message)
}

fn native_failure(message: &str) -> MediaError {
    MediaError::new(MediaErrorKind::NativeFailure, message)
}

fn not_found(message: &str) -> MediaError {
    MediaError::new(MediaErrorKind::NotFound, message)
}

#[cfg(test)]
mod tests;
