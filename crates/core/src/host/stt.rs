//! Live-recognition result returned by the unified [`AudioService`].

/// A finished transcription.
#[derive(Debug, Clone)]
pub struct SttTranscript {
    /// Recognized text (empty when nothing was heard).
    pub text: String,
    /// BCP-47 language actually detected, when the provider reports it.
    pub language: Option<String>,
    /// Confidence in `[0, 1]`, when the provider reports it.
    pub confidence: Option<f32>,
}
