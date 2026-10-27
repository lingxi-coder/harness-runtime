//! Stable, standalone contracts for host-owned media capabilities.
//!
//! This crate intentionally has no dependency on the engine, agent/model SDKs,
//! or a particular storage or signaling provider. Hosts inject a
//! [`MediaService`] and, where needed, a configured [`MediaConnector`]. Model
//! requests contain typed opaque references; credentials, SDP and provider
//! configuration remain inside host implementations.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use std::fmt;
use std::sync::Arc;
use thiserror::Error;

pub mod audio;
pub mod camera;
pub mod stt;
pub mod tts;
pub mod voice;

/// Maximum number of events returned from a single observation call.
pub const MAX_MEDIA_EVENT_BATCH_SIZE: u16 = 128;
/// Maximum source identities a single session may expose in observed frames.
pub const MAX_MEDIA_SESSION_SOURCE_REFS: usize = 16;
/// Maximum UTF-8 byte length of transcript text or error detail in one event.
pub const MAX_MEDIA_EVENT_TEXT_BYTES: usize = 1_024;
/// Maximum media payload size accepted by a default operation context.
pub const DEFAULT_MAX_MEDIA_PAYLOAD_BYTES: u64 = 8 * 1024 * 1024;

/// Stable identity shared by hosts, transports, and media implementations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MediaOperationId {
    /// Globally unique UUID v4 string.
    pub id: String,
    /// Monotonic operation generation assigned by the host.
    pub generation: u64,
    /// Media service instance epoch assigned by the host.
    pub service_epoch: u64,
}

impl MediaOperationId {
    /// Mint an operation ID for a host-selected generation and service epoch.
    #[must_use]
    pub fn new(generation: u64, service_epoch: u64) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            generation,
            service_epoch,
        }
    }
}

/// Trusted owner of a long-lived device or remote media resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MediaOwner {
    /// Conversation session; tool and agent IDs do not change its lifetime.
    Session {
        /// Stable conversation or session identifier.
        session_id: String,
    },
    /// UI app instance.
    Ui {
        /// Stable host UI instance identifier.
        instance_id: String,
    },
    /// Host-owned operation outside a chat.
    System {
        /// Stable host service instance identifier.
        instance_id: String,
    },
}

/// Per-call attribution, separate from the stable resource owner.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaInitiator {
    /// Agent identifier, when called from an agent tool.
    pub agent_id: Option<String>,
    /// Tool-use identifier, when called by a model tool.
    pub tool_use_id: Option<String>,
    /// Host request identifier, when called by a hosted app or UI.
    pub request_id: Option<String>,
}

/// Trusted operation context minted by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaOperationContext {
    /// Operation identity.
    pub identity: MediaOperationId,
    /// Stable owner for any resource created by the operation.
    pub owner: MediaOwner,
    /// Per-call attribution, not a resource owner.
    pub initiator: Option<MediaInitiator>,
    /// Remaining time budget; receivers use a local monotonic timer.
    pub timeout_budget_ms: Option<u64>,
    /// Maximum media bytes that may be collected or returned by this call.
    pub max_payload_bytes: u64,
}

impl MediaOperationContext {
    /// Create a context using host-issued identity and ownership.
    #[must_use]
    pub fn new(identity: MediaOperationId, owner: MediaOwner) -> Self {
        Self {
            identity,
            owner,
            initiator: None,
            timeout_budget_ms: None,
            max_payload_bytes: DEFAULT_MAX_MEDIA_PAYLOAD_BYTES,
        }
    }

    /// Set the attribution for this operation.
    #[must_use]
    pub fn with_initiator(mut self, initiator: MediaInitiator) -> Self {
        self.initiator = Some(initiator);
        self
    }

    /// Set a host-selected time budget in milliseconds.
    #[must_use]
    pub fn with_timeout_budget_ms(mut self, timeout_budget_ms: u64) -> Self {
        self.timeout_budget_ms = Some(timeout_budget_ms);
        self
    }

    /// Set the host-selected payload cap in bytes.
    #[must_use]
    pub fn with_max_payload_bytes(mut self, max_payload_bytes: u64) -> Self {
        self.max_payload_bytes = max_payload_bytes;
        self
    }
}

macro_rules! opaque_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub String);

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

opaque_id!(MediaSessionId, "Opaque stable identity of a media session.");
opaque_id!(RecordingId, "Opaque stable identity of a recording.");
opaque_id!(
    MediaSourceId,
    "Opaque identity of a host-discovered media source."
);
opaque_id!(MediaRef, "Opaque reference to host-owned media bytes.");
opaque_id!(
    MediaConnectorId,
    "Opaque identity of a host-configured connector."
);

/// Opaque reference to a discovered media input or output source.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MediaSourceRef(pub MediaSourceId);

impl From<MediaSourceId> for MediaSourceRef {
    fn from(value: MediaSourceId) -> Self {
        Self(value)
    }
}

/// Opaque reference to a recording resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordingRef(pub RecordingId);

impl From<RecordingId> for RecordingRef {
    fn from(value: RecordingId) -> Self {
        Self(value)
    }
}

/// Opaque reference to a media session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MediaSessionRef(pub MediaSessionId);

impl From<MediaSessionId> for MediaSessionRef {
    fn from(value: MediaSessionId) -> Self {
        Self(value)
    }
}

/// Media family represented by a payload or source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaKind {
    /// Encoded or raw audio.
    Audio,
    /// Still image.
    Image,
    /// Time-based video.
    Video,
}

/// Host-discovered source category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaSourceKind {
    /// Local microphone input.
    Microphone,
    /// Local camera input.
    Camera,
    /// Local screen or display capture input.
    Screen,
    /// Local speaker or audio output.
    Speaker,
    /// Host-managed media library.
    Library,
    /// Connector-managed remote source.
    Remote,
}

/// Supported use of a discovered source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaSourceUsage {
    /// One-shot capture.
    Capture,
    /// Continuous recording.
    Record,
    /// Live transcription.
    Transcribe,
    /// Media playback.
    Play,
    /// Synthesized speech playback.
    Speak,
    /// Participation in a media session.
    Session,
    /// Reading an image from a host-managed library.
    ReadImage,
}

/// One source discovered by an owner-scoped source listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSource {
    /// Opaque source identity.
    pub reference: MediaSourceRef,
    /// Source category.
    pub kind: MediaSourceKind,
    /// Actual audio, image, or video modality when the source has one.
    ///
    /// This is independent of `kind`; a `Remote` source can still identify an
    /// audio or video track. Unknown modality remains `None` and does not match
    /// a request that explicitly filters by media kind.
    pub media_kind: Option<MediaKind>,
    /// Safe, user-facing source label.
    pub label: String,
    /// Uses that this source supports.
    pub usages: Vec<MediaSourceUsage>,
    /// Operations supported for this particular source.
    pub supported_operations: Vec<MediaOperationKind>,
    /// Whether the host selected this as the default for its category.
    pub is_default: bool,
}

