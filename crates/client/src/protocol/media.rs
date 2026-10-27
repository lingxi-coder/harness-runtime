//! Versioned transport DTOs for app-scoped native media callbacks.
//!
//! These records are the JSON/UniFFI edge only. Hosts convert them to the
//! canonical, engine-independent `media-api` contracts at the app boundary.

use serde::{Deserialize, Serialize};

/// Maximum encoded frame bytes crossing a native media callback.
pub const MAX_MEDIA_DEVICE_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Maximum serialized request envelope accepted by the authenticated media
/// bridge, accounting for the JSON expansion of byte arrays.
pub const MAX_MEDIA_DEVICE_WIRE_BYTES: usize = 16 * 1024 * 1024;
/// Maximum offer or answer SDP accepted by the product media callback.
pub const MAX_REMOTE_MEDIA_SDP_BYTES: usize = 64 * 1024;
/// Maximum single peer-media control barrier sequence (one hour at 20 ms).
pub const MAX_MEDIA_CONTROL_AUDIO_SEQUENCE: u64 = 180_000;
/// Maximum 48 kHz sample frames covered by a single control barrier.
pub const MAX_MEDIA_CONTROL_AUDIO_SAMPLE_FRAMES: u64 = MAX_MEDIA_CONTROL_AUDIO_SEQUENCE * 960;
/// Maximum media request or transcript text carried by this protocol.
pub const MAX_MEDIA_TEXT_BYTES: usize = 64 * 1024;
/// Maximum opaque JSON text accepted for host-owned audio settings updates.
pub const MAX_MEDIA_AUDIO_SETTINGS_JSON_BYTES: usize = 64 * 1024;
/// Maximum number of supported media modes advertised for one host route.
pub const MAX_MEDIA_CONNECTION_MODES: usize = 3;
/// Maximum source categories advertised for one host route.
pub const MAX_MEDIA_CONNECTION_SOURCE_KINDS: usize = 3;
/// Maximum source, status, or event items returned in one result.
pub const MAX_MEDIA_COLLECTION_ITEMS: usize = 256;
/// Maximum safe diagnostic text in an event/error record.
pub const MAX_MEDIA_EVENT_TEXT_BYTES: usize = 1_024;

/// Host-issued operation identity shared by one media request and response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaOperationIdDto {
    /// UUID v4 string.
    pub id: String,
    /// Host-monotonic operation generation.
    pub generation: u64,
    /// Media service instance epoch.
    pub service_epoch: u64,
}

impl MediaOperationIdDto {
    /// Check the host-issued identifier before it crosses an app boundary.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.id, "media operation ID")
    }
}

/// Trusted owner of a device or remote media resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaOwnerDto {
    /// Conversation session owner.
    Session { session_id: String },
    /// App UI owner.
    Ui { instance_id: String },
    /// Host-owned operation outside a chat.
    System { instance_id: String },
}

impl MediaOwnerDto {
    /// Bound owner identity strings supplied by a native host adapter.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Session { session_id } => validate_identifier(session_id, "media session owner"),
            Self::Ui { instance_id } => validate_identifier(instance_id, "media UI owner"),
            Self::System { instance_id } => validate_identifier(instance_id, "media system owner"),
        }
    }
}

/// Per-operation attribution, separate from the stable owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaInitiatorDto {
    /// Agent ID, when invoked by an Agent.
    pub agent_id: Option<String>,
    /// Tool-use ID, when invoked by a model tool.
    pub tool_use_id: Option<String>,
    /// Host request ID, when invoked by an app or UI.
    pub request_id: Option<String>,
}

impl MediaInitiatorDto {
    /// Bound optional attribution fields.
    pub fn validate(&self) -> Result<(), String> {
        for value in [
            self.agent_id.as_deref(),
            self.tool_use_id.as_deref(),
            self.request_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_identifier(value, "media initiator ID")?;
        }
        Ok(())
    }
}

/// Media family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum MediaKindDto {
    /// Encoded or raw audio.
    Audio,
    /// Still image.
    Image,
    /// Time-based video.
    Video,
}

/// Discovered local or remote source category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum MediaSourceKindDto {
    /// Microphone input.
    Microphone,
    /// Camera input.
    Camera,
    /// Screen input.
    Screen,
    /// Speaker output.
    Speaker,
    /// Host-managed media library.
    Library,
    /// Connector-managed remote source.
    Remote,
}

/// Supported use of a media source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum MediaSourceUsageDto {
    /// One-shot capture.
    Capture,
    /// Continuous recording.
    Record,
    /// Live transcription.
    Transcribe,
    /// Playback.
    Play,
    /// Synthesized speech playback.
    Speak,
    /// Realtime media session.
    Session,
    /// Reading an image from a host library.
    ReadImage,
}

/// Supported media operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum MediaOperationKindDto {
    /// List media sources.
    Sources,
    /// Query current resource status.
    Status,
    /// Capture one bounded item.
    Capture,
    /// Start a recording.
    Record,
    /// Transcribe live or stored audio.
    Transcribe,
    /// Synthesize speech audio.
    Synthesize,
    /// Play a stored asset.
    Play,
    /// Speak text on the device.
    Speak,
    /// Start a connector session.
    SessionStart,
    /// Update a connector session.
    SessionUpdate,
    /// Read available events.
    Observe,
    /// Wait for events.
    Wait,
    /// Stop an owner-visible resource.
    Stop,
}

/// Operation support readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum MediaReadinessStateDto {
    /// Ready now.
    Ready,
    /// A permission request may be presented.
    NeedsPermission,
    /// An incompatible lease is active.
    Busy,
    /// Selected offline model is missing.
    MissingModel,
    /// Temporarily unavailable.
    Unavailable,
}

/// Readiness for one supported operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaOperationReadinessDto {
    /// Operation described.
    pub operation: MediaOperationKindDto,
    /// Current transient readiness.
    pub state: MediaReadinessStateDto,
}

/// App-scoped media support snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaCapabilitiesDto {
    /// Media service instance epoch.
    pub service_epoch: u64,
    /// Revision of stable supported operations and media kinds.
    pub support_revision: u64,
    /// Stable supported operations.
    pub supported_operations: Vec<MediaOperationKindDto>,
    /// Supported media kinds.
    pub supported_media_kinds: Vec<MediaKindDto>,
    /// Current transient readiness.
    pub readiness: Vec<MediaOperationReadinessDto>,
    /// Maximum encoded media payload in bytes.
    pub max_payload_bytes: u64,
}

impl MediaCapabilitiesDto {
    /// Check support snapshots before they cross the native bridge.
    pub fn validate(&self) -> Result<(), String> {
        validate_collection_len(self.supported_operations.len(), "media capabilities")?;
        validate_collection_len(self.supported_media_kinds.len(), "media kinds")?;
        validate_collection_len(self.readiness.len(), "media readiness")?;
        if self.max_payload_bytes == 0
            || self.max_payload_bytes > MAX_MEDIA_DEVICE_FRAME_BYTES as u64
        {
            return Err("media capability payload limit is outside supported bounds".into());
        }
        if self
            .readiness
            .iter()
            .any(|readiness| !self.supported_operations.contains(&readiness.operation))
        {
            return Err("media readiness must describe a supported operation".into());
        }
        Ok(())
    }
}

/// Opaque source reference returned by the host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaSourceRefDto {
    /// Host-owned opaque source ID.
    pub id: String,
}

impl MediaSourceRefDto {
    /// Bound the opaque source reference.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.id, "media source reference")
    }
}

/// Opaque session reference returned by the shared media service.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaSessionRefDto {
    /// Host-owned media session identity.
    pub id: String,
}

impl MediaSessionRefDto {
    /// Bound the opaque media session reference.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.id, "media session reference")
    }
}

/// Owner-visible resource address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaResourceRefDto {
    /// Discovered source.
    Source { source: MediaSourceRefDto },
    /// Recording.
    Recording { recording_id: String },
    /// Media session.
    Session { session_id: String },
    /// Stored media item.
    Media { media_ref: String },
    /// In-flight operation.
    Operation { operation_id: MediaOperationIdDto },
}

impl MediaResourceRefDto {
    /// Validate every opaque ID nested in a resource address.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Source { source } => source.validate(),
            Self::Recording { recording_id } => {
                validate_identifier(recording_id, "media recording reference")
            }
            Self::Session { session_id } => {
                validate_identifier(session_id, "media session reference")
            }
            Self::Media { media_ref } => validate_identifier(media_ref, "media reference"),
            Self::Operation { operation_id } => operation_id.validate(),
        }
    }
}

