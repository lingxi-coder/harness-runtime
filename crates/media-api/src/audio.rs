//! Specialized audio execution contract within the shared media service.
//!
//! One app-scoped service owns capture, recognition, synthesis, playback,
//! status, cancellation, and owner teardown. Host composition resolves provider
//! routing; native adapters retain device configuration, permissions and leases.

use crate::stt::SttTranscript;
use crate::tts::TtsAudio;
use crate::voice::VoiceRecording;
use crate::{MediaInitiator, MediaOperationContext, MediaOperationId, MediaOwner, RecordingRef};
use async_trait::async_trait;
use std::fmt;
use thiserror::Error;

/// Audio names retained for specialized APIs while sharing canonical identity.
pub type AudioOperationId = MediaOperationId;
/// Audio ownership uses the same canonical owner as all other media resources.
pub type AudioOwner = MediaOwner;
/// Audio operation context is the canonical media operation context.
pub type AudioOperationContext = MediaOperationContext;
/// Audio attribution uses the same initiator metadata as other media calls.
pub type AudioInitiator = MediaInitiator;

/// Audio spelling for the common owner-scoped recording reference.
pub type AudioRecordingHandle = RecordingRef;

/// One device-local audio intent.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioOperation {
    /// Capture one bounded clip using device endpoint detection.
    Capture {
        /// Requested sample rate in Hz.
        sample_rate_hz: u32,
        /// Requested encoded or raw format.
        format: String,
    },
    /// Play supplied PCM and complete after actual playback.
    Play {
        /// PCM payload to play.
        audio: TtsAudio,
    },
    /// Transcribe an already captured recording.
    Transcribe {
        /// Recorded audio to transcribe.
        recording: VoiceRecording,
        /// Optional BCP-47 language hint.
        language: Option<String>,
    },
    /// Start raw capture with the exact requested format.
    StartRecording {
        /// Requested sample rate in Hz.
        sample_rate_hz: u32,
        /// Requested encoded or raw format.
        format: String,
    },
    /// Stop the matching owner's capture.
    StopRecording {
        /// Owner-scoped recording handle to stop.
        handle: AudioRecordingHandle,
    },
    /// Capture and transcribe live microphone speech.
    Listen {
        /// Optional BCP-47 language hint.
        language: Option<String>,
    },
    /// Synthesize PCM without playing it.
    Synthesize {
        /// Text to synthesize.
        text: String,
        /// Optional BCP-47 language hint.
        language: Option<String>,
        /// Optional speech rate.
        rate: Option<f32>,
        /// Optional host-supported voice name.
        voice: Option<String>,
    },
    /// Play speech and complete only after actual playback ends.
    Speak {
        /// Text to synthesize and play.
        text: String,
        /// Optional BCP-47 language hint.
        language: Option<String>,
        /// Optional speech rate.
        rate: Option<f32>,
        /// Optional host-supported voice name.
        voice: Option<String>,
    },
    /// Query owner-scoped capture and playback state.
    Status {
        /// Optional recording handle whose state is requested.
        handle: Option<AudioRecordingHandle>,
    },
    /// End only the owner in the request context.
    EndOwner,
}

/// Operation exposed by the device service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AudioOperationKind {
    /// Single bounded clip capture.
    Capture,
    /// Supplied PCM playback.
    Play,
    /// Recorded-file recognition.
    Transcribe,
    /// Raw recording.
    Record,
    /// Live recognition.
    Listen,
    /// Silent synthesis.
    Synthesize,
    /// Speech playback.
    Speak,
}

/// Transient readiness, independent of operation support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioReadinessState {
    /// Ready now.
    Ready,
    /// A user-authorized request may need permission.
    NeedsPermission,
    /// Another owner holds an incompatible device lease.
    Busy,
    /// Selected offline model is missing.
    MissingModel,
    /// Selected provider is temporarily unavailable.
    Unavailable,
}

/// Readiness for one supported operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioOperationReadiness {
    /// Supported operation being described.
    pub operation: AudioOperationKind,
    /// Transient readiness state.
    pub state: AudioReadinessState,
}

/// Device audio support and transient readiness snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioCapabilitySnapshot {
    /// Native AudioService instance epoch.
    pub service_epoch: u64,
    /// Changes only when supported operations change.
    pub support_revision: u64,
    /// Stable set of implemented operation kinds.
    pub supported_operations: Vec<AudioOperationKind>,
    /// Current readiness of supported operations.
    pub readiness: Vec<AudioOperationReadiness>,
    /// Maximum raw audio bytes that may be collected or returned.
    pub max_payload_bytes: u64,
}

/// Owner-scoped current device I/O status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioStatus {
    /// Whether the queried owner or handle is recording.
    pub recording: bool,
    /// Whether the queried owner is playing.
    pub playing: bool,
}

