//! Shared UniFFI boundary and core::host adapter for native audio services.

use super::media_owner_dto::owner_to_audio_dto;
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use client::protocol::audio::{
    AudioCapabilitySnapshotDto, AudioErrorDto, AudioErrorKindDto, AudioInitiatorDto,
    AudioOperationDto, AudioOperationIdDto, AudioOperationKindDto, AudioOperationRequestDto,
    AudioOperationResultDto, AudioOperationRoutePolicyDto, AudioReadinessStateDto,
    MAX_AUDIO_PAYLOAD_BYTES, MAX_AUDIO_SAMPLE_RATE_HZ,
};
use lingxi_core::host::audio::{
    AudioCapabilitySnapshot, AudioError, AudioErrorKind, AudioInitiator, AudioOperation,
    AudioOperationContext, AudioOperationId, AudioOperationKind, AudioOperationReadiness,
    AudioOperationSuccess, AudioOwner, AudioReadinessState, AudioService, AudioStatus,
};
use lingxi_core::host::{SttTranscript, TtsAudio, VoiceRecording};
use media_api::MediaOperationIncarnation;
use std::sync::Arc;
use tokio::runtime::Handle;

/// Internal failure to target cancellation at the native device service.
#[derive(Debug, thiserror::Error)]
pub enum AudioFfiError {
    /// A native callback rejected or could not deliver cancellation.
    #[error("audio cancellation failed: {message}")]
    NativeFailure { message: String },
}

/// One app-scoped native audio callback service.
#[async_trait]
pub trait NativeAudioService: Send + Sync {
    /// Current support/readiness snapshot. Must not request permission.
    fn capabilities(&self) -> AudioCapabilitySnapshotDto;
    /// Execute one operation and return a structured terminal outcome.
    ///
    /// The adapter provides an internal identity unique to this native
    /// callback attempt; it may differ from the Rust caller's public ID.
    async fn execute(&self, request: AudioOperationRequestDto) -> AudioOperationResultDto;
    /// Cancel only one pending operation identity.
    ///
    /// Returning `Ok(())` is a completion receipt: the callback has resolved
    /// this identity and this cancellation can no longer affect a later
    /// operation. If that cannot be established, return an error so the Rust
    /// adapter keeps the identity fenced.
    async fn cancel(&self, identity: AudioOperationIdDto) -> Result<(), AudioFfiError>;
}

/// Adapt a platform callback into the domain service used by tools and hosts.
pub fn from_native_audio_service(callback: Arc<dyn NativeAudioService>) -> Arc<dyn AudioService> {
    Arc::new(NativeAudioServiceAdapter {
        callback,
        active: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    })
}

/// Compose device-only native callbacks with offline installations and hosted
/// cloud audio. The callback passed here must expose local device operations;
/// a callback which already resolves the complete v4 policy should instead use
/// `from_native_audio_service` so the policy is resolved exactly once.
/// `owner` binds the tool catalog to a trusted current session when supplied.
pub fn from_unified_audio_services(
    callback: Arc<dyn NativeAudioService>,
    offline: Vec<media_runtime::OfflineAudioBackend>,
    cloud_host: Option<Arc<dyn media_runtime::CloudAudioHost>>,
    settings: media_runtime::AudioRoutingSettings,
    owner: Option<AudioOwner>,
) -> Arc<dyn AudioService> {
    let service = Arc::new(media_runtime::UnifiedAudioService::new(
        from_native_audio_service(callback),
        offline,
        cloud_host,
        settings,
    ));
    match owner {
        Some(owner) => service.for_owner(owner),
        None => service,
    }
}

/// Shared native-facing audio collection ceiling derived from the bridge's
/// existing frame budget and framing allowance.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn max_audio_payload_bytes() -> u64 {
    MAX_AUDIO_PAYLOAD_BYTES as u64
}

struct NativeAudioServiceAdapter {
    callback: Arc<dyn NativeAudioService>,
    active: NativeAudioActiveMap,
}

type NativeAudioActiveMap =
    Arc<std::sync::Mutex<std::collections::HashMap<AudioOperationId, Arc<NativeAudioOperation>>>>;

struct NativeAudioOperation {
    identity_dto: AudioOperationIdDto,
    callback: Arc<dyn NativeAudioService>,
    owner: AudioOwner,
    incarnation: MediaOperationIncarnation,
    lifecycle: std::sync::Mutex<NativeAudioOperationLifecycle>,
    changed: tokio::sync::Notify,
}

struct NativeAudioOperationLifecycle {
    finished: bool,
    cancellation_attempt: u64,
    cancellation: Option<NativeAudioCancellationState>,
    recording: Option<media_api::RecordingRef>,
}

#[derive(Debug)]
enum NativeAudioCancellationState {
    InFlight { attempt: u64 },
    Confirmed { attempt: u64 },
    Unknown { attempt: u64, error: AudioError },
}

struct NativeCancelOnDrop {
    active: NativeAudioActiveMap,
    identity: AudioOperationId,
    operation: Arc<NativeAudioOperation>,
    armed: bool,
}

impl NativeCancelOnDrop {
    fn finish(&mut self) {
        if self.armed {
            self.armed = false;
            finish_native_audio_operation(
                self.active.clone(),
                self.identity.clone(),
                self.operation.clone(),
                false,
            );
        }
    }
}

impl Drop for NativeCancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.armed = false;
            finish_native_audio_operation(
                self.active.clone(),
                self.identity.clone(),
                self.operation.clone(),
                true,
            );
        }
    }
}

struct NativeAudioCancelTaskGuard {
    active: NativeAudioActiveMap,
    identity: AudioOperationId,
    operation: Arc<NativeAudioOperation>,
    attempt: u64,
    settled: bool,
}

impl Drop for NativeAudioCancelTaskGuard {
    fn drop(&mut self) {
        if !self.settled {
            finish_native_audio_cancel(
                &self.active,
                &self.identity,
                &self.operation,
                self.attempt,
                Err(AudioError::new(
                    AudioErrorKind::NativeFailure,
                    "native audio cancellation ended without a completion receipt",
                )),
            );
        }
    }
}

fn finish_native_audio_operation(
    active: NativeAudioActiveMap,
    identity: AudioOperationId,
    operation: Arc<NativeAudioOperation>,
    cancel_on_drop: bool,
) {
    let dispatch = {
        let mut active_map = active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let same_operation = active_map
            .get(&identity)
            .is_some_and(|current| Arc::ptr_eq(current, &operation));
        if !same_operation {
            return;
        }
        let mut lifecycle = operation
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle.finished = true;
        if cancel_on_drop && lifecycle.cancellation.is_none() {
            let attempt = lifecycle.cancellation_attempt.saturating_add(1).max(1);
            lifecycle.cancellation_attempt = attempt;
            lifecycle.cancellation = Some(NativeAudioCancellationState::InFlight { attempt });
            Some(attempt)
        } else {
            let can_remove = matches!(
                &lifecycle.cancellation,
                Some(NativeAudioCancellationState::Confirmed { .. })
            ) || (lifecycle.cancellation.is_none()
                && lifecycle.recording.is_none());
            if can_remove {
                active_map.remove(&identity);
            }
            None
        }
    };
    operation.changed.notify_waiters();
    if let Some(attempt) = dispatch {
        dispatch_native_audio_cancel(active, identity, operation, attempt);
    }
}

