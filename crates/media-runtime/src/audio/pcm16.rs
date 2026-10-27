//! Bounded continuous PCM16LE rate and channel conversion for media providers.
//!
//! The converter owns one Rubato sinc-resampler state for a stream. Call
//! [`Pcm16StreamConverter::push`] with any whole-frame chunks, then call
//! [`Pcm16StreamConverter::finish`] once to flush its filter tail and receive
//! exactly the nearest integral output-frame count for the total input.

use std::collections::VecDeque;
use std::fmt;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::Resampler;

/// Lowest supported PCM sample rate.
pub const MIN_PCM16_STREAM_SAMPLE_RATE_HZ: u32 = 8_000;
/// Highest supported PCM sample rate.
pub const MAX_PCM16_STREAM_SAMPLE_RATE_HZ: u32 = 192_000;
/// Maximum input frames accepted by one push call: one second at 192 kHz.
pub const MAX_PCM16_STREAM_PUSH_FRAMES: usize = MAX_PCM16_STREAM_SAMPLE_RATE_HZ as usize;
/// Maximum resampler tail blocks processed by `finish`.
const MAX_FLUSH_BLOCKS: usize = 16;
/// Maximum converted output frames held internally before a push drains them.
const MAX_PENDING_OUTPUT_FRAMES: usize =
    MAX_PCM16_STREAM_PUSH_FRAMES + (MAX_PCM16_STREAM_SAMPLE_RATE_HZ as usize / 50);

/// A bounded PCM16 stream conversion error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pcm16StreamError {
    /// Input or output rate was outside 8–192 kHz.
    InvalidSampleRate,
    /// Input or output channel count was not mono or stereo.
    InvalidChannelCount,
    /// Input bytes did not contain complete interleaved PCM16 frames.
    IncompleteInputFrame,
    /// One push exceeded the one-second input bound.
    InputChunkTooLarge,
    /// Buffered unprocessed input exceeded its memory bound.
    InputBufferTooLarge,
    /// Buffered converted audio exceeded its memory bound.
    OutputBufferTooLarge,
    /// A buffer allocation failed.
    AllocationFailed,
    /// Rubato could not construct the requested converter.
    ResamplerInitializationFailed,
    /// Rubato rejected a processing block.
    ResamplerProcessingFailed,
    /// The output frame count overflowed the platform's supported integer size.
    OutputFrameCountOverflow,
    /// The bounded tail flush could not produce the required output length.
    TailFlushIncomplete,
    /// The stream was already finished.
    StreamFinished,
}

impl fmt::Display for Pcm16StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidSampleRate => "PCM sample rate must be between 8 and 192 kHz",
            Self::InvalidChannelCount => "PCM channel count must be mono or stereo",
            Self::IncompleteInputFrame => "PCM16LE input ended in a partial sample frame",
            Self::InputChunkTooLarge => "PCM16LE input chunk exceeds its one-second limit",
            Self::InputBufferTooLarge => "PCM16LE resampler input buffer exceeds its limit",
            Self::OutputBufferTooLarge => "PCM16LE resampler output buffer exceeds its limit",
            Self::AllocationFailed => "PCM16LE resampler could not allocate a bounded buffer",
            Self::ResamplerInitializationFailed => "PCM16LE resampler initialization failed",
            Self::ResamplerProcessingFailed => "PCM16LE resampler processing failed",
            Self::OutputFrameCountOverflow => "PCM16LE output frame count overflowed",
            Self::TailFlushIncomplete => "PCM16LE resampler tail flush was incomplete",
            Self::StreamFinished => "PCM16LE resampler stream is already finished",
        })
    }
}

impl std::error::Error for Pcm16StreamError {}

/// Continuous bounded converter between interleaved signed PCM16LE streams.
///
/// The resampler retains its filter history across calls, so splitting an input
/// stream into different whole-frame chunk boundaries does not alter the
/// concatenated output. Stereo input is averaged when producing mono; mono is
/// duplicated when producing stereo. Stereo-to-stereo conversion preserves
/// channels independently.
pub struct Pcm16StreamConverter {
    input_rate_hz: u32,
    input_channels: u16,
    output_rate_hz: u32,
    output_channels: u16,
    resampler_channels: usize,
    max_input_buffer_frames: usize,
    resampler: rubato::Async<f32>,
    input: VecDeque<f32>,
    output: VecDeque<f32>,
    scratch: Vec<f32>,
    startup_frames_to_discard: usize,
    input_frames_total: u64,
    output_frames_emitted: u64,
    finished: bool,
}