/// Host-discovered input or output source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaSourceDto {
    /// Opaque source identity.
    pub reference: MediaSourceRefDto,
    /// Source category.
    pub kind: MediaSourceKindDto,
    /// Actual audio, image, or video modality when known.
    pub media_kind: Option<MediaKindDto>,
    /// Safe user-facing label.
    pub label: String,
    /// Supported source uses.
    pub usages: Vec<MediaSourceUsageDto>,
    /// Supported operations for this source.
    pub supported_operations: Vec<MediaOperationKindDto>,
    /// Whether selected as the default source of this kind.
    pub is_default: bool,
}

impl MediaSourceDto {
    /// Validate the source identity and bounded display/catalog fields.
    pub fn validate(&self) -> Result<(), String> {
        self.reference.validate()?;
        validate_text(&self.label, 256, "media source label")?;
        if self.usages.len() > MAX_MEDIA_COLLECTION_ITEMS
            || self.supported_operations.len() > MAX_MEDIA_COLLECTION_ITEMS
        {
            return Err("media source metadata exceeds the collection limit".into());
        }
        Ok(())
    }
}

/// Safe metadata about a stored media item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaMetadataDto {
    /// Media family.
    pub kind: MediaKindDto,
    /// MIME type.
    pub mime_type: String,
    /// Encoded byte length.
    pub byte_len: u64,
    /// Capture timestamp in Unix milliseconds, when the item was captured.
    pub captured_at_unix_ms: Option<u64>,
    /// Duration in milliseconds.
    pub duration_ms: Option<u64>,
    /// Image/video width.
    pub width_px: Option<u32>,
    /// Image/video height.
    pub height_px: Option<u32>,
    /// Audio sample rate.
    pub sample_rate_hz: Option<u32>,
    /// Audio channel count.
    pub channels: Option<u16>,
}

impl MediaMetadataDto {
    /// Validate safe metadata before returning it to a client.
    pub fn validate(&self) -> Result<(), String> {
        validate_text(&self.mime_type, 128, "media MIME type")?;
        if self.byte_len > MAX_MEDIA_DEVICE_FRAME_BYTES as u64 {
            return Err("media metadata exceeds the payload limit".into());
        }
        for dimension in [self.width_px, self.height_px].into_iter().flatten() {
            if dimension == 0 || dimension > 16_384 {
                return Err("media dimension is outside the supported bounds".into());
            }
        }
        if self.sample_rate_hz == Some(0) || self.channels == Some(0) {
            return Err("media audio metadata contains a zero sample setting".into());
        }
        Ok(())
    }
}

/// Opaque stored media asset and safe metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaAssetDto {
    /// Owner-scoped opaque media reference.
    pub media_ref: String,
    /// Safe media metadata.
    pub metadata: MediaMetadataDto,
}

impl MediaAssetDto {
    /// Validate asset identity and metadata.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.media_ref, "media asset reference")?;
        self.metadata.validate()
    }
}

/// Limits for a bounded capture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaCaptureOptionsDto {
    /// Maximum capture duration.
    pub max_duration_ms: Option<u64>,
    /// Maximum image dimension.
    pub max_dimension_px: Option<u32>,
    /// JPEG quality between 0 and 1.
    pub jpeg_quality: Option<f32>,
}

/// Limits and output format for a recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaRecordOptionsDto {
    /// Maximum duration.
    pub max_duration_ms: Option<u64>,
    /// Requested MIME type.
    pub mime_type: Option<String>,
}

/// Input for transcription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaTranscriptionInputDto {
    /// Live source input.
    LiveSource { source: MediaSourceRefDto },
    /// Stored media input.
    StoredMedia { media_ref: String },
}

/// Audio or video session mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum MediaSessionModeDto {
    /// Audio transport.
    Audio,
    /// Video transport.
    Video,
    /// Audio and video transport.
    AudioVideo,
}

/// Session purpose; Conversation may attach an Agent continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum MediaSessionIntentDto {
    /// Observation only.
    Observation,
    /// Conversation with an attached Agent.
    Conversation,
}

/// Provider-independent session request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaSessionRequestDto {
    /// Session mode.
    pub mode: MediaSessionModeDto,
    /// Purpose of this session.
    pub intent: MediaSessionIntentDto,
    /// Optional microphone source.
    pub audio_source: Option<MediaSourceRefDto>,
    /// Optional camera or screen source.
    pub video_source: Option<MediaSourceRefDto>,
}

impl MediaSessionRequestDto {
    /// Validate typed source selection for a new session.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(source) = &self.audio_source {
            source.validate()?;
        }
        if let Some(source) = &self.video_source {
            source.validate()?;
        }
        Ok(())
    }
}

/// Typed update applied to an active session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaSessionUpdateDto {
    /// Enable or disable one media kind.
    SetEnabled { kind: MediaKindDto, enabled: bool },
    /// Select an owner-visible source.
    SelectSource {
        kind: MediaKindDto,
        source: MediaSourceRefDto,
    },
    /// Set output gain from 0 to 1.
    SetPlaybackGain { gain: f32 },
}

impl MediaSessionUpdateDto {
    /// Validate an update without trusting float values from foreign callers.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::SelectSource { source, .. } => source.validate(),
            Self::SetPlaybackGain { gain } if !gain.is_finite() || !(0.0..=1.0).contains(gain) => {
                Err("media playback gain must be finite and between zero and one".into())
            }
            Self::SetEnabled { .. } | Self::SetPlaybackGain { .. } => Ok(()),
        }
    }
}

/// Session lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum MediaSessionStateDto {
    /// Connector is preparing.
    Preparing,
    /// Transport is ready for media.
    Ready,
    /// Recovering.
    Reconnecting,
    /// Ended normally.
    Ended,
    /// Failed.
    Failed,
}

/// Current session state and enabled kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaSessionStatusDto {
    /// Lifecycle state.
    pub state: MediaSessionStateDto,
    /// Whether audio is enabled.
    pub audio_enabled: bool,
    /// Whether video is enabled.
    pub video_enabled: bool,
}

/// One host-issued media operation request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaOperationRequestDto {
    /// Identity shared by request, cancellation and result.
    pub identity: MediaOperationIdDto,
    /// Stable owner.
    pub owner: MediaOwnerDto,
    /// Per-call attribution.
    pub initiator: Option<MediaInitiatorDto>,
    /// Remaining operation budget in milliseconds.
    pub timeout_budget_ms: Option<u64>,
    /// Encoded payload ceiling.
    pub max_payload_bytes: u64,
    /// Typed operation.
    pub operation: MediaOperationDto,
}

impl MediaOperationRequestDto {
    /// Validate trusted operation metadata and the typed operation payload.
    pub fn validate(&self) -> Result<(), String> {
        self.identity.validate()?;
        self.owner.validate()?;
        if let Some(initiator) = &self.initiator {
            initiator.validate()?;
        }
        if self.max_payload_bytes == 0
            || self.max_payload_bytes > MAX_MEDIA_DEVICE_FRAME_BYTES as u64
            || self
                .timeout_budget_ms
                .is_some_and(|ms| ms == 0 || ms > 3_600_000)
        {
            return Err("media operation limits are outside the supported bounds".into());
        }
        self.operation.validate()
    }
}

/// Typed app media operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaOperationDto {
    /// Discover sources.
    Sources {
        kind: Option<MediaKindDto>,
        usage: Option<MediaSourceUsageDto>,
    },
    /// Query resource status.
    Status { target: Option<MediaResourceRefDto> },
    /// Capture a bounded item.
    Capture {
        source: MediaSourceRefDto,
        options: MediaCaptureOptionsDto,
    },
    /// Start a recording.
    Record {
        source: MediaSourceRefDto,
        options: MediaRecordOptionsDto,
    },
    /// Transcribe a live or stored input.
    Transcribe {
        input: MediaTranscriptionInputDto,
        language: Option<String>,
    },
    /// Synthesize audio without playback.
    Synthesize {
        text: String,
        language: Option<String>,
        rate: Option<f32>,
        voice: Option<String>,
    },
    /// Play an owner-visible asset.
    Play {
        media_ref: String,
        source: Option<MediaSourceRefDto>,
    },
    /// Synthesize and play speech.
    Speak {
        text: String,
        language: Option<String>,
        rate: Option<f32>,
        voice: Option<String>,
    },
    /// Start a connector session.
    SessionStart { request: MediaSessionRequestDto },
    /// Update a connector session.
    SessionUpdate {
        session_id: String,
        update: MediaSessionUpdateDto,
    },
    /// Read events.
    Observe {
        target: MediaResourceRefDto,
        after_sequence: Option<u64>,
        limit: u16,
    },
    /// Wait for events.
    Wait {
        target: MediaResourceRefDto,
        after_sequence: Option<u64>,
        timeout_ms: u64,
        limit: u16,
    },
    /// Stop a recording, session or operation.
    Stop { target: MediaResourceRefDto },
}

