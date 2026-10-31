//! Host-owned audio routing, independent of conversation completion engines.
#![forbid(unsafe_code)]

mod cloud;
mod pcm16;
use async_trait::async_trait;
pub use cloud::{CloudAudioCredentialSource, CloudAudioHost, HostedCloudRoute};
/// Shared host audio contracts, independent of any transport or chat engine.
pub use media_api::audio::*;
pub use media_api::stt::SttTranscript;
pub use media_api::tts::TtsAudio;
pub use media_api::voice::VoiceRecording;
use media_api::MediaOperationIncarnation;
pub use pcm16::{
    Pcm16StreamConverter, Pcm16StreamError, MAX_PCM16_STREAM_PUSH_FRAMES,
    MAX_PCM16_STREAM_SAMPLE_RATE_HZ, MIN_PCM16_STREAM_SAMPLE_RATE_HZ,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// A cloud route selected by trusted configuration, never a model argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloudBinding {
    /// Pin the current session's exact profile and account.
    FollowSession,
    /// Explicit profile for this operation kind.
    ExplicitProfile {
        /// Exact SDK profile name.
        profile_name: String,
    },
}
/// One immutable operation's backend selection.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum AudioRouteMode {
    /// System first, then installed compatible offline; never cloud.
    #[default]
    Automatic,
    /// Device system backend.
    System,
    /// A selected host configuration that cannot be interpreted safely.
    Invalid {
        /// Actionable configuration validation failure.
        reason: String,
    },
    /// Delegate configured local selection to the injected native host. The
    /// host owns its exact System/Offline resolver and installed-model catalog;
    /// the common runtime constrains the call to local execution.
    HostConfigured,
    /// An explicitly installed offline backend.
    Offline {
        /// Installed backend identifier.
        backend_id: String,
    },
    /// Explicit cloud selection with an audio-specific model.
    Cloud {
        /// Session following or explicit trusted profile.
        binding: CloudBinding,
        /// Audio model from this operation catalog.
        model: Option<String>,
    },
}
/// Trusted per-operation defaults from one host configuration snapshot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioOperationDefaults {
    /// Language used when the operation does not provide a language hint.
    pub language: Option<String>,
    /// Speech rate used when the operation does not provide one.
    pub rate: Option<f32>,
    /// Backend voice name, already validated against its selected route by the host.
    pub voice: Option<String>,
    /// Exact cloud voice scope from trusted host configuration. The selected
    /// profile and effective SDK model are checked before provider dispatch.
    pub voice_scope: Option<AudioVoiceScope>,
}

/// Exact profile/model scope required by a configured provider voice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioVoiceScope {
    /// Required exact provider profile.
    pub profile_name: Option<String>,
    /// Required effective SDK model, which may be resolved from provider
    /// capabilities when the user has not selected one explicitly.
    pub model_id: Option<String>,
}
/// Per-kind routes. Omitted kinds are automatic.
#[derive(Clone, Debug, Default)]
pub struct AudioRoutingSettings {
    /// Independent routes for recognition, synthesis and playback.
    pub operations: BTreeMap<AudioOperationKind, AudioRouteMode>,
    /// Defaults pinned together with the selected route before dispatch.
    pub defaults: BTreeMap<AudioOperationKind, AudioOperationDefaults>,
}
/// An installed, host-validated offline audio implementation.
#[derive(Clone)]
pub struct OfflineAudioBackend {
    /// Stable model/backend installation identity.
    pub id: String,
    /// Device-local implementation with its own support/readiness.
    pub service: Arc<dyn AudioService>,
}
/// One app-scoped audio service. Device leases remain owned by the native host.
pub struct UnifiedAudioService {
    device: Arc<dyn AudioService>,
    offline: Vec<OfflineAudioBackend>,
    host: Option<Arc<dyn CloudAudioHost>>,
    service_epoch: u64,
    settings: RwLock<AudioRoutingSettings>,
    active: Arc<Mutex<HashMap<AudioOperationId, Arc<ActiveOperation>>>>,
    projection_revision: Mutex<HashMap<AudioOwner, (Vec<AudioOperationKind>, u64)>>,
}

pub(crate) fn resolve_service_epoch(requested: u64, backend_epoch: u64) -> u64 {
    if requested == 0 {
        backend_epoch.max(1)
    } else {
        requested
    }
}

struct ActiveOperation {
    owner: AudioOwner,
    token: CancellationToken,
    target: Arc<dyn AudioService>,
    cancel_context: AudioOperationContext,
    incarnation: MediaOperationIncarnation,
    lifecycle: Mutex<ActiveOperationLifecycle>,
    changed: Notify,
}

struct ActiveOperationLifecycle {
    finished: bool,
    cancellation: Option<AudioCancellationState>,
    recording: Option<media_api::RecordingRef>,
    native_work: Option<Box<dyn media_api::MediaNativeWork>>,
}