impl Pcm16StreamConverter {
    /// Create a streaming converter for the requested input and output formats.
    pub fn new(
        input_sample_rate_hz: u32,
        input_channels: u16,
        output_sample_rate_hz: u32,
        output_channels: u16,
    ) -> Result<Self, Pcm16StreamError> {
        if !(MIN_PCM16_STREAM_SAMPLE_RATE_HZ..=MAX_PCM16_STREAM_SAMPLE_RATE_HZ)
            .contains(&input_sample_rate_hz)
            || !(MIN_PCM16_STREAM_SAMPLE_RATE_HZ..=MAX_PCM16_STREAM_SAMPLE_RATE_HZ)
                .contains(&output_sample_rate_hz)
        {
            return Err(Pcm16StreamError::InvalidSampleRate);
        }
        if !matches!(input_channels, 1 | 2) || !matches!(output_channels, 1 | 2) {
            return Err(Pcm16StreamError::InvalidChannelCount);
        }

        let resampler_channels = usize::from(input_channels.min(output_channels));
        let input_chunk_frames = usize::try_from(input_sample_rate_hz.div_ceil(100))
            .map_err(|_| Pcm16StreamError::OutputFrameCountOverflow)?;
        let resampler = rubato::Async::new_sinc(
            f64::from(output_sample_rate_hz) / f64::from(input_sample_rate_hz),
            1.0,
            &rubato::SincInterpolationParameters::default(),
            input_chunk_frames,
            resampler_channels,
            rubato::FixedAsync::Input,
        )
        .map_err(|_| Pcm16StreamError::ResamplerInitializationFailed)?;
        let scratch_len = resampler
            .output_frames_max()
            .checked_mul(resampler_channels)
            .ok_or(Pcm16StreamError::OutputFrameCountOverflow)?;
        let scratch = vec![0.0; scratch_len];
        let startup_frames_to_discard = resampler.output_delay();
        let max_input_buffer_frames = usize::try_from(input_sample_rate_hz)
            .unwrap_or(usize::MAX)
            .saturating_add(input_chunk_frames);

        Ok(Self {
            input_rate_hz: input_sample_rate_hz,
            input_channels,
            output_rate_hz: output_sample_rate_hz,
            output_channels,
            resampler_channels,
            max_input_buffer_frames,
            resampler,
            input: VecDeque::new(),
            output: VecDeque::new(),
            scratch,
            startup_frames_to_discard,
            input_frames_total: 0,
            output_frames_emitted: 0,
            finished: false,
        })
    }

    /// Convert one bounded chunk, returning currently available whole output frames.
    ///
    /// A call accepts at most one second of input and preserves the resampler's
    /// history for subsequent chunks. Output can be temporarily empty while the
    /// sinc filter gathers enough future input; [`Self::finish`] emits its tail.
    pub fn push(&mut self, pcm16le: &[u8]) -> Result<Vec<u8>, Pcm16StreamError> {
        self.ensure_open()?;
        let bytes_per_frame = usize::from(self.input_channels) * 2;
        if !pcm16le.len().is_multiple_of(bytes_per_frame) {
            return Err(Pcm16StreamError::IncompleteInputFrame);
        }
        let input_frames = pcm16le.len() / bytes_per_frame;
        if input_frames > usize::try_from(self.input_rate_hz).unwrap_or(usize::MAX) {
            return Err(Pcm16StreamError::InputChunkTooLarge);
        }
        let current_buffered_frames = self.input.len() / self.resampler_channels;
        if current_buffered_frames.saturating_add(input_frames) > self.max_input_buffer_frames {
            return Err(Pcm16StreamError::InputBufferTooLarge);
        }
        let input_frames_total = self
            .input_frames_total
            .checked_add(
                u64::try_from(input_frames)
                    .map_err(|_| Pcm16StreamError::OutputFrameCountOverflow)?,
            )
            .ok_or(Pcm16StreamError::OutputFrameCountOverflow)?;
        let input_sample_count = input_frames
            .checked_mul(self.resampler_channels)
            .ok_or(Pcm16StreamError::OutputFrameCountOverflow)?;
        self.input
            .try_reserve(input_sample_count)
            .map_err(|_| Pcm16StreamError::AllocationFailed)?;
        self.append_input(pcm16le);
        self.input_frames_total = input_frames_total;
        self.process_available()?;
        self.emit_until(self.expected_output_frames()?)
    }