/// Operation exposed by the common service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaOperationKind {
    /// Discover owner-visible sources.
    Sources,
    /// Query owner-visible resource status.
    Status,
    /// Capture one bounded media item.
    Capture,
    /// Start continuous recording.
    Record,
    /// Transcribe a live source or stored media item.
    Transcribe,
    /// Synthesize audio without playing it.
    Synthesize,
    /// Play an existing media item.
    Play,
    /// Synthesize and play speech.
    Speak,
    /// Start a media session through a configured connector.
    SessionStart,
    /// Update an active media session.
    SessionUpdate,
    /// Read available events for a target.
    Observe,
    /// Wait for new events from a target.
    Wait,
    /// Stop an owner-visible resource.
    Stop,
}

/// Transient state for an operation that the host supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaReadinessState {
    /// Ready now.
    Ready,
    /// A user-authorized request may need permission.
    NeedsPermission,
    /// Another owner holds an incompatible device lease.
    Busy,
    /// A selected offline model is missing.
    MissingModel,
    /// A selected provider is temporarily unavailable.
    Unavailable,
}

/// Readiness for one supported operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaOperationReadiness {
    /// Supported operation being described.
    pub operation: MediaOperationKind,
    /// Transient readiness state.
    pub state: MediaReadinessState,
}

/// Media support and transient readiness snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaCapabilities {
    /// Native service instance epoch.
    pub service_epoch: u64,
    /// Changes only when supported operations or media kinds change.
    pub support_revision: u64,
    /// Stable set of operations implemented by this service.
    pub supported_operations: Vec<MediaOperationKind>,
    /// Media kinds accepted or returned by supported operations.
    pub supported_media_kinds: Vec<MediaKind>,
    /// Current readiness of supported operations.
    pub readiness: Vec<MediaOperationReadiness>,
    /// Maximum raw media bytes that may be collected or returned.
    pub max_payload_bytes: u64,
}

impl MediaCapabilities {
    /// Whether this service implements an operation.
    #[must_use]
    pub fn supports(&self, operation: MediaOperationKind) -> bool {
        self.supported_operations.contains(&operation)
    }

    /// Current readiness for a supported operation, if one is reported.
    #[must_use]
    pub fn readiness_for(&self, operation: MediaOperationKind) -> Option<MediaReadinessState> {
        self.readiness
            .iter()
            .find(|entry| entry.operation == operation)
            .map(|entry| entry.state)
    }
}

/// Host-owned resource address used by status, observation, and stop.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MediaResourceRef {
    /// In-flight operation that may be observed or stopped.
    Operation(MediaOperationId),
    /// Discovered source.
    Source(MediaSourceRef),
    /// Recording in progress or completed.
    Recording(RecordingRef),
    /// Active media session.
    Session(MediaSessionRef),
    /// Stored media artifact.
    Media(MediaRef),
}

/// Current lifecycle state of an owner-visible resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaResourceState {
    /// Available for use.
    Ready,
    /// Capture, recording, playback, or session is active.
    Active,
    /// Stop has been requested and is being finalized.
    Stopping,
    /// Resource completed successfully.
    Completed,
    /// Resource failed.
    Failed,
}

/// Owner-visible state of one media resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaResourceStatus {
    /// Resource queried.
    pub resource: MediaResourceRef,
    /// Current lifecycle state.
    pub state: MediaResourceState,
    /// Start timestamp in Unix milliseconds, when known.
    pub started_at_unix_ms: Option<u64>,
    /// Last state-change timestamp in Unix milliseconds, when known.
    pub updated_at_unix_ms: Option<u64>,
}

/// Capture limits and image-processing preferences.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CaptureOptions {
    /// Maximum capture duration in milliseconds, when capturing audio or video.
    pub max_duration_ms: Option<u64>,
    /// Maximum longer-side dimension in pixels for captured still images.
    pub max_dimension_px: Option<u32>,
    /// JPEG quality in `[0.0, 1.0]` when image re-encoding is supported.
    pub jpeg_quality: Option<f32>,
}

/// Continuous recording limits and output format preference.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecordOptions {
    /// Maximum recording duration in milliseconds.
    pub max_duration_ms: Option<u64>,
    /// Requested MIME type; the service reports the actual type on stop.
    pub mime_type: Option<String>,
}

/// Input selection for speech transcription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptionInput {
    /// Transcribe speech from a live microphone source.
    LiveSource(MediaSourceRef),
    /// Transcribe a previously stored audio asset.
    StoredMedia(MediaRef),
}

/// Provider-independent media session mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaSessionMode {
    /// Audio transport only.
    Audio,
    /// Video transport only.
    Video,
    /// Audio and video transport.
    AudioVideo,
}

/// Whether a media session is for passive observation or an active conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaSessionIntent {
    /// Observe media without attaching it to an active agent turn.
    Observation,
    /// Attach media to the current conversation or active turn.
    Conversation,
}

/// Request to start a session using host-configured transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSessionRequest {
    /// How the host should attach this session to conversation state.
    pub intent: MediaSessionIntent,
    /// Requested media mode.
    pub mode: MediaSessionMode,
    /// Optional owner-visible microphone source.
    pub audio_source: Option<MediaSourceRef>,
    /// Optional owner-visible camera or screen source.
    pub video_source: Option<MediaSourceRef>,
}

/// One bounded PCM16 little-endian audio frame from capture or to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPcmFrame {
    /// Audio sample rate in Hz.
    pub sample_rate_hz: u32,
    /// Number of interleaved channels.
    pub channels: u16,
    /// Interleaved signed PCM16 little-endian samples.
    pub pcm16le: Vec<u8>,
}

impl MediaPcmFrame {
    /// Validate sample layout and enforce the owner-selected byte cap.
    pub fn validate(&self, max_bytes: u64) -> Result<(), MediaError> {
        if self.sample_rate_hz == 0
            || self.channels == 0
            || self.pcm16le.is_empty()
            || !self
                .pcm16le
                .len()
                .is_multiple_of(usize::from(self.channels) * 2)
        {
            return Err(MediaError::new(
                MediaErrorKind::InvalidRequest,
                "PCM frame has invalid sample rate, channels, or byte alignment",
            ));
        }
        validate_frame_size(self.pcm16le.len(), max_bytes)
    }
}