fn dispatch_native_audio_cancel(
    active: NativeAudioActiveMap,
    identity: AudioOperationId,
    operation: Arc<NativeAudioOperation>,
    attempt: u64,
) {
    let Ok(runtime) = Handle::try_current() else {
        finish_native_audio_cancel(
            &active,
            &identity,
            &operation,
            attempt,
            Err(AudioError::new(
                AudioErrorKind::NativeFailure,
                "native audio cancellation has no runtime to obtain a completion receipt",
            )),
        );
        return;
    };
    runtime.spawn(async move {
        let mut completion = NativeAudioCancelTaskGuard {
            active: active.clone(),
            identity: identity.clone(),
            operation: operation.clone(),
            attempt,
            settled: false,
        };
        let result = operation
            .callback
            .cancel(operation.identity_dto.clone())
            .await
            .map_err(|error| AudioError::new(AudioErrorKind::NativeFailure, error.to_string()));
        finish_native_audio_cancel(&active, &identity, &operation, attempt, result);
        completion.settled = true;
    });
}

fn finish_native_audio_cancel(
    active: &NativeAudioActiveMap,
    identity: &AudioOperationId,
    operation: &Arc<NativeAudioOperation>,
    attempt: u64,
    result: Result<(), AudioError>,
) {
    let mut active_map = active
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let same_operation = active_map
        .get(identity)
        .is_some_and(|current| Arc::ptr_eq(current, operation));
    if !same_operation {
        return;
    }
    let mut lifecycle = operation
        .lifecycle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if matches!(
        &lifecycle.cancellation,
        Some(NativeAudioCancellationState::InFlight { attempt: current }) if *current == attempt
    ) {
        lifecycle.cancellation = Some(match result {
            Ok(()) => NativeAudioCancellationState::Confirmed { attempt },
            Err(error) => NativeAudioCancellationState::Unknown { attempt, error },
        });
    }
    let can_remove = lifecycle.finished
        && matches!(
            &lifecycle.cancellation,
            Some(NativeAudioCancellationState::Confirmed { .. })
        );
    if can_remove {
        active_map.remove(identity);
    }
    drop(lifecycle);
    drop(active_map);
    operation.changed.notify_waiters();
}

async fn cancel_native_audio_identity(
    active: &NativeAudioActiveMap,
    context: &AudioOperationContext,
) -> Result<(), AudioError> {
    let identity = &context.identity;
    let (operation, dispatch, attempt) = {
        let active_map = active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(operation) = active_map.get(identity).cloned() else {
            return Ok(());
        };
        if operation.owner != context.owner
            || !operation
                .incarnation
                .same_instance(&context.operation_incarnation())
        {
            return Ok(());
        }
        let mut lifecycle = operation
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &lifecycle.cancellation {
            Some(NativeAudioCancellationState::InFlight { attempt })
            | Some(NativeAudioCancellationState::Confirmed { attempt }) => {
                (operation.clone(), false, *attempt)
            }
            Some(NativeAudioCancellationState::Unknown { .. }) | None => {
                let attempt = lifecycle.cancellation_attempt.saturating_add(1).max(1);
                lifecycle.cancellation_attempt = attempt;
                lifecycle.cancellation = Some(NativeAudioCancellationState::InFlight { attempt });
                (operation.clone(), true, attempt)
            }
        }
    };
    if dispatch {
        dispatch_native_audio_cancel(active.clone(), identity.clone(), operation.clone(), attempt);
    }
    loop {
        let notified = operation.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let outcome = {
            let lifecycle = operation
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &lifecycle.cancellation {
                Some(NativeAudioCancellationState::Confirmed { .. }) if lifecycle.finished => {
                    Some(Ok(()))
                }
                Some(NativeAudioCancellationState::Confirmed { .. }) => None,
                Some(NativeAudioCancellationState::Unknown { error, .. }) => {
                    Some(Err(error.clone()))
                }
                Some(NativeAudioCancellationState::InFlight { .. }) => None,
                None => Some(Ok(())),
            }
        };
        if let Some(outcome) = outcome {
            return outcome;
        }
        notified.await;
    }
}

impl NativeAudioServiceAdapter {
    async fn execute_with_policy(
        &self,
        mut context: AudioOperationContext,
        operation: AudioOperation,
        route_policy: Option<AudioOperationRoutePolicyDto>,
    ) -> Result<AudioOperationSuccess, AudioError> {
        let capabilities = self.capabilities_for(&context.owner);
        if context.identity.service_epoch != capabilities.service_epoch {
            return Err(AudioError::new(
                AudioErrorKind::InvalidRequest,
                "stale native audio service epoch",
            ));
        }
        if context.timeout_budget_ms == Some(0) {
            return Err(AudioError::new(
                AudioErrorKind::Timeout,
                "audio deadline has elapsed",
            ));
        }
        let incarnation = context.operation_incarnation();
        if !context.runtime_admitted() && !context.claim_operation_admission() {
            return Err(AudioError::new(
                AudioErrorKind::InvalidRequest,
                "native audio context has already been admitted",
            ));
        }
        context = context.with_operation_incarnation(incarnation.clone());
        if let Some(kind) = operation.kind() {
            if !capabilities.supported_operations.contains(&kind) {
                return Err(AudioError::new(
                    AudioErrorKind::Unsupported,
                    "native audio operation is unsupported",
                ));
            }
            if let Some(entry) = capabilities
                .readiness
                .iter()
                .find(|entry| entry.operation == kind)
            {
                let error = match entry.state {
                    AudioReadinessState::Busy => Some(AudioErrorKind::Busy),
                    AudioReadinessState::MissingModel => Some(AudioErrorKind::ModelMissing),
                    AudioReadinessState::Unavailable => Some(AudioErrorKind::Unavailable),
                    _ => None,
                };
                if let Some(kind) = error {
                    return Err(AudioError::new(kind, "native audio backend is not ready"));
                }
            }
        }
        let max_payload_bytes = context
            .max_payload_bytes
            .min(capabilities.max_payload_bytes)
            .min(MAX_AUDIO_PAYLOAD_BYTES as u64);
        let input_bytes = match &operation {
            AudioOperation::Play { audio } => {
                if audio.pcm.is_empty()
                    || audio.pcm.len() % 2 != 0
                    || audio.sample_rate_hz == 0
                    || audio.sample_rate_hz > MAX_AUDIO_SAMPLE_RATE_HZ
                {
                    return Err(AudioError::new(
                        AudioErrorKind::InvalidRequest,
                        "playback requires nonempty PCM16 mono and a valid sample rate",
                    ));
                }
                audio.pcm.len()
            }
            AudioOperation::Transcribe { recording, .. } => recording.audio_bytes.len(),
            _ => 0,
        };
        if input_bytes as u64 > max_payload_bytes
            || (max_payload_bytes == 0 && operation.kind().is_some())
        {
            return Err(AudioError::new(
                AudioErrorKind::MediaTooLarge,
                "audio input exceeds the device transfer limit",
            ));
        }
        let expected_result = expected_result_for(&operation);
        let recording_origin = match &operation {
            AudioOperation::StopRecording { handle } => {
                let matches = self
                    .active
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .iter()
                    .filter(|(_, active)| {
                        active.owner == context.owner
                            && active
                                .lifecycle
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .recording
                                .as_ref()
                                == Some(handle)
                    })
                    .map(|(identity, active)| (identity.clone(), active.incarnation))
                    .collect::<Vec<_>>();
                match matches.as_slice() {
                    [] => {
                        return Err(AudioError::new(
                            AudioErrorKind::NotRecording,
                            "native recording origin was not found for this owner",
                        ))
                    }
                    [origin] => Some(origin.clone()),
                    _ => {
                        return Err(AudioError::new(
                            AudioErrorKind::NativeFailure,
                            "native recording origin is ambiguous",
                        ))
                    }
                }
            }
            _ => None,
        };
        let native_identity =
            AudioOperationId::new(context.identity.generation, capabilities.service_epoch);
        let identity = identity_to_dto(&native_identity);
        let request = AudioOperationRequestDto {
            identity: identity.clone(),
            owner: owner_to_audio_dto(&context.owner)
                .map_err(|message| AudioError::new(AudioErrorKind::InvalidRequest, message))?,
            initiator: context.initiator.as_ref().map(initiator_to_dto),
            timeout_budget_ms: context.timeout_budget_ms,
            route_policy,
            max_payload_bytes,
            operation: operation_to_dto(operation),
        };
        let native_operation = Arc::new(NativeAudioOperation {
            identity_dto: identity.clone(),
            callback: self.callback.clone(),
            owner: context.owner.clone(),
            incarnation,
            lifecycle: std::sync::Mutex::new(NativeAudioOperationLifecycle {
                finished: false,
                cancellation_attempt: 0,
                cancellation: None,
                recording: None,
            }),
            changed: tokio::sync::Notify::new(),
        });
        {
            let mut active = self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if active.contains_key(&context.identity) {
                return Err(AudioError::new(
                    AudioErrorKind::InvalidRequest,
                    "duplicate active native audio identity",
                ));
            }
            active.insert(context.identity.clone(), native_operation.clone());
        }
        let mut cancel_on_drop = NativeCancelOnDrop {
            active: self.active.clone(),
            identity: context.identity.clone(),
            operation: native_operation,
            armed: true,
        };
        let result = self.callback.execute(request).await;
        match &result {
            AudioOperationResultDto::FailedWithoutResource { error } => {
                cancel_on_drop.finish();
                return Err(error_from_dto(error.clone()));
            }
            AudioOperationResultDto::Failed { error } => {
                // A plain failure does not prove that native callbacks or
                // resources ended. The guard issues an exact cancellation.
                return Err(error_from_dto(error.clone()));
            }
            _ => {}
        }
        if !result_matches(expected_result, &result) {
            return Err(AudioError::new(
                AudioErrorKind::NativeFailure,
                "native service returned a result for a different audio operation",
            ));
        }
        let converted = operation_result_from_dto(result, max_payload_bytes);
        if let Ok(AudioOperationSuccess::RecordingStarted { handle }) = &converted {
            let mut lifecycle = cancel_on_drop
                .operation
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lifecycle.recording = Some(handle.clone());
            if let Some(NativeAudioCancellationState::Confirmed { attempt }) =
                &lifecycle.cancellation
            {
                let attempt = *attempt;
                lifecycle.cancellation = Some(NativeAudioCancellationState::Unknown {
                    attempt,
                    error: AudioError::new(
                        AudioErrorKind::NativeFailure,
                        "recording started after its cancellation receipt",
                    ),
                });
            }
        }
        if let (Some((identity, incarnation)), Ok(AudioOperationSuccess::Recording { .. })) =
            (recording_origin, &converted)
        {
            let origin = AudioOperationContext::new(identity, context.owner.clone())
                .with_operation_handle(incarnation);
            cancel_native_audio_identity(&self.active, &origin).await?;
        }
        if converted.is_ok() {
            cancel_on_drop.finish();
        }
        converted
    }
}

