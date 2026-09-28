//! Shared UniFFI boundary and platform-api adapter for native audio services.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use client::protocol::audio::{
    AudioCapabilitySnapshotDto, AudioErrorDto, AudioErrorKindDto, AudioInitiatorDto,
    AudioOperationDto, AudioOperationIdDto, AudioOperationKindDto, AudioOperationRequestDto,
    AudioOperationResultDto, AudioOwnerDto, AudioReadinessStateDto, MAX_AUDIO_PAYLOAD_BYTES,
    MAX_AUDIO_SAMPLE_RATE_HZ,
};
use platform_api::audio::{
    AudioCapabilitySnapshot, AudioError, AudioErrorKind, AudioInitiator, AudioOperation,
    AudioOperationContext, AudioOperationId, AudioOperationKind, AudioOperationReadiness,
    AudioOperationSuccess, AudioOwner, AudioReadinessState, AudioRecordingHandle, AudioService,
    AudioStatus,
};
use platform_api::{SttTranscript, TtsAudio, VoiceRecording};
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
    async fn execute(&self, request: AudioOperationRequestDto) -> AudioOperationResultDto;
    /// Cancel only one pending operation identity.
    async fn cancel(&self, identity: AudioOperationIdDto) -> Result<(), AudioFfiError>;
}

/// Adapt a platform callback into the domain service used by tools and hosts.
pub fn from_native_audio_service(callback: Arc<dyn NativeAudioService>) -> Arc<dyn AudioService> {
    Arc::new(NativeAudioServiceAdapter { callback })
}

/// Shared native-facing audio collection ceiling derived from the bridge's
/// existing frame budget and framing allowance.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn max_audio_payload_bytes() -> u64 {
    MAX_AUDIO_PAYLOAD_BYTES as u64
}

struct NativeAudioServiceAdapter {
    callback: Arc<dyn NativeAudioService>,
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
        let expected_result = expected_result_for(&operation);
        let identity = identity_to_dto(&context.identity);
        let request = AudioOperationRequestDto {
            identity: identity.clone(),
            owner: owner_to_dto(&context.owner),
            initiator: context.initiator.as_ref().map(initiator_to_dto),
            timeout_budget_ms: context.timeout_budget_ms,
            max_payload_bytes: context
                .max_payload_bytes
                .min(MAX_AUDIO_PAYLOAD_BYTES as u64),
            operation: operation_to_dto(operation),
        };
        let mut cancel_on_drop = NativeCancelOnDrop {
            callback: self.callback.clone(),
            identity,
            armed: true,
        };
        let result = self.callback.execute(request).await;
        if let AudioOperationResultDto::Failed { error } = &result {
            cancel_on_drop.armed = false;
            return Err(error_from_dto(error.clone()));
        }
        if !result_matches(expected_result, &result) {
            return Err(AudioError::new(
                AudioErrorKind::NativeFailure,
                "native service returned a result for a different audio operation",
            ));
        }
        let converted = operation_result_from_dto(result, context.max_payload_bytes);
        if converted.is_ok() {
            cancel_on_drop.armed = false;
        }
        converted
    }

    async fn cancel(&self, identity: AudioOperationId) -> Result<(), AudioError> {
        self.callback
            .cancel(identity_to_dto(&identity))
            .await
            .map_err(|error| AudioError::new(AudioErrorKind::NativeFailure, error.to_string()))
    }
}

struct NativeCancelOnDrop {
    callback: Arc<dyn NativeAudioService>,
    identity: AudioOperationIdDto,
    armed: bool,
}

impl Drop for NativeCancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let callback = self.callback.clone();
        let identity = self.identity.clone();
        if let Ok(runtime) = Handle::try_current() {
            runtime.spawn(async move {
                let _ = callback.cancel(identity).await;
            });
        }
    }
}