/// One bounded raw video frame in a platform-neutral pixel layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaVideoFrame {
    /// Packed 8-bit red, green, blue, alpha pixels.
    Rgba8 {
        /// Frame width in pixels.
        width_px: u32,
        /// Frame height in pixels.
        height_px: u32,
        /// Byte distance between adjacent rows.
        stride_bytes: u32,
        /// Packed pixel bytes, including row padding.
        bytes: Vec<u8>,
    },
    /// Planar 8-bit YUV 4:2:0 pixels.
    I420 {
        /// Frame width in pixels.
        width_px: u32,
        /// Frame height in pixels.
        height_px: u32,
        /// Byte distance between adjacent Y rows.
        y_stride_bytes: u32,
        /// Byte distance between adjacent U rows.
        u_stride_bytes: u32,
        /// Byte distance between adjacent V rows.
        v_stride_bytes: u32,
        /// Luma plane bytes, including row padding.
        y: Vec<u8>,
        /// U chroma plane bytes, including row padding.
        u: Vec<u8>,
        /// V chroma plane bytes, including row padding.
        v: Vec<u8>,
    },
}

impl MediaVideoFrame {
    /// Validate plane geometry and enforce the owner-selected byte cap.
    pub fn validate(&self, max_bytes: u64) -> Result<(), MediaError> {
        let byte_len = match self {
            Self::Rgba8 {
                width_px,
                height_px,
                stride_bytes,
                bytes,
            } => {
                let minimum_stride = width_px.checked_mul(4).ok_or_else(|| {
                    MediaError::new(
                        MediaErrorKind::InvalidRequest,
                        "RGBA frame dimensions overflow",
                    )
                })?;
                let required_len = usize::try_from(*stride_bytes)
                    .ok()
                    .and_then(|stride| {
                        usize::try_from(*height_px)
                            .ok()
                            .and_then(|height| stride.checked_mul(height))
                    })
                    .ok_or_else(|| {
                        MediaError::new(
                            MediaErrorKind::InvalidRequest,
                            "RGBA frame dimensions overflow",
                        )
                    })?;
                if *width_px == 0
                    || *height_px == 0
                    || *stride_bytes < minimum_stride
                    || bytes.len() != required_len
                {
                    return Err(MediaError::new(
                        MediaErrorKind::InvalidRequest,
                        "RGBA frame dimensions, stride, or plane length are invalid",
                    ));
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
                let chroma_width = width_px.div_ceil(2);
                let chroma_height = height_px.div_ceil(2);
                let plane_len = |stride: u32, height: u32| {
                    usize::try_from(stride).ok().and_then(|stride| {
                        usize::try_from(height)
                            .ok()
                            .and_then(|height| stride.checked_mul(height))
                    })
                };
                let y_len = plane_len(*y_stride_bytes, *height_px).ok_or_else(|| {
                    MediaError::new(
                        MediaErrorKind::InvalidRequest,
                        "I420 frame dimensions overflow",
                    )
                })?;
                let u_len = plane_len(*u_stride_bytes, chroma_height).ok_or_else(|| {
                    MediaError::new(
                        MediaErrorKind::InvalidRequest,
                        "I420 frame dimensions overflow",
                    )
                })?;
                let v_len = plane_len(*v_stride_bytes, chroma_height).ok_or_else(|| {
                    MediaError::new(
                        MediaErrorKind::InvalidRequest,
                        "I420 frame dimensions overflow",
                    )
                })?;
                if *width_px == 0
                    || *height_px == 0
                    || *y_stride_bytes < *width_px
                    || *u_stride_bytes < chroma_width
                    || *v_stride_bytes < chroma_width
                    || y.len() != y_len
                    || u.len() != u_len
                    || v.len() != v_len
                {
                    return Err(MediaError::new(
                        MediaErrorKind::InvalidRequest,
                        "I420 frame dimensions, strides, or plane lengths are invalid",
                    ));
                }
                y.len()
                    .checked_add(u.len())
                    .and_then(|size| size.checked_add(v.len()))
                    .ok_or_else(|| {
                        MediaError::new(
                            MediaErrorKind::InvalidRequest,
                            "I420 frame plane sizes overflow",
                        )
                    })?
            }
        };
        validate_frame_size(byte_len, max_bytes)
    }
}

fn validate_frame_size(byte_len: usize, max_bytes: u64) -> Result<(), MediaError> {
    if u64::try_from(byte_len).unwrap_or(u64::MAX) > max_bytes {
        return Err(MediaError::new(
            MediaErrorKind::MediaTooLarge,
            "media frame exceeds the owner-selected payload limit",
        ));
    }
    Ok(())
}

/// Captured input from an owner-bound native media lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaFramePayload {
    /// Audio captured from a microphone or decoded media source.
    Audio(MediaPcmFrame),
    /// Video captured from a camera or screen source.
    Video(MediaVideoFrame),
}

/// One timestamped frame from a shared native media lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFrame {
    /// Owner-visible source that produced the frame.
    pub source: MediaSourceRef,
    /// Monotonic sequence within the source stream.
    pub sequence: u64,
    /// Capture timestamp in Unix milliseconds.
    pub captured_at_unix_ms: u64,
    /// Typed frame data.
    pub payload: MediaFramePayload,
}

impl MediaFrame {
    /// Validate frame layout and enforce the owner-selected byte cap.
    pub fn validate(&self, max_bytes: u64) -> Result<(), MediaError> {
        match &self.payload {
            MediaFramePayload::Audio(frame) => frame.validate(max_bytes),
            MediaFramePayload::Video(frame) => frame.validate(max_bytes),
        }
    }
}

/// Confirmation that an audio frame was accepted by the native output path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaRenderReceipt {
    /// Number of interleaved sample frames accepted by this write call.
    /// Audio flush and interrupt barriers report zero here.
    pub accepted_sample_frames: u32,
    /// Number of sample frames confirmed played since the previous flush or
    /// interrupt barrier.
    pub played_sample_frames: u32,
    /// Timestamp of the last confirmed device render callback, when available.
    pub rendered_at_unix_ms: Option<u64>,
}

/// Owner-bound native lease shared by capture, RTC input, and local rendering.
#[async_trait]
pub trait MediaDeviceSession: Send + Sync {
    /// Wait for the next captured frame; `None` means the lease has ended.
    async fn next_input(&self) -> Result<Option<MediaFrame>, MediaError>;

    /// Write decoded remote PCM to the native output path.
    async fn write_audio(&self, frame: MediaPcmFrame) -> Result<MediaRenderReceipt, MediaError>;