enum AudioCancellationState {
    InFlight,
    Confirmed,
    Unknown(AudioError),
}
enum SelectedRoute {
    Local(Arc<dyn AudioService>),
    HostConfigured(Arc<dyn AudioService>),
    Cloud {
        route: HostedCloudRoute,
        model: Option<String>,
    },
}
impl UnifiedAudioService {
    /// Compose native audio and installed offline backends with optional cloud hosting.
    pub fn new(
        device: Arc<dyn AudioService>,
        offline: Vec<OfflineAudioBackend>,
        host: Option<Arc<dyn CloudAudioHost>>,
        settings: AudioRoutingSettings,
    ) -> Self {
        let service_epoch = device.capabilities().service_epoch;
        Self::new_with_service_epoch(device, offline, host, settings, service_epoch)
    }

    /// Compose the router with an app-owned logical epoch while keeping each
    /// selected backend's private epoch for native dispatch and cancellation.
    pub fn new_with_service_epoch(
        device: Arc<dyn AudioService>,
        offline: Vec<OfflineAudioBackend>,
        host: Option<Arc<dyn CloudAudioHost>>,
        settings: AudioRoutingSettings,
        service_epoch: u64,
    ) -> Self {
        let service_epoch =
            resolve_service_epoch(service_epoch, device.capabilities().service_epoch);
        Self {
            device,
            offline,
            host,
            service_epoch,
            settings: RwLock::new(settings),
            active: Arc::new(Mutex::new(HashMap::new())),
            projection_revision: Mutex::new(HashMap::new()),
        }
    }
    /// Bind model-facing capability projection to one trusted session owner.
    pub fn for_owner(self: &Arc<Self>, owner: AudioOwner) -> Arc<dyn AudioService> {
        Arc::new(OwnerAudioService {
            service: self.clone(),
            owner,
        })
    }

    /// Cancel one active operation only when its trusted owner still matches.
    /// The active identity remains reserved until the downstream ID-only
    /// cancellation call returns.
    pub(crate) async fn cancel_for_owner(
        &self,
        identity: &AudioOperationId,
        owner: &AudioOwner,
        incarnation: &MediaOperationIncarnation,
    ) -> Result<(), AudioError> {
        self.cancel_identity(identity, Some(owner), Some(incarnation))
            .await
    }

    /// Cancel one exact operation admitted by the media runtime. A mismatched
    /// or already-finished incarnation is a no-op even if the public ID has
    /// since appeared elsewhere.
    pub(crate) async fn cancel_for_incarnation(
        &self,
        identity: &AudioOperationId,
        owner: &AudioOwner,
        incarnation: &MediaOperationIncarnation,
    ) -> Result<(), AudioError> {
        self.cancel_identity(identity, Some(owner), Some(incarnation))
            .await
    }