impl MediaOperationDto {
    /// Validate strings, references, counts, and numeric options before host
    /// service dispatch.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Sources { .. } => Ok(()),
            Self::Status { target } => target
                .as_ref()
                .map_or(Ok(()), MediaResourceRefDto::validate),
            Self::Capture { source, options } => {
                source.validate()?;
                if options
                    .max_duration_ms
                    .is_some_and(|ms| ms == 0 || ms > 3_600_000)
                    || options
                        .max_dimension_px
                        .is_some_and(|px| px == 0 || px > 16_384)
                    || options
                        .jpeg_quality
                        .is_some_and(|q| !q.is_finite() || !(0.0..=1.0).contains(&q))
                {
                    return Err("media capture options are outside the supported bounds".into());
                }
                Ok(())
            }
            Self::Record { source, options } => {
                source.validate()?;
                if options
                    .max_duration_ms
                    .is_some_and(|ms| ms == 0 || ms > 3_600_000)
                {
                    return Err("media recording duration is outside the supported bounds".into());
                }
                if let Some(mime) = &options.mime_type {
                    validate_text(mime, 128, "recording MIME type")?;
                }
                Ok(())
            }
            Self::Transcribe { input, language } => {
                match input {
                    MediaTranscriptionInputDto::LiveSource { source } => source.validate()?,
                    MediaTranscriptionInputDto::StoredMedia { media_ref } => {
                        validate_identifier(media_ref, "transcription media reference")?
                    }
                }
                if let Some(language) = language {
                    validate_text(language, 64, "transcription language")?;
                }
                Ok(())
            }
            Self::Synthesize {
                text,
                language,
                rate,
                voice,
            }
            | Self::Speak {
                text,
                language,
                rate,
                voice,
            } => {
                validate_text(text, MAX_MEDIA_TEXT_BYTES, "speech text")?;
                if let Some(language) = language {
                    validate_text(language, 64, "speech language")?;
                }
                if rate.is_some_and(|rate| !rate.is_finite() || !(0.1..=4.0).contains(&rate)) {
                    return Err("speech rate is outside the supported bounds".into());
                }
                if let Some(voice) = voice {
                    validate_text(voice, 128, "voice name")?;
                }
                Ok(())
            }
            Self::Play { media_ref, source } => {
                validate_identifier(media_ref, "playback media reference")?;
                source.as_ref().map_or(Ok(()), MediaSourceRefDto::validate)
            }
            Self::SessionStart { request } => request.validate(),
            Self::SessionUpdate { session_id, update } => {
                validate_identifier(session_id, "media session reference")?;
                update.validate()
            }
            Self::Observe { target, limit, .. } => {
                target.validate()?;
                if *limit == 0 || *limit > 128 {
                    return Err("media event limit is outside the supported bounds".into());
                }
                Ok(())
            }
            Self::Wait {
                target,
                timeout_ms,
                limit,
                ..
            } => {
                target.validate()?;
                if *timeout_ms > 120_000 || *limit == 0 || *limit > 128 {
                    return Err("media wait limits are outside the supported bounds".into());
                }
                Ok(())
            }
            Self::Stop { target } => target.validate(),
        }
    }
}

/// Successful typed media operation result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaResultDto {
    /// Source inventory.
    Sources { sources: Vec<MediaSourceDto> },
    /// Current resource status.
    Status {
        resources: Vec<MediaResourceStatusDto>,
    },
    /// Captured media asset.
    Captured { asset: MediaAssetDto },
    /// Recording started.
    RecordingStarted { recording_id: String },
    /// Recording stopped and finalized.
    RecordingStopped {
        recording_id: String,
        asset: MediaAssetDto,
    },
    /// Speech transcript.
    Transcript {
        text: String,
        language: Option<String>,
        confidence: Option<f32>,
    },
    /// Synthesized audio asset.
    Synthesized { asset: MediaAssetDto },
    /// Playback completed.
    PlaybackCompleted { duration_ms: u64 },
    /// Speech playback completed.
    SpeechCompleted { duration_ms: u64 },
    /// Session started.
    SessionStarted {
        session_id: String,
        status: MediaSessionStatusDto,
    },
    /// Session updated.
    SessionUpdated {
        session_id: String,
        status: MediaSessionStatusDto,
    },
    /// Target events.
    Events { batch: MediaEventBatchDto },
    /// Resource stopped.
    Stopped { resource: MediaResourceRefDto },
}

impl MediaResultDto {
    /// Validate bounded vectors and opaque refs before returning a result.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Sources { sources } => {
                validate_collection_len(sources.len(), "media sources")?;
                for source in sources {
                    source.validate()?;
                }
            }
            Self::Status { resources } => {
                validate_collection_len(resources.len(), "media statuses")?;
                for status in resources {
                    status.resource.validate()?;
                }
            }
            Self::Captured { asset } | Self::Synthesized { asset } => asset.validate()?,
            Self::RecordingStarted { recording_id } => {
                validate_identifier(recording_id, "media recording reference")?
            }
            Self::RecordingStopped {
                recording_id,
                asset,
            } => {
                validate_identifier(recording_id, "media recording reference")?;
                asset.validate()?;
            }
            Self::Transcript {
                text,
                language,
                confidence,
            } => {
                validate_text(text, MAX_MEDIA_TEXT_BYTES, "media transcript")?;
                if let Some(language) = language {
                    validate_text(language, 64, "transcript language")?;
                }
                if confidence
                    .is_some_and(|score| !score.is_finite() || !(0.0..=1.0).contains(&score))
                {
                    return Err("transcript confidence is outside the supported bounds".into());
                }
            }
            Self::PlaybackCompleted { .. } | Self::SpeechCompleted { .. } => {}
            Self::SessionStarted { session_id, status }
            | Self::SessionUpdated { session_id, status } => {
                validate_identifier(session_id, "media session reference")?;
                status.validate()?;
            }
            Self::Events { batch } => batch.validate()?,
            Self::Stopped { resource } => resource.validate()?,
        }
        Ok(())
    }
}

/// Trusted host-control service action. Owner, initiator, operation identity,
/// and payload caps are supplied by the authenticated bridge host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaServiceActionDto {
    /// Execute one typed owner-scoped media operation.
    Execute {
        operation: MediaOperationDto,
        timeout_budget_ms: Option<u64>,
        max_payload_bytes: Option<u64>,
    },
    /// Read bytes after the host authorizes the owner-scoped reference.
    ReadMedia { media_ref: String },
    /// Cancel an operation after the host verifies its owner.
    Cancel { operation_id: MediaOperationIdDto },
    /// Update host-owned audio settings using opaque JSON.
    ///
    /// The generic client protocol validates JSON syntax and byte size only;
    /// product-specific normalization and persistence belong to the host.
    UpdateAudioSettings { configuration_json: String },
    /// Register an approved host media route with the product connector.
    RegisterConnection {
        connection_id: String,
        label: String,
        supported_modes: Vec<MediaSessionModeDto>,
        source_kinds: Vec<MediaSourceKindDto>,
    },
    /// Unregister a host media route from the product connector.
    UnregisterConnection { connection_id: String },
}

/// Host-private request for the trusted app's single MediaService instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaServiceRequestDto {
    /// Correlation ID echoed by the response.
    pub request_id: String,
    /// Typed host action; the caller cannot supply owner or credentials.
    pub action: MediaServiceActionDto,
}

