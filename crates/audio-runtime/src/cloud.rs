//! SDK-only cloud speech adapter; no provider codecs or credentials live here.
use crate::{check_ready, CloudBinding};
use async_trait::async_trait;
use lingxi_core::host::audio::*;
use lingxi_core::host::{SttTranscript, TtsAudio};
use lingxi_llm_client as sdk;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Exact provider snapshot pinned by the host before dispatch.
#[derive(Clone)]
pub struct HostedCloudRoute {
    /// Immutable SDK configuration revision, independent of chat models.
    pub snapshot: sdk::ClientSnapshot,
    /// Exact profile; never guessed from a provider family or model string.
    pub profile_name: String,
    /// Stable account identity corresponding to supplied credentials.
    pub account_scope: String,
}
impl HostedCloudRoute {
    fn sdk_route(&self) -> sdk::audio::AudioRoute {
        sdk::audio::AudioRoute::new(self.profile_name.clone(), self.account_scope.clone())
    }
}
/// Host credential and trusted-session boundary. Local audio requires no host.
#[async_trait]
pub trait CloudAudioHost: Send + Sync {
    /// Resolve the current session or explicit profile without fetching secrets.
    /// FollowSession must reject non-session owners without trusted session state.
    fn resolve(
        &self,
        owner: &AudioOwner,
        binding: &CloudBinding,
    ) -> Result<HostedCloudRoute, AudioError>;
    /// Supply credentials only for the exact pinned profile/account.
    async fn request_options(
        &self,
        route: &HostedCloudRoute,
        context: &AudioOperationContext,
    ) -> Result<sdk::RequestOptions, AudioError>;
    /// Exact container supported by the native bounded capture implementation.
    /// Android uses WAV; Apple hosts may override with m4a.
    fn capture_format(&self) -> &str {
        "wav"
    }
    /// Observe authoritative SDK usage for host accounting.
    fn record_usage(&self, _context: &AudioOperationContext, _usage: &sdk::audio::AudioUsage) {}
}
fn sdk_error(error: sdk::audio::AudioError) -> AudioError {
    let kind = match error.kind {
        sdk::audio::AudioErrorKind::InvalidRequest => AudioErrorKind::InvalidRequest,
        sdk::audio::AudioErrorKind::Unsupported => AudioErrorKind::Unsupported,
        sdk::audio::AudioErrorKind::MediaTooLarge => AudioErrorKind::MediaTooLarge,
        _ => AudioErrorKind::Unavailable,
    };
    AudioError::new(kind, error.to_string())
}
fn sdk_kind(kind: AudioOperationKind) -> Result<sdk::audio::AudioOperation, AudioError> {
    match kind {
        AudioOperationKind::Listen | AudioOperationKind::Transcribe => {
            Ok(sdk::audio::AudioOperation::FileTranscription)
        }
        AudioOperationKind::Synthesize | AudioOperationKind::Speak => {
            Ok(sdk::audio::AudioOperation::Synthesis)
        }
        _ => Err(super::unsupported("cloud operation is unsupported")),
    }
}
pub(crate) fn preflight(
    route: &HostedCloudRoute,
    kind: AudioOperationKind,
    model: Option<&str>,
    device: &AudioCapabilitySnapshot,
) -> Result<(), AudioError> {
    if route.profile_name.trim().is_empty() || route.account_scope.trim().is_empty() {
        return Err(AudioError::new(
            AudioErrorKind::InvalidRequest,
            "cloud audio requires an exact profile and account",
        ));
    }
    let operation = sdk_kind(kind)?;
    let caps = route
        .snapshot
        .audio()
        .capabilities(&route.sdk_route())
        .map_err(sdk_error)?;
    if !caps.supports(operation) {
        return Err(super::unsupported(
            "selected provider does not support this audio operation",
        ));
    }
    let declared = caps.model(operation, model).map_err(sdk_error)?;
    if matches!(operation, sdk::audio::AudioOperation::Synthesis)
        && (declared.pcm_sample_rate_hz.is_none()
            || declared.pcm_channels != Some(1)
            || declared.pcm_bits_per_sample != Some(16))
    {
        return Err(super::unsupported(
            "selected audio adapter does not declare device-compatible PCM geometry",
        ));
    }
    if matches!(operation, sdk::audio::AudioOperation::Synthesis)
        && !declared.formats.contains(&sdk::audio::AudioFormat::Pcm16Le)
    {
        return Err(super::unsupported(
            "selected audio model does not support device PCM playback",
        ));
    }
    let resource = match kind {
        AudioOperationKind::Listen => Some(AudioOperationKind::Capture),
        AudioOperationKind::Speak => Some(AudioOperationKind::Play),
        _ => None,
    };
    if let Some(resource) = resource {
        if !device.supported_operations.contains(&resource) {
            return Err(super::unsupported(
                "cloud audio requires a device capability which is unsupported",
            ));
        }
    }
    Ok(())
}
fn remaining(
    context: &AudioOperationContext,
    started: Instant,
) -> Result<AudioOperationContext, AudioError> {
    let mut child = context.clone();
    let budget = context.timeout_budget_ms.unwrap_or(30_000);
    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let left = budget.saturating_sub(elapsed);
    if left == 0 {
        return Err(AudioError::new(
            AudioErrorKind::Timeout,
            "audio deadline has elapsed",
        ));
    }
    child.timeout_budget_ms = Some(left);
    Ok(child)
}
pub(crate) async fn execute(
    host: &dyn CloudAudioHost,
    device: &Arc<dyn AudioService>,
    route: &HostedCloudRoute,
    model: Option<String>,
    context: AudioOperationContext,
    operation: AudioOperation,
    started: Instant,
) -> Result<AudioOperationSuccess, AudioError> {
    let mut options = host.request_options(route, &context).await?;
    // An authenticator may be host-provided, but the SDK's stable resource
    // account identity must match the route captured before asynchronous work.
    if options
        .account_scope
        .as_ref()
        .is_some_and(|scope| scope != &route.account_scope)
    {
        return Err(AudioError::new(
            AudioErrorKind::InvalidRequest,
            "audio credentials do not match the pinned account",
        ));
    }
    options.account_scope = Some(route.account_scope.clone());
    options.fallback_credentials.clear();
    let play = matches!(&operation, AudioOperation::Speak { .. });
    let operation = match operation {
        AudioOperation::Listen { language } => {
            check_ready(
                &device.capabilities_for(&context.owner),
                AudioOperationKind::Capture,
            )?;
            let captured = device
                .execute(
                    remaining(&context, started)?,
                    AudioOperation::Capture {
                        sample_rate_hz: 24_000,
                        format: host.capture_format().into(),
                    },
                )
                .await?;
            let AudioOperationSuccess::Recording { recording } = captured else {
                return Err(AudioError::new(
                    AudioErrorKind::NativeFailure,
                    "device capture returned a different result",
                ));
            };
            AudioOperation::Transcribe {
                recording,
                language,
            }
        }
        operation => operation,
    };
    options.total_timeout = remaining(&context, started)?
        .timeout_budget_ms
        .map(Duration::from_millis);
    let service = route.snapshot.audio();
    match operation {
        AudioOperation::Transcribe {
            recording,
            language,
        } => {
            if recording.audio_bytes.is_empty()
                || recording.audio_bytes.len() as u64 > context.max_payload_bytes
            {
                return Err(AudioError::new(
                    AudioErrorKind::MediaTooLarge,
                    "recording exceeds the audio payload bound or is empty",
                ));
            }
            let filename = match recording.mime_type.as_str() {
                "audio/m4a" | "audio/mp4" => "capture.m4a",
                "audio/wav" | "audio/x-wav" => "capture.wav",
                "audio/mpeg" => "capture.mp3",
                "audio/flac" => "capture.flac",
                "audio/ogg" => "capture.ogg",
                _ => "capture.audio",
            };
            let input = sdk::audio::AudioInput::from_bytes(
                filename,
                recording.mime_type,
                recording.audio_bytes,
            );
            let result = service
                .transcribe(
                    &route.sdk_route(),
                    input,
                    &sdk::audio::TranscriptionRequest {
                        model,
                        language,
                        raw_format: None,
                    },
                    &options,
                )
                .await
                .map_err(sdk_error)?;
            host.record_usage(&context, &result.usage);
            Ok(AudioOperationSuccess::Transcript {
                transcript: SttTranscript {
                    text: result.text,
                    language: result.language,
                    confidence: None,
                },
            })
        }
        AudioOperation::Synthesize {
            text,
            language,
            voice,
            rate,
        }
        | AudioOperation::Speak {
            text,
            language,
            voice,
            rate,
        } => {
            // Rate is not uniformly supported by provider audio APIs. Reject an
            // effective override instead of silently changing requested speech.
            if rate.is_some_and(|rate| !rate.is_finite() || rate != 1.0) {
                return Err(super::unsupported(
                    "selected cloud speech adapter does not support a playback rate override",
                ));
            }
            let output = service
                .synthesize(
                    &route.sdk_route(),
                    &sdk::audio::SynthesisRequest {
                        model,
                        text,
                        voice,
                        language,
                        format: sdk::audio::AudioFormat::Pcm16Le,
                    },
                    &options,
                )
                .await
                .map_err(sdk_error)?;
            let sdk::audio::AudioOutput::Stream(stream) = output else {
                return Err(super::unsupported(
                    "provider returned URL audio; device playback requires a bounded PCM stream",
                ));
            };
            let max_bytes = usize::try_from(context.max_payload_bytes).unwrap_or(usize::MAX);
            let collected = stream.collect(max_bytes).await.map_err(sdk_error)?;
            let metadata = collected.metadata;
            if metadata.format != sdk::audio::AudioFormat::Pcm16Le
                || metadata.channels != Some(1)
                || metadata.bits_per_sample != Some(16)
                || collected.bytes.len() % 2 != 0
            {
                return Err(super::unsupported(
                    "provider did not return PCM16 little-endian mono audio",
                ));
            }
            let sample_rate_hz = metadata
                .sample_rate_hz
                .filter(|rate| *rate > 0)
                .ok_or_else(|| super::unsupported("provider PCM sample rate is unknown"))?;
            host.record_usage(&context, &collected.usage);
            let audio = TtsAudio {
                pcm: collected.bytes.to_vec(),
                sample_rate_hz,
            };
            if play {
                check_ready(
                    &device.capabilities_for(&context.owner),
                    AudioOperationKind::Play,
                )?;
                device
                    .execute(
                        remaining(&context, started)?,
                        AudioOperation::Play { audio },
                    )
                    .await
            } else {
                Ok(AudioOperationSuccess::Synthesized { audio })
            }
        }
        _ => Err(super::unsupported("cloud operation is unsupported")),
    }
}