#[async_trait]
impl AudioService for NativeAudioServiceAdapter {
    fn capabilities(&self) -> AudioCapabilitySnapshot {
        capabilities_from_dto(self.callback.capabilities())
    }

    async fn execute(
        &self,
        context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        self.execute_with_policy(context, operation, None).await
    }

    async fn execute_host_configured_local(
        &self,
        context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        self.execute_with_policy(
            context,
            operation,
            Some(AudioOperationRoutePolicyDto::HostConfiguredLocalOnly),
        )
        .await
    }

    async fn cancel(&self, context: AudioOperationContext) -> Result<(), AudioError> {
        cancel_native_audio_identity(&self.active, &context).await
    }
}

fn identity_to_dto(value: &AudioOperationId) -> AudioOperationIdDto {
    AudioOperationIdDto {
        id: value.id.clone(),
        generation: value.generation,
        service_epoch: value.service_epoch,
    }
}

fn initiator_to_dto(value: &AudioInitiator) -> AudioInitiatorDto {
    AudioInitiatorDto {
        agent_id: value.agent_id.clone(),
        tool_use_id: value.tool_use_id.clone(),
        request_id: value.request_id.clone(),
    }
}

fn operation_to_dto(value: AudioOperation) -> AudioOperationDto {
    match value {
        AudioOperation::Capture {
            sample_rate_hz,
            format,
        } => AudioOperationDto::Capture {
            sample_rate_hz,
            format,
        },
        AudioOperation::Play { audio } => AudioOperationDto::Play {
            pcm_base64: STANDARD.encode(audio.pcm),
            sample_rate_hz: audio.sample_rate_hz,
        },
        AudioOperation::Transcribe {
            recording,
            language,
        } => AudioOperationDto::Transcribe {
            audio_base64: STANDARD.encode(recording.audio_bytes),
            mime_type: recording.mime_type,
            language,
        },
        AudioOperation::StartRecording {
            sample_rate_hz,
            format,
        } => AudioOperationDto::StartRecording {
            sample_rate_hz,
            format,
        },
        AudioOperation::StopRecording { handle } => AudioOperationDto::StopRecording {
            handle: handle.0 .0,
        },
        AudioOperation::Listen { language } => AudioOperationDto::Listen { language },
        AudioOperation::Synthesize {
            text,
            language,
            rate,
            voice,
        } => AudioOperationDto::Synthesize {
            text,
            language,
            rate,
            voice,
        },
        AudioOperation::Speak {
            text,
            language,
            rate,
            voice,
        } => AudioOperationDto::Speak {
            text,
            language,
            rate,
            voice,
        },
        AudioOperation::Status { handle } => AudioOperationDto::Status {
            handle: handle.map(|handle| handle.0 .0),
        },
        AudioOperation::EndOwner => AudioOperationDto::EndOwner,
    }
}

fn capabilities_from_dto(value: AudioCapabilitySnapshotDto) -> AudioCapabilitySnapshot {
    AudioCapabilitySnapshot {
        service_epoch: value.service_epoch,
        support_revision: value.support_revision,
        supported_operations: value
            .supported_operations
            .into_iter()
            .map(operation_kind_from_dto)
            .collect(),
        readiness: value
            .readiness
            .into_iter()
            .map(|entry| AudioOperationReadiness {
                operation: operation_kind_from_dto(entry.operation),
                state: readiness_from_dto(entry.state),
            })
            .collect(),
        max_payload_bytes: value.max_payload_bytes.min(MAX_AUDIO_PAYLOAD_BYTES as u64),
    }
}

fn operation_kind_from_dto(value: AudioOperationKindDto) -> AudioOperationKind {
    match value {
        AudioOperationKindDto::Capture => AudioOperationKind::Capture,
        AudioOperationKindDto::Play => AudioOperationKind::Play,
        AudioOperationKindDto::Transcribe => AudioOperationKind::Transcribe,
        AudioOperationKindDto::Record => AudioOperationKind::Record,
        AudioOperationKindDto::Listen => AudioOperationKind::Listen,
        AudioOperationKindDto::Synthesize => AudioOperationKind::Synthesize,
        AudioOperationKindDto::Speak => AudioOperationKind::Speak,
    }
}