    /// Wait until audio accepted before this call has reached the native render
    /// callback. The barrier snapshots its target at entry, so later writes do
    /// not extend the wait. The receipt reports actual playback since the
    /// preceding audio barrier and has `accepted_sample_frames == 0`.
    /// An interrupt or stop must preempt this wait. A preempted flush must
    /// return its remaining confirmed progress or an explicit cancellation;
    /// it must not count discarded samples as played or delay interruption.
    ///
    /// Non-audio adapters may keep the default Unsupported implementation;
    /// adapters that accept audio must implement callback-confirmed progress.
    async fn flush_audio(&self) -> Result<MediaRenderReceipt, MediaError> {
        Err(MediaError::new(
            MediaErrorKind::Unsupported,
            "audio flush is unsupported",
        ))
    }

    /// Interrupt queued audio accepted before this call without stopping
    /// capture or the media session. The barrier snapshots its target at entry
    /// and the receipt reports only actual playback since the preceding audio
    /// barrier, with `accepted_sample_frames == 0`.
    /// This operation must not wait for an earlier flush to finish rendering.
    /// Concurrent barriers must account for each played frame at most once.
    ///
    /// Non-audio adapters may keep the default Unsupported implementation;
    /// adapters that accept audio must report callback-confirmed progress.
    async fn interrupt_audio(&self) -> Result<MediaRenderReceipt, MediaError> {
        Err(MediaError::new(
            MediaErrorKind::Unsupported,
            "audio interruption is unsupported",
        ))
    }

    /// Write decoded remote video to the native render path.
    async fn write_video(&self, frame: MediaVideoFrame) -> Result<(), MediaError>;

    /// Apply a typed source or playback update to this lease.
    async fn update(&self, update: MediaSessionUpdate) -> Result<MediaSessionStatus, MediaError>;

    /// Stop capture and rendering and release the shared native lease.
    async fn stop(&self) -> Result<(), MediaError>;
}

/// Host platform port that opens one owner-bound native media lease.
#[async_trait]
pub trait MediaDevicePorts: Send + Sync {
    /// Open the shared audio/video inputs and output selected by the request.
    async fn open(
        &self,
        context: MediaOperationContext,
        request: MediaSessionRequest,
    ) -> Result<Arc<dyn MediaDeviceSession>, MediaError>;
}

/// Bounded duplex decoded-audio bridge for one already-open media session.
#[async_trait]
pub trait MediaAudioChannel: Send + Sync {
    /// Read the next captured or decoded audio frame; `None` means the channel ended.
    /// The returned `MediaFrame` must contain an audio payload.
    async fn read(&self) -> Result<Option<MediaFrame>, MediaError>;

    /// Queue decoded PCM for the session's native or RTC render path.
    async fn write(&self, frame: MediaPcmFrame) -> Result<MediaRenderReceipt, MediaError>;

    /// Snapshot the queued-write watermark, wait until it has drained, and
    /// return newly confirmed playback progress. The barrier receipt has
    /// `accepted_sample_frames == 0`. Interrupt and stop preempt this wait;
    /// discarded audio is never reported as played.
    async fn flush(&self) -> Result<MediaRenderReceipt, MediaError>;

    /// Interrupt playback through the queued-write watermark without ending
    /// the session or capture path, then return newly confirmed playback
    /// progress. The barrier receipt has `accepted_sample_frames == 0`.
    /// It must preempt a pending flush and account for played frames only once.
    async fn interrupt(&self) -> Result<MediaRenderReceipt, MediaError>;
}

/// Typed control applied to a live media session.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaSessionUpdate {
    /// Enable or disable sending a media kind.
    SetEnabled {
        /// Media kind to enable or disable.
        kind: MediaKind,
        /// Whether sending is enabled.
        enabled: bool,
    },
    /// Select an owner-visible source for a media kind.
    SelectSource {
        /// Media kind being configured.
        kind: MediaKind,
        /// Owner-visible replacement source.
        source: MediaSourceRef,
    },
    /// Set playback gain from `0.0` (muted) to `1.0` (full gain).
    SetPlaybackGain {
        /// Linear playback gain, clamped by the native implementation.
        gain: f32,
    },
}

/// Current session lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaSessionState {
    /// Connector is preparing transport.
    Preparing,
    /// Transport is ready for media.
    Ready,
    /// Connector is recovering transport.
    Reconnecting,
    /// Session ended normally.
    Ended,
    /// Session failed.
    Failed,
}

/// Current state and enabled media kinds for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSessionStatus {
    /// Session lifecycle state.
    pub state: MediaSessionState,
    /// Whether audio is currently enabled.
    pub audio_enabled: bool,
    /// Whether video is currently enabled.
    pub video_enabled: bool,
}

/// Cursor after which an observer wants events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MediaEventCursor {
    /// Last event sequence already observed.
    pub sequence: u64,
}

/// Typed media event emitted by a service or connector.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaEventKind {
    /// Resource state changed.
    StateChanged {
        /// Resource whose state changed.
        resource: MediaResourceRef,
        /// New state.
        state: MediaResourceState,
    },
    /// A captured video or image frame is available by opaque reference.
    Frame {
        /// Source that produced this frame.
        source: MediaSourceRef,
        /// Opaque asset and safe media metadata; call `read_media` for bytes.
        asset: MediaAsset,
    },
    /// A partial or final transcript is available.
    Transcript {
        /// Recognized speech.
        transcript: stt::SttTranscript,
    },
    /// A non-content operational error occurred.
    Error {
        /// Machine-readable error class.
        kind: MediaErrorKind,
        /// Safe diagnostic detail.
        message: String,
    },
}

/// One timestamped event for a specific owner-visible target.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaEvent {
    /// Monotonic event sequence within the target's event stream.
    pub sequence: u64,
    /// Event timestamp in Unix milliseconds.
    pub occurred_at_unix_ms: u64,
    /// Target to which the event belongs.
    pub target: MediaResourceRef,
    /// Operation that produced the event, when applicable.
    pub operation_id: Option<MediaOperationId>,
    /// Typed event data.
    pub kind: MediaEventKind,
}

impl MediaEvent {
    /// Validate event content bounds without modifying or truncating its text.
    pub fn validate(&self) -> Result<(), MediaError> {
        let text = match &self.kind {
            MediaEventKind::Transcript { transcript } => Some(transcript.text.as_str()),
            MediaEventKind::Error { message, .. } => Some(message.as_str()),
            MediaEventKind::StateChanged { .. } | MediaEventKind::Frame { .. } => None,
        };
        if text.is_some_and(|text| text.len() > MAX_MEDIA_EVENT_TEXT_BYTES) {
            return Err(MediaError::new(
                MediaErrorKind::MediaTooLarge,
                "media event text exceeds the UTF-8 byte limit",
            ));
        }
        Ok(())
    }
}

/// Bounded batch of events for exactly one target.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaEventBatch {
    /// Target whose events are returned.
    pub target: MediaResourceRef,
    /// At most [`MAX_MEDIA_EVENT_BATCH_SIZE`] events.
    pub events: Vec<MediaEvent>,
    /// Cursor after the last returned event, when one was returned.
    pub next_cursor: Option<MediaEventCursor>,
    /// Whether older events were dropped before this batch.
    pub dropped_before_cursor: bool,
}