impl MediaServiceRequestDto {
    /// Validate the public action before the bridge dispatches to host policy.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.request_id, "media service request ID")?;
        match &self.action {
            MediaServiceActionDto::Execute {
                operation,
                timeout_budget_ms,
                max_payload_bytes,
            } => {
                operation.validate()?;
                if timeout_budget_ms.is_some_and(|ms| ms == 0 || ms > 3_600_000)
                    || max_payload_bytes.is_some_and(|bytes| {
                        bytes == 0 || bytes > MAX_MEDIA_DEVICE_FRAME_BYTES as u64
                    })
                {
                    return Err(
                        "media service request limits are outside the supported bounds".into(),
                    );
                }
            }
            MediaServiceActionDto::ReadMedia { media_ref } => {
                validate_identifier(media_ref, "media content reference")?
            }
            MediaServiceActionDto::Cancel { operation_id } => operation_id.validate()?,
            MediaServiceActionDto::UpdateAudioSettings { configuration_json } => {
                if configuration_json.len() > MAX_MEDIA_AUDIO_SETTINGS_JSON_BYTES {
                    return Err("audio settings JSON exceeds the byte limit".into());
                }
                serde_json::from_str::<serde::de::IgnoredAny>(configuration_json)
                    .map_err(|_| "audio settings must contain valid JSON".to_owned())?;
            }
            MediaServiceActionDto::RegisterConnection {
                connection_id,
                label,
                supported_modes,
                source_kinds,
            } => {
                validate_identifier(connection_id, "media connection ID")?;
                validate_text(label, 256, "media connection label")?;
                if label.trim().is_empty()
                    || supported_modes.is_empty()
                    || supported_modes.len() > MAX_MEDIA_CONNECTION_MODES
                    || supported_modes
                        .iter()
                        .enumerate()
                        .any(|(index, mode)| supported_modes[..index].contains(mode))
                {
                    return Err("media connection modes are invalid or duplicated".into());
                }
                if source_kinds.is_empty()
                    || source_kinds.len() > MAX_MEDIA_CONNECTION_SOURCE_KINDS
                    || source_kinds
                        .iter()
                        .enumerate()
                        .any(|(index, kind)| source_kinds[..index].contains(kind))
                    || source_kinds.iter().any(|kind| {
                        !matches!(
                            kind,
                            MediaSourceKindDto::Microphone
                                | MediaSourceKindDto::Camera
                                | MediaSourceKindDto::Screen
                        )
                    })
                {
                    return Err("media connection source kinds are invalid or duplicated".into());
                }
                let has_audio = source_kinds.contains(&MediaSourceKindDto::Microphone);
                let has_video = source_kinds.iter().any(|kind| {
                    matches!(
                        kind,
                        MediaSourceKindDto::Camera | MediaSourceKindDto::Screen
                    )
                });
                let needs_audio = supported_modes.iter().any(|mode| {
                    matches!(
                        mode,
                        MediaSessionModeDto::Audio | MediaSessionModeDto::AudioVideo
                    )
                });
                let needs_video = supported_modes.iter().any(|mode| {
                    matches!(
                        mode,
                        MediaSessionModeDto::Video | MediaSessionModeDto::AudioVideo
                    )
                });
                if (needs_audio && !has_audio) || (needs_video && !has_video) {
                    return Err(
                        "media connection modes do not match the available source kinds".into(),
                    );
                }
            }
            MediaServiceActionDto::UnregisterConnection { connection_id } => {
                validate_identifier(connection_id, "media connection ID")?
            }
        }
        Ok(())
    }
}

/// Host-private result from a MediaService action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaServiceResultDto {
    /// Operation result and host-minted identity used for observe/cancel.
    Operation {
        operation_id: MediaOperationIdDto,
        result: MediaResultDto,
    },
    /// Read owner-scoped media bytes.
    Content { content: MediaContentDto },
    /// Cancellation completed.
    Cancelled,
    /// The host applied the audio settings update.
    SettingsUpdated,
    /// The host applied a connector route registration update.
    ConnectionUpdated,
}

/// Correlated response from the trusted app media service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaServiceResponseDto {
    /// Correlation ID from the request.
    pub request_id: String,
    /// Successful typed result.
    pub result: Option<MediaServiceResultDto>,
    /// Safe failure detail.
    pub error: Option<MediaErrorDto>,
}

impl MediaServiceResponseDto {
    /// Validate that a response has one result or one bounded error.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.request_id, "media service request ID")?;
        if self.result.is_some() == self.error.is_some() {
            return Err("media service response must contain exactly one result or error".into());
        }
        if let Some(error) = &self.error {
            error.validate()?;
        }
        match self.result.as_ref() {
            Some(MediaServiceResultDto::Operation {
                operation_id,
                result,
            }) => {
                operation_id.validate()?;
                result.validate()?;
            }
            Some(MediaServiceResultDto::Content { content }) => content.validate()?,
            Some(
                MediaServiceResultDto::Cancelled
                | MediaServiceResultDto::SettingsUpdated
                | MediaServiceResultDto::ConnectionUpdated,
            )
            | None => {}
        }
        Ok(())
    }
}

impl MediaSessionStatusDto {
    /// Check session state against its negotiated media mode.
    pub fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Current owner-visible resource status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaResourceStatusDto {
    /// Resource address.
    pub resource: MediaResourceRefDto,
    /// Lifecycle state.
    pub state: MediaResourceStateDto,
    /// Start time in Unix milliseconds.
    pub started_at_unix_ms: Option<u64>,
    /// Last update time in Unix milliseconds.
    pub updated_at_unix_ms: Option<u64>,
}

impl MediaResourceStatusDto {
    /// Validate an owner-visible status reference.
    pub fn validate(&self) -> Result<(), String> {
        self.resource.validate()
    }
}

/// Resource state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum MediaResourceStateDto {
    /// Available.
    Ready,
    /// Active.
    Active,
    /// Stopping.
    Stopping,
    /// Complete.
    Completed,
    /// Failed.
    Failed,
}

/// Bounded owner-visible event result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaEventBatchDto {
    /// Target resource.
    pub target: MediaResourceRefDto,
    /// Ordered events after the supplied cursor.
    pub events: Vec<MediaEventDto>,
    /// Cursor after the last event.
    pub next_sequence: Option<u64>,
    /// Whether earlier events were dropped.
    pub dropped_before_cursor: bool,
}

impl MediaEventBatchDto {
    /// Enforce the canonical event count, target, and text bounds.
    pub fn validate(&self) -> Result<(), String> {
        self.target.validate()?;
        validate_collection_len(self.events.len(), "media events")?;
        if self.events.len() > 128 {
            return Err("media event batch exceeds the event-count limit".into());
        }
        for event in &self.events {
            if event.target != self.target {
                return Err("media event batch contains another target".into());
            }
            event.validate()?;
        }
        if self
            .events
            .windows(2)
            .any(|pair| pair[0].sequence >= pair[1].sequence)
        {
            return Err("media event batch sequence must be strictly increasing".into());
        }
        if self.next_sequence != self.events.last().map(|event| event.sequence) {
            return Err("media event batch cursor must match its final event".into());
        }
        Ok(())
    }
}

/// One timestamped typed media event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaEventDto {
    /// Monotonic sequence.
    pub sequence: u64,
    /// Event time in Unix milliseconds.
    pub occurred_at_unix_ms: u64,
    /// Target resource.
    pub target: MediaResourceRefDto,
    /// Operation that produced the event.
    pub operation_id: Option<MediaOperationIdDto>,
    /// Typed payload.
    pub kind: MediaEventKindDto,
}

impl MediaEventDto {
    /// Validate target, operation ID, and the typed event payload.
    pub fn validate(&self) -> Result<(), String> {
        self.target.validate()?;
        if let Some(operation_id) = &self.operation_id {
            operation_id.validate()?;
        }
        match &self.kind {
            MediaEventKindDto::StateChanged { resource, .. } => {
                resource.validate()?;
                if resource != &self.target {
                    return Err("media state event resource must match its target".into());
                }
                Ok(())
            }
            MediaEventKindDto::Frame { source, asset } => {
                source.validate()?;
                if let MediaResourceRefDto::Source { source: target } = &self.target {
                    if target != source {
                        return Err("media frame source must match its event target".into());
                    }
                }
                asset.validate()
            }
            MediaEventKindDto::Transcript {
                text,
                language,
                confidence,
            } => {
                validate_text(text, MAX_MEDIA_EVENT_TEXT_BYTES, "media event transcript")?;
                if let Some(language) = language {
                    validate_text(language, 64, "transcript language")?;
                }
                if confidence
                    .is_some_and(|score| !score.is_finite() || !(0.0..=1.0).contains(&score))
                {
                    return Err("transcript confidence is outside the supported bounds".into());
                }
                Ok(())
            }
            MediaEventKindDto::Error { message, .. } => {
                validate_text(message, MAX_MEDIA_EVENT_TEXT_BYTES, "media event error")
            }
        }
    }
}

/// Event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaEventKindDto {
    /// Resource lifecycle change.
    StateChanged {
        resource: MediaResourceRefDto,
        state: MediaResourceStateDto,
    },
    /// Captured media asset.
    Frame {
        source: MediaSourceRefDto,
        asset: MediaAssetDto,
    },
    /// Transcript text.
    Transcript {
        text: String,
        language: Option<String>,
        confidence: Option<f32>,
    },
    /// Safe operational error.
    Error {
        kind: MediaErrorKindDto,
        message: String,
    },
}

/// Encoded media returned for an owner-visible reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaContentDto {
    /// Opaque owner-visible reference.
    pub media_ref: String,
    /// Safe metadata.
    pub metadata: MediaMetadataDto,
    /// Encoded bytes, bounded by `max_payload_bytes`.
    pub bytes: Vec<u8>,
}

