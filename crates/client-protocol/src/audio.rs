//! Audio operation and capability DTOs shared by the desktop bridge and
//! mobile UniFFI surfaces.
//!
//! This module contains transport data only. Device configuration and
//! execution rules live in the device-local AudioService; the platform-api
//! domain types are mapped explicitly at their host boundaries.

use serde::{Deserialize, Serialize};

/// Existing bridge frame ceiling. Kept here because this leaf contract cannot
/// depend on `bridge`; `bridge` has a parity test against its own frame limit.
pub const AUDIO_BRIDGE_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Room reserved for the audio response envelope, command fields and metadata.
pub const AUDIO_RESPONSE_FRAMING_ALLOWANCE_BYTES: usize = 64 * 1024;
/// Largest base64 string that fits in one bounded bridge frame.
pub const MAX_AUDIO_BASE64_LENGTH: usize =
    AUDIO_BRIDGE_FRAME_BYTES - AUDIO_RESPONSE_FRAMING_ALLOWANCE_BYTES;
/// Largest raw audio length whose base64 encoding fits the frame, aligned to
/// PCM16 samples.
pub const MAX_AUDIO_PAYLOAD_BYTES: usize = (MAX_AUDIO_BASE64_LENGTH / 4) * 3;
/// Highest plausible device PCM sample rate accepted by the shared clients.
pub const MAX_AUDIO_SAMPLE_RATE_HZ: u32 = 768_000;

/// Globally unique operation identity plus its reuse and service-instance
/// guards. Numeric fields stay within JavaScript's safe integer range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AudioOperationIdDto {
    /// UUID v4 shared by one request across all bridges.
    pub id: String,
    /// Monotonic within this producer's process lifetime. Independent
    /// producers may reuse values; compare the full UUID/generation/epoch tuple.
    pub generation: u64,
    /// Device AudioService instance epoch from its capability snapshot.
    pub service_epoch: u64,
}

/// Trusted device-resource owner created by the host, never by a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AudioOwnerDto {
    /// Chat session whose tool calls may start and later stop a recording.
    Session {
        /// Stable session identifier.
        session_id: String,
    },
    /// Local App runtime instance.
    LocalApp {
        /// Validated app identifier.
        app_id: String,
        /// Runtime generation that prevents stale requests stopping a new run.
        runtime_generation: u64,
    },
    /// UI app instance.
    Ui {
        /// Stable UI instance identifier.
        instance_id: String,
    },
    /// Host-owned operation not tied to a chat or app runtime.
    System {
        /// Stable host instance identifier.
        instance_id: String,
    },
}

/// Trusted call attribution kept separate from the stable resource owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AudioInitiatorDto {
    /// Agent identifier, when invoked from an agent tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Tool-use identifier, when invoked by a model tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Host request identifier, when invoked by a Local App or UI request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// One host-created audio request, including the skew-safe remaining time and
/// payload bound the native collector must honor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AudioOperationRequestDto {
    /// Operation identity.
    pub identity: AudioOperationIdDto,
    /// Stable owner of any resource created by this operation.
    pub owner: AudioOwnerDto,
    /// Per-call attribution, separate from the resource owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<AudioInitiatorDto>,
    /// Remaining operation budget in milliseconds, converted to a local
    /// monotonic deadline by each receiving process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_budget_ms: Option<u64>,
    /// Maximum raw audio bytes that may be collected or returned.
    pub max_payload_bytes: u64,
    /// Requested operation.
    pub operation: AudioOperationDto,
}

/// One device-local audio operation. Wire tagged by `type`, snake case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AudioOperationDto {
    /// Start raw capture and return an owner-bound handle.
    StartRecording {
        /// Requested sample rate in Hz.
        sample_rate_hz: u32,
        /// Requested container/codec, which must be honored exactly.
        format: String,
    },
    /// Stop the matching owner's recording handle and return captured media.
    StopRecording {
        /// Previously returned recording handle.
        handle: String,
    },
    /// Capture and transcribe live microphone speech.
    Listen {
        /// BCP-47 language hint; absent means device locale at operation start.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    /// Synthesize PCM without playing it.
    Synthesize {
        /// Text to synthesize.
        text: String,
        /// Per-operation BCP-47 language override.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        /// Per-operation rate override in the supported range 0.5–2.0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate: Option<f32>,
        /// Per-operation voice override.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        voice: Option<String>,
    },
    /// Play speech and complete only after playback actually finishes.
    Speak {
        /// Text to play.
        text: String,
        /// Per-operation BCP-47 language override.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        /// Per-operation rate override in the supported range 0.5–2.0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate: Option<f32>,
        /// Per-operation voice override.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        voice: Option<String>,
    },
    /// Query owner-scoped recording and playback state. Failure is explicit.
    Status {
        /// Optional recording handle to inspect.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handle: Option<String>,
    },
    /// End only the stable owner carried by the request.
    EndOwner,
}