impl MediaEventBatch {
    /// Build an event batch while enforcing the protocol maximum.
    pub fn bounded(
        target: MediaResourceRef,
        mut events: Vec<MediaEvent>,
        mut dropped_before_cursor: bool,
    ) -> Result<Self, MediaError> {
        events.retain(|event| event.target == target);
        if events.len() > usize::from(MAX_MEDIA_EVENT_BATCH_SIZE) {
            events.drain(..events.len() - usize::from(MAX_MEDIA_EVENT_BATCH_SIZE));
            dropped_before_cursor = true;
        }
        let next_cursor = events.last().map(|event| MediaEventCursor {
            sequence: event.sequence,
        });
        let batch = Self {
            target,
            events,
            next_cursor,
            dropped_before_cursor,
        };
        batch.validate()?;
        Ok(batch)
    }

    /// Validate target, event-count, and per-event text bounds.
    pub fn validate(&self) -> Result<(), MediaError> {
        if self.events.len() > usize::from(MAX_MEDIA_EVENT_BATCH_SIZE) {
            return Err(MediaError::new(
                MediaErrorKind::MediaTooLarge,
                "media event batch exceeds the event-count limit",
            ));
        }
        for event in &self.events {
            if event.target != self.target {
                return Err(MediaError::new(
                    MediaErrorKind::InvalidRequest,
                    "media event batch contains an event for another target",
                ));
            }
            event.validate()?;
        }
        Ok(())
    }
}

/// Safe, non-secret metadata about stored media bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaMetadata {
    /// Media family.
    pub kind: MediaKind,
    /// MIME type reported by the producer or native platform.
    pub mime_type: String,
    /// Encoded payload size in bytes.
    pub byte_len: u64,
    /// Capture timestamp in Unix milliseconds, when the item was captured.
    pub captured_at_unix_ms: Option<u64>,
    /// Duration in milliseconds, when the payload is time-based.
    pub duration_ms: Option<u64>,
    /// Still-image or video width in pixels, when known.
    pub width_px: Option<u32>,
    /// Still-image or video height in pixels, when known.
    pub height_px: Option<u32>,
    /// Audio sample rate in Hz, when known.
    pub sample_rate_hz: Option<u32>,
    /// Audio channel count, when known.
    pub channels: Option<u16>,
}

/// Stored media asset returned by reference-producing operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaAsset {
    /// Opaque reference to owner-scoped media bytes.
    pub reference: MediaRef,
    /// Safe metadata for presentation and filtering.
    pub metadata: MediaMetadata,
}

/// Newly captured bytes passed to a host resource store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPayload {
    /// Safe metadata for these bytes.
    pub metadata: MediaMetadata,
    /// Encoded bytes, bounded by the operation context.
    pub bytes: Vec<u8>,
}

impl MediaPayload {
    /// Validate both the host payload cap and metadata byte count.
    pub fn validate(&self, max_bytes: u64) -> Result<(), MediaError> {
        let actual = u64::try_from(self.bytes.len()).unwrap_or(u64::MAX);
        if actual > max_bytes || actual != self.metadata.byte_len {
            return Err(MediaError::new(
                MediaErrorKind::MediaTooLarge,
                "media payload size does not match its declared bounded size",
            ));
        }
        Ok(())
    }
}

/// Media bytes returned to a caller after owner-scoped authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaContent {
    /// Opaque owner-scoped reference.
    pub reference: MediaRef,
    /// Safe metadata.
    pub metadata: MediaMetadata,
    /// Encoded media bytes.
    pub bytes: Vec<u8>,
}

impl MediaContent {
    /// Validate both the host payload cap and metadata byte count.
    pub fn validate(&self, max_bytes: u64) -> Result<(), MediaError> {
        let actual = u64::try_from(self.bytes.len()).unwrap_or(u64::MAX);
        if actual > max_bytes || actual != self.metadata.byte_len {
            return Err(MediaError::new(
                MediaErrorKind::MediaTooLarge,
                "media content size does not match its declared bounded size",
            ));
        }
        Ok(())
    }
}

/// Typed owner-scoped media operation.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaOperation {
    /// List sources, optionally filtered by kind and supported use.
    Sources {
        /// Optional media-kind filter.
        kind: Option<MediaKind>,
        /// Optional supported-use filter.
        usage: Option<MediaSourceUsage>,
    },
    /// Query one resource or all resources for the current owner.
    Status {
        /// Resource to inspect; `None` queries all owner-visible resources.
        target: Option<MediaResourceRef>,
    },
    /// Capture one bounded image, audio clip, or video clip.
    Capture {
        /// Owner-visible source to capture from.
        source: MediaSourceRef,
        /// Capture limits and image-processing preferences.
        options: CaptureOptions,
    },
    /// Start continuous capture and return a recording reference.
    Record {
        /// Owner-visible source to record from.
        source: MediaSourceRef,
        /// Duration and output format constraints.
        options: RecordOptions,
    },
    /// Transcribe live speech or previously stored media.
    Transcribe {
        /// Live source or stored media input.
        input: TranscriptionInput,
        /// Optional BCP-47 language hint.
        language: Option<String>,
    },
    /// Synthesize audio without playing it.
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
    /// Play an existing stored asset.
    Play {
        /// Owner-visible media item to play.
        media: MediaRef,
        /// Optional owner-visible output source.
        source: Option<MediaSourceRef>,
    },
    /// Synthesize and play speech, completing after playback ends.
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
    /// Start a session through the host-selected connector.
    SessionStart {
        /// Provider-independent desired session configuration.
        request: MediaSessionRequest,
    },
    /// Apply a typed update to an active session.
    SessionUpdate {
        /// Session to update.
        session: MediaSessionRef,
        /// Typed update; contains no signaling or credential fields.
        update: MediaSessionUpdate,
    },
    /// Read currently available events for one target.
    Observe {
        /// Source, session, recording, or media target to observe.
        target: MediaResourceRef,
        /// Return events after this cursor.
        after: Option<MediaEventCursor>,
        /// Requested batch size, capped at the protocol maximum.
        limit: u16,
    },
    /// Wait for target events up to a bounded timeout.
    Wait {
        /// Source, session, recording, or media target to observe.
        target: MediaResourceRef,
        /// Return events after this cursor.
        after: Option<MediaEventCursor>,
        /// Maximum wait in milliseconds.
        timeout_ms: u64,
        /// Requested batch size, capped at the protocol maximum.
        limit: u16,
    },
    /// Stop an owner-visible resource; stopping a recording yields its artifact.
    Stop {
        /// Recording, session, or other stoppable resource.
        target: MediaResourceRef,
    },
}