    /// Flush the filter tail and return the remaining output, exactly once.
    ///
    /// The total output length is the nearest integral frame count to
    /// `input_frames * output_rate / input_rate`; synthetic tail samples from
    /// the zero-padded sinc filter are trimmed to that count.
    pub fn finish(&mut self) -> Result<Vec<u8>, Pcm16StreamError> {
        self.ensure_open()?;
        let target_frames = self.expected_output_frames()?;
        let mut output = self.emit_until(target_frames)?;

        for _ in 0..MAX_FLUSH_BLOCKS {
            if self.output_frames_emitted >= target_frames {
                break;
            }
            let needed_frames = self.resampler.input_frames_next();
            let buffered_frames = self.input.len() / self.resampler_channels;
            if buffered_frames < needed_frames {
                let padded_frames = needed_frames - buffered_frames;
                let padded_samples = padded_frames
                    .checked_mul(self.resampler_channels)
                    .ok_or(Pcm16StreamError::OutputFrameCountOverflow)?;
                self.input
                    .try_reserve(padded_samples)
                    .map_err(|_| Pcm16StreamError::AllocationFailed)?;
                self.input.extend(std::iter::repeat_n(0.0, padded_samples));
            }
            self.process_one()?;
            output.extend(self.emit_until(target_frames)?);
        }

        if self.output_frames_emitted < target_frames {
            return Err(Pcm16StreamError::TailFlushIncomplete);
        }
        self.input.clear();
        self.output.clear();
        self.finished = true;
        Ok(output)
    }

    /// Input sample rate, in hertz.
    #[must_use]
    pub fn input_sample_rate_hz(&self) -> u32 {
        self.input_rate_hz
    }

    /// Input channel count.
    #[must_use]
    pub fn input_channels(&self) -> u16 {
        self.input_channels
    }

    /// Output sample rate, in hertz.
    #[must_use]
    pub fn output_sample_rate_hz(&self) -> u32 {
        self.output_rate_hz
    }

    /// Output channel count.
    #[must_use]
    pub fn output_channels(&self) -> u16 {
        self.output_channels
    }

    fn ensure_open(&self) -> Result<(), Pcm16StreamError> {
        if self.finished {
            Err(Pcm16StreamError::StreamFinished)
        } else {
            Ok(())
        }
    }

    fn append_input(&mut self, pcm16le: &[u8]) {
        let input_channels = usize::from(self.input_channels);
        for frame in pcm16le.chunks_exact(input_channels * 2) {
            let left = f32::from(i16::from_le_bytes([frame[0], frame[1]])) / 32_768.0;
            if input_channels == 1 {
                self.input.push_back(left);
            } else {
                let right = f32::from(i16::from_le_bytes([frame[2], frame[3]])) / 32_768.0;
                if self.resampler_channels == 2 {
                    self.input.push_back(left);
                    self.input.push_back(right);
                } else {
                    self.input.push_back((left + right) * 0.5);
                }
            }
        }
    }

    fn process_available(&mut self) -> Result<(), Pcm16StreamError> {
        while self.input.len() / self.resampler_channels >= self.resampler.input_frames_next() {
            self.process_one()?;
        }
        Ok(())
    }