impl MediaContentDto {
    /// Check reference, metadata size, and actual bytes before sending content.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.media_ref, "media content reference")?;
        self.metadata.validate()?;
        if self.bytes.len() > MAX_MEDIA_DEVICE_FRAME_BYTES
            || self.bytes.len() as u64 != self.metadata.byte_len
        {
            return Err("media content bytes exceed or disagree with metadata".into());
        }
        validate_wire_frame_bytes(self.bytes.len())
    }
}

/// Machine-readable media error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaErrorDto {
    /// Failure class.
    pub kind: MediaErrorKindDto,
    /// Safe error detail.
    pub message: String,
}

impl MediaErrorDto {
    /// Bound safe error text before it crosses the transport.
    pub fn validate(&self) -> Result<(), String> {
        validate_text(&self.message, MAX_MEDIA_EVENT_TEXT_BYTES, "media error")
    }
}

/// Media failure class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum MediaErrorKindDto {
    /// Permission denied.
    PermissionDenied,
    /// Incompatible lease active.
    Busy,
    /// Caller cancelled.
    Cancelled,
    /// Operation timed out.
    Timeout,
    /// No data available.
    NoData,
    /// Stale or unauthorized reference.
    NotFound,
    /// Provider or device unavailable.
    Unavailable,
    /// Operation unsupported.
    Unsupported,
    /// Offline model missing.
    ModelMissing,
    /// Invalid request.
    InvalidRequest,
    /// Processing failed.
    ProcessingFailed,
    /// Native failure.
    NativeFailure,
    /// Storage failure.
    StoreFailure,
    /// Payload too large.
    MediaTooLarge,
}

/// Native session-open request passed to MediaDevicePorts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaDeviceOpenRequestDto {
    /// Operation identity and epoch.
    pub identity: MediaOperationIdDto,
    /// Stable resource owner.
    pub owner: MediaOwnerDto,
    /// Per-call attribution.
    pub initiator: Option<MediaInitiatorDto>,
    /// Remaining operation time budget.
    pub timeout_budget_ms: Option<u64>,
    /// Maximum media payload bytes.
    pub max_payload_bytes: u64,
    /// Requested transport/session mode.
    pub request: MediaSessionRequestDto,
}

/// Opaque native device lease returned from open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaDeviceLeaseDto {
    /// Opaque host-issued lease ID.
    pub lease_id: String,
}

/// One bounded PCM16 little-endian audio frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaPcmFrameDto {
    /// Sample rate in Hz.
    pub sample_rate_hz: u32,
    /// Interleaved channel count.
    pub channels: u16,
    /// Interleaved PCM16LE bytes.
    pub pcm16le: Vec<u8>,
}

impl MediaPcmFrameDto {
    /// Check sample metadata and raw bytes before a host allocates or sends the
    /// frame across an FFI or bridge boundary.
    pub fn validate(&self) -> Result<(), String> {
        if self.sample_rate_hz == 0 || self.channels == 0 {
            return Err("PCM frame requires a sample rate and at least one channel".into());
        }
        if self.pcm16le.is_empty() || self.pcm16le.len() % 2 != 0 {
            return Err("PCM16LE frame must contain a non-empty whole number of samples".into());
        }
        validate_frame_bytes(self.pcm16le.len())
    }
}

/// Bounded raw video frame in a platform-neutral pixel format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaVideoFrameDto {
    /// Packed RGBA8 pixels.
    Rgba8 {
        width_px: u32,
        height_px: u32,
        stride_bytes: u32,
        bytes: Vec<u8>,
    },
    /// Planar YUV420 pixels.
    I420 {
        width_px: u32,
        height_px: u32,
        y_stride_bytes: u32,
        u_stride_bytes: u32,
        v_stride_bytes: u32,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    },
}

impl MediaVideoFrameDto {
    /// Check dimensions, strides, exact plane lengths and the raw frame cap.
    pub fn validate(&self) -> Result<(), String> {
        let payload_len = match self {
            Self::Rgba8 {
                width_px,
                height_px,
                stride_bytes,
                bytes,
            } => {
                let row_bytes = width_px
                    .checked_mul(4)
                    .ok_or_else(|| "RGBA frame row size overflow".to_owned())?;
                if *width_px == 0 || *height_px == 0 || *stride_bytes < row_bytes {
                    return Err("RGBA frame dimensions or stride are invalid".into());
                }
                let expected = usize::try_from(*stride_bytes)
                    .ok()
                    .and_then(|stride| usize::try_from(*height_px).ok()?.checked_mul(stride))
                    .ok_or_else(|| "RGBA frame size overflow".to_owned())?;
                if bytes.len() != expected {
                    return Err(
                        "RGBA frame byte length does not match its stride and height".into(),
                    );
                }
                bytes.len()
            }
            Self::I420 {
                width_px,
                height_px,
                y_stride_bytes,
                u_stride_bytes,
                v_stride_bytes,
                y,
                u,
                v,
            } => {
                if *width_px == 0 || *height_px == 0 {
                    return Err("I420 frame dimensions must be non-zero".into());
                }
                let chroma_width = width_px.div_ceil(2);
                let chroma_height = height_px.div_ceil(2);
                if *y_stride_bytes < *width_px
                    || *u_stride_bytes < chroma_width
                    || *v_stride_bytes < chroma_width
                {
                    return Err("I420 frame stride is smaller than its plane width".into());
                }
                let plane_len = |stride: u32, rows: u32| -> Result<usize, String> {
                    usize::try_from(stride)
                        .ok()
                        .and_then(|stride| usize::try_from(rows).ok()?.checked_mul(stride))
                        .ok_or_else(|| "I420 frame size overflow".to_owned())
                };
                let y_len = plane_len(*y_stride_bytes, *height_px)?;
                let u_len = plane_len(*u_stride_bytes, chroma_height)?;
                let v_len = plane_len(*v_stride_bytes, chroma_height)?;
                if y.len() != y_len || u.len() != u_len || v.len() != v_len {
                    return Err(
                        "I420 frame byte lengths do not match their strides and dimensions".into(),
                    );
                }
                let total = y_len
                    .checked_add(u_len)
                    .and_then(|total| total.checked_add(v_len))
                    .ok_or_else(|| "I420 frame size overflow".to_owned())?;
                total
            }
        };
        validate_frame_bytes(payload_len)
    }
}

/// Audio or video captured by one native media lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", content = "frame", rename_all = "snake_case")]
pub enum MediaFramePayloadDto {
    /// Captured PCM16 audio.
    Audio(MediaPcmFrameDto),
    /// Captured camera or screen frame.
    Video(MediaVideoFrameDto),
}

/// Timestamped native media input frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaFrameDto {
    /// Owner-visible source.
    pub source: MediaSourceRefDto,
    /// Monotonic source sequence.
    pub sequence: u64,
    /// Capture time in Unix milliseconds.
    pub captured_at_unix_ms: u64,
    /// Typed frame payload.
    pub payload: MediaFramePayloadDto,
}

impl MediaFrameDto {
    /// Validate a captured frame before it is returned to the host bridge.
    pub fn validate(&self) -> Result<(), String> {
        self.source.validate()?;
        match &self.payload {
            MediaFramePayloadDto::Audio(frame) => frame.validate(),
            MediaFramePayloadDto::Video(frame) => frame.validate(),
        }
    }
}

/// Native renderer receipt for an accepted PCM frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaRenderReceiptDto {
    /// Accepted interleaved sample frames.
    pub accepted_sample_frames: u32,
    /// Sample frames known to have reached the renderer after flush/interrupt.
    pub played_sample_frames: u32,
    /// Native render callback time in Unix milliseconds.
    pub rendered_at_unix_ms: Option<u64>,
}

/// Native media session update request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaDeviceUpdateRequestDto {
    /// Opaque lease identifier.
    pub lease_id: String,
    /// Typed update.
    pub update: MediaSessionUpdateDto,
}

/// Native audio write request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaDeviceAudioWriteRequestDto {
    /// Opaque lease identifier.
    pub lease_id: String,
    /// Bounded PCM payload.
    pub frame: MediaPcmFrameDto,
}

/// Native video write request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaDeviceVideoWriteRequestDto {
    /// Opaque lease identifier.
    pub lease_id: String,
    /// Bounded video payload.
    pub frame: MediaVideoFrameDto,
}

/// Native audio barrier request for one shared media lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaDeviceAudioControlRequestDto {
    /// Opaque native lease identifier.
    pub lease_id: String,
}