fn readiness_from_dto(value: AudioReadinessStateDto) -> AudioReadinessState {
    match value {
        AudioReadinessStateDto::Ready => AudioReadinessState::Ready,
        AudioReadinessStateDto::NeedsPermission => AudioReadinessState::NeedsPermission,
        AudioReadinessStateDto::Busy => AudioReadinessState::Busy,
        AudioReadinessStateDto::MissingModel => AudioReadinessState::MissingModel,
        AudioReadinessStateDto::Unavailable => AudioReadinessState::Unavailable,
    }
}

fn operation_result_from_dto(
    value: AudioOperationResultDto,
    max_payload_bytes: u64,
) -> Result<AudioOperationSuccess, AudioError> {
    match value {
        AudioOperationResultDto::RecordingStarted { handle } => {
            if handle.trim().is_empty() {
                return Err(AudioError::new(
                    AudioErrorKind::NativeFailure,
                    "native service returned an empty recording handle",
                ));
            }
            Ok(AudioOperationSuccess::RecordingStarted {
                handle: media_api::RecordingRef(media_api::RecordingId(handle)),
            })
        }
        AudioOperationResultDto::Recording {
            audio_base64,
            mime_type,
        } => {
            let audio_bytes = decode_bounded_audio(&audio_base64, max_payload_bytes)?;
            if audio_bytes.is_empty() {
                return Err(AudioError::new(
                    AudioErrorKind::NativeFailure,
                    "native service returned an empty recording",
                ));
            }
            Ok(AudioOperationSuccess::Recording {
                recording: VoiceRecording {
                    audio_bytes,
                    mime_type,
                },
            })
        }
        AudioOperationResultDto::Transcript {
            text,
            language,
            confidence,
        } => {
            if confidence.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
                return Err(AudioError::new(
                    AudioErrorKind::NativeFailure,
                    "native service returned invalid transcript confidence",
                ));
            }
            Ok(AudioOperationSuccess::Transcript {
                transcript: SttTranscript {
                    text,
                    language,
                    confidence,
                },
            })
        }
        AudioOperationResultDto::Synthesized {
            pcm_base64,
            sample_rate_hz,
        } => {
            let pcm = decode_bounded_audio(&pcm_base64, max_payload_bytes)?;
            if pcm.is_empty()
                || pcm.len() % 2 != 0
                || sample_rate_hz == 0
                || sample_rate_hz > MAX_AUDIO_SAMPLE_RATE_HZ
            {
                return Err(AudioError::new(
                    AudioErrorKind::NativeFailure,
                    "native synthesis returned invalid PCM16 mono audio",
                ));
            }
            Ok(AudioOperationSuccess::Synthesized {
                audio: TtsAudio {
                    pcm,
                    sample_rate_hz,
                },
            })
        }
        AudioOperationResultDto::PlaybackCompleted { duration_ms } => {
            Ok(AudioOperationSuccess::PlaybackCompleted { duration_ms })
        }
        AudioOperationResultDto::Status { status } => Ok(AudioOperationSuccess::Status {
            status: AudioStatus {
                recording: status.recording,
                playing: status.playing,
            },
        }),
        AudioOperationResultDto::OwnerEnded => Ok(AudioOperationSuccess::OwnerEnded),
        AudioOperationResultDto::Failed { error }
        | AudioOperationResultDto::FailedWithoutResource { error } => Err(error_from_dto(error)),
        _ => Err(AudioError::new(
            AudioErrorKind::NativeFailure,
            "native service returned an unknown audio result",
        )),
    }
}

#[derive(Clone, Copy)]
enum ExpectedResult {
    RecordingStarted,
    Recording,
    Transcript,
    Synthesized,
    PlaybackCompleted,
    Status,
    OwnerEnded,
}

fn expected_result_for(operation: &AudioOperation) -> ExpectedResult {
    match operation {
        AudioOperation::Capture { .. } => ExpectedResult::Recording,
        AudioOperation::Play { .. } => ExpectedResult::PlaybackCompleted,
        AudioOperation::Transcribe { .. } => ExpectedResult::Transcript,
        AudioOperation::StartRecording { .. } => ExpectedResult::RecordingStarted,
        AudioOperation::StopRecording { .. } => ExpectedResult::Recording,
        AudioOperation::Listen { .. } => ExpectedResult::Transcript,
        AudioOperation::Synthesize { .. } => ExpectedResult::Synthesized,
        AudioOperation::Speak { .. } => ExpectedResult::PlaybackCompleted,
        AudioOperation::Status { .. } => ExpectedResult::Status,
        AudioOperation::EndOwner => ExpectedResult::OwnerEnded,
    }
}

fn result_matches(expected: ExpectedResult, result: &AudioOperationResultDto) -> bool {
    matches!(
        (expected, result),
        (
            ExpectedResult::RecordingStarted,
            AudioOperationResultDto::RecordingStarted { .. }
        ) | (
            ExpectedResult::Recording,
            AudioOperationResultDto::Recording { .. }
        ) | (
            ExpectedResult::Transcript,
            AudioOperationResultDto::Transcript { .. }
        ) | (
            ExpectedResult::Synthesized,
            AudioOperationResultDto::Synthesized { .. }
        ) | (
            ExpectedResult::PlaybackCompleted,
            AudioOperationResultDto::PlaybackCompleted { .. }
        ) | (
            ExpectedResult::Status,
            AudioOperationResultDto::Status { .. }
        ) | (
            ExpectedResult::OwnerEnded,
            AudioOperationResultDto::OwnerEnded
        )
    )
}

fn decode_bounded_audio(encoded: &str, max_payload_bytes: u64) -> Result<Vec<u8>, AudioError> {
    let encoded_limit = max_payload_bytes
        .min(MAX_AUDIO_PAYLOAD_BYTES as u64)
        .div_ceil(3)
        .saturating_mul(4);
    if encoded.len() as u64 > encoded_limit {
        return Err(AudioError::new(
            AudioErrorKind::MediaTooLarge,
            "audio exceeds the device transfer limit",
        ));
    }
    let bytes = STANDARD.decode(encoded).map_err(|error| {
        AudioError::new(
            AudioErrorKind::NativeFailure,
            format!("native service returned invalid base64 audio: {error}"),
        )
    })?;
    if bytes.len() as u64 > max_payload_bytes {
        return Err(AudioError::new(
            AudioErrorKind::MediaTooLarge,
            "audio exceeds the device transfer limit",
        ));
    }
    Ok(bytes)
}

