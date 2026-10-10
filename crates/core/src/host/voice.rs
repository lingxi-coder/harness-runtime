//! Encoded recording payload returned by the unified [`AudioService`].

/// A finished recording.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceRecording {
    /// Encoded audio bytes.
    pub audio_bytes: Vec<u8>,
    /// MIME type of `audio_bytes` (e.g. `"audio/m4a"`).
    pub mime_type: String,
}