fn identity_to_dto(value: &AudioOperationId) -> AudioOperationIdDto {
    AudioOperationIdDto {
        id: value.id.clone(),
        generation: value.generation,
        service_epoch: value.service_epoch,
    }
}

fn owner_to_dto(value: &AudioOwner) -> AudioOwnerDto {
    match value {
        AudioOwner::Session { session_id } => AudioOwnerDto::Session {
            session_id: session_id.clone(),
        },
        AudioOwner::LocalApp {
            app_id,
            runtime_generation,
        } => AudioOwnerDto::LocalApp {
            app_id: app_id.clone(),
            runtime_generation: *runtime_generation,
        },
        AudioOwner::Ui { instance_id } => AudioOwnerDto::Ui {
            instance_id: instance_id.clone(),
        },
        AudioOwner::System { instance_id } => AudioOwnerDto::System {
            instance_id: instance_id.clone(),
        },
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
        AudioOperation::StartRecording {
            sample_rate_hz,
            format,
        } => AudioOperationDto::StartRecording {
            sample_rate_hz,
            format,
        },
        AudioOperation::StopRecording { handle } => {
            AudioOperationDto::StopRecording { handle: handle.0 }
        }
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
            handle: handle.map(|handle| handle.0),
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
                handle: AudioRecordingHandle(handle),
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
        AudioOperationResultDto::Failed { error } => Err(error_from_dto(error)),
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify;

    struct FakeNativeAudio {
        result: AudioOperationResultDto,
        release_execute: Option<Arc<Notify>>,
        entered: Arc<Notify>,
        cancelled: AtomicUsize,
        cancelled_ids: std::sync::Mutex<Vec<AudioOperationIdDto>>,
    }

    #[async_trait]
    impl NativeAudioService for FakeNativeAudio {
        fn capabilities(&self) -> AudioCapabilitySnapshotDto {
            AudioCapabilitySnapshotDto {
                service_epoch: 7,
                support_revision: 1,
                supported_operations: vec![AudioOperationKindDto::Record],
                readiness: vec![AudioOperationReadinessDto {
                    operation: AudioOperationKindDto::Record,
                    state: AudioReadinessStateDto::Ready,
                }],
                max_payload_bytes: MAX_AUDIO_PAYLOAD_BYTES as u64,
            }
        }

        async fn execute(&self, request: AudioOperationRequestDto) -> AudioOperationResultDto {
            self.entered.notify_one();
            if let Some(release) = &self.release_execute {
                release.notified().await;
            }
            assert_eq!(request.identity.service_epoch, 7);
            self.result.clone()
        }

        async fn cancel(&self, identity: AudioOperationIdDto) -> Result<(), AudioFfiError> {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
            self.cancelled_ids.lock().unwrap().push(identity);
            Ok(())
        }
    }

    fn context(max_payload_bytes: u64) -> AudioOperationContext {
        AudioOperationContext {
            identity: AudioOperationId::new(2, 7),
            owner: AudioOwner::Session {
                session_id: "session-1".into(),
            },
            initiator: None,
            timeout_budget_ms: Some(1_000),
            max_payload_bytes,
        }
    }

    fn adapter(
        result: AudioOperationResultDto,
        release_execute: Option<Arc<Notify>>,
    ) -> (Arc<dyn AudioService>, Arc<FakeNativeAudio>) {
        let fake = Arc::new(FakeNativeAudio {
            result,
            release_execute,
            entered: Arc::new(Notify::new()),
            cancelled: AtomicUsize::new(0),
            cancelled_ids: std::sync::Mutex::new(Vec::new()),
        });
        let service = from_native_audio_service(fake.clone());
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
                if handle.0 == "recording-1"
        ));
        tokio::task::yield_now().await;
        assert_eq!(fake.cancelled.load(Ordering::SeqCst), 0);
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
    async fn terminal_failure_is_not_retried_as_cancellation() {
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
        tokio::task::yield_now().await;
        assert_eq!(fake.cancelled.load(Ordering::SeqCst), 0);
    }
}
