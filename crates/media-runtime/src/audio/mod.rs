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
pub use pcm16::{
    Pcm16StreamConverter, Pcm16StreamError, MAX_PCM16_STREAM_PUSH_FRAMES,
    MAX_PCM16_STREAM_SAMPLE_RATE_HZ, MIN_PCM16_STREAM_SAMPLE_RATE_HZ,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
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
    active: Mutex<HashMap<AudioOperationId, ActiveOperation>>,
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
    target_identity: AudioOperationId,
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
            active: Mutex::new(HashMap::new()),
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
struct ActiveGuard<'a> {
    service: &'a UnifiedAudioService,
    identity: AudioOperationId,
    armed: bool,
}
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        if let Some(active) = self.service.active.lock().unwrap().remove(&self.identity) {
            active.token.cancel();
            if self.armed {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let target_identity = active.target_identity.clone();
                    runtime.spawn(async move {
                        let _ = active.target.cancel(target_identity).await;
                    });
                }
            }
        }
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
        if matches!(operation, AudioOperation::EndOwner) {
            for active in self.active.lock().unwrap().values() {
                if active.owner == context.owner {
                    active.token.cancel();
                }
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
        {
            let mut active = self.active.lock().unwrap();
            if active.contains_key(&context.identity) {
                return Err(AudioError::new(
                    AudioErrorKind::InvalidRequest,
                    "duplicate active audio identity",
                ));
            }
            active.insert(
                context.identity.clone(),
                ActiveOperation {
                    owner: context.owner.clone(),
                    token: token.clone(),
                    target,
                    target_identity,
                },
            );
        }
        let max_payload_bytes = context.max_payload_bytes;
        let mut guard = ActiveGuard {
            service: self,
            identity: context.identity.clone(),
            armed: true,
        };
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
            _ = tokio::time::sleep(Duration::from_millis(budget)) => Err(AudioError::new(AudioErrorKind::Timeout, "audio deadline has elapsed")),
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
            guard.armed = false;
        }
        result
    }
    async fn cancel(&self, identity: AudioOperationId) -> Result<(), AudioError> {
        let target = self.active.lock().unwrap().get(&identity).map(|active| {
            active.token.cancel();
            (active.target.clone(), active.target_identity.clone())
        });
        if let Some((target, target_identity)) = target {
            target.cancel(target_identity).await
        } else {
            Ok(())
        }
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
    async fn cancel(&self, identity: AudioOperationId) -> Result<(), AudioError> {
        let permitted = self
            .service
            .active
            .lock()
            .unwrap()
            .get(&identity)
            .is_some_and(|active| active.owner == self.owner);
        if permitted {
            self.service.cancel(identity).await
        } else {
            Ok(())
        }
    }
}