impl MediaOperation {
    /// Operation kind used for capability checks.
    #[must_use]
    pub fn kind(&self) -> MediaOperationKind {
        match self {
            Self::Sources { .. } => MediaOperationKind::Sources,
            Self::Status { .. } => MediaOperationKind::Status,
            Self::Capture { .. } => MediaOperationKind::Capture,
            Self::Record { .. } => MediaOperationKind::Record,
            Self::Transcribe { .. } => MediaOperationKind::Transcribe,
            Self::Synthesize { .. } => MediaOperationKind::Synthesize,
            Self::Play { .. } => MediaOperationKind::Play,
            Self::Speak { .. } => MediaOperationKind::Speak,
            Self::SessionStart { .. } => MediaOperationKind::SessionStart,
            Self::SessionUpdate { .. } => MediaOperationKind::SessionUpdate,
            Self::Observe { .. } => MediaOperationKind::Observe,
            Self::Wait { .. } => MediaOperationKind::Wait,
            Self::Stop { .. } => MediaOperationKind::Stop,
        }
    }
}

/// Successful result from one media operation.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaResult {
    /// Owner-visible source inventory.
    Sources {
        /// Sources matching the requested filters.
        sources: Vec<MediaSource>,
    },
    /// Owner-visible resource statuses.
    Status {
        /// Current status records.
        resources: Vec<MediaResourceStatus>,
    },
    /// A one-shot captured asset.
    Captured {
        /// Opaque asset reference and safe metadata; use `read_media` for bytes.
        asset: MediaAsset,
    },
    /// Continuous recording began.
    RecordingStarted {
        /// Handle to the active recording resource.
        recording: RecordingRef,
    },
    /// A recording stopped and its actual finalized artifact is available.
    RecordingStopped {
        /// Stopped recording resource.
        recording: RecordingRef,
        /// Finalized artifact and safe metadata.
        asset: MediaAsset,
    },
    /// Speech transcription result.
    Transcript {
        /// Recognized text and provider metadata.
        transcript: stt::SttTranscript,
    },
    /// Synthesized audio asset.
    Synthesized {
        /// Opaque asset reference and safe metadata; use `read_media` for bytes.
        asset: MediaAsset,
    },
    /// Playback completed after actual output.
    PlaybackCompleted {
        /// Actual playback duration in milliseconds.
        duration_ms: u64,
    },
    /// Speech playback completed after actual output.
    SpeechCompleted {
        /// Actual playback duration in milliseconds.
        duration_ms: u64,
    },
    /// A media session started.
    SessionStarted {
        /// Opaque session reference.
        session: MediaSessionRef,
        /// Current session state.
        status: MediaSessionStatus,
    },
    /// A media session update was applied.
    SessionUpdated {
        /// Opaque session reference.
        session: MediaSessionRef,
        /// Current session state.
        status: MediaSessionStatus,
    },
    /// Events for the requested target.
    Events {
        /// Bounded timestamped event batch.
        batch: MediaEventBatch,
    },
    /// A resource other than an active recording was stopped.
    Stopped {
        /// Stopped resource.
        resource: MediaResourceRef,
    },
}

/// Structured media failure class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaErrorKind {
    /// A required microphone, camera, or media permission was denied.
    PermissionDenied,
    /// An incompatible media lease is active.
    Busy,
    /// The caller cancelled the operation.
    Cancelled,
    /// The operation exceeded its time budget.
    Timeout,
    /// No speech or media was produced.
    NoData,
    /// A reference is missing, stale, or owned by another owner.
    NotFound,
    /// The selected provider or device is unavailable.
    Unavailable,
    /// The requested operation or format is unsupported.
    Unsupported,
    /// The selected offline model is missing.
    ModelMissing,
    /// Request arguments are invalid.
    InvalidRequest,
    /// Speech synthesis or native media processing failed.
    ProcessingFailed,
    /// A native device or transport operation failed.
    NativeFailure,
    /// The resource store failed.
    StoreFailure,
    /// Media exceeded the bounded payload size.
    MediaTooLarge,
}

impl fmt::Display for MediaErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PermissionDenied => "permission_denied",
            Self::Busy => "busy",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::NoData => "no_data",
            Self::NotFound => "not_found",
            Self::Unavailable => "unavailable",
            Self::Unsupported => "unsupported",
            Self::ModelMissing => "model_missing",
            Self::InvalidRequest => "invalid_request",
            Self::ProcessingFailed => "processing_failed",
            Self::NativeFailure => "native_failure",
            Self::StoreFailure => "store_failure",
            Self::MediaTooLarge => "media_too_large",
        })
    }
}

/// Structured media operation failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{kind}: {message}")]
pub struct MediaError {
    /// Machine-readable class.
    pub kind: MediaErrorKind,
    /// Safe diagnostic or UI detail; never credentials or media bytes.
    pub message: String,
}

