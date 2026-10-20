//! Shared VP8 codec. Native camera/screen adapters supply packed I420 according
//! to the returned layout; platform code does not own a second WebRTC engine.
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::{AppSink, AppSrc};
use gstreamer_video::VideoInfo;
use std::time::Duration;
#[derive(Clone, Debug)]
pub struct VideoLayout {
    pub width: u32,
    pub height: u32,
    pub size: usize,
    pub offsets: Vec<usize>,
    pub strides: Vec<i32>,
}
impl VideoLayout {
    pub fn i420(width: u32, height: u32) -> Result<Self, String> {
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return Err("invalid video dimensions".into());
        }
        let info = VideoInfo::builder(gstreamer_video::VideoFormat::I420, width, height)
            .build()
            .map_err(|_| "invalid I420 layout")?;
        if info.size() > 4 * 1024 * 1024 {
            return Err("video frame exceeds memory budget".into());
        }
        Ok(Self {
            width,
            height,
            size: info.size(),
            offsets: info.offset().to_vec(),
            strides: info.stride().to_vec(),
        })
    }
}
#[derive(Debug)]
pub struct VideoFrame {
    pub layout: VideoLayout,
    pub pixels: Vec<u8>,
    pub pts: Duration,
}
pub struct VideoCodec {
    pipeline: gst::Pipeline,
    source: AppSrc,
    sink: AppSink,
    layout: VideoLayout,
    encoding: bool,
    last_pts: Option<Duration>,
}
impl VideoCodec {
    pub fn encoder(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self, String> {
        if !(1..=60).contains(&fps) || !(64000..=20_000_000).contains(&bitrate) {
            return Err("invalid video encoding limits".into());
        }
        Self::build(width, height, Some((fps, bitrate)))
    }
    pub fn decoder(width: u32, height: u32) -> Result<Self, String> {
        Self::build(width, height, None)
    }
    fn build(width: u32, height: u32, encoding: Option<(u32, u32)>) -> Result<Self, String> {
        crate::runtime::ensure_initialized().map_err(str::to_owned)?;
        let layout = VideoLayout::i420(width, height)?;
        let raw = gst::Caps::builder("video/x-raw")
            .field("format", "I420")
            .field("width", width as i32)
            .field("height", height as i32)
            .build();
        let caps = if let Some((fps, _)) = encoding {
            let mut caps = raw.clone();
            caps.make_mut()
                .structure_mut(0)
                .unwrap()
                .set("framerate", gst::Fraction::new(fps as i32, 1));
            caps
        } else {
            gst::Caps::builder("video/x-vp8").build()
        };
        let source = AppSrc::builder()
            .is_live(true)
            .format(gst::Format::Time)
            .block(false)
            .max_buffers(2)
            .max_bytes((layout.size * 2) as u64)
            .caps(&caps)
            .build();
        let sink = AppSink::builder().sync(false).max_buffers(2).build();
        let codec = if let Some((fps, bitrate)) = encoding {
            gst::ElementFactory::make("vp8enc")
                .property("deadline", 1i64)
                .property("cpu-used", 8i32)
                .property("threads", 2i32)
                .property("keyframe-max-dist", (fps * 2) as i32)
                .property("target-bitrate", bitrate as i32)
                .build()
        } else {
            gst::ElementFactory::make("vp8dec").build()
        }
        .map_err(|_| "VP8 codec unavailable")?;
        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([source.upcast_ref(), &codec, sink.upcast_ref()])
            .map_err(|_| "video pipeline assembly failed")?;
        if encoding.is_some() && width.max(height) > 1280 {
            // Keep display-sized screen capture from monopolizing the media
            // worker. Scaling and encoding remain identical across platforms.
            let longest = u64::from(width.max(height));
            let scaled_width = ((u64::from(width) * 1280 / longest).max(2) as u32) & !1;
            let scaled_height = ((u64::from(height) * 1280 / longest).max(2) as u32) & !1;
            let scale = gst::ElementFactory::make("videoscale")
                .build()
                .map_err(|_| "video scaler unavailable")?;
            let filter = gst::ElementFactory::make("capsfilter")
                .property(
                    "caps",
                    gst::Caps::builder("video/x-raw")
                        .field("format", "I420")
                        .field("width", scaled_width as i32)
                        .field("height", scaled_height as i32)
                        .build(),
                )
                .build()
                .map_err(|_| "video scale caps unavailable")?;
            pipeline
                .add_many([&scale, &filter])
                .map_err(|_| "video scale assembly failed")?;
            gst::Element::link_many([
                source.upcast_ref(),
                &scale,
                &filter,
                &codec,
                sink.upcast_ref(),
            ])
            .map_err(|_| "video scale link failed")?;
        } else {
            gst::Element::link_many([source.upcast_ref(), &codec, sink.upcast_ref()])
                .map_err(|_| "video pipeline link failed")?;
        }
        if encoding.is_none() {
            sink.set_caps(Some(&raw));
        }
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| "video pipeline start failed")?;
        Ok(Self {
            pipeline,
            source,
            sink,
            layout,
            encoding: encoding.is_some(),
            last_pts: None,
        })
    }
    pub fn layout(&self) -> &VideoLayout {
        &self.layout
    }
    /// Called on a media worker. At most two queued buffers, one-second decode deadline.
    pub fn process(
        &mut self,
        bytes: Vec<u8>,
        pts: Duration,
        duration: Duration,
    ) -> Result<Vec<u8>, String> {
        if bytes.is_empty()
            || bytes.len() > 4 * 1024 * 1024
            || (self.encoding && bytes.len() != self.layout.size)
            || duration.is_zero()
            || duration > Duration::from_secs(1)
            || self.last_pts.is_some_and(|last| pts <= last)
        {
            return Err("invalid video frame or timestamp".into());
        }
        if self.source.current_level_buffers() >= 2 {
            return Err("video queue saturated".into());
        }
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        let buffer_ref = buffer.get_mut().ok_or("video buffer ownership failed")?;
        buffer_ref.set_pts(gst::ClockTime::from_nseconds(
            u64::try_from(pts.as_nanos()).map_err(|_| "video timestamp overflow")?,
        ));
        buffer_ref.set_duration(gst::ClockTime::from_nseconds(duration.as_nanos() as u64));
        self.source
            .push_buffer(buffer)
            .map_err(|_| "video source rejected frame")?;
        self.last_pts = Some(pts);
        let sample = self
            .sink
            .try_pull_sample(gst::ClockTime::from_seconds(1))
            .ok_or("video codec timed out")?;
        let data = sample
            .buffer()
            .ok_or("video codec returned no buffer")?
            .map_readable()
            .map_err(|_| "video buffer mapping failed")?;
        if !self.encoding && data.len() != self.layout.size {
            return Err("decoded video layout changed".into());
        }
        Ok(data.as_slice().to_vec())
    }
}
impl Drop for VideoCodec {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_screen_frames_are_scaled_before_vp8_encoding() {
        crate::runtime::test_init();
        crate::runtime::load_video_plugins(std::path::Path::new(env!("LINGXI_DEV_GST_PLUGIN_DIR")))
            .unwrap();
        let mut encoder = VideoCodec::encoder(1536, 768, 30, 2_000_000).unwrap();
        let encoded = encoder
            .process(
                vec![128; encoder.layout().size],
                Duration::ZERO,
                Duration::from_nanos(1_000_000_000 / 30),
            )
            .unwrap();
        let mut decoder = VideoCodec::decoder(1280, 640).unwrap();
        let pixels = decoder
            .process(
                encoded,
                Duration::ZERO,
                Duration::from_nanos(1_000_000_000 / 30),
            )
            .unwrap();
        assert_eq!(pixels.len(), VideoLayout::i420(1280, 640).unwrap().size);
    }
    #[test]
    fn vp8_roundtrip_preserves_dimensions_timing_and_signal() {
        crate::runtime::test_init();
        crate::runtime::load_video_plugins(std::path::Path::new(env!("LINGXI_DEV_GST_PLUGIN_DIR")))
            .unwrap();
        let mut encoder = VideoCodec::encoder(64, 48, 15, 256000).unwrap();
        let mut decoder = VideoCodec::decoder(64, 48).unwrap();
        let layout = encoder.layout().clone();
        for index in 0..5u64 {
            let mut pixels = vec![128; layout.size];
            for row in 0..48usize {
                let start = layout.offsets[0] + row * layout.strides[0] as usize;
                pixels[start..start + 64].fill(80 + index as u8 * 10);
            }
            let pts = Duration::from_millis(index * 67);
            let duration = Duration::from_millis(67);
            let encoded = encoder.process(pixels, pts, duration).unwrap();
            let decoded = decoder.process(encoded, pts, duration).unwrap();
            assert_eq!(decoded.len(), layout.size);
            let average = decoded[..64].iter().map(|v| f64::from(*v)).sum::<f64>() / 64.;
            assert!((average - f64::from(80 + index as u8 * 10)).abs() < 6.);
        }
        assert!(
            encoder
                .process(
                    vec![0; layout.size],
                    Duration::ZERO,
                    Duration::from_millis(67)
                )
                .is_err()
        );
    }
}
