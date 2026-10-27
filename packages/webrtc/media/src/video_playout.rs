//! Bounded per-SSRC VP8 jitter/decode pipeline. Video may discard late frames;
//! its buffers never consume audio's independently bounded ingress permits.
use crate::video::{VideoFrame, VideoLayout};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::{AppSink, AppSrc};
use gstreamer_video::VideoInfo;
use std::time::{Duration, Instant};
pub struct VideoPlayout {
    pipeline: gst::Pipeline,
    source: AppSrc,
    sink: AppSink,
    anchor: Instant,
}
impl VideoPlayout {
    pub fn new() -> Result<Self, String> {
        Self::new_at(Instant::now())
    }
    pub(crate) fn new_at(anchor: Instant) -> Result<Self, String> {
        crate::runtime::ensure_initialized().map_err(str::to_owned)?;
        let source = AppSrc::builder()
            .is_live(true)
            .format(gst::Format::Time)
            .block(false)
            .max_buffers(64)
            .max_bytes(2 * 1024 * 1024)
            .caps(
                &gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("encoding-name", "VP8")
                    .field("clock-rate", 90000i32)
                    .field("payload", 96i32)
                    .build(),
            )
            .build();
        let jitter = gst::ElementFactory::make("rtpjitterbuffer")
            .property("latency", 80u32)
            .property("drop-on-latency", true)
            .property("do-lost", true)
            .build()
            .map_err(|_| "video jitter buffer unavailable")?;
        let depay = gst::ElementFactory::make("rtpvp8depay")
            .build()
            .map_err(|_| "VP8 depayloader unavailable")?;
        let decoder = gst::ElementFactory::make("vp8dec")
            .build()
            .map_err(|_| "VP8 decoder unavailable")?;
        let convert = gst::ElementFactory::make("videoconvert")
            .build()
            .map_err(|_| "video converter unavailable")?;
        let sink = AppSink::builder()
            .sync(false)
            .max_buffers(2)
            .leaky_type(gstreamer_app::AppLeakyType::Downstream)
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "I420")
                    .build(),
            )
            .build();
        let pipeline = gst::Pipeline::new();
        let elements = [
            source.upcast_ref(),
            &jitter,
            &depay,
            &decoder,
            &convert,
            sink.upcast_ref(),
        ];
        pipeline
            .add_many(elements)
            .map_err(|_| "video decoder assembly failed")?;
        gst::Element::link_many(elements).map_err(|_| "video decoder link failed")?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| "video decoder start failed")?;
        Ok(Self {
            pipeline,
            source,
            sink,
            anchor,
        })
    }

    pub(crate) fn frame_time(&self, pts: Duration) -> Result<Instant, &'static str> {
        self.anchor
            .checked_add(pts)
            .ok_or("decoded video timestamp overflow")
    }

    pub fn push(&self, packet: lingxi_webrtc_core::ReceivedRtp) -> Result<(), String> {
        let Some(elapsed) = packet.at.checked_duration_since(self.anchor) else {
            return Ok(());
        };
        if self.source.current_level_buffers() >= 64
            || self.source.current_level_bytes() >= 2 * 1024 * 1024
        {
            return Err("video decoder saturated".into());
        }
        let mut buffer = gst::Buffer::from_slice(packet);
        buffer
            .get_mut()
            .ok_or("video packet ownership failed")?
            .set_pts(gst::ClockTime::from_nseconds(elapsed.as_nanos() as u64));
        self.source
            .push_buffer(buffer)
            .map_err(|_| "video decoder rejected packet")?;
        Ok(())
    }
    pub fn take_frame(&self) -> Result<Option<VideoFrame>, String> {
        if let Some(error) = self
            .pipeline
            .bus()
            .and_then(|bus| bus.pop_filtered(&[gst::MessageType::Error]))
        {
            let _ = error;
            return Err("video decode failed".into());
        }
        let Some(sample) = self.sink.try_pull_sample(gst::ClockTime::ZERO) else {
            return Ok(None);
        };
        let info = VideoInfo::from_caps(sample.caps().ok_or("missing decoded video caps")?)
            .map_err(|_| "invalid decoded video caps")?;
        if info.size() > 4 * 1024 * 1024 {
            return Err("decoded video exceeds frame budget".into());
        }
        let buffer = sample.buffer().ok_or("missing decoded video buffer")?;
        let pixels = buffer
            .map_readable()
            .map_err(|_| "decoded video mapping failed")?
            .as_slice()
            .to_vec();
        Ok(Some(VideoFrame {
            layout: VideoLayout {
                width: info.width(),
                height: info.height(),
                size: info.size(),
                offsets: info.offset().to_vec(),
                strides: info.stride().to_vec(),
            },
            pixels,
            pts: std::time::Duration::from_nanos(buffer.pts().map(|v| v.nseconds()).unwrap_or(0)),
        }))
    }
}
impl Drop for VideoPlayout {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::VideoCodec;
    use lingxi_webrtc_core::incoming::Incoming;
    use rtc::sansio::Protocol;
    use rtc::{
        interceptor::{Packet, TaggedPacket},
        rtp::{codec::vp8::Vp8Payloader, packetizer::Payloader},
    };
    use std::time::Duration;