/// Native media device callback operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaDeviceRequestDto {
    /// Open an owner-bound native lease.
    Open { request: MediaDeviceOpenRequestDto },
    /// Read the next captured input.
    NextInput { lease_id: String },
    /// Render decoded remote audio.
    WriteAudio {
        request: MediaDeviceAudioWriteRequestDto,
    },
    /// Render decoded remote video.
    WriteVideo {
        request: MediaDeviceVideoWriteRequestDto,
    },
    /// Wait until audio accepted before this barrier has played.
    FlushAudio {
        request: MediaDeviceAudioControlRequestDto,
    },
    /// Interrupt queued local audio and report actual playback since the previous barrier.
    InterruptAudio {
        request: MediaDeviceAudioControlRequestDto,
    },
    /// Update the lease.
    Update {
        request: MediaDeviceUpdateRequestDto,
    },
    /// Stop and release the lease.
    Stop { lease_id: String },
}

/// Host-private envelope that correlates every device callback uniformly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaDeviceRequestEnvelopeDto {
    /// Host-issued request identifier echoed by the response.
    pub request_id: String,
    /// Typed callback operation.
    pub request: MediaDeviceRequestDto,
}

impl MediaDeviceRequestEnvelopeDto {
    /// Validate raw and expanded payload sizes before dispatching the callback.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.request_id, "media device request ID")?;
        let frame_bytes = match &self.request {
            MediaDeviceRequestDto::Open { request } => {
                request.identity.validate()?;
                request.owner.validate()?;
                if let Some(initiator) = &request.initiator {
                    initiator.validate()?;
                }
                if request
                    .timeout_budget_ms
                    .is_some_and(|ms| ms == 0 || ms > 3_600_000)
                {
                    return Err("media open time budget is invalid".into());
                }
                if request.max_payload_bytes == 0
                    || request.max_payload_bytes > MAX_MEDIA_DEVICE_FRAME_BYTES as u64
                {
                    return Err("media open payload limit is invalid".into());
                }
                request.request.validate()?;
                0
            }
            MediaDeviceRequestDto::NextInput { lease_id }
            | MediaDeviceRequestDto::Stop { lease_id } => {
                validate_identifier(lease_id, "media lease ID")?;
                0
            }
            MediaDeviceRequestDto::WriteAudio { request } => {
                validate_identifier(&request.lease_id, "media lease ID")?;
                request.frame.validate()?;
                request.frame.pcm16le.len()
            }
            MediaDeviceRequestDto::WriteVideo { request } => {
                validate_identifier(&request.lease_id, "media lease ID")?;
                request.frame.validate()?;
                request.frame.payload_len()?
            }
            MediaDeviceRequestDto::FlushAudio { request }
            | MediaDeviceRequestDto::InterruptAudio { request } => {
                validate_identifier(&request.lease_id, "media lease ID")?;
                0
            }
            MediaDeviceRequestDto::Update { request } => {
                validate_identifier(&request.lease_id, "media lease ID")?;
                request.update.validate()?;
                0
            }
        };
        validate_wire_frame_bytes(frame_bytes)
    }
}

/// Correlated native media callback response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaDeviceResponseDto {
    /// Request ID supplied by the host.
    pub request_id: String,
    /// Typed operation result.
    pub result: MediaDeviceResultDto,
}

impl MediaDeviceResponseDto {
    /// Validate captured/rendered frame bounds before sending a host response.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.request_id, "media device response request ID")?;
        let frame_bytes = match &self.result {
            MediaDeviceResultDto::Opened { lease } => {
                validate_identifier(&lease.lease_id, "media lease ID")?;
                0
            }
            MediaDeviceResultDto::Input { frame: Some(frame) } => {
                frame.validate()?;
                frame.payload_len()?
            }
            MediaDeviceResultDto::AudioWritten { .. }
            | MediaDeviceResultDto::AudioFlushed { .. }
            | MediaDeviceResultDto::AudioInterrupted { .. }
            | MediaDeviceResultDto::VideoWritten
            | MediaDeviceResultDto::Updated { .. }
            | MediaDeviceResultDto::Stopped
            | MediaDeviceResultDto::Error { .. }
            | MediaDeviceResultDto::Input { frame: None } => 0,
        };
        if let MediaDeviceResultDto::Error { error } = &self.result {
            error.validate()?;
        }
        if let MediaDeviceResultDto::Updated { status } = &self.result {
            status.validate()?;
        }
        match &self.result {
            MediaDeviceResultDto::AudioFlushed { receipt }
            | MediaDeviceResultDto::AudioInterrupted { receipt }
                if receipt.accepted_sample_frames != 0 =>
            {
                return Err("media audio barrier receipt cannot accept new sample frames".into());
            }
            _ => {}
        }
        validate_wire_frame_bytes(frame_bytes)
    }
}

/// Native media callback result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaDeviceResultDto {
    /// Lease opened.
    Opened { lease: MediaDeviceLeaseDto },
    /// Input frame or end of stream.
    Input { frame: Option<MediaFrameDto> },
    /// Audio frame accepted.
    AudioWritten { receipt: MediaRenderReceiptDto },
    /// Audio playback drained through the requested barrier.
    AudioFlushed { receipt: MediaRenderReceiptDto },
    /// Audio playback was interrupted and the played count acknowledged.
    AudioInterrupted { receipt: MediaRenderReceiptDto },
    /// Video frame accepted.
    VideoWritten,
    /// Lease updated.
    Updated { status: MediaSessionStatusDto },
    /// Lease stopped.
    Stopped,
    /// Native operation failed.
    Error { error: MediaErrorDto },
}

/// SDP description kind for host-controlled media negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum MediaSessionDescriptionTypeDto {
    /// Remote peer offer.
    Offer,
    /// Host-generated answer.
    Answer,
}

/// Bounded SDP description exchanged only between trusted host adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaSessionDescriptionDto {
    /// Offer or answer.
    pub kind: MediaSessionDescriptionTypeDto,
    /// SDP text, limited by `MAX_REMOTE_MEDIA_SDP_BYTES`.
    pub sdp: String,
}

impl MediaSessionDescriptionDto {
    /// Bound SDP before sending or accepting a host-control negotiation packet.
    pub fn validate(&self) -> Result<(), String> {
        if self.sdp.is_empty() || self.sdp.len() > MAX_REMOTE_MEDIA_SDP_BYTES {
            return Err("media session description exceeds its byte limit".into());
        }
        Ok(())
    }
}

/// Correlated host-control request for native media session negotiation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaConnectionRequestDto {
    /// Correlation identity.
    pub request_id: String,
    /// Opaque host-selected connector/connection identity.
    pub connection_id: String,
    /// Active conversation session.
    pub session_id: String,
    /// Opaque media session returned by `media_session_start`.
    pub media_session_ref: MediaSessionRefDto,
    /// Media operation identity.
    pub operation_id: MediaOperationIdDto,
    /// Stable owner.
    pub owner: MediaOwnerDto,
    /// Requested session mode.
    pub mode: MediaSessionModeDto,
    /// Conversation or observation intent.
    pub intent: MediaSessionIntentDto,
    /// Optional microphone source.
    pub audio_source: Option<MediaSourceRefDto>,
    /// Optional camera/screen source.
    pub video_source: Option<MediaSourceRefDto>,
    /// Bounded peer offer; never exposed to model tool arguments.
    pub description: MediaSessionDescriptionDto,
}

impl MediaConnectionRequestDto {
    /// Bound the host-private negotiation message before transport allocation.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.request_id, "media connection request ID")?;
        validate_identifier(&self.connection_id, "media connection ID")?;
        validate_identifier(&self.session_id, "media session ID")?;
        self.media_session_ref.validate()?;
        self.operation_id.validate()?;
        self.owner.validate()?;
        if let Some(source) = &self.audio_source {
            source.validate()?;
        }
        if let Some(source) = &self.video_source {
            source.validate()?;
        }
        self.description.validate()?;
        if self.description.kind != MediaSessionDescriptionTypeDto::Offer {
            return Err("media connection request must carry an Offer".into());
        }
        Ok(())
    }
}

/// Correlated host-control response for native media session negotiation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaConnectionResponseDto {
    /// Correlation identity from the request.
    pub request_id: String,
    /// Opaque connection identity from the request.
    pub connection_id: String,
    /// Media session reference from the request, echoed exactly by the host.
    pub media_session_ref: MediaSessionRefDto,
    /// Bounded answer when successful.
    pub description: Option<MediaSessionDescriptionDto>,
    /// Safe failure detail when unsuccessful.
    pub error: Option<MediaErrorDto>,
}

