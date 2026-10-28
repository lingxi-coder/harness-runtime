//! Device-independent callback ports. One serialized capture callback and one
//! serialized render callback own their handles. Neither waits on the media worker.
use crate::{
    AudioControls, AudioState,
    buffers::{
        BLOCK, Buffers, CaptureBoundary, Frame, FramePacker, MAX_CALLBACK_FRAMES, Playback,
        record_peak,
    },
    capture_worker::CaptureWorker,
    playback::PlaybackPort,
    playout::Playout,
    processing::Processor,
};
use crossbeam_queue::ArrayQueue;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const DECODED_PCM_QUEUE_FRAMES: usize = 32;
const MAX_DECODED_FRAME_DURATION: Duration = Duration::from_millis(120);

pub struct CapturePort {
    buffers: Arc<Buffers>,
    packer: FramePacker,
    boundary: CaptureBoundary,
    rate: u32,
}
pub struct RenderPort {
    buffers: Arc<Buffers>,
    playback: Playback,
    reference: FramePacker,
    rate: u32,
}

/// One bounded, timestamped mono frame decoded from incoming RTP audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedAudioFrame {
    /// Monotonic sequence within this playout instance.
    pub sequence: u64,
    /// Monotonic presentation time mapped from RTP ingress and GStreamer running time.
    pub at: Instant,
    /// PCM sample rate selected when the playout instance was created.
    pub sample_rate_hz: u32,
    /// Decoded frames are mono.
    pub channels: u16,
    /// Interleaved signed PCM16 little-endian samples.
    pub pcm16le: Vec<u8>,
}

struct QueuedPcmFrame {
    sequence: u64,
    pts_ns: u64,
    pcm16le: Vec<u8>,
}

struct PcmOutputState {
    queue: ArrayQueue<QueuedPcmFrame>,
    sample_rate_hz: u32,
    next_sequence: AtomicU64,
    failed: AtomicBool,
}

/// Bounded decoded-PCM output shared with the GStreamer sink thread.
#[derive(Clone)]
pub(crate) struct PcmOutput(Arc<PcmOutputState>);

impl PcmOutput {
    fn new(sample_rate_hz: u32) -> Self {
        Self(Arc::new(PcmOutputState {
            queue: ArrayQueue::new(DECODED_PCM_QUEUE_FRAMES),
            sample_rate_hz,
            next_sequence: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        }))
    }

    pub(crate) fn rate(&self) -> u32 {
        self.0.sample_rate_hz
    }

    pub(crate) fn push(
        &self,
        samples_f32le: &[u8],
        pts: Option<gstreamer::ClockTime>,
    ) -> Result<(), &'static str> {
        if self.0.failed.load(Ordering::Acquire) {
            return Err("decoded audio output failed");
        }
        let sample_count = samples_f32le.len() / 4;
        let max_samples = (u64::from(self.0.sample_rate_hz)
            * MAX_DECODED_FRAME_DURATION.as_millis() as u64
            / 1_000) as usize;
        if samples_f32le.is_empty()
            || !samples_f32le.len().is_multiple_of(4)
            || sample_count > max_samples
        {
            self.fail();
            return Err("decoded audio frame exceeds its sample bound");
        }
        let Some(pts) = pts else {
            self.fail();
            return Err("decoded audio frame has no timestamp");
        };
        let sequence = match self.0.next_sequence.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |sequence| sequence.checked_add(1),
        ) {
            Ok(sequence) => sequence,
            Err(_) => {
                self.fail();
                return Err("decoded audio sequence exhausted");
            }
        };
        let mut pcm16le = Vec::with_capacity(sample_count * 2);
        for bytes in samples_f32le.chunks_exact(4) {
            let sample = f32::from_le_bytes(
                bytes
                    .try_into()
                    .map_err(|_| "invalid decoded audio sample")?,
            );
            if !sample.is_finite() {
                self.fail();
                return Err("decoded audio contains an invalid sample");
            }
            let sample = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
            pcm16le.extend_from_slice(&sample.to_le_bytes());
        }
        if self
            .0
            .queue
            .push(QueuedPcmFrame {
                sequence,
                pts_ns: pts.nseconds(),
                pcm16le,
            })
            .is_err()
        {
            self.fail();
            return Err("decoded audio output queue is full");
        }
        Ok(())
    }

    fn pop(&self) -> Option<QueuedPcmFrame> {
        self.0.queue.pop()
    }

    fn is_empty(&self) -> bool {
        self.0.queue.is_empty()
    }

    fn clear(&self) {
        while self.0.queue.pop().is_some() {}
    }

    fn failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }

    pub(crate) fn fail(&self) {
        self.0.failed.store(true, Ordering::Release);
    }
}