    async fn cancel_identity(
        &self,
        identity: &AudioOperationId,
        expected_owner: Option<&AudioOwner>,
        expected_incarnation: Option<&MediaOperationIncarnation>,
    ) -> Result<(), AudioError> {
        let (operation, dispatch) = {
            let active = self.active.lock().unwrap();
            let Some(operation) = active.get(identity).cloned() else {
                return Ok(());
            };
            if expected_owner.is_some_and(|owner| owner != &operation.owner) {
                return Ok(());
            }
            if expected_incarnation
                .is_some_and(|incarnation| !operation.incarnation.same_instance(incarnation))
            {
                return Ok(());
            }
            let mut lifecycle = operation.lifecycle.lock().unwrap();
            match &lifecycle.cancellation {
                Some(AudioCancellationState::InFlight | AudioCancellationState::Confirmed) => {
                    (operation.clone(), false)
                }
                Some(AudioCancellationState::Unknown(_)) | None => {
                    lifecycle.cancellation = Some(AudioCancellationState::InFlight);
                    operation.token.cancel();
                    (operation.clone(), true)
                }
            }
        };
        if dispatch {
            dispatch_audio_cancel(self.active.clone(), identity.clone(), operation.clone());
        }
        wait_for_audio_cancel(&operation).await
    }
    /// Atomically replace routes; already running calls keep their selected snapshot.
    pub fn update_settings(&self, settings: AudioRoutingSettings) {
        *self.settings.write().unwrap() = settings;
    }
    fn select(
        &self,
        owner: &AudioOwner,
        kind: AudioOperationKind,
        settings: &AudioRoutingSettings,
    ) -> Result<SelectedRoute, AudioError> {
        // Capture, playback and resource controls are always native resources.
        let mode = if matches!(
            kind,
            AudioOperationKind::Record | AudioOperationKind::Capture | AudioOperationKind::Play
        ) {
            AudioRouteMode::System
        } else {
            settings.operations.get(&kind).cloned().unwrap_or_default()
        };
        let supports = |service: &Arc<dyn AudioService>| {
            service
                .capabilities_for(owner)
                .supported_operations
                .contains(&kind)
        };
        match mode {
            AudioRouteMode::Invalid { reason } => {
                Err(AudioError::new(AudioErrorKind::InvalidRequest, reason))
            }
            AudioRouteMode::Automatic => {
                let readiness = |service: &Arc<dyn AudioService>| {
                    service
                        .capabilities_for(owner)
                        .readiness
                        .into_iter()
                        .find(|entry| entry.operation == kind)
                        .map(|entry| entry.state)
                };
                let system_supported = supports(&self.device);
                if system_supported
                    && !matches!(
                        readiness(&self.device),
                        Some(AudioReadinessState::Unavailable | AudioReadinessState::MissingModel)
                    )
                {
                    return Ok(SelectedRoute::Local(self.device.clone()));
                }
                if let Some(entry) = self.offline.iter().find(|entry| {
                    supports(&entry.service)
                        && !matches!(
                            readiness(&entry.service),
                            Some(
                                AudioReadinessState::Unavailable
                                    | AudioReadinessState::MissingModel
                            )
                        )
                }) {
                    return Ok(SelectedRoute::Local(entry.service.clone()));
                }
                if system_supported {
                    return Ok(SelectedRoute::Local(self.device.clone()));
                }
                self.offline
                    .iter()
                    .find(|entry| supports(&entry.service))
                    .map(|entry| SelectedRoute::Local(entry.service.clone()))
                    .ok_or_else(|| {
                        unsupported(
                            "no system or installed offline backend supports this operation",
                        )
                    })
            }
            AudioRouteMode::System if supports(&self.device) => {
                Ok(SelectedRoute::Local(self.device.clone()))
            }
            AudioRouteMode::System => Err(unsupported("device audio operation is unsupported")),
            AudioRouteMode::HostConfigured if supports(&self.device) => {
                Ok(SelectedRoute::HostConfigured(self.device.clone()))
            }
            AudioRouteMode::HostConfigured => Err(unsupported(
                "the configured local audio backend does not support this operation",
            )),
            AudioRouteMode::Offline { backend_id } => {
                let entry = self
                    .offline
                    .iter()
                    .find(|entry| entry.id == backend_id)
                    .ok_or_else(|| {
                        AudioError::new(
                            AudioErrorKind::ModelMissing,
                            "selected offline audio backend is not installed",
                        )
                    })?;
                if !supports(&entry.service) {
                    return Err(unsupported(
                        "selected offline backend does not support this operation",
                    ));
                }
                Ok(SelectedRoute::Local(entry.service.clone()))
            }
            AudioRouteMode::Cloud { binding, model } => {
                let host = self.host.as_ref().ok_or_else(|| {
                    AudioError::new(
                        AudioErrorKind::Unavailable,
                        "cloud audio host is unavailable",
                    )
                })?;
                let route = host.resolve(owner, &binding)?;
                let effective_model = cloud::preflight(
                    &route,
                    kind,
                    model.as_deref(),
                    &self.device.capabilities_for(owner),
                )?;
                if let Some(scope) = settings
                    .defaults
                    .get(&kind)
                    .and_then(|defaults| defaults.voice_scope.as_ref())
                {
                    if scope.profile_name.as_deref() != Some(route.profile_name.as_str())
                        || scope.model_id.as_deref() != effective_model.as_deref()
                    {
                        return Err(AudioError::new(
                            AudioErrorKind::InvalidRequest,
                            "the saved provider voice does not match the selected profile and effective model",
                        ));
                    }
                }
                Ok(SelectedRoute::Cloud { route, model })
            }
        }
    }
    fn projected(&self, owner: &AudioOwner) -> AudioCapabilitySnapshot {
        let native = self.device.capabilities_for(owner);
        let settings = self.settings.read().unwrap().clone();
        let mut out = AudioCapabilitySnapshot {
            service_epoch: self.service_epoch,
            supported_operations: Vec::new(),
            readiness: Vec::new(),
            ..native.clone()
        };
        for kind in [
            AudioOperationKind::Record,
            AudioOperationKind::Capture,
            AudioOperationKind::Play,
            AudioOperationKind::Listen,
            AudioOperationKind::Transcribe,
            AudioOperationKind::Synthesize,
            AudioOperationKind::Speak,
        ] {
            match self.select(owner, kind, &settings) {
                Ok(SelectedRoute::Local(service)) => {
                    let caps = service.capabilities_for(owner);
                    out.supported_operations.push(kind);
                    out.readiness.push(AudioOperationReadiness {
                        operation: kind,
                        state: caps
                            .readiness
                            .iter()
                            .find(|r| r.operation == kind)
                            .map_or(AudioReadinessState::Ready, |r| r.state),
                    });
                }
                Ok(SelectedRoute::HostConfigured(service)) => {
                    let caps = service.capabilities_for(owner);
                    out.supported_operations.push(kind);
                    out.readiness.push(AudioOperationReadiness {
                        operation: kind,
                        state: caps
                            .readiness
                            .iter()
                            .find(|r| r.operation == kind)
                            .map_or(AudioReadinessState::Ready, |r| r.state),
                    });
                }
                Ok(SelectedRoute::Cloud { .. }) => {
                    out.supported_operations.push(kind);
                    let resource = match kind {
                        AudioOperationKind::Listen => Some(AudioOperationKind::Capture),
                        AudioOperationKind::Speak => Some(AudioOperationKind::Play),
                        _ => None,
                    };
                    let state = resource
                        .and_then(|k| {
                            native
                                .readiness
                                .iter()
                                .find(|r| r.operation == k)
                                .map(|r| r.state)
                        })
                        .unwrap_or(AudioReadinessState::Ready);
                    out.readiness.push(AudioOperationReadiness {
                        operation: kind,
                        state,
                    });
                }
                Err(error) if error.kind != AudioErrorKind::Unsupported => {
                    // Unsupported is structural. Unavailable or missing models remain
                    // visible so settings can recover them without changing schemas.
                    out.supported_operations.push(kind);
                    out.readiness.push(AudioOperationReadiness {
                        operation: kind,
                        state: if error.kind == AudioErrorKind::ModelMissing {
                            AudioReadinessState::MissingModel
                        } else {
                            AudioReadinessState::Unavailable
                        },
                    });
                }
                Err(_) => {}
            }
        }
        self.apply_support_revision(owner, &mut out);
        out
    }