/// Structured audio failure class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioErrorKind {
    /// Microphone or speech permission was denied.
    PermissionDenied,
    /// An incompatible audio lease is active.
    Busy,
    /// The caller cancelled the operation.
    Cancelled,
    /// The operation exceeded its time budget.
    Timeout,
    /// No speech was recognized.
    NoSpeech,
    /// The recording handle is missing, stale, or owned by another owner.
    NotRecording,
    /// The selected provider is unavailable.
    Unavailable,
    /// The requested operation or output format is unsupported.
    Unsupported,
    /// The selected offline model is absent.
    ModelMissing,
    /// The requested voice is absent.
    VoiceMissing,
    /// The request arguments are invalid.
    InvalidRequest,
    /// Speech synthesis failed.
    SynthesisFailed,
    /// A native or transport operation failed.
    NativeFailure,
    /// Audio exceeded the bounded payload.
    MediaTooLarge,
}

impl fmt::Display for AudioErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PermissionDenied => "permission_denied",
            Self::Busy => "busy",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::NoSpeech => "no_speech",
            Self::NotRecording => "not_recording",
            Self::Unavailable => "unavailable",
            Self::Unsupported => "unsupported",
            Self::ModelMissing => "model_missing",
            Self::VoiceMissing => "voice_missing",
            Self::InvalidRequest => "invalid_request",
            Self::SynthesisFailed => "synthesis_failed",
            Self::NativeFailure => "native_failure",
            Self::MediaTooLarge => "media_too_large",
        })
    }
}

/// Structured audio operation failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{kind}: {message}")]
pub struct AudioError {
    /// Machine-readable class.
    pub kind: AudioErrorKind,
    /// Safe diagnostic or UI detail; never audio or transcript content.
    pub message: String,
}

impl AudioError {
    /// Create a structured failure.
    #[must_use]
    pub fn new(kind: AudioErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// Successful result from one audio operation.
#[derive(Debug, Clone)]
pub enum AudioOperationSuccess {
    /// Capture began and remains bound to this handle.
    RecordingStarted {
        /// Owner-scoped handle of the active recording.
        handle: AudioRecordingHandle,
    },
    /// Encoded media from capture stop.
    Recording {
        /// Finalized recorded audio.
        recording: VoiceRecording,
    },
    /// Live microphone transcript.
    Transcript {
        /// Recognized speech and provider metadata.
        transcript: SttTranscript,
    },
    /// Nonempty PCM generated silently.
    Synthesized {
        /// Synthesized PCM audio.
        audio: TtsAudio,
    },
    /// Actual playback completed.
    PlaybackCompleted {
        /// Actual playback duration in milliseconds.
        duration_ms: u64,
    },
    /// Owner-scoped device state.
    Status {
        /// Queried recording and playback state.
        status: AudioStatus,
    },
    /// The request owner ended.
    OwnerEnded,
}

/// App-scoped device audio service.
#[async_trait]
pub trait AudioService: Send + Sync {
    /// Current support and transient readiness. Must not request permission.
    fn capabilities(&self) -> AudioCapabilitySnapshot;

    /// Owner-specific capability projection, including trusted session routing.
    fn capabilities_for(&self, _owner: &AudioOwner) -> AudioCapabilitySnapshot {
        self.capabilities()
    }

    /// Execute an owner-scoped operation. A returned recording handle outlives
    /// this future; dropping an unfinished call cancels only its identity.
    async fn execute(
        &self,
        context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError>;

    /// Execute a route selected from the app's trusted local audio settings.
    /// Implementations must use one immutable native settings snapshot and
    /// reject if its recognition/speech route now selects a cloud provider.
    /// This boundary lets the app own installed offline model selection while
    /// preventing a stale common-router decision from falling through to cloud.
    async fn execute_host_configured_local(
        &self,
        _context: AudioOperationContext,
        _operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError> {
        Err(AudioError::new(
            AudioErrorKind::Unsupported,
            "native audio service cannot enforce a host-configured local route",
        ))
    }

    /// Cancel only the matching in-flight operation incarnation.
    ///
    /// The context carries a host-minted opaque handle in addition to its
    /// public ID. Returning `Ok(())` is a completion receipt: this service has
    /// resolved that handle, and this request can no longer affect another
    /// context that reuses the public ID. Implementations that cannot confirm
    /// that must return an error and retain the handle as unresolved.
    async fn cancel(&self, context: AudioOperationContext) -> Result<(), AudioError>;
}

impl AudioOperation {
    /// Capability kind required by this operation; lifecycle controls need none.
    pub fn kind(&self) -> Option<AudioOperationKind> {
        Some(match self {
            Self::Capture { .. } => AudioOperationKind::Capture,
            Self::Play { .. } => AudioOperationKind::Play,
            Self::Transcribe { .. } => AudioOperationKind::Transcribe,
            Self::StartRecording { .. } | Self::StopRecording { .. } => AudioOperationKind::Record,
            Self::Listen { .. } => AudioOperationKind::Listen,
            Self::Synthesize { .. } => AudioOperationKind::Synthesize,
            Self::Speak { .. } => AudioOperationKind::Speak,
            Self::Status { .. } | Self::EndOwner => return None,
        })
    }
}