impl MediaError {
    /// Create a structured media failure.
    #[must_use]
    pub fn new(kind: MediaErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// Owner-scoped media capability injected by a host.
#[async_trait]
pub trait MediaService: Send + Sync {
    /// Current support and transient readiness. Must not request permission.
    fn capabilities(&self) -> MediaCapabilities;

    /// Owner-specific capability projection, including trusted routing.
    fn capabilities_for(&self, _owner: &MediaOwner) -> MediaCapabilities {
        self.capabilities()
    }

    /// Execute one typed, owner-scoped media operation.
    async fn execute(
        &self,
        context: MediaOperationContext,
        operation: MediaOperation,
    ) -> Result<MediaResult, MediaError>;

    /// Adopt a trusted incoming session into this owner's lifecycle registry.
    ///
    /// Implementations preserve and return `session.reference()` as the registry
    /// key; an existing reference must be rejected rather than replaced.
    ///
    /// Services that do not support incoming sessions may keep this default.
    async fn attach_session(
        &self,
        _context: MediaOperationContext,
        _session: Box<dyn MediaSession>,
    ) -> Result<MediaSessionRef, MediaError> {
        Err(MediaError::new(
            MediaErrorKind::Unsupported,
            "incoming media sessions are not supported",
        ))
    }

    /// Obtain the existing session's decoded-audio bridge for host conversation routing.
    ///
    /// Implementations must validate that `session` belongs to `context.owner`.
    /// This host-only path never opens a second native device lease.
    async fn session_audio(
        &self,
        _context: MediaOperationContext,
        _session: MediaSessionRef,
    ) -> Result<Arc<dyn MediaAudioChannel>, MediaError> {
        Err(MediaError::new(
            MediaErrorKind::Unsupported,
            "session audio bridge is not supported",
        ))
    }

    /// Read bounded bytes for an owner-visible media reference.
    async fn read_media(
        &self,
        context: MediaOperationContext,
        media: MediaRef,
    ) -> Result<MediaContent, MediaError>;

    /// Cancel only the matching in-flight operation.
    async fn cancel(&self, identity: MediaOperationId) -> Result<(), MediaError>;
}

/// Host-provided store for owner-scoped encoded media bytes.
#[async_trait]
pub trait MediaResourceStore: Send + Sync {
    /// Persist a payload and return an opaque reference with safe metadata.
    async fn put(
        &self,
        context: &MediaOperationContext,
        payload: MediaPayload,
    ) -> Result<MediaAsset, MediaError>;

    /// Read a media payload, bounded by `max_bytes` and the owner in context.
    async fn get(
        &self,
        context: &MediaOperationContext,
        media: &MediaRef,
        max_bytes: u64,
    ) -> Result<MediaPayload, MediaError>;

    /// Delete only an owner-visible media resource.
    async fn delete(
        &self,
        context: &MediaOperationContext,
        media: &MediaRef,
    ) -> Result<(), MediaError>;
}

/// Host-configured media connector for actual native or external transports.
#[async_trait]
pub trait MediaConnector: Send + Sync {
    /// Opaque connector identity selected by host configuration.
    fn id(&self) -> MediaConnectorId;

    /// Discover connector-backed, owner-visible sources.
    ///
    /// Connectors that do not publish remote sources may keep the empty default.
    async fn sources(
        &self,
        _context: MediaOperationContext,
    ) -> Result<Vec<MediaSource>, MediaError> {
        Ok(Vec::new())
    }

    /// Start a session with host-injected transport configuration.
    async fn start_session(
        &self,
        context: MediaOperationContext,
        request: MediaSessionRequest,
    ) -> Result<Box<dyn MediaSession>, MediaError>;
}

/// Active session transport created by a configured connector.
#[async_trait]
pub trait MediaSession: Send + Sync {
    /// Opaque reference of this session.
    fn reference(&self) -> &MediaSessionRef;

    /// Notify the session that a host service has installed it in its owner registry.
    ///
    /// Implementations may use this synchronous hook to arm terminal lifecycle
    /// notification. It is called only after successful registration and outside
    /// the service's registry lock; it must not call back into the service.
    fn on_registered(&self) {}

    /// Current transport status, without synthesizing readiness.
    fn status(&self) -> MediaSessionStatus;

    /// Owner-visible source identities this session is authorized to include
    /// in frame events. The runtime unions these with sources selected at
    /// session start or by successful session updates, bounds the list by
    /// [`MAX_MEDIA_SESSION_SOURCE_REFS`], and rejects frame events for any
    /// other source. Incoming sessions that do not expose frame events may
    /// keep the empty default.
    fn authorized_sources(&self) -> Vec<MediaSourceRef> {
        Vec::new()
    }

    /// Access this session's decoded-audio bridge, if it has one.
    async fn audio_channel(&self) -> Result<Arc<dyn MediaAudioChannel>, MediaError> {
        Err(MediaError::new(
            MediaErrorKind::Unsupported,
            "session does not provide a decoded-audio bridge",
        ))
    }

    /// Apply a typed update to this active session.
    async fn update(
        &self,
        context: MediaOperationContext,
        update: MediaSessionUpdate,
    ) -> Result<MediaSessionStatus, MediaError>;

    /// Observe this session's timestamped events.
    async fn observe(
        &self,
        context: MediaOperationContext,
        after: Option<MediaEventCursor>,
        limit: u16,
        wait_ms: Option<u64>,
    ) -> Result<MediaEventBatch, MediaError>;

    /// Stop the session and release its transport resources.
    async fn stop(&self, context: MediaOperationContext) -> Result<MediaSessionStatus, MediaError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> MediaResourceRef {
        MediaResourceRef::Session(MediaSessionRef(MediaSessionId::from("session-1")))
    }

    #[test]
    fn operation_kind_is_stable_for_each_semantic_request() {
        let operation = MediaOperation::Transcribe {
            input: TranscriptionInput::LiveSource(MediaSourceRef(MediaSourceId::from("mic-1"))),
            language: Some("en-US".to_owned()),
        };
        assert_eq!(operation.kind(), MediaOperationKind::Transcribe);

        let operation = MediaOperation::Observe {
            target: target(),
            after: None,
            limit: 500,
        };
        assert_eq!(operation.kind(), MediaOperationKind::Observe);
    }

    #[test]
    fn readiness_does_not_imply_support() {
        let capabilities = MediaCapabilities {
            service_epoch: 1,
            support_revision: 1,
            supported_operations: vec![MediaOperationKind::Capture],
            supported_media_kinds: vec![MediaKind::Image],
            readiness: vec![MediaOperationReadiness {
                operation: MediaOperationKind::Capture,
                state: MediaReadinessState::NeedsPermission,
            }],
            max_payload_bytes: 4_096,
        };

        assert!(capabilities.supports(MediaOperationKind::Capture));
        assert!(!capabilities.supports(MediaOperationKind::Record));
        assert_eq!(
            capabilities.readiness_for(MediaOperationKind::Capture),
            Some(MediaReadinessState::NeedsPermission)
        );
        assert_eq!(capabilities.readiness_for(MediaOperationKind::Record), None);
    }

    #[test]
    fn remote_source_category_does_not_erase_its_modality() {
        let source = MediaSource {
            reference: MediaSourceRef(MediaSourceId::from("remote-video-1")),
            kind: MediaSourceKind::Remote,
            media_kind: Some(MediaKind::Video),
            label: "Remote camera".to_owned(),
            usages: vec![MediaSourceUsage::Session],
            supported_operations: vec![MediaOperationKind::SessionStart],
            is_default: false,
        };

        assert_eq!(source.kind, MediaSourceKind::Remote);
        assert_eq!(source.media_kind, Some(MediaKind::Video));
    }

    #[test]
    fn event_batches_keep_only_the_newest_protocol_bounded_events() {
        let events = (0..usize::from(MAX_MEDIA_EVENT_BATCH_SIZE) + 3)
            .map(|sequence| MediaEvent {
                sequence: u64::try_from(sequence).expect("test sequence fits"),
                occurred_at_unix_ms: u64::try_from(sequence).expect("test timestamp fits"),
                target: target(),
                operation_id: None,
                kind: MediaEventKind::StateChanged {
                    resource: target(),
                    state: MediaResourceState::Active,
                },
            })
            .collect();
        let batch = MediaEventBatch::bounded(target(), events, true)
            .expect("state-only events fit the event text limit");

        assert_eq!(batch.events.len(), usize::from(MAX_MEDIA_EVENT_BATCH_SIZE));
        assert_eq!(batch.events[0].sequence, 3);
        assert_eq!(batch.next_cursor, Some(MediaEventCursor { sequence: 130 }));
        assert!(batch.dropped_before_cursor);
    }

    #[test]
    fn event_batches_exclude_events_from_other_targets() {
        let correct_target = target();
        let unrelated_target =
            MediaResourceRef::Source(MediaSourceRef(MediaSourceId::from("mic-2")));
        let event = |sequence, target| MediaEvent {
            sequence,
            occurred_at_unix_ms: sequence,
            target,
            operation_id: None,
            kind: MediaEventKind::StateChanged {
                resource: correct_target.clone(),
                state: MediaResourceState::Active,
            },
        };

        let batch = MediaEventBatch::bounded(
            correct_target.clone(),
            vec![event(1, unrelated_target), event(2, correct_target.clone())],
            false,
        )
        .expect("the matching state event is bounded");

        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].target, correct_target);
        assert_eq!(batch.events[0].sequence, 2);
    }