    fn catalog_projected(&self) -> AudioCapabilitySnapshot {
        let device = self.device.capabilities();
        let settings = self.settings.read().unwrap().clone();
        let catalog_cloud = self
            .host
            .as_ref()
            .map_or_else(Vec::new, |host| host.catalog_supported_operations());
        let mut supported_operations = Vec::new();
        for kind in [
            AudioOperationKind::Record,
            AudioOperationKind::Capture,
            AudioOperationKind::Play,
            AudioOperationKind::Listen,
            AudioOperationKind::Transcribe,
            AudioOperationKind::Synthesize,
            AudioOperationKind::Speak,
        ] {
            let mode = if matches!(
                kind,
                AudioOperationKind::Record | AudioOperationKind::Capture | AudioOperationKind::Play
            ) {
                AudioRouteMode::System
            } else {
                settings.operations.get(&kind).cloned().unwrap_or_default()
            };
            let system_supports = device.supported_operations.contains(&kind);
            let supported = match mode {
                AudioRouteMode::Automatic => {
                    system_supports
                        || self.offline.iter().any(|entry| {
                            entry
                                .service
                                .capabilities()
                                .supported_operations
                                .contains(&kind)
                        })
                }
                AudioRouteMode::System => system_supports,
                AudioRouteMode::HostConfigured => system_supports,
                AudioRouteMode::Invalid { .. } => {
                    system_supports
                        || self.offline.iter().any(|entry| {
                            entry
                                .service
                                .capabilities()
                                .supported_operations
                                .contains(&kind)
                        })
                        || (catalog_cloud.contains(&kind)
                            && match kind {
                                AudioOperationKind::Listen => device
                                    .supported_operations
                                    .contains(&AudioOperationKind::Capture),
                                AudioOperationKind::Speak => device
                                    .supported_operations
                                    .contains(&AudioOperationKind::Play),
                                AudioOperationKind::Transcribe | AudioOperationKind::Synthesize => {
                                    true
                                }
                                _ => false,
                            })
                }
                AudioRouteMode::Offline { backend_id } => self.offline.iter().any(|entry| {
                    entry.id == backend_id
                        && entry
                            .service
                            .capabilities()
                            .supported_operations
                            .contains(&kind)
                }),
                AudioRouteMode::Cloud { .. } => {
                    catalog_cloud.contains(&kind)
                        && match kind {
                            AudioOperationKind::Listen => device
                                .supported_operations
                                .contains(&AudioOperationKind::Capture),
                            AudioOperationKind::Speak => device
                                .supported_operations
                                .contains(&AudioOperationKind::Play),
                            AudioOperationKind::Transcribe | AudioOperationKind::Synthesize => true,
                            AudioOperationKind::Record
                            | AudioOperationKind::Capture
                            | AudioOperationKind::Play => false,
                        }
                }
            };
            if supported {
                supported_operations.push(kind);
            }
        }
        let mut out = AudioCapabilitySnapshot {
            service_epoch: self.service_epoch,
            support_revision: 1,
            supported_operations,
            readiness: Vec::new(),
            max_payload_bytes: device.max_payload_bytes,
        };
        let catalog_owner = AudioOwner::System {
            instance_id: "audio-catalog".to_owned(),
        };
        self.apply_support_revision(&catalog_owner, &mut out);
        out
    }