/// Host-facing decoded audio playout backed by the existing RTP jitter/decode pipeline.
///
/// This type owns no device. Feed incoming [`lingxi_webrtc_core::ReceivedRtp`] packets,
/// then consume decoded PCM frames and pass them to the host's already-open output path.
/// The output queue holds at most 32 frames, each no longer than 120 ms; overflow fails
/// the playout instead of dropping decoded speech.
pub struct DecodedAudioPlayout {
    playout: Playout,
    output: PcmOutput,
    sample_rate_hz: u32,
    input_finished: bool,
    interrupted: bool,
}

impl DecodedAudioPlayout {
    /// Create a mono PCM16LE playout stream at the requested output rate.
    pub fn new(sample_rate_hz: u32) -> Result<Self, &'static str> {
        if !(8_000..=192_000).contains(&sample_rate_hz) {
            return Err("invalid decoded audio sample rate");
        }
        let output = PcmOutput::new(sample_rate_hz);
        let playout = Playout::new_pcm(sample_rate_hz, output.clone())?;
        Ok(Self {
            playout,
            output,
            sample_rate_hz,
            input_finished: false,
            interrupted: false,
        })
    }

    /// Push one owned RTP packet into the existing bounded jitter/decode pipeline.
    pub fn push(&mut self, packet: lingxi_webrtc_core::ReceivedRtp) -> Result<(), &'static str> {
        if self.interrupted {
            return Err("decoded audio playout was interrupted");
        }
        if self.input_finished {
            return Err("decoded audio playout input is finished");
        }
        self.playout.push(packet)
    }

    /// Push an owned RTP buffer while retaining its owner until GStreamer releases it.
    pub fn push_owned<T: AsRef<[u8]> + Send + 'static>(
        &mut self,
        packet: T,
        at: Instant,
    ) -> Result<(), &'static str> {
        if self.interrupted {
            return Err("decoded audio playout was interrupted");
        }
        if self.input_finished {
            return Err("decoded audio playout input is finished");
        }
        self.playout.push_owned(packet, at)
    }

    /// Take one decoded frame if available; `None` means no frame is ready yet.
    pub fn try_read(&mut self) -> Result<Option<DecodedAudioFrame>, &'static str> {
        if self.interrupted {
            return Ok(None);
        }
        self.playout.check()?;
        if self.output.failed() {
            return Err("decoded audio playout failed");
        }
        let Some(frame) = self.output.pop() else {
            return Ok(None);
        };
        let at = self.playout.presentation_time(frame.pts_ns)?;
        Ok(Some(DecodedAudioFrame {
            sequence: frame.sequence,
            at,
            sample_rate_hz: self.sample_rate_hz,
            channels: 1,
            pcm16le: frame.pcm16le,
        }))
    }

    /// End RTP input and let the jitter/decode pipeline finish queued audio.
    /// Consume frames until [`Self::is_drained`] before flushing the host output path.
    pub fn finish_input(&mut self) -> Result<(), &'static str> {
        if self.interrupted {
            return Err("decoded audio playout was interrupted");
        }
        if !self.input_finished {
            self.playout.finish_input()?;
            self.input_finished = true;
        }
        Ok(())
    }

    /// Whether EOS was observed and all decoded frames have been consumed.
    /// This does not mean that frames already passed to the host have played.
    pub fn is_drained(&self) -> bool {
        self.interrupted || (self.playout.ended() && self.output.is_empty())
    }

    /// Stop immediately and discard frames that have not yet been handed to the host.
    /// Actual played progress is reported by the host output path's render receipt.
    pub fn interrupt(&mut self) -> Result<(), &'static str> {
        if !self.interrupted {
            self.playout.interrupt()?;
            self.output.clear();
            self.interrupted = true;
        }
        Ok(())
    }
}
/// Shared media worker for native iOS/Android adapters and synthetic fixtures.
pub struct NativeAudio {
    worker: CaptureWorker,
    playback: PlaybackPort,
    playout: Option<Playout>,
}
impl NativeAudio {
    pub fn new(
        capture_rate: u32,
        render_rate: u32,
        capture_callback_frames: u32,
        render_callback_frames: u32,
    ) -> io::Result<(Self, CapturePort, RenderPort)> {
        if !(8000..=192000).contains(&capture_rate) || !(8000..=192000).contains(&render_rate) {
            return Err(io::Error::other("invalid device sample rate"));
        }
        if capture_callback_frames == 0
            || render_callback_frames == 0
            || capture_callback_frames as usize > MAX_CALLBACK_FRAMES
            || render_callback_frames as usize > MAX_CALLBACK_FRAMES
        {
            return Err(io::Error::other("invalid callback frame count"));
        }
        let processor = Processor::new(capture_rate, render_rate).map_err(io::Error::other)?;
        processor
            .validate_callback_timing(capture_callback_frames, render_callback_frames)
            .map_err(io::Error::other)?;
        let buffers = Arc::new(Buffers::new(capture_rate, render_rate));
        let playback = PlaybackPort::new(buffers.clone(), render_rate);
        Ok((
            Self {
                worker: CaptureWorker {
                    buffers: buffers.clone(),
                    processor,
                    pending: Default::default(),
                },
                playback,
                playout: None,
            },
            CapturePort {
                buffers: buffers.clone(),
                packer: Default::default(),
                boundary: Default::default(),
                rate: capture_rate,
            },
            RenderPort {
                buffers,
                playback: Default::default(),
                reference: Default::default(),
                rate: render_rate,
            },
        ))
    }
    pub fn controls(&mut self, controls: AudioControls) -> io::Result<()> {
        let previous = self.worker.buffers.speaker.load(Ordering::Acquire);
        self.worker.set_controls(controls)?;
        if previous != self.worker.buffers.speaker.load(Ordering::Acquire) {
            drop(self.playout.take());
            let _producer = self
                .playback
                .0
                .producer
                .lock()
                .map_err(|_| io::Error::other("speaker writer failed"))?;
            while let Some(frame) = self.worker.buffers.playback.pop() {
                self.worker
                    .buffers
                    .queued
                    .fetch_sub(frame.len as u32, Ordering::AcqRel);
            }
            while self.worker.buffers.rendered.pop().is_some() {}
            self.worker.processor.reset_render();
            if !controls.speaker_suppressed {
                self.playout =
                    Some(Playout::new(self.playback.writer()).map_err(io::Error::other)?);
            }
        }
        Ok(())
    }
    pub async fn service(
        &mut self,
        session: &mut lingxi_webrtc_core::RtcSession,
    ) -> io::Result<usize> {
        session
            .set_playback_suppressed(self.playout.is_none())
            .map_err(io::Error::other)?;
        for _ in 0..64 {
            let Some(packet) = session.take_audio_packet().map_err(io::Error::other)? else {
                break;
            };
            if let Some(playout) = &self.playout {
                playout.push(packet).map_err(io::Error::other)?;
            }
        }
        if let Some(playout) = &self.playout {
            playout.check().map_err(io::Error::other)?;
        }
        self.worker.service(session, Instant::now).await
    }
    pub fn state(&self) -> io::Result<AudioState> {
        self.worker.buffers.take_state()
    }
}
impl Drop for NativeAudio {
    fn drop(&mut self) {
        self.worker.buffers.failed.store(true, Ordering::Release);
        drop(self.playout.take());
    }
}
impl CapturePort {
    /// `callback` and `captured` share the platform device clock. `at` is the
    /// first sample mapped into the host monotonic clock, not its arrival time.
    pub fn submit(
        &mut self,
        pcm: &[f32],
        at: Instant,
        callback: Duration,
        captured: Duration,
    ) -> Result<(), &'static str> {
        if self.buffers.failed.load(Ordering::Acquire) {
            return Err("audio device closed");
        }
        if captured > callback {
            self.packer.reset();
            return Err("capture timestamp is after callback");
        }
        if pcm.is_empty() || pcm.len() > MAX_CALLBACK_FRAMES || pcm.iter().any(|v| !v.is_finite()) {
            self.packer.reset();
            return Err("invalid capture callback");
        }
        let generation = self.buffers.microphone.load(Ordering::Acquire);
        if !self.boundary.accepts(generation, callback, captured) {
            self.packer.reset();
            return Ok(());
        }
        for sample in pcm {
            record_peak(&self.buffers.microphone_peak, *sample);
        }
        self.packer.discard_capture_gap(at, f64::from(self.rate));
        for (index, samples) in pcm.chunks(BLOCK).enumerate() {
            let mut frame = Frame {
                samples: [0.; BLOCK],
                len: samples.len(),
                at: at + Duration::from_secs_f64((index * BLOCK) as f64 / f64::from(self.rate)),
                generation,
            };
            frame.samples[..samples.len()].copy_from_slice(samples);
            if !self
                .packer
                .push(frame, f64::from(self.rate), &self.buffers.capture)
            {
                self.packer.reset();
                self.buffers.capture_dropped.store(true, Ordering::Release);
            }
        }
        Ok(())
    }
}
impl RenderPort {
    /// Fill the device's preallocated mono buffer and retain the actual output as AEC reference.
    pub fn render(&mut self, output: &mut [f32], at: Instant) -> Result<(), &'static str> {
        if self.buffers.failed.load(Ordering::Acquire) {
            output.fill(0.);
            return Err("audio device closed");
        }
        if output.len() > MAX_CALLBACK_FRAMES {
            output.fill(0.);
            return Err("oversized render callback");
        }
        if !self.buffers.serviced.load(Ordering::Acquire) {
            output.fill(0.);
            return Ok(());
        }
        for (index, samples) in output.chunks_mut(BLOCK).enumerate() {
            let mut reference = Frame {
                samples: [0.; BLOCK],
                len: samples.len(),
                at: at + Duration::from_secs_f64((index * BLOCK) as f64 / f64::from(self.rate)),
                generation: self.buffers.speaker.load(Ordering::Acquire),
            };
            for (sample, actual) in samples.iter_mut().zip(&mut reference.samples) {
                *sample = self.playback.next(&self.buffers).unwrap_or(0.);
                *actual = *sample;
                record_peak(&self.buffers.speaker_peak, *sample);
            }
            if !self
                .reference
                .push(reference, f64::from(self.rate), &self.buffers.rendered)
            {
                self.reference.reset();
                self.buffers.render_dropped.store(true, Ordering::Release);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_capture_meter_tracks_accepted_audio_and_clears_on_read() {
        let (owner, mut capture, _) = NativeAudio::new(48000, 48000, 256, 256).unwrap();
        owner.worker.buffers.microphone.store(2, Ordering::Release);
        let at = Instant::now();
        capture
            .submit(&[0.75; 256], at, Duration::ZERO, Duration::ZERO)
            .unwrap();
        assert_eq!(owner.state().unwrap().microphone_peak, 0);
        capture
            .submit(&[0.5; 256], at, Duration::ZERO, Duration::ZERO)
            .unwrap();
        assert_eq!(owner.state().unwrap().microphone_peak, 32767);
        assert_eq!(owner.state().unwrap().microphone_peak, 0);
    }
    #[test]
    fn callbacks_are_retired_when_the_native_owner_closes() {
        let (owner, mut capture, mut render) = NativeAudio::new(48000, 48000, 256, 256).unwrap();
        let at = Instant::now();
        assert!(
            capture
                .submit(&[0.; 256], at, Duration::ZERO, Duration::ZERO)
                .is_ok()
        );
        drop(owner);
        assert!(
            capture
                .submit(&[0.; 256], at, Duration::ZERO, Duration::ZERO)
                .is_err()
        );
        let mut output = [1.; 256];
        assert!(render.render(&mut output, at).is_err());
        assert!(output.iter().all(|s| *s == 0.));
    }
    #[test]
    fn mute_transition_preserves_inflight_playback_accounting() {
        crate::runtime::test_init();
        let (mut owner, _, mut render) = NativeAudio::new(48000, 48000, 256, 256).unwrap();
        owner
            .controls(AudioControls {
                microphone_muted: true,
                speaker_suppressed: false,
            })
            .unwrap();
        owner.worker.buffers.serviced.store(true, Ordering::Release);
        let writer = owner.playback.writer();
        let pcm: Vec<_> = (0..256).flat_map(|_| 0.25f32.to_le_bytes()).collect();
        writer.write(&pcm).unwrap();
        let mut partial = [0.; 32];
        render.render(&mut partial, Instant::now()).unwrap();
        assert!(partial.iter().all(|v| *v == 0.25));
        assert_eq!(owner.state().unwrap().speaker_peak, 16383);
        owner
            .controls(AudioControls {
                microphone_muted: true,
                speaker_suppressed: true,
            })
            .unwrap();
        render.render(&mut partial, Instant::now()).unwrap();
        assert!(partial.iter().all(|v| *v == 0.));
        assert_eq!(owner.worker.buffers.queued.load(Ordering::Acquire), 0);
    }

    #[test]
    fn decoded_pcm_output_preserves_pts_and_converts_to_bounded_pcm16le() {
        let output = PcmOutput::new(48_000);
        let samples = [0.5f32, -1.0, 1.0];
        let bytes: Vec<_> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        output
            .push(&bytes, Some(gstreamer::ClockTime::from_mseconds(20)))
            .unwrap();

        let frame = output.pop().unwrap();
        assert_eq!(frame.sequence, 0);
        assert_eq!(frame.pts_ns, 20_000_000);
        assert_eq!(frame.pcm16le.len(), 6);
        let samples: Vec<_> = frame
            .pcm16le
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes(bytes.try_into().unwrap()))
            .collect();
        assert_eq!(samples, [16_384, -32_767, 32_767]);
    }

    #[test]
    fn decoded_pcm_output_fails_closed_on_oversize_or_full_queue() {
        let output = PcmOutput::new(8_000);
        let oversized = vec![0; (960 + 1) * 4];
        assert!(
            output
                .push(&oversized, Some(gstreamer::ClockTime::ZERO))
                .is_err()
        );
        assert!(output.failed());

        let output = PcmOutput::new(8_000);
        let one_sample = 0.0f32.to_le_bytes();
        for _ in 0..DECODED_PCM_QUEUE_FRAMES {
            output
                .push(&one_sample, Some(gstreamer::ClockTime::ZERO))
                .unwrap();
        }
        assert!(
            output
                .push(&one_sample, Some(gstreamer::ClockTime::ZERO))
                .is_err()
        );
        assert!(output.failed());
        assert_eq!(output.0.queue.len(), DECODED_PCM_QUEUE_FRAMES);
    }
}