    #[test]
    fn event_text_bounds_are_utf8_bytes_and_never_truncated() {
        let valid_text = "é".repeat(MAX_MEDIA_EVENT_TEXT_BYTES / 2);
        let valid = MediaEvent {
            sequence: 1,
            occurred_at_unix_ms: 1,
            target: target(),
            operation_id: None,
            kind: MediaEventKind::Transcript {
                transcript: stt::SttTranscript {
                    text: valid_text.clone(),
                    language: Some("fr-FR".to_owned()),
                    confidence: None,
                },
            },
        };
        assert_eq!(valid_text.len(), MAX_MEDIA_EVENT_TEXT_BYTES);
        assert!(valid.validate().is_ok());

        let too_long = format!("{valid_text}é");
        let invalid = MediaEvent {
            kind: MediaEventKind::Transcript {
                transcript: stt::SttTranscript {
                    text: too_long.clone(),
                    language: Some("fr-FR".to_owned()),
                    confidence: None,
                },
            },
            ..valid.clone()
        };
        assert_eq!(
            invalid
                .validate()
                .expect_err("oversized transcript must be rejected")
                .kind,
            MediaErrorKind::MediaTooLarge
        );
        let MediaEventKind::Transcript { transcript } = invalid.kind else {
            panic!("test event remains a transcript");
        };
        assert_eq!(
            transcript.text, too_long,
            "validation must not truncate UTF-8"
        );

        let error = MediaEvent {
            kind: MediaEventKind::Error {
                kind: MediaErrorKind::NativeFailure,
                message: "x".repeat(MAX_MEDIA_EVENT_TEXT_BYTES + 1),
            },
            ..valid
        };
        assert_eq!(
            error
                .validate()
                .expect_err("oversized error detail must be rejected")
                .kind,
            MediaErrorKind::MediaTooLarge
        );
    }

    #[test]
    fn bounded_event_batch_rejects_oversized_text_without_truncating() {
        let text = "x".repeat(MAX_MEDIA_EVENT_TEXT_BYTES + 1);
        let event = MediaEvent {
            sequence: 1,
            occurred_at_unix_ms: 1,
            target: target(),
            operation_id: None,
            kind: MediaEventKind::Error {
                kind: MediaErrorKind::NativeFailure,
                message: text.clone(),
            },
        };
        let result = MediaEventBatch::bounded(target(), vec![event], false);

        assert_eq!(
            result
                .expect_err("batch constructor must enforce event text bounds")
                .kind,
            MediaErrorKind::MediaTooLarge
        );
        assert_eq!(text.len(), MAX_MEDIA_EVENT_TEXT_BYTES + 1);
    }

    #[test]
    fn media_payload_requires_exact_declared_size_within_host_cap() {
        let payload = MediaPayload {
            metadata: MediaMetadata {
                kind: MediaKind::Image,
                mime_type: "image/jpeg".to_owned(),
                byte_len: 3,
                captured_at_unix_ms: Some(1234),
                duration_ms: None,
                width_px: Some(2),
                height_px: Some(1),
                sample_rate_hz: None,
                channels: None,
            },
            bytes: vec![1, 2, 3],
        };
        assert!(payload.validate(3).is_ok());
        assert_eq!(
            payload
                .validate(2)
                .expect_err("payload must be capped")
                .kind,
            MediaErrorKind::MediaTooLarge
        );
    }

    #[test]
    fn captured_audio_frame_checks_sample_alignment_and_payload_cap() {
        let frame = MediaFrame {
            source: MediaSourceRef(MediaSourceId::from("mic-1")),
            sequence: 7,
            captured_at_unix_ms: 1_234,
            payload: MediaFramePayload::Audio(MediaPcmFrame {
                sample_rate_hz: 48_000,
                channels: 2,
                pcm16le: vec![0; 8],
            }),
        };

        assert!(frame.validate(8).is_ok());
        assert_eq!(
            frame
                .validate(7)
                .expect_err("frame must honor the cap")
                .kind,
            MediaErrorKind::MediaTooLarge
        );
        let mut misaligned = frame;
        if let MediaFramePayload::Audio(audio) = &mut misaligned.payload {
            audio.pcm16le.pop();
        }
        assert_eq!(
            misaligned
                .validate(8)
                .expect_err("PCM samples must be interleaved and aligned")
                .kind,
            MediaErrorKind::InvalidRequest
        );
    }

    #[test]
    fn captured_i420_frame_checks_strides_and_plane_lengths() {
        let frame = MediaFrame {
            source: MediaSourceRef(MediaSourceId::from("camera-1")),
            sequence: 1,
            captured_at_unix_ms: 1_234,
            payload: MediaFramePayload::Video(MediaVideoFrame::I420 {
                width_px: 2,
                height_px: 2,
                y_stride_bytes: 2,
                u_stride_bytes: 1,
                v_stride_bytes: 1,
                y: vec![0; 4],
                u: vec![0],
                v: vec![0],
            }),
        };

        assert!(frame.validate(6).is_ok());
        assert_eq!(
            frame
                .validate(5)
                .expect_err("frame must honor the cap")
                .kind,
            MediaErrorKind::MediaTooLarge
        );
    }

    #[test]
    fn audio_owner_and_operation_id_are_canonical_media_types() {
        let owner: audio::AudioOwner = MediaOwner::Session {
            session_id: "conversation-1".to_owned(),
        };
        let identity: audio::AudioOperationId = MediaOperationId::new(1, 2);
        let context = audio::AudioOperationContext::new(identity, owner);
        assert_eq!(context.max_payload_bytes, DEFAULT_MAX_MEDIA_PAYLOAD_BYTES);
    }
}