    fn apply_support_revision(&self, owner: &AudioOwner, snapshot: &mut AudioCapabilitySnapshot) {
        let mut revisions = self.projection_revision.lock().unwrap();
        let revision = revisions
            .entry(owner.clone())
            .or_insert_with(|| (snapshot.supported_operations.clone(), 1));
        if revision.0 != snapshot.supported_operations {
            revision.0 = snapshot.supported_operations.clone();
            revision.1 = revision.1.saturating_add(1);
        }
        snapshot.support_revision = revision.1;
    }
}
fn unsupported(message: &str) -> AudioError {
    AudioError::new(AudioErrorKind::Unsupported, message)
}
fn apply_defaults(
    mut operation: AudioOperation,
    defaults: Option<&AudioOperationDefaults>,
) -> Result<AudioOperation, AudioError> {
    let Some(defaults) = defaults else {
        return Ok(operation);
    };
    match &mut operation {
        AudioOperation::Listen { language } | AudioOperation::Transcribe { language, .. } => {
            if language.is_none() {
                *language = defaults.language.clone();
            }
        }
        AudioOperation::Synthesize {
            language,
            rate,
            voice,
            ..
        }
        | AudioOperation::Speak {
            language,
            rate,
            voice,
            ..
        } => {
            if language.is_none() {
                *language = defaults.language.clone();
            }
            if rate.is_none() {
                *rate = defaults.rate;
            }
            if voice.is_none() {
                *voice = defaults.voice.clone();
            }
            if rate.is_some_and(|value| !value.is_finite() || !(0.1..=4.0).contains(&value)) {
                return Err(AudioError::new(
                    AudioErrorKind::InvalidRequest,
                    "configured audio rate is invalid",
                ));
            }
        }
        _ => {}
    }
    Ok(operation)
}
fn check_ready(caps: &AudioCapabilitySnapshot, kind: AudioOperationKind) -> Result<(), AudioError> {
    if !caps.supported_operations.contains(&kind) {
        return Err(unsupported("audio operation is unsupported"));
    }
    match caps
        .readiness
        .iter()
        .find(|entry| entry.operation == kind)
        .map(|entry| entry.state)
    {
        Some(AudioReadinessState::Busy) => Err(AudioError::new(
            AudioErrorKind::Busy,
            "audio backend is busy",
        )),
        Some(AudioReadinessState::MissingModel) => Err(AudioError::new(
            AudioErrorKind::ModelMissing,
            "audio model is not installed",
        )),
        Some(AudioReadinessState::Unavailable) => Err(AudioError::new(
            AudioErrorKind::Unavailable,
            "audio backend is unavailable",
        )),
        _ => Ok(()),
    }
}
struct ActiveGuard {
    active: Arc<Mutex<HashMap<AudioOperationId, Arc<ActiveOperation>>>>,
    identity: AudioOperationId,
    operation: Arc<ActiveOperation>,
    armed: bool,
}
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        finish_audio_operation(
            self.active.clone(),
            self.identity.clone(),
            self.operation.clone(),
            self.armed,
        );
    }
}

struct AudioCancelTaskGuard {
    active: Arc<Mutex<HashMap<AudioOperationId, Arc<ActiveOperation>>>>,
    identity: AudioOperationId,
    operation: Arc<ActiveOperation>,
    settled: bool,
}

impl Drop for AudioCancelTaskGuard {
    fn drop(&mut self) {
        if !self.settled {
            finish_audio_cancel(
                &self.active,
                &self.identity,
                &self.operation,
                Err(AudioError::new(
                    AudioErrorKind::NativeFailure,
                    "audio cancellation ended without a completion receipt",
                )),
            );
        }
    }
}

fn dispatch_audio_cancel(
    active: Arc<Mutex<HashMap<AudioOperationId, Arc<ActiveOperation>>>>,
    identity: AudioOperationId,
    operation: Arc<ActiveOperation>,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        finish_audio_cancel(
            &active,
            &identity,
            &operation,
            Err(AudioError::new(
                AudioErrorKind::NativeFailure,
                "audio cancellation has no runtime to obtain a completion receipt",
            )),
        );
        return;
    };
    runtime.spawn(async move {
        let mut completion = AudioCancelTaskGuard {
            active: active.clone(),
            identity: identity.clone(),
            operation: operation.clone(),
            settled: false,
        };
        let result = operation
            .target
            .cancel(operation.cancel_context.clone())
            .await;
        finish_audio_cancel(&active, &identity, &operation, result);
        completion.settled = true;
    });
}