/// Structured audio failure class, wire encoded as snake-case string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AudioErrorKindDto {
    /// Microphone or system speech permission was denied.
    PermissionDenied,
    /// An incompatible audio lease is active.
    Busy,
    /// The caller cancelled the operation.
    Cancelled,
    /// The operation exceeded its time budget.
    Timeout,
    /// No speech was recognized.
    NoSpeech,
    /// The recording handle is absent, stale, or owned by another owner.
    NotRecording,
    /// The selected audio provider is unavailable.
    Unavailable,
    /// The requested operation or output format is unsupported.
    Unsupported,
    /// The requested offline model is absent or unavailable.
    ModelMissing,
    /// The requested voice is absent or unavailable.
    VoiceMissing,
    /// The request arguments are invalid.
    InvalidRequest,
    /// The selected synthesizer failed.
    SynthesisFailed,
    /// A native or transport operation failed.
    NativeFailure,
    /// Audio exceeded the bounded bridge payload.
    MediaTooLarge,
}

/// Structured audio failure with bounded, user-readable detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AudioErrorDto {
    /// Branchable error class.
    pub kind: AudioErrorKindDto,
    /// Human-readable detail, never audio or transcript content.
    pub message: String,
}

/// Owner-scoped current device I/O state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AudioStatusDto {
    /// Whether the queried owner/handle is actively recording.
    pub recording: bool,
    /// Whether the queried owner has active playback.
    pub playing: bool,
}

/// Result of a device audio operation. Failures are explicit terminal results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AudioOperationResultDto {
    /// Recording began and now belongs to the returned handle.
    RecordingStarted {
        /// Owner-bound recording handle.
        handle: String,
    },
    /// Finished encoded recording.
    Recording {
        /// Base64 media bytes.
        audio_base64: String,
        /// MIME type such as `audio/m4a`.
        mime_type: String,
    },
    /// Live microphone transcription.
    Transcript {
        /// Recognized text.
        text: String,
        /// Detected language, when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        /// Confidence in `[0, 1]`, when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f32>,
    },
    /// Nonempty PCM16-LE mono audio from silent synthesis.
    Synthesized {
        /// Base64 PCM bytes.
        pcm_base64: String,
        /// Actual PCM sample rate in Hz.
        sample_rate_hz: u32,
    },
    /// Playback actually completed.
    PlaybackCompleted {
        /// Playback duration in milliseconds.
        duration_ms: u64,
    },
    /// Owner-scoped device state.
    Status {
        /// Current status.
        status: AudioStatusDto,
    },
    /// The request owner was ended.
    OwnerEnded,
    /// The operation failed.
    Failed {
        /// Structured error class and detail.
        error: AudioErrorDto,
    },
}

/// Operation kind used by supported-capability projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum AudioOperationKindDto {
    /// Raw recording.
    Record,
    /// Live microphone transcription.
    Listen,
    /// Silent PCM synthesis.
    Synthesize,
    /// Speech playback.
    Speak,
}

/// Transient readiness of an operation; it does not change support/schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum AudioReadinessStateDto {
    /// Ready to start now.
    Ready,
    /// The first user-authorized operation may need permission.
    NeedsPermission,
    /// Another owner currently holds a conflicting lease.
    Busy,
    /// The selected offline model is missing.
    MissingModel,
    /// The selected provider is not currently available.
    Unavailable,
}

/// Readiness entry for one supported operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AudioOperationReadinessDto {
    /// Supported operation being described.
    pub operation: AudioOperationKindDto,
    /// Current transient readiness.
    pub state: AudioReadinessStateDto,
}

/// Device audio support and transient readiness snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AudioCapabilitySnapshotDto {
    /// Current AudioService instance epoch.
    pub service_epoch: u64,
    /// Increments only when the supported operation set changes.
    pub support_revision: u64,
    /// Stable set of implemented operations, independent of readiness.
    pub supported_operations: Vec<AudioOperationKindDto>,
    /// Transient readiness of supported operations.
    pub readiness: Vec<AudioOperationReadinessDto>,
    /// Maximum raw bytes that can pass through the audio bridge.
    pub max_payload_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_payload_limit_is_frame_derived_and_pcm16_aligned() {
        assert_eq!(MAX_AUDIO_BASE64_LENGTH, 16_711_680);
        assert_eq!(MAX_AUDIO_PAYLOAD_BYTES, 12_533_760);
        assert_eq!(MAX_AUDIO_PAYLOAD_BYTES % 2, 0);
        assert_eq!(
            MAX_AUDIO_PAYLOAD_BYTES.div_ceil(3) * 4,
            MAX_AUDIO_BASE64_LENGTH
        );
    }
}
