//! Host-owned audio routing, independent of conversation completion engines.
#![forbid(unsafe_code)]

mod cloud;
use async_trait::async_trait;
pub use cloud::{CloudAudioHost, HostedCloudRoute};
/// Shared host audio contracts, independent of any transport or chat engine.
pub use lingxi_core::host::audio::*;
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
/// Per-kind routes. Omitted kinds are automatic.
#[derive(Clone, Debug, Default)]
pub struct AudioRoutingSettings {
    /// Independent routes for recognition, synthesis and playback.
    pub operations: BTreeMap<AudioOperationKind, AudioRouteMode>,
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
    settings: RwLock<AudioRoutingSettings>,
    active: Mutex<HashMap<AudioOperationId, ActiveOperation>>,
    projection_revision: Mutex<(Vec<AudioOperationKind>, u64)>,
}
struct ActiveOperation {
    owner: AudioOwner,
    token: CancellationToken,
    target: Arc<dyn AudioService>,
}
enum SelectedRoute {
    Local(Arc<dyn AudioService>),
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
        Self {
            device,
            offline,
            host,
            settings: RwLock::new(settings),
            active: Mutex::new(HashMap::new()),
            projection_revision: Mutex::new((Vec::new(), 1)),
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
    ) -> Result<SelectedRoute, AudioError> {
        // Capture, playback and resource controls are always native resources.
        let mode = if matches!(
            kind,
            AudioOperationKind::Record | AudioOperationKind::Capture | AudioOperationKind::Play
        ) {
            AudioRouteMode::System
        } else {
            self.settings
                .read()
                .unwrap()
                .operations
                .get(&kind)
                .cloned()
                .unwrap_or_default()
        };
        let supports = |service: &Arc<dyn AudioService>| {
            service
                .capabilities_for(owner)
                .supported_operations
                .contains(&kind)
        };
        match mode {
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
                cloud::preflight(
                    &route,
                    kind,
                    model.as_deref(),
                    &self.device.capabilities_for(owner),
                )?;
                Ok(SelectedRoute::Cloud { route, model })
            }
        }
    }
    fn projected(&self, owner: &AudioOwner) -> AudioCapabilitySnapshot {
        let native = self.device.capabilities_for(owner);
        let mut out = AudioCapabilitySnapshot {
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
            match self.select(owner, kind) {
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
        let mut revision = self.projection_revision.lock().unwrap();
        if revision.0 != out.supported_operations {
            revision.0 = out.supported_operations.clone();
            revision.1 = revision.1.saturating_add(1);
        }
        out.support_revision = revision.1;
        out
    }
}
fn unsupported(message: &str) -> AudioError {
    AudioError::new(AudioErrorKind::Unsupported, message)
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
                let identity = self.identity.clone();
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        let _ = active.target.cancel(identity).await;
                    });
                }
            }
        }
    }
}
#[async_trait]
impl AudioService for UnifiedAudioService {
    fn capabilities(&self) -> AudioCapabilitySnapshot {
        self.projected(&AudioOwner::System {
            instance_id: "audio-catalog".into(),
        })
    }
    fn capabilities_for(&self, owner: &AudioOwner) -> AudioCapabilitySnapshot {
        self.projected(owner)
    }
    async fn execute(
        &self,
        mut context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        if context.identity.service_epoch != self.device.capabilities().service_epoch {
            return Err(AudioError::new(
                AudioErrorKind::InvalidRequest,
                "stale audio service epoch",
            ));
        }
        context.max_payload_bytes = context
            .max_payload_bytes
            .min(self.device.capabilities().max_payload_bytes);
        if context.max_payload_bytes == 0 && operation.kind().is_some() {
            return Err(AudioError::new(
                AudioErrorKind::InvalidRequest,
                "audio payload budget is zero",
            ));
        }
        let budget = context.timeout_budget_ms.unwrap_or(30_000);
        if budget == 0 {
            return Err(AudioError::new(
                AudioErrorKind::Timeout,
                "audio deadline has elapsed",
            ));
        }
        if matches!(operation, AudioOperation::EndOwner) {
            for active in self.active.lock().unwrap().values() {
                if active.owner == context.owner {
                    active.token.cancel();
                }
            }
        }
        let selected = operation
            .kind()
            .map(|kind| self.select(&context.owner, kind))
            .transpose()?;
        if let (Some(kind), Some(SelectedRoute::Local(service))) = (operation.kind(), &selected) {
            check_ready(&service.capabilities_for(&context.owner), kind)?;
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
            let target = match &selected {
                Some(SelectedRoute::Local(service)) => service.clone(),
                _ => self.device.clone(),
            };
            active.insert(
                context.identity.clone(),
                ActiveOperation {
                    owner: context.owner.clone(),
                    token: token.clone(),
                    target,
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
                        context,
                        operation,
                        started,
                    )
                    .await
                }
                Some(SelectedRoute::Local(service)) => service.execute(context, operation).await,
                None => self.device.execute(context, operation).await,
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
            active.target.clone()
        });
        if let Some(target) = target {
            target.cancel(identity).await
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
