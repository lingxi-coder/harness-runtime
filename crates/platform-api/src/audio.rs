//! Device-local audio execution contract.
//!
//! One app-scoped service owns capture, recognition, synthesis, playback,
//! status, cancellation, and owner teardown. The native service resolves
//! device configuration and provider routing.

use crate::{SttTranscript, TtsAudio, VoiceRecording};
use async_trait::async_trait;
use std::fmt;
use thiserror::Error;

/// Stable identity shared by the host, transport, and native service.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AudioOperationId {
    /// Globally unique UUID v4.
    pub id: String,
    /// Monotonic operation generation, JavaScript-safe.
    pub generation: u64,
    /// Native AudioService instance epoch, JavaScript-safe.
    pub service_epoch: u64,
}

impl AudioOperationId {
    /// Mint one globally unique operation ID with the current service epoch.
    #[must_use]
    pub fn new(generation: u64, service_epoch: u64) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            generation,
            service_epoch,
        }
    }
}

/// Trusted owner of a long-lived device resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AudioOwner {
    /// Conversation session; tool and agent IDs do not change its lifetime.
    Session { session_id: String },
    /// Local App runtime instance.
    LocalApp {
        app_id: String,
        runtime_generation: u64,
    },
    /// UI app instance.
    Ui { instance_id: String },
    /// Host-owned operation outside a chat or Local App.
    System { instance_id: String },
}

/// Per-call attribution, separate from the stable resource owner.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioInitiator {
    /// Agent identifier, when called from an agent tool.
    pub agent_id: Option<String>,
    /// Tool-use identifier, when called by a model tool.
    pub tool_use_id: Option<String>,
    /// Host request identifier, when called by a Local App or UI.
    pub request_id: Option<String>,
}

/// Trusted operation context minted by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOperationContext {
    /// Operation identity.
    pub identity: AudioOperationId,
    /// Stable owner for any resource created by the operation.
    pub owner: AudioOwner,
    /// Per-call attribution, not a resource owner.
    pub initiator: Option<AudioInitiator>,
    /// Remaining time budget; receivers use a local monotonic timer.
    pub timeout_budget_ms: Option<u64>,
    /// Maximum raw audio bytes to collect or return.
    pub max_payload_bytes: u64,
}

/// Opaque recording session handle returned by capture start.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AudioRecordingHandle(pub String);

/// One device-local audio intent.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioOperation {
    /// Start raw capture with the exact requested format.
    StartRecording { sample_rate_hz: u32, format: String },
    /// Stop the matching owner's capture.
    StopRecording { handle: AudioRecordingHandle },
    /// Capture and transcribe live microphone speech.
    Listen { language: Option<String> },
    /// Synthesize PCM without playing it.
    Synthesize {
        text: String,
        language: Option<String>,
        rate: Option<f32>,
        voice: Option<String>,
    },
    /// Play speech and complete only after actual playback ends.
    Speak {
        text: String,
        language: Option<String>,
        rate: Option<f32>,
        voice: Option<String>,
    },
    /// Query owner-scoped capture and playback state.
    Status {
        handle: Option<AudioRecordingHandle>,
    },
    /// End only the owner in the request context.
    EndOwner,
}

/// Operation exposed by the device service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioOperationKind {
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
    RecordingStarted { handle: AudioRecordingHandle },
    /// Encoded media from capture stop.
    Recording { recording: VoiceRecording },
    /// Live microphone transcript.
    Transcript { transcript: SttTranscript },
    /// Nonempty PCM generated silently.
    Synthesized { audio: TtsAudio },
    /// Actual playback completed.
    PlaybackCompleted { duration_ms: u64 },
    /// Owner-scoped device state.
    Status { status: AudioStatus },
    /// The request owner ended.
    OwnerEnded,
}

/// App-scoped device audio service.
#[async_trait]
pub trait AudioService: Send + Sync {
    /// Current support and transient readiness. Must not request permission.
    fn capabilities(&self) -> AudioCapabilitySnapshot;

    /// Execute an owner-scoped operation. A returned recording handle outlives
    /// this future; dropping an unfinished call cancels only its identity.
    async fn execute(
        &self,
        context: AudioOperationContext,
        operation: AudioOperation,
    ) -> Result<AudioOperationSuccess, AudioError>;

    /// Cancel only the matching in-flight operation.
    async fn cancel(&self, identity: AudioOperationId) -> Result<(), AudioError>;
}