fn error_from_dto(value: AudioErrorDto) -> AudioError {
    let kind = match value.kind {
        AudioErrorKindDto::PermissionDenied => AudioErrorKind::PermissionDenied,
        AudioErrorKindDto::Busy => AudioErrorKind::Busy,
        AudioErrorKindDto::Cancelled => AudioErrorKind::Cancelled,
        AudioErrorKindDto::Timeout => AudioErrorKind::Timeout,
        AudioErrorKindDto::NoSpeech => AudioErrorKind::NoSpeech,
        AudioErrorKindDto::NotRecording => AudioErrorKind::NotRecording,
        AudioErrorKindDto::Unavailable => AudioErrorKind::Unavailable,
        AudioErrorKindDto::Unsupported => AudioErrorKind::Unsupported,
        AudioErrorKindDto::ModelMissing => AudioErrorKind::ModelMissing,
        AudioErrorKindDto::VoiceMissing => AudioErrorKind::VoiceMissing,
        AudioErrorKindDto::InvalidRequest => AudioErrorKind::InvalidRequest,
        AudioErrorKindDto::SynthesisFailed => AudioErrorKind::SynthesisFailed,
        AudioErrorKindDto::NativeFailure => AudioErrorKind::NativeFailure,
        AudioErrorKindDto::MediaTooLarge => AudioErrorKind::MediaTooLarge,
        _ => AudioErrorKind::NativeFailure,
    };
    AudioError::new(kind, value.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use client::protocol::audio::{
        AudioErrorKindDto, AudioOperationReadinessDto, AudioOperationResultDto,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify;

    struct FakeNativeAudio {
        result: AudioOperationResultDto,
        release_execute: Option<Arc<Notify>>,
        release_second_execute: Option<Arc<Notify>>,
        entered: Arc<Notify>,
        execute_calls: AtomicUsize,
        release_cancel: Option<Arc<Notify>>,
        release_second_cancel: std::sync::Mutex<Option<Arc<Notify>>>,
        cancel_started: Arc<Notify>,
        cancel_finished: Arc<Notify>,
        fail_cancel: AtomicBool,
        cancelled: AtomicUsize,
        cancel_returns: AtomicUsize,
        cancelled_ids: std::sync::Mutex<Vec<AudioOperationIdDto>>,
    }

    #[async_trait]
    impl NativeAudioService for FakeNativeAudio {
        fn capabilities(&self) -> AudioCapabilitySnapshotDto {
            AudioCapabilitySnapshotDto {
                service_epoch: 7,
                support_revision: 1,
                supported_operations: vec![
                    AudioOperationKindDto::Record,
                    AudioOperationKindDto::Capture,
                    AudioOperationKindDto::Play,
                    AudioOperationKindDto::Transcribe,
                    AudioOperationKindDto::Listen,
                    AudioOperationKindDto::Speak,
                    AudioOperationKindDto::Synthesize,
                ],
                readiness: vec![AudioOperationReadinessDto {
                    operation: AudioOperationKindDto::Record,
                    state: AudioReadinessStateDto::Ready,
                }],
                max_payload_bytes: MAX_AUDIO_PAYLOAD_BYTES as u64,
            }
        }

        async fn execute(&self, request: AudioOperationRequestDto) -> AudioOperationResultDto {
            let call = self.execute_calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            let release = match call {
                0 => self.release_execute.as_ref(),
                1 => self.release_second_execute.as_ref(),
                _ => None,
            };
            if let Some(release) = release {
                release.notified().await;
            }
            assert_eq!(request.identity.service_epoch, 7);
            self.result.clone()
        }

        async fn cancel(&self, identity: AudioOperationIdDto) -> Result<(), AudioFfiError> {
            let call = self.cancelled.fetch_add(1, Ordering::SeqCst);
            self.cancelled_ids.lock().unwrap().push(identity);
            self.cancel_started.notify_one();
            let release = if call == 1 {
                self.release_second_cancel.lock().unwrap().clone()
            } else {
                self.release_cancel.clone()
            };
            if let Some(release) = release {
                release.notified().await;
            }
            self.cancel_returns.fetch_add(1, Ordering::SeqCst);
            self.cancel_finished.notify_waiters();
            if self.fail_cancel.load(Ordering::SeqCst) {
                Err(AudioFfiError::NativeFailure {
                    message: "cancel receipt unavailable".into(),
                })
            } else {
                Ok(())
            }
        }
    }

    fn context(max_payload_bytes: u64) -> AudioOperationContext {
        AudioOperationContext::new(
            AudioOperationId::new(2, 7),
            AudioOwner::Session {
                session_id: "session-1".into(),
            },
        )
        .with_timeout_budget_ms(1_000)
        .with_max_payload_bytes(max_payload_bytes)
    }

    fn context_for_identity(identity: &AudioOperationId) -> AudioOperationContext {
        AudioOperationContext::new(
            identity.clone(),
            AudioOwner::Session {
                session_id: "session-1".into(),
            },
        )
        .with_timeout_budget_ms(1_000)
        .with_max_payload_bytes(1024)
    }

    fn adapter(
        result: AudioOperationResultDto,
        release_execute: Option<Arc<Notify>>,
    ) -> (Arc<NativeAudioServiceAdapter>, Arc<FakeNativeAudio>) {
        controlled_adapter(result, release_execute, None, false)
    }

    fn controlled_adapter(
        result: AudioOperationResultDto,
        release_execute: Option<Arc<Notify>>,
        release_cancel: Option<Arc<Notify>>,
        fail_cancel: bool,
    ) -> (Arc<NativeAudioServiceAdapter>, Arc<FakeNativeAudio>) {
        let fake = Arc::new(FakeNativeAudio {
            result,
            release_execute,
            release_second_execute: None,
            entered: Arc::new(Notify::new()),
            execute_calls: AtomicUsize::new(0),
            release_cancel,
            release_second_cancel: std::sync::Mutex::new(None),
            cancel_started: Arc::new(Notify::new()),
            cancel_finished: Arc::new(Notify::new()),
            fail_cancel: AtomicBool::new(fail_cancel),
            cancelled: AtomicUsize::new(0),
            cancel_returns: AtomicUsize::new(0),
            cancelled_ids: std::sync::Mutex::new(Vec::new()),
        });
        let service = Arc::new(NativeAudioServiceAdapter {
            callback: fake.clone(),
            active: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        });
        (service, fake)
    }

    async fn wait_for_cancel(fake: &FakeNativeAudio) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while fake.cancelled.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop guard should deliver targeted native cancellation");
    }

    async fn wait_for_cancel_return(fake: &FakeNativeAudio) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while fake.cancel_returns.load(Ordering::SeqCst) == 0 {
                let notified = fake.cancel_finished.notified();
                if fake.cancel_returns.load(Ordering::SeqCst) == 0 {
                    notified.await;
                }
            }
        })
        .await
        .expect("native cancel callback should return");
    }

    fn speak() -> AudioOperation {
        AudioOperation::Speak {
            text: "hello".into(),
            language: None,
            rate: None,
            voice: None,
        }
    }

    #[tokio::test]
    async fn dropped_native_call_fences_identity_until_cancel_receipt() {
        let release_execute = Arc::new(Notify::new());
        let release_cancel = Arc::new(Notify::new());
        let (service, fake) = controlled_adapter(
            AudioOperationResultDto::PlaybackCompleted { duration_ms: 2 },
            Some(release_execute),
            Some(release_cancel.clone()),
            false,
        );
        let operation_context = context(1024);
        let identity = operation_context.identity.clone();
        let operation_service = service.clone();
        let operation =
            tokio::spawn(
                async move { operation_service.execute(operation_context, speak()).await },
            );
        fake.entered.notified().await;
        operation.abort();
        let _ = operation.await;
        fake.cancel_started.notified().await;

        let duplicate = service
            .execute(context_for_identity(&identity), speak())
            .await
            .unwrap_err();
        assert_eq!(duplicate.kind, AudioErrorKind::InvalidRequest);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        release_cancel.notify_one();
        wait_for_cancel_return(&fake).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !service.active.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("confirmed native cancellation should release the identity");
        let retry = service
            .execute(context_for_identity(&identity), speak())
            .await
            .unwrap();
        assert!(matches!(
            retry,
            AudioOperationSuccess::PlaybackCompleted { .. }
        ));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unknown_native_cancel_receipt_can_retry_the_same_incarnation() {
        let (service, fake) = controlled_adapter(
            AudioOperationResultDto::PlaybackCompleted { duration_ms: 2 },
            Some(Arc::new(Notify::new())),
            None,
            true,
        );
        let operation_context = context(1024);
        let identity = operation_context.identity.clone();
        let operation_service = service.clone();
        let execute_context = operation_context.clone();
        let operation =
            tokio::spawn(async move { operation_service.execute(execute_context, speak()).await });
        fake.entered.notified().await;
        operation.abort();
        let _ = operation.await;
        fake.cancel_started.notified().await;
        wait_for_cancel_return(&fake).await;

        let operation = service
            .active
            .lock()
            .unwrap()
            .get(&identity)
            .cloned()
            .unwrap();
        assert!(matches!(
            &operation.lifecycle.lock().unwrap().cancellation,
            Some(NativeAudioCancellationState::Unknown { attempt: 1, .. })
        ));
        let duplicate = service
            .execute(context_for_identity(&identity), speak())
            .await
            .unwrap_err();
        assert_eq!(duplicate.kind, AudioErrorKind::InvalidRequest);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        fake.fail_cancel.store(false, Ordering::SeqCst);
        service
            .cancel(operation_context)
            .await
            .expect("an exact-handle retry may obtain a later native receipt");
        assert_eq!(fake.cancelled.load(Ordering::SeqCst), 2);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !service.active.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("confirmed retry releases the fenced operation identity");
    }

    #[tokio::test]
    async fn concurrent_native_cancel_retries_are_serialized_and_ignore_late_receipts() {
        let (service, fake) = controlled_adapter(
            AudioOperationResultDto::PlaybackCompleted { duration_ms: 2 },
            Some(Arc::new(Notify::new())),
            None,
            true,
        );
        let operation_context = context(1024);
        let identity = operation_context.identity.clone();
        let operation_service = service.clone();
        let execute_context = operation_context.clone();
        let operation =
            tokio::spawn(async move { operation_service.execute(execute_context, speak()).await });
        fake.entered.notified().await;
        operation.abort();
        let _ = operation.await;
        fake.cancel_started.notified().await;
        wait_for_cancel_return(&fake).await;

        let active = service
            .active
            .lock()
            .unwrap()
            .get(&identity)
            .cloned()
            .unwrap();
        let first_attempt = match &active.lifecycle.lock().unwrap().cancellation {
            Some(NativeAudioCancellationState::Unknown { attempt, .. }) => *attempt,
            state => panic!("first attempt should remain unknown, got {state:?}"),
        };
        fake.fail_cancel.store(false, Ordering::SeqCst);
        let release_second_cancel = Arc::new(Notify::new());
        *fake.release_second_cancel.lock().unwrap() = Some(release_second_cancel.clone());

        let first_service = service.clone();
        let first_context = operation_context.clone();
        let retry_one = tokio::spawn(async move { first_service.cancel(first_context).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while fake.cancelled.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first retry dispatches one new native cancel attempt");

        let second_service = service.clone();
        let second_context = operation_context.clone();
        let retry_two = tokio::spawn(async move { second_service.cancel(second_context).await });
        tokio::task::yield_now().await;
        assert_eq!(fake.cancelled.load(Ordering::SeqCst), 2);

        // Simulate a delayed completion from attempt one after attempt two has
        // registered. It must not settle or remove the newer attempt.
        finish_native_audio_cancel(&service.active, &identity, &active, first_attempt, Ok(()));
        assert!(matches!(
            &active.lifecycle.lock().unwrap().cancellation,
            Some(NativeAudioCancellationState::InFlight { attempt })
                if *attempt == first_attempt + 1
        ));
        release_second_cancel.notify_one();
        retry_one.await.unwrap().unwrap();
        retry_two.await.unwrap().unwrap();
        assert_eq!(fake.cancelled.load(Ordering::SeqCst), 2);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !service.active.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("newer confirmed receipt releases the identity");
    }

    #[tokio::test]
    async fn unpolled_old_cancel_cannot_retarget_a_reused_identity() {
        let fake = Arc::new(FakeNativeAudio {
            result: AudioOperationResultDto::PlaybackCompleted { duration_ms: 2 },
            release_execute: None,
            release_second_execute: Some(Arc::new(Notify::new())),
            entered: Arc::new(Notify::new()),
            execute_calls: AtomicUsize::new(0),
            release_cancel: None,
            release_second_cancel: std::sync::Mutex::new(None),
            cancel_started: Arc::new(Notify::new()),
            cancel_finished: Arc::new(Notify::new()),
            fail_cancel: AtomicBool::new(false),
            cancelled: AtomicUsize::new(0),
            cancel_returns: AtomicUsize::new(0),
            cancelled_ids: std::sync::Mutex::new(Vec::new()),
        });
        let service = Arc::new(NativeAudioServiceAdapter {
            callback: fake.clone(),
            active: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        });
        let operation_context = context(1024);
        let identity = operation_context.identity.clone();
        service
            .execute(operation_context.clone(), speak())
            .await
            .unwrap();

        let stale_cancel = service.cancel(operation_context);
        let second_service = service.clone();
        let second = tokio::spawn(async move {
            second_service
                .execute(context_for_identity(&identity), speak())
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while fake.execute_calls.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second native call has entered");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 2);
        stale_cancel.await.unwrap();
        assert!(!second.is_finished());
        assert_eq!(fake.cancelled.load(Ordering::SeqCst), 0);
        fake.release_second_execute.as_ref().unwrap().notify_one();
        assert!(matches!(
            second.await.unwrap().unwrap(),
            AudioOperationSuccess::PlaybackCompleted { .. }
        ));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn successful_start_disarms_operation_cancel_and_keeps_handle() {
        let (service, fake) = adapter(
            AudioOperationResultDto::RecordingStarted {
                handle: "recording-1".into(),
            },
            None,
        );
        let result = service
            .execute(
                context(1024),
                AudioOperation::StartRecording {
                    sample_rate_hz: 16_000,
                    format: "wav".into(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            AudioOperationSuccess::RecordingStarted { handle }
                if handle.0 .0 == "recording-1"
        ));
        tokio::task::yield_now().await;
        assert_eq!(fake.cancelled.load(Ordering::SeqCst), 0);
    }

    #[derive(Default)]
    struct RecordingNative {
        starts: std::sync::Mutex<Vec<AudioOperationIdDto>>,
        cancels: std::sync::Mutex<Vec<AudioOperationIdDto>>,
        stops: AtomicUsize,
        fail_cancel: AtomicBool,
        hold_origin_cancel: AtomicBool,
        origin_cancel_started: Notify,
        origin_cancel_release: Notify,
    }

    #[async_trait]
    impl NativeAudioService for RecordingNative {
        fn capabilities(&self) -> AudioCapabilitySnapshotDto {
            AudioCapabilitySnapshotDto {
                service_epoch: 7,
                support_revision: 1,
                supported_operations: vec![AudioOperationKindDto::Record],
                readiness: vec![],
                max_payload_bytes: MAX_AUDIO_PAYLOAD_BYTES as u64,
            }
        }

        async fn execute(&self, request: AudioOperationRequestDto) -> AudioOperationResultDto {
            match request.operation {
                AudioOperationDto::StartRecording { .. } => {
                    self.starts.lock().unwrap().push(request.identity);
                    AudioOperationResultDto::RecordingStarted {
                        handle: "same-recording".into(),
                    }
                }
                AudioOperationDto::StopRecording { .. } => {
                    self.stops.fetch_add(1, Ordering::SeqCst);
                    AudioOperationResultDto::Recording {
                        audio_base64: "AQI=".into(),
                        mime_type: "audio/wav".into(),
                    }
                }
                AudioOperationDto::EndOwner => AudioOperationResultDto::OwnerEnded,
                AudioOperationDto::Status { .. } => AudioOperationResultDto::Status {
                    status: client::protocol::audio::AudioStatusDto {
                        recording: false,
                        playing: false,
                    },
                },
                _ => panic!("unexpected recording test operation"),
            }
        }

        async fn cancel(&self, identity: AudioOperationIdDto) -> Result<(), AudioFfiError> {
            let is_origin = self.starts.lock().unwrap().first() == Some(&identity);
            self.cancels.lock().unwrap().push(identity);
            if is_origin && self.hold_origin_cancel.load(Ordering::SeqCst) {
                self.origin_cancel_started.notify_one();
                self.origin_cancel_release.notified().await;
            }
            if self.fail_cancel.load(Ordering::SeqCst) {
                Err(AudioFfiError::NativeFailure {
                    message: "no exact native close receipt".into(),
                })
            } else {
                Ok(())
            }
        }
    }

    fn recording_context(generation: u64, owner: &str) -> AudioOperationContext {
        AudioOperationContext::new(
            AudioOperationId::new(generation, 7),
            AudioOwner::Session {
                session_id: owner.into(),
            },
        )
        .with_timeout_budget_ms(1_000)
        .with_max_payload_bytes(1024)
    }

    fn start_recording() -> AudioOperation {
        AudioOperation::StartRecording {
            sample_rate_hz: 16_000,
            format: "wav".into(),
        }
    }

    #[tokio::test]
    async fn live_native_recording_cancel_uses_the_original_uuid_and_retries_unknown() {
        let fake = Arc::new(RecordingNative::default());
        fake.fail_cancel.store(true, Ordering::SeqCst);
        let service = from_native_audio_service(fake.clone());
        let start = recording_context(210, "owner-a");
        assert!(matches!(
            service
                .execute(start.clone(), start_recording())
                .await
                .unwrap(),
            AudioOperationSuccess::RecordingStarted { .. }
        ));
        let original_native = fake.starts.lock().unwrap()[0].clone();
        assert_ne!(original_native.id, start.identity.id.to_string());
        let mut other_owner = start.clone();
        other_owner.owner = AudioOwner::Session {
            session_id: "owner-b".into(),
        };
        service.cancel(other_owner).await.unwrap();
        let stale = AudioOperationContext::new(start.identity.clone(), start.owner.clone());
        service.cancel(stale).await.unwrap();
        assert!(fake.cancels.lock().unwrap().is_empty());
        assert_eq!(fake.stops.load(Ordering::SeqCst), 0);
        assert!(service.cancel(start.clone()).await.is_err());
        fake.fail_cancel.store(false, Ordering::SeqCst);
        service.cancel(start).await.unwrap();
        assert_eq!(
            *fake.cancels.lock().unwrap(),
            vec![original_native.clone(), original_native]
        );
    }

    #[tokio::test]
    async fn native_cancel_waits_for_late_start_result_before_confirming() {
        let release = Arc::new(Notify::new());
        let (service, fake) = adapter(
            AudioOperationResultDto::RecordingStarted {
                handle: "late-native-start".into(),
            },
            Some(release.clone()),
        );
        let context = recording_context(211, "owner-a");
        let starting_service = service.clone();
        let starting_context = context.clone();
        let start = tokio::spawn(async move {
            starting_service
                .execute(starting_context, start_recording())
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), fake.entered.notified())
            .await
            .unwrap();
        let cancelling_service = service.clone();
        let cancelling_context = context.clone();
        let mut cancel =
            tokio::spawn(async move { cancelling_service.cancel(cancelling_context).await });
        tokio::time::timeout(Duration::from_secs(1), fake.cancel_started.notified())
            .await
            .unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut cancel)
            .await
            .is_err());
        release.notify_one();
        assert!(matches!(
            start.await.unwrap().unwrap(),
            AudioOperationSuccess::RecordingStarted { .. }
        ));
        assert!(
            cancel.await.unwrap().is_err(),
            "a late start invalidates the earlier receipt"
        );
        service.cancel(context).await.unwrap();
        assert_eq!(fake.cancelled.load(Ordering::SeqCst), 2);
        let ids = fake.cancelled_ids.lock().unwrap();
        assert_eq!(ids[0], ids[1]);
    }

    #[tokio::test]
    async fn native_recording_stop_closes_only_the_matching_owner_and_origin() {
        let fake = Arc::new(RecordingNative::default());
        let service = from_native_audio_service(fake.clone());
        let start_a = recording_context(220, "owner-a");
        let start_b = recording_context(221, "owner-b");
        let AudioOperationSuccess::RecordingStarted { handle: a } =
            service.execute(start_a, start_recording()).await.unwrap()
        else {
            panic!("start a")
        };
        let AudioOperationSuccess::RecordingStarted { handle: b } =
            service.execute(start_b, start_recording()).await.unwrap()
        else {
            panic!("start b")
        };
        assert_eq!(a, b);
        service
            .execute(
                recording_context(222, "owner-a"),
                AudioOperation::StopRecording { handle: a },
            )
            .await
            .unwrap();
        assert_eq!(
            *fake.cancels.lock().unwrap(),
            vec![fake.starts.lock().unwrap()[0].clone()]
        );
        service
            .execute(
                recording_context(223, "owner-b"),
                AudioOperation::StopRecording { handle: b },
            )
            .await
            .unwrap();
        assert_eq!(*fake.cancels.lock().unwrap(), *fake.starts.lock().unwrap());
    }

    #[tokio::test]
    async fn cancel_of_stop_waits_for_original_recording_close() {
        let fake = Arc::new(RecordingNative::default());
        fake.hold_origin_cancel.store(true, Ordering::SeqCst);
        let service = from_native_audio_service(fake.clone());
        let start = recording_context(224, "owner-a");
        let AudioOperationSuccess::RecordingStarted { handle } =
            service.execute(start, start_recording()).await.unwrap()
        else {
            panic!("recording start")
        };
        let stop_context = recording_context(225, "owner-a");
        let stopping_service = service.clone();
        let stopping_context = stop_context.clone();
        let stop = tokio::spawn(async move {
            stopping_service
                .execute(stopping_context, AudioOperation::StopRecording { handle })
                .await
        });
        tokio::time::timeout(
            Duration::from_secs(1),
            fake.origin_cancel_started.notified(),
        )
        .await
        .unwrap();
        let cancelling_service = service.clone();
        let mut cancel = tokio::spawn(async move { cancelling_service.cancel(stop_context).await });
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut cancel)
            .await
            .is_err());
        assert_eq!(
            fake.cancels.lock().unwrap().len(),
            2,
            "Stop cancel must reach its own native UUID"
        );
        fake.origin_cancel_release.notify_one();
        assert!(matches!(
            stop.await.unwrap().unwrap(),
            AudioOperationSuccess::Recording { .. }
        ));
        cancel.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn native_duplicate_recording_handle_does_not_choose_an_origin() {
        let fake = Arc::new(RecordingNative::default());
        let service = from_native_audio_service(fake.clone());
        let a = recording_context(230, "owner-a");
        let b = recording_context(231, "owner-a");
        let AudioOperationSuccess::RecordingStarted { handle } =
            service.execute(a.clone(), start_recording()).await.unwrap()
        else {
            panic!("start")
        };
        service.execute(b.clone(), start_recording()).await.unwrap();
        assert_eq!(
            service
                .execute(
                    recording_context(232, "owner-a"),
                    AudioOperation::StopRecording { handle }
                )
                .await
                .unwrap_err()
                .kind,
            AudioErrorKind::NativeFailure
        );
        assert!(fake.cancels.lock().unwrap().is_empty());
        assert_eq!(fake.stops.load(Ordering::SeqCst), 0);
        service.cancel(a).await.unwrap();
        service.cancel(b).await.unwrap();
        assert_eq!(fake.cancels.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn native_recording_stop_rejects_another_owners_handle_before_dispatch() {
        let fake = Arc::new(RecordingNative::default());
        let service = from_native_audio_service(fake.clone());
        let start = recording_context(235, "owner-a");
        let AudioOperationSuccess::RecordingStarted { handle } = service
            .execute(start.clone(), start_recording())
            .await
            .unwrap()
        else {
            panic!("start")
        };
        let error = service
            .execute(
                recording_context(236, "owner-b"),
                AudioOperation::StopRecording { handle },
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, AudioErrorKind::NotRecording);
        assert_eq!(fake.stops.load(Ordering::SeqCst), 0);
        assert!(fake.cancels.lock().unwrap().is_empty());
        service.cancel(start).await.unwrap();
    }

    struct EmptyMediaStore;

    #[async_trait]
    impl media_api::MediaResourceStore for EmptyMediaStore {
        async fn put(
            &self,
            _: &media_api::MediaOperationContext,
            _: media_api::MediaPayload,
        ) -> Result<media_api::MediaAsset, media_api::MediaError> {
            unreachable!("direct recording attachment does not use the media store")
        }
        async fn get(
            &self,
            _: &media_api::MediaOperationContext,
            _: &media_api::MediaRef,
            _: u64,
        ) -> Result<media_api::MediaPayload, media_api::MediaError> {
            unreachable!("direct recording attachment does not use the media store")
        }
        async fn delete(
            &self,
            _: &media_api::MediaOperationContext,
            _: &media_api::MediaRef,
        ) -> Result<(), media_api::MediaError> {
            unreachable!("direct recording attachment does not use the media store")
        }
    }

    #[tokio::test]
    async fn scoped_unified_native_recording_needs_the_original_native_close_receipt() {
        for confirmed in [false, true] {
            let fake = Arc::new(RecordingNative::default());
            fake.fail_cancel.store(!confirmed, Ordering::SeqCst);
            let native = from_native_audio_service(fake.clone());
            let unified = Arc::new(media_runtime::UnifiedAudioService::new(
                native,
                Vec::new(),
                None,
                media_runtime::AudioRoutingSettings::default(),
            ));
            let media = media_runtime::MediaRuntimeBuilder::new(Arc::new(EmptyMediaStore)).build();
            let host = media_runtime::MediaAttachmentHost::new();
            let attachment = host.attach(media).unwrap();
            let scoped = attachment.audio_service(unified).unwrap();
            let start = recording_context(240, "owner-a");
            assert!(matches!(
                scoped.execute(start, start_recording()).await.unwrap(),
                AudioOperationSuccess::RecordingStarted { .. }
            ));
            let origin = fake.starts.lock().unwrap()[0].clone();
            let result = attachment.revoke(Duration::from_secs(1)).await;
            let calls = fake.cancels.lock().unwrap();
            assert!(
                !calls.is_empty(),
                "revocation must reach the native origin callback"
            );
            assert!(calls.iter().all(|identity| *identity == origin));
            if confirmed {
                assert_eq!(result, media_runtime::MediaAttachmentRevocation::Confirmed);
            } else {
                assert!(matches!(
                    result,
                    media_runtime::MediaAttachmentRevocation::Unknown { .. }
                ));
            }
        }
    }

    #[tokio::test]
    async fn empty_recording_handle_is_rejected_and_cancelled() {
        let (service, fake) = adapter(
            AudioOperationResultDto::RecordingStarted {
                handle: "  ".into(),
            },
            None,
        );
        let error = service
            .execute(
                context(1024),
                AudioOperation::StartRecording {
                    sample_rate_hz: 16_000,
                    format: "wav".into(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, AudioErrorKind::NativeFailure);
        wait_for_cancel(&fake).await;
    }

    #[tokio::test]
    async fn dropping_pending_operation_sends_its_exact_identity_to_cancel() {
        let release = Arc::new(Notify::new());
        let (service, fake) = adapter(
            AudioOperationResultDto::PlaybackCompleted { duration_ms: 2 },
            Some(release),
        );
        let entered = fake.entered.clone();
        let task = tokio::spawn(async move {
            service
                .execute(
                    context(1024),
                    AudioOperation::Speak {
                        text: "hello".into(),
                        language: None,
                        rate: None,
                        voice: None,
                    },
                )
                .await
        });
        entered.notified().await;
        task.abort();
        let _ = task.await;
        wait_for_cancel(&fake).await;
        let cancelled = fake.cancelled_ids.lock().unwrap();
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].service_epoch, 7);
        assert!(!cancelled[0].id.is_empty());
    }

    #[tokio::test]
    async fn wrong_result_variant_is_rejected_and_targeted_cancelled() {
        let (service, fake) = adapter(
            AudioOperationResultDto::RecordingStarted {
                handle: "recording-1".into(),
            },
            None,
        );
        let error = service
            .execute(
                context(1024),
                AudioOperation::Speak {
                    text: "hello".into(),
                    language: None,
                    rate: None,
                    voice: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, AudioErrorKind::NativeFailure);
        wait_for_cancel(&fake).await;
    }

    #[tokio::test]
    async fn oversized_and_invalid_pcm_results_are_rejected_and_cancelled() {
        let (oversized, oversized_fake) = adapter(
            AudioOperationResultDto::Synthesized {
                pcm_base64: STANDARD.encode([1_u8, 2, 3, 4]),
                sample_rate_hz: 22_050,
            },
            None,
        );
        let oversize_error = oversized
            .execute(
                context(2),
                AudioOperation::Synthesize {
                    text: "hello".into(),
                    language: None,
                    rate: None,
                    voice: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(oversize_error.kind, AudioErrorKind::MediaTooLarge);
        wait_for_cancel(&oversized_fake).await;

        let (invalid, invalid_fake) = adapter(
            AudioOperationResultDto::Synthesized {
                pcm_base64: STANDARD.encode([1_u8, 2]),
                sample_rate_hz: MAX_AUDIO_SAMPLE_RATE_HZ + 1,
            },
            None,
        );
        assert!(invalid
            .execute(
                context(1024),
                AudioOperation::Synthesize {
                    text: "hello".into(),
                    language: None,
                    rate: None,
                    voice: None,
                },
            )
            .await
            .is_err());
        wait_for_cancel(&invalid_fake).await;
    }

    #[tokio::test]
    async fn ordinary_failure_requests_native_cancel_instead_of_assuming_cleanup() {
        let (service, fake) = adapter(
            AudioOperationResultDto::Failed {
                error: AudioErrorDto {
                    kind: AudioErrorKindDto::PermissionDenied,
                    message: "permission denied".into(),
                },
            },
            None,
        );
        let error = service
            .execute(context(1024), AudioOperation::Listen { language: None })
            .await
            .unwrap_err();
        assert_eq!(error.kind, AudioErrorKind::PermissionDenied);
        wait_for_cancel(&fake).await;
    }

    #[tokio::test]
    async fn proven_resource_free_failure_is_not_retried_as_cancellation() {
        let (service, fake) = adapter(
            AudioOperationResultDto::FailedWithoutResource {
                error: AudioErrorDto {
                    kind: AudioErrorKindDto::PermissionDenied,
                    message: "permission denied".into(),
                },
            },
            None,
        );
        let error = service
            .execute(context(1024), AudioOperation::Listen { language: None })
            .await
            .unwrap_err();
        assert_eq!(error.kind, AudioErrorKind::PermissionDenied);
        tokio::task::yield_now().await;
        assert_eq!(fake.cancelled.load(Ordering::SeqCst), 0);
    }
}
