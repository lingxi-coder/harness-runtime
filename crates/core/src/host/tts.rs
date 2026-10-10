//! PCM synthesis payload returned by the unified [`AudioService`].

/// Synthesized audio: 16-bit signed little-endian PCM, mono.
#[derive(Debug, Clone, PartialEq)]
pub struct TtsAudio {
    /// Raw PCM16 frames.
    pub pcm: Vec<u8>,
    /// Actual sample rate of `pcm` in Hz.
    pub sample_rate_hz: u32,
}