impl MediaConnectionResponseDto {
    /// Bound the host-private response before transport allocation.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.request_id, "media connection request ID")?;
        validate_identifier(&self.connection_id, "media connection ID")?;
        self.media_session_ref.validate()?;
        if let Some(description) = &self.description {
            description.validate()?;
            if description.kind != MediaSessionDescriptionTypeDto::Answer {
                return Err("media connection response must carry an Answer".into());
            }
        }
        if let Some(error) = &self.error {
            error.validate()?;
        }
        if self.description.is_some() == self.error.is_some() {
            return Err(
                "media connection response must contain exactly one of answer or error".into(),
            );
        }
        Ok(())
    }

    /// Check that this response is correlated to the exact host request.
    #[must_use]
    pub fn matches_request(&self, request: &MediaConnectionRequestDto) -> bool {
        self.request_id == request.request_id
            && self.connection_id == request.connection_id
            && self.media_session_ref == request.media_session_ref
    }
}

/// Per-session peer audio barrier action sent only to the authenticated native
/// media adapter. Product-specific control notifications stay outside Harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaControlActionDto {
    /// Drain peer playback through the requested remote input barrier.
    FlushAudio {
        target_sequence: u64,
        target_sample_frames: u64,
    },
    /// Stop peer playback and report actual output through the barrier.
    InterruptAudio {
        target_sequence: u64,
        target_sample_frames: u64,
    },
    /// Close the remote native media session and release its device lease.
    /// The response receipt is sent only after callback-backed interruption
    /// and native lease cleanup succeed.
    Close,
}

/// Host-private, generation-bound audio control for one media connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaControlRequestDto {
    /// Correlation identity echoed by the native response.
    pub request_id: String,
    /// Opaque host-selected connection identity.
    pub connection_id: String,
    /// Media service session that owns this connection.
    pub media_session_ref: MediaSessionRefDto,
    /// Host-issued connection generation; stale control is rejected.
    pub generation: u64,
    /// Typed playback barrier.
    pub action: MediaControlActionDto,
}

impl MediaControlRequestDto {
    /// Validate opaque identities before routing to the native adapter.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.request_id, "media control request ID")?;
        validate_identifier(&self.connection_id, "media connection ID")?;
        self.media_session_ref.validate()?;
        let action_valid = match &self.action {
            MediaControlActionDto::FlushAudio {
                target_sequence,
                target_sample_frames,
            }
            | MediaControlActionDto::InterruptAudio {
                target_sequence,
                target_sample_frames,
            } => {
                *target_sequence <= MAX_MEDIA_CONTROL_AUDIO_SEQUENCE
                    && *target_sample_frames <= MAX_MEDIA_CONTROL_AUDIO_SAMPLE_FRAMES
                    && (*target_sequence == 0) == (*target_sample_frames == 0)
            }
            MediaControlActionDto::Close => true,
        };
        if self.generation == 0 || !action_valid {
            return Err("media control generation or action bounds are invalid".into());
        }
        Ok(())
    }
}

/// Correlated result of a native peer audio barrier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MediaControlResponseDto {
    /// Correlation identity from the request.
    pub request_id: String,
    /// Opaque connection identity from the request.
    pub connection_id: String,
    /// Media service session identity from the request.
    pub media_session_ref: MediaSessionRefDto,
    /// Connection generation from the request.
    pub generation: u64,
    /// Actual playback receipt when the barrier completed.
    pub receipt: Option<MediaRenderReceiptDto>,
    /// Safe native failure detail.
    pub error: Option<MediaErrorDto>,
}

impl MediaControlResponseDto {
    /// Validate response shape and correlation against one pending request.
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier(&self.request_id, "media control request ID")?;
        validate_identifier(&self.connection_id, "media connection ID")?;
        self.media_session_ref.validate()?;
        if self.generation == 0 || self.receipt.is_some() == self.error.is_some() {
            return Err("media control response has invalid generation or result shape".into());
        }
        if let Some(error) = &self.error {
            error.validate()?;
        }
        if self
            .receipt
            .is_some_and(|receipt| receipt.accepted_sample_frames != 0)
        {
            return Err("media audio barrier receipt cannot accept new sample frames".into());
        }
        Ok(())
    }

    /// Check that the response belongs to the exact request and generation.
    #[must_use]
    pub fn matches_request(&self, request: &MediaControlRequestDto) -> bool {
        self.request_id == request.request_id
            && self.connection_id == request.connection_id
            && self.media_session_ref == request.media_session_ref
            && self.generation == request.generation
    }
}

impl MediaVideoFrameDto {
    fn payload_len(&self) -> Result<usize, String> {
        match self {
            Self::Rgba8 { bytes, .. } => Ok(bytes.len()),
            Self::I420 { y, u, v, .. } => y
                .len()
                .checked_add(u.len())
                .and_then(|total| total.checked_add(v.len()))
                .ok_or_else(|| "I420 frame size overflow".to_owned()),
        }
    }
}

impl MediaFrameDto {
    fn payload_len(&self) -> Result<usize, String> {
        match &self.payload {
            MediaFramePayloadDto::Audio(frame) => Ok(frame.pcm16le.len()),
            MediaFramePayloadDto::Video(frame) => frame.payload_len(),
        }
    }
}

fn validate_identifier(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 256 {
        return Err(format!("{label} is invalid"));
    }
    Ok(())
}

fn validate_text(value: &str, max_bytes: usize, label: &str) -> Result<(), String> {
    if value.len() > max_bytes {
        return Err(format!("{label} exceeds its byte limit"));
    }
    Ok(())
}

fn validate_collection_len(len: usize, label: &str) -> Result<(), String> {
    if len > MAX_MEDIA_COLLECTION_ITEMS {
        return Err(format!("{label} exceed the collection limit"));
    }
    Ok(())
}

fn validate_frame_bytes(bytes: usize) -> Result<(), String> {
    if bytes == 0 || bytes > MAX_MEDIA_DEVICE_FRAME_BYTES {
        return Err("media device frame exceeds the raw byte limit".into());
    }
    Ok(())
}

