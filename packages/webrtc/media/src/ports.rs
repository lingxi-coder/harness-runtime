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
use std::{
    io,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

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
}
