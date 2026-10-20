//! Pumps bounded device queues through capture processing and the outgoing audio track.
//! Devices retain real stream ownership; all queued and partial capture obey mute generations.
//! Muted sessions send generated Opus silence independently of device capture.
//! Retain at most one encoded batch and return between packets so controls can take priority.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::buffers::Buffers;
use super::processing;
use crate::MAX_CAPTURE_AGE;
use crate::service_failure::ServiceFailure;

pub(super) struct CaptureWorker {
    pub(super) buffers: Arc<Buffers>,
    pub(super) processor: processing::Processor,
    pub(super) pending: VecDeque<crate::EncodedAudio>,
}

impl CaptureWorker {
    pub(super) fn set_controls(&mut self, controls: crate::AudioControls) -> io::Result<()> {
        // Retire the old sink before rebuilding microphone processing.
        self.buffers
            .set_speaker_disabled(controls.speaker_suppressed)?;
        let previous = self.buffers.microphone.load(Ordering::Acquire);
        Buffers::set_disabled(&self.buffers.microphone, controls.microphone_muted)?;
        if previous != self.buffers.microphone.load(Ordering::Acquire) {
            self.pending.clear();
            self.processor.reset().map_err(io::Error::other)?;
        }
        Ok(())
    }

    pub(super) async fn service(
        &mut self,
        audio: &mut lingxi_webrtc_core::RtcSession,
        now: impl Fn() -> Instant,
    ) -> io::Result<usize> {
        // Some backends start callbacks during stream construction, before open returns.
        self.buffers.serviced.store(true, Ordering::Release);
        let generation = self.buffers.microphone.load(Ordering::Acquire);
        if self.buffers.render_dropped.swap(false, Ordering::AcqRel) {
            self.processor.reset_render();
            for _ in 0..self.buffers.rendered.capacity() {
                if self.buffers.rendered.pop().is_none() {
                    break;
                }
            }
        }
        if self.buffers.capture_dropped.swap(false, Ordering::AcqRel) {
            self.pending.clear();
            self.processor.reset().map_err(io::Error::other)?;
            for _ in 0..self.buffers.capture.capacity() {
                if self.buffers.capture.pop().is_none() {
                    break;
                }
            }
        }
        let mut render_stale = false;
        for _ in 0..self.buffers.rendered.capacity() {
            let Some(frame) = self.buffers.rendered.pop() else {
                break;
            };
            if frame.at.elapsed() > MAX_CAPTURE_AGE {
                if !render_stale {
                    self.processor.reset_render();
                    render_stale = true;
                }
                continue;
            }
            if frame.generation != self.buffers.speaker.load(Ordering::Acquire) {
                continue;
            }
            render_stale = false;
            self.processor
                .render(&frame)
                .map_err(|_| io::Error::other(ServiceFailure::Render))?;
        }
        if self.pending.is_empty() {
            let mut capture_stale = false;
            for _ in 0..self.buffers.capture.capacity() {
                let Some(frame) = self.buffers.capture.pop() else {
                    break;
                };
                if generation.is_multiple_of(2) && frame.generation == generation {
                    if now().saturating_duration_since(frame.at) > processing::MAX_PROCESSING_DELAY
                    {
                        self.pending.clear();
                        if !capture_stale {
                            self.processor.reset().map_err(io::Error::other)?;
                            capture_stale = true;
                        }
                        continue;
                    }
                    capture_stale = false;
                    let frames = self.processor.capture(&frame, &now);
                    match frames {
                        Ok(frames) => self.pending.extend(frames),
                        Err(processing::PROCESSING_LATE) => {
                            self.pending.clear();
                            self.processor.reset().map_err(io::Error::other)?;
                        }
                        Err(_) => return Err(io::Error::other(ServiceFailure::Capture)),
                    }
                }
            }
        }
        if self.buffers.failed.load(Ordering::Acquire) {
            return Err(io::Error::other(ServiceFailure::Device));
        }
        let Some(frame) = self.pending.pop_front() else {
            if !generation.is_multiple_of(2) {
                audio
                    .send_opus_if_due(silence_packet()?, now())
                    .await
                    .map_err(|_| io::Error::other(ServiceFailure::Send))?;
            }
            return Ok(0);
        };
        if now().saturating_duration_since(frame.at) > processing::MAX_PROCESSING_DELAY {
            self.pending.clear();
            self.processor.reset().map_err(io::Error::other)?;
            return Ok(0);
        }
        audio
            .send_opus(frame.data, frame.at)
            .await
            .map_err(|_| io::Error::other(ServiceFailure::Send))?;
        Ok(1)
    }
}

fn silence_packet() -> io::Result<Vec<u8>> {
    static PACKET: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    if let Some(packet) = PACKET.get() {
        return Ok(packet.clone());
    }
    let mut encoder = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)
        .map_err(io::Error::other)?;
    let mut packet = vec![0; 1275];
    let size = encoder
        .encode_float(&[0.; 960], &mut packet)
        .map_err(io::Error::other)?;
    packet.truncate(size);
    let _ = PACKET.set(packet.clone());
    Ok(packet)
}