    fn process_one(&mut self) -> Result<(), Pcm16StreamError> {
        let frames = self.resampler.input_frames_next();
        let samples = self.input.make_contiguous();
        let input = InterleavedSlice::new(samples, self.resampler_channels, frames)
            .map_err(|_| Pcm16StreamError::ResamplerProcessingFailed)?;
        let output_capacity = self.scratch.len() / self.resampler_channels;
        let mut output =
            InterleavedSlice::new_mut(&mut self.scratch, self.resampler_channels, output_capacity)
                .map_err(|_| Pcm16StreamError::ResamplerProcessingFailed)?;
        let (consumed, produced) = self
            .resampler
            .process_into_buffer(&input, &mut output, None)
            .map_err(|_| Pcm16StreamError::ResamplerProcessingFailed)?;
        if consumed != frames {
            return Err(Pcm16StreamError::ResamplerProcessingFailed);
        }
        self.input.drain(..consumed * self.resampler_channels);

        let skipped = self.startup_frames_to_discard.min(produced);
        self.startup_frames_to_discard -= skipped;
        let first_sample = skipped * self.resampler_channels;
        let end_sample = produced * self.resampler_channels;
        let pending_frames = self.output.len() / self.resampler_channels;
        if pending_frames.saturating_add(produced - skipped) > MAX_PENDING_OUTPUT_FRAMES {
            return Err(Pcm16StreamError::OutputBufferTooLarge);
        }
        self.output
            .try_reserve(end_sample - first_sample)
            .map_err(|_| Pcm16StreamError::AllocationFailed)?;
        self.output
            .extend(self.scratch[first_sample..end_sample].iter().copied());
        Ok(())
    }

    fn expected_output_frames(&self) -> Result<u64, Pcm16StreamError> {
        let scaled = u128::from(self.input_frames_total)
            .checked_mul(u128::from(self.output_rate_hz))
            .ok_or(Pcm16StreamError::OutputFrameCountOverflow)?;
        let rounded = scaled
            .checked_add(u128::from(self.input_rate_hz / 2))
            .ok_or(Pcm16StreamError::OutputFrameCountOverflow)?
            / u128::from(self.input_rate_hz);
        u64::try_from(rounded).map_err(|_| Pcm16StreamError::OutputFrameCountOverflow)
    }

    fn emit_until(&mut self, target_frames: u64) -> Result<Vec<u8>, Pcm16StreamError> {
        let frames_available = self.output.len() / self.resampler_channels;
        let frames_needed = target_frames.saturating_sub(self.output_frames_emitted);
        let frames_to_emit = usize::try_from(frames_needed.min(frames_available as u64))
            .map_err(|_| Pcm16StreamError::OutputFrameCountOverflow)?;
        let output_capacity = frames_to_emit
            .checked_mul(usize::from(self.output_channels))
            .and_then(|samples| samples.checked_mul(2))
            .ok_or(Pcm16StreamError::OutputFrameCountOverflow)?;
        let mut bytes = Vec::with_capacity(output_capacity);
        for _ in 0..frames_to_emit {
            let left = self
                .output
                .pop_front()
                .ok_or(Pcm16StreamError::ResamplerProcessingFailed)?;
            let (left, right) = if self.resampler_channels == 2 {
                (
                    left,
                    self.output
                        .pop_front()
                        .ok_or(Pcm16StreamError::ResamplerProcessingFailed)?,
                )
            } else {
                (left, left)
            };
            append_pcm16le(&mut bytes, left);
            if self.output_channels == 2 {
                append_pcm16le(&mut bytes, right);
            }
        }
        self.output_frames_emitted = self
            .output_frames_emitted
            .checked_add(
                u64::try_from(frames_to_emit)
                    .map_err(|_| Pcm16StreamError::OutputFrameCountOverflow)?,
            )
            .ok_or(Pcm16StreamError::OutputFrameCountOverflow)?;
        Ok(bytes)
    }
}

fn append_pcm16le(output: &mut Vec<u8>, sample: f32) {
    let sample = if sample.is_nan() {
        0
    } else if sample <= -1.0 {
        i16::MIN
    } else if sample >= 1.0 {
        i16::MAX
    } else {
        (sample * 32_767.0).round() as i16
    };
    output.extend_from_slice(&sample.to_le_bytes());
}

#[cfg(test)]
mod tests;