fn finish_audio_cancel(
    active: &Arc<Mutex<HashMap<AudioOperationId, Arc<ActiveOperation>>>>,
    identity: &AudioOperationId,
    operation: &Arc<ActiveOperation>,
    result: Result<(), AudioError>,
) {
    let mut active_map = active.lock().unwrap();
    let same_operation = active_map
        .get(identity)
        .is_some_and(|current| Arc::ptr_eq(current, operation));
    if !same_operation {
        return;
    }
    let mut lifecycle = operation.lifecycle.lock().unwrap();
    if matches!(
        &lifecycle.cancellation,
        Some(AudioCancellationState::InFlight)
    ) {
        lifecycle.cancellation = Some(match result {
            Ok(()) => AudioCancellationState::Confirmed,
            Err(error) => AudioCancellationState::Unknown(error),
        });
    }
    let can_remove = lifecycle.finished
        && matches!(
            &lifecycle.cancellation,
            Some(AudioCancellationState::Confirmed)
        );
    if can_remove {
        active_map.remove(identity);
    }
    let work = settled_native_work(&mut lifecycle);
    drop(lifecycle);
    drop(active_map);
    resolve_native_work(work);
    operation.changed.notify_waiters();
}

// A cancel receipt can race with the operation's terminal result. Wait for
// both before confirming: a late RecordingStarted must remain unknown even
// if an earlier cancellation claimed success.
fn settled_native_work(
    lifecycle: &mut ActiveOperationLifecycle,
) -> Option<Box<dyn media_api::MediaNativeWork>> {
    if !lifecycle.finished {
        return None;
    }
    match &lifecycle.cancellation {
        Some(AudioCancellationState::Confirmed) => {}
        Some(AudioCancellationState::Unknown(_) | AudioCancellationState::InFlight) => return None,
        None if lifecycle.recording.is_none() => {}
        None => return None,
    }
    lifecycle.native_work.take()
}

fn resolve_native_work(work: Option<Box<dyn media_api::MediaNativeWork>>) {
    if let Some(work) = work {
        work.confirm();
    }
}

fn finish_audio_operation(
    active: Arc<Mutex<HashMap<AudioOperationId, Arc<ActiveOperation>>>>,
    identity: AudioOperationId,
    operation: Arc<ActiveOperation>,
    armed: bool,
) {
    let (dispatch, work) = {
        let mut active_map = active.lock().unwrap();
        let same_operation = active_map
            .get(&identity)
            .is_some_and(|current| Arc::ptr_eq(current, &operation));
        if !same_operation {
            return;
        }
        let mut lifecycle = operation.lifecycle.lock().unwrap();
        lifecycle.finished = true;
        let dispatch = if armed && lifecycle.cancellation.is_none() {
            lifecycle.cancellation = Some(AudioCancellationState::InFlight);
            operation.token.cancel();
            true
        } else {
            let can_remove = matches!(
                &lifecycle.cancellation,
                Some(AudioCancellationState::Confirmed)
            ) || (lifecycle.cancellation.is_none()
                && lifecycle.recording.is_none());
            if can_remove {
                active_map.remove(&identity);
            }
            false
        };
        (dispatch, settled_native_work(&mut lifecycle))
    };
    resolve_native_work(work);
    operation.changed.notify_waiters();
    if dispatch {
        dispatch_audio_cancel(active, identity, operation);
    }
}

async fn wait_for_audio_cancel(operation: &Arc<ActiveOperation>) -> Result<(), AudioError> {
    loop {
        let notified = operation.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let outcome = {
            let lifecycle = operation.lifecycle.lock().unwrap();
            match &lifecycle.cancellation {
                Some(AudioCancellationState::Confirmed) if lifecycle.finished => Some(Ok(())),
                Some(AudioCancellationState::Confirmed) => None,
                Some(AudioCancellationState::Unknown(error)) => Some(Err(error.clone())),
                Some(AudioCancellationState::InFlight) => None,
                None => Some(Ok(())),
            }
        };
        if let Some(outcome) = outcome {
            return outcome;
        }
        notified.await;
    }
}

