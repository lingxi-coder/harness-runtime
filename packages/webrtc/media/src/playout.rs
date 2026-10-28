//! One explicit RTP jitter/decode pipeline per audible epoch; no autoplugging or device discovery.
//! Owned RTP memory retains ingress permits, bounding bursts without dropping valid queued speech.
//! Native bus messages never accumulate or escape.

use super::audio_sink::Sink;
use super::playback::PlaybackWriter;
use crate::ports::PcmOutput;
use gst::prelude::*;
use gstreamer as gst;
use lingxi_webrtc_core::ReceivedRtp;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;

pub(super) struct Playout {
    pipeline: gst::Pipeline,
    source: gstreamer_app::AppSrc,
    anchor: (Instant, gst::ClockTime),
    failed: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
    latency: Arc<AtomicBool>,
}

impl Playout {
    pub(super) fn presentation_time(&self, pts_ns: u64) -> Result<Instant, &'static str> {
        let offset_ns = pts_ns.saturating_sub(self.anchor.1.nseconds());
        self.anchor
            .0
            .checked_add(std::time::Duration::from_nanos(offset_ns))
            .ok_or("decoded audio timestamp overflow")
    }

    pub(super) fn new(writer: PlaybackWriter) -> Result<Self, &'static str> {
        let rate = writer.rate();
        Self::build(rate, Sink::new_playback(writer))
    }

    pub(super) fn new_pcm(rate: u32, output: PcmOutput) -> Result<Self, &'static str> {
        Self::build(rate, Sink::new_pcm(output))
    }

    fn build(rate: u32, sink: Sink) -> Result<Self, &'static str> {
        crate::runtime::ensure_initialized()?;
        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("channels", 1i32)
            .field("rate", rate as i32)
            .build();
        let source = gstreamer_app::AppSrc::builder()
            .is_live(true)
            .format(gst::Format::Time)
            .block(false)
            .max_buffers(64)
            .max_bytes(2 * 1024 * 1024)
            .caps(
                &gst::Caps::builder("application/x-rtp")
                    .field("media", "audio")
                    .field("encoding-name", "OPUS")
                    .field("clock-rate", 48000i32)
                    .field("payload", i32::from(crate::audio_track::OPUS_PAYLOAD_TYPE))
                    .build(),
            )
            .build();
        let jitter = gst::ElementFactory::make("rtpjitterbuffer")
            .property("latency", 60u32)
            .property_from_str("mode", "slave")
            // Latency is the reordering deadline, not a cap on valid burst audio.
            // Ingress permits already bound retained packets and bytes through decoding.
            .property("drop-on-latency", false)
            .property("do-lost", true)
            .property("do-retransmission", false)
            .build()
            .map_err(|_| "jitter buffer unavailable")?;
        let depay = gst::ElementFactory::make("rtpopusdepay")
            .build()
            .map_err(|_| "audio depayloader unavailable")?;
        let decoder = gst::ElementFactory::make("opusdec")
            .property("plc", true)
            .property("use-inband-fec", false)
            .build()
            .map_err(|_| "audio decoder unavailable")?;
        let convert = gst::ElementFactory::make("audioconvert")
            .build()
            .map_err(|_| "audio converter unavailable")?;
        let resample = gst::ElementFactory::make("audioresample")
            .build()
            .map_err(|_| "audio resampler unavailable")?;
        let filter = gst::ElementFactory::make("capsfilter")
            .property("caps", caps)
            .build()
            .map_err(|_| "audio format filter unavailable")?;
        let pipeline = gst::Pipeline::new();
        let elements = [
            source.upcast_ref(),
            &jitter,
            &depay,
            &decoder,
            &convert,
            &resample,
            &filter,
            sink.upcast_ref(),
        ];
        pipeline
            .add_many(elements)
            .map_err(|_| "audio pipeline assembly failed")?;
        gst::Element::link_many(elements).map_err(|_| "audio pipeline linking failed")?;
        pipeline.use_clock(Some(&gst::SystemClock::obtain()));
        let failed = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(AtomicBool::new(false));
        let latency = Arc::new(AtomicBool::new(true));
        let failure = failed.clone();
        let end = ended.clone();
        let changed = latency.clone();
        pipeline
            .bus()
            .ok_or("audio bus unavailable")?
            .set_sync_handler(move |_, message| {
                match message.view() {
                    gst::MessageView::Error(_) | gst::MessageView::ClockLost(_) => {
                        failure.store(true, Ordering::Release);
                    }
                    gst::MessageView::Eos(_) => end.store(true, Ordering::Release),
                    gst::MessageView::Latency(_) => {
                        changed.store(true, Ordering::Release);
                    }
                    _ => {}
                }
                gst::BusSyncReply::Drop
            });
        let mut playout = Self {
            pipeline,
            source,
            anchor: (Instant::now(), gst::ClockTime::ZERO),
            failed,
            ended,
            latency,
        };
        playout
            .pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| "audio pipeline start failed")?;
        playout.anchor = (
            Instant::now(),
            playout
                .pipeline
                .current_running_time()
                .ok_or("audio clock unavailable")?,
        );
        Ok(playout)
    }

    pub(super) fn push(&self, packet: ReceivedRtp) -> Result<(), &'static str> {
        let at = packet.at;
        self.push_owned(packet, at)
    }

    pub(super) fn push_owned<T: AsRef<[u8]> + Send + 'static>(
        &self,
        packet: T,
        at: Instant,
    ) -> Result<(), &'static str> {
        let Some(elapsed) = at.checked_duration_since(self.anchor.0) else {
            return Ok(());
        };
        let time = self
            .anchor
            .1
            .checked_add(gst::ClockTime::try_from(elapsed).map_err(|_| "audio timestamp overflow")?)
            .ok_or("audio timestamp overflow")?;
        let mut buffer = gst::Buffer::from_slice(packet);
        buffer
            .get_mut()
            .ok_or("audio packet unexpectedly shared")?
            .set_pts(time);
        self.source
            .push_buffer(buffer)
            .map_err(|_| "audio pipeline rejected packet")?;
        self.check()
    }

    pub(super) fn check(&self) -> Result<(), &'static str> {
        if self.failed.load(Ordering::Acquire) {
            return Err("audio playback failed");
        }
        if self.latency.swap(false, Ordering::AcqRel) {
            self.pipeline
                .recalculate_latency()
                .map_err(|_| "audio latency negotiation failed")?;
        }
        Ok(())
    }

    pub(super) fn finish_input(&self) -> Result<(), &'static str> {
        self.source
            .end_of_stream()
            .map(|_| ())
            .map_err(|_| "audio pipeline rejected EOS")
    }

    pub(super) fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    pub(super) fn interrupt(&self) -> Result<(), &'static str> {
        self.pipeline
            .set_state(gst::State::Null)
            .map_err(|_| "audio pipeline interruption failed")?;
        self.ended.store(true, Ordering::Release);
        Ok(())
    }
}

impl Drop for Playout {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
#[path = "playout_tests.rs"]
mod tests;