fn validate_wire_frame_bytes(bytes: usize) -> Result<(), String> {
    validate_frame_bytes(bytes).or_else(|error| if bytes == 0 { Ok(()) } else { Err(error) })?;
    // JSON byte arrays expand to at most four bytes per payload byte (decimal
    // value plus separator). Saturating arithmetic avoids overflow on hostile
    // dimensions or lengths.
    if bytes.saturating_mul(4).saturating_add(16 * 1024) > MAX_MEDIA_DEVICE_WIRE_BYTES {
        return Err("media device frame exceeds the encoded wire byte limit".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i420_requires_exact_bounded_plane_lengths() {
        let frame = MediaVideoFrameDto::I420 {
            width_px: 4,
            height_px: 2,
            y_stride_bytes: 4,
            u_stride_bytes: 2,
            v_stride_bytes: 2,
            y: vec![0; 8],
            u: vec![0; 2],
            v: vec![0; 2],
        };
        assert!(frame.validate().is_ok());

        let malformed = MediaVideoFrameDto::I420 {
            width_px: 4,
            height_px: 2,
            y_stride_bytes: 4,
            u_stride_bytes: 2,
            v_stride_bytes: 2,
            y: vec![0; 7],
            u: vec![0; 2],
            v: vec![0; 2],
        };
        assert!(malformed.validate().is_err());
    }

    #[test]
    fn device_frame_checks_raw_size_and_json_expansion_before_dispatch() {
        let request = MediaDeviceRequestEnvelopeDto {
            request_id: "write-1".into(),
            request: MediaDeviceRequestDto::WriteAudio {
                request: MediaDeviceAudioWriteRequestDto {
                    lease_id: "lease-1".into(),
                    frame: MediaPcmFrameDto {
                        sample_rate_hz: 48_000,
                        channels: 2,
                        pcm16le: vec![0; MAX_MEDIA_DEVICE_WIRE_BYTES / 4 + 1],
                    },
                },
            },
        };
        assert!(request.validate().is_err());

        let odd_pcm = MediaPcmFrameDto {
            sample_rate_hz: 48_000,
            channels: 2,
            pcm16le: vec![0; 3],
        };
        assert!(odd_pcm.validate().is_err());
    }

    #[test]
    fn media_event_batch_enforces_target_count_and_text_limits() {
        let target = MediaResourceRefDto::Session {
            session_id: "session-1".into(),
        };
        let wrong_target = MediaResourceRefDto::Session {
            session_id: "session-2".into(),
        };
        let batch = MediaEventBatchDto {
            target: target.clone(),
            events: vec![MediaEventDto {
                sequence: 1,
                occurred_at_unix_ms: 1,
                target: wrong_target,
                operation_id: None,
                kind: MediaEventKindDto::Transcript {
                    text: "spoken text".into(),
                    language: None,
                    confidence: None,
                },
            }],
            next_sequence: Some(1),
            dropped_before_cursor: false,
        };
        assert!(batch.validate().is_err());

        let too_many = MediaEventBatchDto {
            target: target.clone(),
            events: (0..129)
                .map(|sequence| MediaEventDto {
                    sequence,
                    occurred_at_unix_ms: sequence,
                    target: target.clone(),
                    operation_id: None,
                    kind: MediaEventKindDto::Error {
                        kind: MediaErrorKindDto::Unavailable,
                        message: "unavailable".into(),
                    },
                })
                .collect(),
            next_sequence: Some(129),
            dropped_before_cursor: false,
        };
        assert!(too_many.validate().is_err());

        let too_long = MediaErrorDto {
            kind: MediaErrorKindDto::Unavailable,
            message: "x".repeat(MAX_MEDIA_EVENT_TEXT_BYTES + 1),
        };
        assert!(too_long.validate().is_err());

        let out_of_order = MediaEventBatchDto {
            target: target.clone(),
            events: vec![1_u64, 1]
                .into_iter()
                .map(|sequence| MediaEventDto {
                    sequence,
                    occurred_at_unix_ms: sequence,
                    target: target.clone(),
                    operation_id: None,
                    kind: MediaEventKindDto::StateChanged {
                        resource: target.clone(),
                        state: MediaResourceStateDto::Active,
                    },
                })
                .collect(),
            next_sequence: Some(1),
            dropped_before_cursor: false,
        };
        assert!(out_of_order.validate().is_err());
    }

    #[test]
    fn operation_and_content_dtos_reject_unbounded_values() {
        let operation = MediaOperationDto::Synthesize {
            text: "x".repeat(MAX_MEDIA_TEXT_BYTES + 1),
            language: None,
            rate: None,
            voice: None,
        };
        assert!(operation.validate().is_err());

        let content = MediaContentDto {
            media_ref: "asset-1".into(),
            metadata: MediaMetadataDto {
                kind: MediaKindDto::Audio,
                mime_type: "audio/wav".into(),
                byte_len: 1,
                captured_at_unix_ms: None,
                duration_ms: None,
                width_px: None,
                height_px: None,
                sample_rate_hz: Some(24_000),
                channels: Some(1),
            },
            bytes: Vec::new(),
        };
        assert!(content.validate().is_err());
    }

    #[test]
    fn media_control_response_is_bound_to_session_and_generation() {
        let request = MediaControlRequestDto {
            request_id: "control-1".into(),
            connection_id: "connection-1".into(),
            media_session_ref: MediaSessionRefDto {
                id: "media-session-1".into(),
            },
            generation: 7,
            action: MediaControlActionDto::InterruptAudio {
                target_sequence: 12,
                target_sample_frames: 5760,
            },
        };
        let response = MediaControlResponseDto {
            request_id: request.request_id.clone(),
            connection_id: request.connection_id.clone(),
            media_session_ref: request.media_session_ref.clone(),
            generation: request.generation,
            receipt: Some(MediaRenderReceiptDto {
                accepted_sample_frames: 0,
                played_sample_frames: 5760,
                rendered_at_unix_ms: Some(42),
            }),
            error: None,
        };
        assert!(request.validate().is_ok());
        assert!(response.validate().is_ok());
        assert!(response.matches_request(&request));

        let stale = MediaControlRequestDto {
            generation: 6,
            ..request
        };
        assert!(!response.matches_request(&stale));
    }

    #[test]
    fn host_audio_settings_action_checks_json_syntax_and_bytes_only() {
        let configuration_json =
            r#"{"version":4,"vendor_extension":{"future_field":true}}"#.to_owned();
        let request = MediaServiceRequestDto {
            request_id: "settings-1".into(),
            action: MediaServiceActionDto::UpdateAudioSettings {
                configuration_json: configuration_json.clone(),
            },
        };
        assert!(request.validate().is_ok());
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            serde_json::from_slice::<MediaServiceRequestDto>(&encoded).unwrap(),
            request
        );

        let invalid_json = MediaServiceRequestDto {
            request_id: "settings-2".into(),
            action: MediaServiceActionDto::UpdateAudioSettings {
                configuration_json: r#"{"version":}"#.into(),
            },
        };
        assert!(invalid_json.validate().is_err());

        let oversized_json = MediaServiceRequestDto {
            request_id: "settings-3".into(),
            action: MediaServiceActionDto::UpdateAudioSettings {
                configuration_json: format!(
                    "\"{}\"",
                    "x".repeat(MAX_MEDIA_AUDIO_SETTINGS_JSON_BYTES)
                ),
            },
        };
        assert!(oversized_json.validate().is_err());

        let response = MediaServiceResponseDto {
            request_id: "settings-1".into(),
            result: Some(MediaServiceResultDto::SettingsUpdated),
            error: None,
        };
        assert!(response.validate().is_ok());
    }

    #[test]
    fn host_route_registration_actions_are_bounded_and_correlated() {
        let request = MediaServiceRequestDto {
            request_id: "route-1".into(),
            action: MediaServiceActionDto::RegisterConnection {
                connection_id: "approved-route".into(),
                label: "Approved screen and audio route".into(),
                supported_modes: vec![MediaSessionModeDto::AudioVideo],
                source_kinds: vec![MediaSourceKindDto::Microphone, MediaSourceKindDto::Screen],
            },
        };
        assert!(request.validate().is_ok());

        let duplicate_modes = MediaServiceRequestDto {
            request_id: "route-2".into(),
            action: MediaServiceActionDto::RegisterConnection {
                connection_id: "approved-route".into(),
                label: "Route".into(),
                supported_modes: vec![MediaSessionModeDto::Audio, MediaSessionModeDto::Audio],
                source_kinds: vec![MediaSourceKindDto::Microphone],
            },
        };
        assert!(duplicate_modes.validate().is_err());

        let too_many_modes = MediaServiceRequestDto {
            request_id: "route-2b".into(),
            action: MediaServiceActionDto::RegisterConnection {
                connection_id: "approved-route".into(),
                label: "Route".into(),
                supported_modes: vec![
                    MediaSessionModeDto::Audio,
                    MediaSessionModeDto::Video,
                    MediaSessionModeDto::AudioVideo,
                    MediaSessionModeDto::Audio,
                ],
                source_kinds: vec![MediaSourceKindDto::Microphone, MediaSourceKindDto::Camera],
            },
        };
        assert!(too_many_modes.validate().is_err());

        let oversized_label = MediaServiceRequestDto {
            request_id: "route-2c".into(),
            action: MediaServiceActionDto::RegisterConnection {
                connection_id: "approved-route".into(),
                label: "x".repeat(257),
                supported_modes: vec![MediaSessionModeDto::Audio],
                source_kinds: vec![MediaSourceKindDto::Microphone],
            },
        };
        assert!(oversized_label.validate().is_err());

        let inconsistent_sources = MediaServiceRequestDto {
            request_id: "route-2d".into(),
            action: MediaServiceActionDto::RegisterConnection {
                connection_id: "approved-route".into(),
                label: "Audio and video route".into(),
                supported_modes: vec![MediaSessionModeDto::AudioVideo],
                source_kinds: vec![MediaSourceKindDto::Microphone],
            },
        };
        assert!(inconsistent_sources.validate().is_err());

        let duplicate_source_kinds = MediaServiceRequestDto {
            request_id: "route-2e".into(),
            action: MediaServiceActionDto::RegisterConnection {
                connection_id: "approved-route".into(),
                label: "Audio route".into(),
                supported_modes: vec![MediaSessionModeDto::Audio],
                source_kinds: vec![
                    MediaSourceKindDto::Microphone,
                    MediaSourceKindDto::Microphone,
                ],
            },
        };
        assert!(duplicate_source_kinds.validate().is_err());

        let unsupported_source_kind = MediaServiceRequestDto {
            request_id: "route-2f".into(),
            action: MediaServiceActionDto::RegisterConnection {
                connection_id: "approved-route".into(),
                label: "Audio route".into(),
                supported_modes: vec![MediaSessionModeDto::Audio],
                source_kinds: vec![MediaSourceKindDto::Remote],
            },
        };
        assert!(unsupported_source_kind.validate().is_err());

        let unregister = MediaServiceRequestDto {
            request_id: "route-3".into(),
            action: MediaServiceActionDto::UnregisterConnection {
                connection_id: "approved-route".into(),
            },
        };
        assert!(unregister.validate().is_ok());
        assert!(MediaServiceResponseDto {
            request_id: "route-1".into(),
            result: Some(MediaServiceResultDto::ConnectionUpdated),
            error: None,
        }
        .validate()
        .is_ok());
    }
}