async fn wait_for_audio_operation_finish(operation: &Arc<ActiveOperation>) {
    loop {
        let notified = operation.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if operation.lifecycle.lock().unwrap().finished {
            return;
        }
        notified.await;
    }
}
#[async_trait]
impl AudioService for UnifiedAudioService {
    fn capabilities(&self) -> AudioCapabilitySnapshot {
        self.catalog_projected()
    }
    fn capabilities_for(&self, owner: &AudioOwner) -> AudioCapabilitySnapshot {
        self.projected(owner)
    }
    async fn execute(
        &self,
        mut context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        let incarnation = context.operation_incarnation();
        if !context.runtime_admitted() && !context.claim_operation_admission() {
            return Err(AudioError::new(
                AudioErrorKind::InvalidRequest,
                "audio operation context has already been admitted",
            ));
        }
        context = context.with_operation_incarnation(incarnation);
        if context.identity.service_epoch != self.service_epoch {
            return Err(AudioError::new(
                AudioErrorKind::InvalidRequest,
                "stale audio service epoch",
            ));
        }
        let budget = context.timeout_budget_ms.unwrap_or(30_000);
        if budget == 0 {
            return Err(AudioError::new(
                AudioErrorKind::Timeout,
                "audio deadline has elapsed",
            ));
        }
        let settings = self.settings.read().unwrap().clone();
        let defaults = operation
            .kind()
            .and_then(|kind| settings.defaults.get(&kind));
        let operation = apply_defaults(operation, defaults)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(budget);
        let recording_origin = match &operation {
            AudioOperation::StopRecording { handle } => {
                let matches = self
                    .active
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, active)| {
                        active.owner == context.owner
                            && active.lifecycle.lock().unwrap().recording.as_ref() == Some(handle)
                    })
                    .map(|(identity, active)| (identity.clone(), active.incarnation))
                    .collect::<Vec<_>>();
                match matches.as_slice() {
                    [] => {
                        return Err(AudioError::new(
                            AudioErrorKind::NotRecording,
                            "recording origin was not found for this owner",
                        ))
                    }
                    [origin] => Some(origin.clone()),
                    _ => {
                        return Err(AudioError::new(
                            AudioErrorKind::NativeFailure,
                            "recording origin is ambiguous",
                        ))
                    }
                }
            }
            _ => None,
        };
        if matches!(operation, AudioOperation::EndOwner) {
            let active_operations = {
                let active = self.active.lock().unwrap();
                if active.contains_key(&context.identity) {
                    return Err(AudioError::new(
                        AudioErrorKind::InvalidRequest,
                        "duplicate active audio identity",
                    ));
                }
                active
                    .iter()
                    .filter(|(_, active)| active.owner == context.owner)
                    .map(|(identity, active)| (identity.clone(), active.clone()))
                    .collect::<Vec<_>>()
            };
            for (identity, active) in active_operations {
                tokio::time::timeout_at(
                    deadline,
                    self.cancel_identity(
                        &identity,
                        Some(&context.owner),
                        Some(&active.incarnation),
                    ),
                )
                .await
                .map_err(|_| {
                    AudioError::new(
                        AudioErrorKind::Timeout,
                        "audio cancellation receipt timed out",
                    )
                })??;
                tokio::time::timeout_at(deadline, wait_for_audio_operation_finish(&active))
                    .await
                    .map_err(|_| {
                        AudioError::new(AudioErrorKind::Timeout, "audio operation did not finish")
                    })?;
            }
        }
        let selected = operation
            .kind()
            .map(|kind| self.select(&context.owner, kind, &settings))
            .transpose()?;
        let target = match &selected {
            Some(SelectedRoute::Local(service) | SelectedRoute::HostConfigured(service)) => {
                service.clone()
            }
            Some(SelectedRoute::Cloud { .. }) | None => self.device.clone(),
        };
        let target_capabilities = target.capabilities_for(&context.owner);
        context.max_payload_bytes = context
            .max_payload_bytes
            .min(target_capabilities.max_payload_bytes);
        if context.max_payload_bytes == 0 && operation.kind().is_some() {
            return Err(AudioError::new(
                AudioErrorKind::InvalidRequest,
                "audio payload budget is zero",
            ));
        }
        let mut target_identity = context.identity.clone();
        target_identity.service_epoch = target_capabilities.service_epoch;
        let mut target_context = context.clone();
        target_context.identity = target_identity.clone();
        if let (Some(kind), Some(selected)) = (operation.kind(), &selected) {
            let service = match selected {
                SelectedRoute::Local(service) | SelectedRoute::HostConfigured(service) => {
                    Some(service)
                }
                SelectedRoute::Cloud { .. } => None,
            };
            if let Some(service) = service {
                check_ready(&service.capabilities_for(&context.owner), kind)?;
            }
        }
        let token = CancellationToken::new();
        let active_operation = Arc::new(ActiveOperation {
            owner: context.owner.clone(),
            token: token.clone(),
            target,
            cancel_context: target_context.clone(),
            incarnation,
            lifecycle: Mutex::new(ActiveOperationLifecycle {
                finished: false,
                cancellation: None,
                recording: None,
                native_work: None,
            }),
            changed: Notify::new(),
        });
        {
            let mut active = self.active.lock().unwrap();
            if active.contains_key(&context.identity) {
                return Err(AudioError::new(
                    AudioErrorKind::InvalidRequest,
                    "duplicate active audio identity",
                ));
            }
            active.insert(context.identity.clone(), active_operation.clone());
        }
        let max_payload_bytes = context.max_payload_bytes;
        let mut guard = ActiveGuard {
            active: self.active.clone(),
            identity: context.identity.clone(),
            operation: active_operation,
            armed: true,
        };
        if context.attachment_admitted() {
            match context.begin_native_work() {
                Ok(Some(work)) => {
                    guard.operation.lifecycle.lock().unwrap().native_work = Some(work);
                }
                Ok(None) => {
                    guard.armed = false;
                    return Err(AudioError::new(
                        AudioErrorKind::Unavailable,
                        "attachment audio has no native work witness",
                    ));
                }
                Err(error) => {
                    guard.armed = false;
                    return Err(AudioError::new(
                        AudioErrorKind::Unavailable,
                        error.to_string(),
                    ));
                }
            }
        }
        let started = Instant::now();
        let run = async {
            match selected {
                Some(SelectedRoute::Cloud { route, model }) => {
                    cloud::execute(
                        self.host.as_ref().unwrap().as_ref(),
                        &self.device,
                        &route,
                        model,
                        target_context,
                        operation,
                        started,
                    )
                    .await
                }
                Some(SelectedRoute::Local(service)) => {
                    service.execute(target_context, operation).await
                }
                Some(SelectedRoute::HostConfigured(service)) => {
                    service
                        .execute_host_configured_local(target_context, operation)
                        .await
                }
                None => self.device.execute(target_context, operation).await,
            }
        };
        let result = tokio::select! {
            biased;
            _ = token.cancelled() => Err(AudioError::new(AudioErrorKind::Cancelled, "audio operation cancelled")),
            _ = tokio::time::sleep_until(deadline) => Err(AudioError::new(AudioErrorKind::Timeout, "audio deadline has elapsed")),
            result = run => result,
        };
        if let Ok(success) = &result {
            let size = match success {
                AudioOperationSuccess::Recording { recording } => recording.audio_bytes.len(),
                AudioOperationSuccess::Synthesized { audio } => audio.pcm.len(),
                _ => 0,
            };
            if size as u64 > max_payload_bytes {
                return Err(AudioError::new(
                    AudioErrorKind::MediaTooLarge,
                    "audio result exceeds the payload bound",
                ));
            }
            if let AudioOperationSuccess::RecordingStarted { handle } = success {
                let mut lifecycle = guard.operation.lifecycle.lock().unwrap();
                lifecycle.recording = Some(handle.clone());
                if matches!(
                    &lifecycle.cancellation,
                    Some(AudioCancellationState::Confirmed)
                ) {
                    lifecycle.cancellation =
                        Some(AudioCancellationState::Unknown(AudioError::new(
                            AudioErrorKind::NativeFailure,
                            "recording started after its cancellation receipt",
                        )));
                }
            }
            guard.armed = false;
        }
        if let (Some((identity, incarnation)), Ok(AudioOperationSuccess::Recording { .. })) =
            (recording_origin, &result)
        {
            tokio::time::timeout_at(
                deadline,
                self.cancel_identity(&identity, Some(&context.owner), Some(&incarnation)),
            )
            .await
            .map_err(|_| {
                AudioError::new(
                    AudioErrorKind::Timeout,
                    "recording origin close receipt timed out",
                )
            })??;
        }
        result
    }
    async fn cancel(&self, context: AudioOperationContext) -> Result<(), AudioError> {
        let incarnation = context.operation_incarnation();
        self.cancel_identity(&context.identity, Some(&context.owner), Some(&incarnation))
            .await
    }
}

#[cfg(test)]
mod tests;

struct OwnerAudioService {
    service: Arc<UnifiedAudioService>,
    owner: AudioOwner,
}
#[async_trait]
impl AudioService for OwnerAudioService {
    fn capabilities(&self) -> AudioCapabilitySnapshot {
        self.service.capabilities_for(&self.owner)
    }
    fn capabilities_for(&self, owner: &AudioOwner) -> AudioCapabilitySnapshot {
        self.service.capabilities_for(owner)
    }
    async fn execute(
        &self,
        context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        if context.owner != self.owner {
            return Err(AudioError::new(
                AudioErrorKind::InvalidRequest,
                "audio session owner mismatch",
            ));
        }
        self.service.execute(context, operation).await
    }
    async fn cancel(&self, context: AudioOperationContext) -> Result<(), AudioError> {
        if context.owner != self.owner {
            return Ok(());
        }
        let incarnation = context.operation_incarnation();
        self.service
            .cancel_for_owner(&context.identity, &self.owner, &incarnation)
            .await
    }
}