    #[test]
    fn fragmented_vp8_rtp_decodes_after_reordering_independently_for_both_sources() {
        crate::runtime::test_init();
        crate::runtime::load_video_plugins(std::path::Path::new(env!("LINGXI_DEV_GST_PLUGIN_DIR")))
            .unwrap();
        let camera = VideoPlayout::new().unwrap();
        let screen = VideoPlayout::new().unwrap();
        let (_, mut ingress, mut incoming) = Incoming::with_video();
        incoming.set_suppressed(false).unwrap();
        for (ssrc, playout, level) in [(71, &camera, 70u8), (72, &screen, 170u8)] {
            let mut encoder = VideoCodec::encoder(64, 48, 15, 256000).unwrap();
            let layout = encoder.layout().clone();
            let mut pixels = vec![128; layout.size];
            for row in 0..48 {
                let offset = layout.offsets[0] + row * layout.strides[0] as usize;
                pixels[offset..offset + 64].fill(level);
            }
            let encoded = encoder
                .process(pixels, Duration::ZERO, Duration::from_millis(67))
                .unwrap();
            let payloads = Vp8Payloader::default().payload(8, &encoded.into()).unwrap();
            assert!(payloads.len() >= 3, "fixture must exercise fragmentation");
            let count = payloads.len();
            let mut packets: Vec<_> = payloads
                .into_iter()
                .enumerate()
                .map(|(index, payload)| rtc::rtp::Packet {
                    header: rtc::rtp::header::Header {
                        version: 2,
                        payload_type: 96,
                        sequence_number: index as u16,
                        timestamp: 90000,
                        ssrc,
                        marker: index + 1 == count,
                        ..Default::default()
                    },
                    payload,
                })
                .collect();
            // Keep the initial sequence, then reorder subsequent fragments.
            packets.swap(1, 2);
            for packet in packets {
                ingress
                    .handle_read(TaggedPacket {
                        now: Instant::now(),
                        transport: Default::default(),
                        message: Packet::Rtp(packet),
                    })
                    .unwrap();
                playout.push(incoming.take().unwrap().unwrap()).unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            let frame = loop {
                if let Some(frame) = playout.take_frame().unwrap() {
                    break frame;
                }
                assert!(Instant::now() < deadline, "video RTP decoder timed out");
                std::thread::park_timeout(Duration::from_millis(2));
            };
            assert_eq!((frame.layout.width, frame.layout.height), (64, 48));
            assert_eq!(frame.pixels.len(), frame.layout.size);
            let average = frame.pixels[..64]
                .iter()
                .map(|v| f64::from(*v))
                .sum::<f64>()
                / 64.;
            assert!((average - f64::from(level)).abs() < 6.);
        }
    }
}
