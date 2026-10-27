//! Native YUV planes enter bounded queues; encoding and RTP stay on the shared
//! media worker. Camera and screen have independent queues and mute epochs.
use crate::{
    video::{VideoCodec, VideoFrame, VideoLayout},
    video_playout::VideoPlayout,
};
use crossbeam_queue::ArrayQueue;
use lingxi_webrtc_core::{RtcSession, VideoSource};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// A visible plane from CVPixelBuffer or Android YUV_420_888. Pixel stride may
/// be two for interleaved NV12 chroma. Padding is never copied into the frame.
pub struct Plane<'a> {
    pub bytes: &'a [u8],
    pub row_stride: usize,
    pub pixel_stride: usize,
}
/// Screenshot buffers use RGBA/BGRA; conversion stays shared across platforms.
pub fn pack_rgba(
    width: u32,
    height: u32,
    plane: Plane<'_>,
    bgra: bool,
) -> Result<VideoFrame, String> {
    crate::rgba::pack(width, height, plane, bgra)
}

pub fn pack_yuv420(width: u32, height: u32, planes: [Plane<'_>; 3]) -> Result<VideoFrame, String> {
    let layout = VideoLayout::i420(width, height)?;
    let dimensions = [
        (width as usize, height as usize),
        (width.div_ceil(2) as usize, height.div_ceil(2) as usize),
        (width.div_ceil(2) as usize, height.div_ceil(2) as usize),
    ];
    // Validate every foreign plane before allocating or reading any pixels.
    for (plane, &(columns, rows)) in planes.iter().zip(&dimensions) {
        let visible = (columns - 1)
            .checked_mul(plane.pixel_stride)
            .and_then(|v| v.checked_add(1))
            .ok_or("video plane overflow")?;
        let needed = (rows - 1)
            .checked_mul(plane.row_stride)
            .and_then(|v| v.checked_add(visible))
            .ok_or("video plane overflow")?;
        if plane.pixel_stride == 0 || plane.row_stride < visible || needed > plane.bytes.len() {
            return Err("invalid video plane bounds".into());
        }
    }
    let mut pixels = vec![0; layout.size];
    for (index, (plane, &(columns, rows))) in planes.into_iter().zip(&dimensions).enumerate() {
        for row in 0..rows {
            let offset = layout.offsets[index] + row * layout.strides[index] as usize;
            let source = row * plane.row_stride;
            if plane.pixel_stride == 1 {
                // Luma and planar chroma are contiguous even when rows have
                // padding. Copy visible bytes in bulk instead of per pixel.
                pixels[offset..offset + columns]
                    .copy_from_slice(&plane.bytes[source..source + columns]);
            } else {
                for column in 0..columns {
                    pixels[offset + column] = plane.bytes[source + column * plane.pixel_stride];
                }
            }
        }
    }
    Ok(VideoFrame {
        layout,
        pixels,
        pts: Duration::ZERO,
    })
}
struct QueuedFrame {
    frame: VideoFrame,
    at: Instant,
    epoch: u64,
}
struct Queue {
    frames: ArrayQueue<QueuedFrame>,
    epoch: AtomicU64,
}
pub struct VideoCapturePort {
    queue: Arc<Queue>,
}
impl VideoCapturePort {
    /// Invoke on the device capture queue; never waits for encoding/network.
    pub fn submit(&self, frame: VideoFrame, at: Instant) -> Result<(), &'static str> {
        let epoch = self.queue.epoch.load(Ordering::Acquire);
        if epoch % 2 == 1 {
            return Ok(());
        }
        let now = Instant::now();
        if at > now || now.duration_since(at) > Duration::from_secs(1) {
            return Err("invalid video capture time");
        }
        let layout = VideoLayout::i420(frame.layout.width, frame.layout.height)
            .map_err(|_| "invalid video layout")?;
        if frame.pixels.len() != layout.size
            || frame.layout.offsets != layout.offsets
            || frame.layout.strides != layout.strides
        {
            return Err("video capture must use the declared I420 layout");
        }
        self.queue
            .frames
            .force_push(QueuedFrame { frame, at, epoch });
        Ok(())
    }
}
struct Sender {
    queue: Arc<Queue>,
    codec: Option<VideoCodec>,
    source: VideoSource,
    origin: Option<Instant>,
    previous: Option<Instant>,
    epoch: u64,
    encoded: u64,
}
impl Sender {
    fn new(source: VideoSource) -> (Self, VideoCapturePort) {
        let queue = Arc::new(Queue {
            frames: ArrayQueue::new(2),
            epoch: AtomicU64::new(1),
        });
        (
            Self {
                queue: queue.clone(),
                codec: None,
                source,
                origin: None,
                previous: None,
                epoch: 1,
                encoded: 0,
            },
            VideoCapturePort { queue },
        )
    }
    fn enable(&self, enabled: bool) -> Result<(), &'static str> {
        let epoch = self.queue.epoch.load(Ordering::Acquire);
        if epoch.is_multiple_of(2) != enabled {
            self.queue.epoch.store(
                epoch.checked_add(1).ok_or("video epoch exhausted")?,
                Ordering::Release,
            );
        }
        Ok(())
    }
    async fn service(&mut self, session: &RtcSession) -> Result<(), String> {
        let epoch = self.queue.epoch.load(Ordering::Acquire);
        if epoch != self.epoch {
            self.codec = None;
            self.origin = None;
            self.previous = None;
            self.epoch = epoch;
        }
        // Prefer the newest captured frame after a scheduling stall.
        let mut newest = None;
        for _ in 0..2 {
            if let Some(frame) = self.queue.frames.pop() {
                newest = Some(frame);
            }
        }
        let Some(queued) = newest else {
            return Ok(());
        };
        if epoch % 2 == 1
            || queued.epoch != epoch
            || queued.at.elapsed() > Duration::from_secs(1)
            || self.previous.is_some_and(|last| queued.at <= last)
        {
            return Ok(());
        }
        let layout = &queued.frame.layout;
        if self
            .codec
            .as_ref()
            .is_none_or(|c| c.layout().width != layout.width || c.layout().height != layout.height)
        {
            self.codec = Some(VideoCodec::encoder(
                layout.width,
                layout.height,
                30,
                2_000_000,
            )?);
            self.origin = None;
            self.previous = None;
        }
        let origin = *self.origin.get_or_insert(queued.at);
        let duration = self
            .previous
            .map(|last| queued.at.duration_since(last).min(Duration::from_secs(1)))
            .unwrap_or(Duration::from_nanos(1_000_000_000 / 30));
        let encoded = self.codec.as_mut().unwrap().process(
            queued.frame.pixels,
            queued.at.duration_since(origin),
            duration,
        )?;
        self.previous = Some(queued.at);
        session
            .send_video(self.source, encoded, duration)
            .await
            .map_err(str::to_owned)?;
        self.encoded = self.encoded.saturating_add(1);
        Ok(())
    }
}
pub struct RemoteVideoFrame {
    pub ssrc: u32,
    /// Monotonic receive timestamp derived from the decoder's RTP ingress
    /// anchor and the decoded frame PTS.
    pub at: Instant,
    pub frame: VideoFrame,
}
pub struct NativeVideo {
    camera: Sender,
    screen: Sender,
    receivers: HashMap<u32, VideoPlayout>,
    receiving: bool,
}
impl NativeVideo {
    pub fn encoded_frames(&self) -> (u64, u64) {
        (self.camera.encoded, self.screen.encoded)
    }

    pub fn new() -> (Self, VideoCapturePort, VideoCapturePort) {
        let (camera, camera_port) = Sender::new(VideoSource::Camera);
        let (screen, screen_port) = Sender::new(VideoSource::Screen);
        (
            Self {
                camera,
                screen,
                receivers: HashMap::new(),
                receiving: false,
            },
            camera_port,
            screen_port,
        )
    }
    pub fn controls(
        &mut self,
        camera: bool,
        screen: bool,
        receiving: bool,
    ) -> Result<(), &'static str> {
        self.camera.enable(camera)?;
        self.screen.enable(screen)?;
        if self.receiving != receiving {
            self.receivers.clear();
        }
        self.receiving = receiving;
        Ok(())
    }
    /// Encode and send queued camera/screen frames without touching the
    /// session's inbound video path. Use this for a SendOnly RTC session.
    pub async fn service_sending(&mut self, session: &mut RtcSession) -> Result<(), String> {
        self.camera.service(session).await?;
        self.screen.service(session).await?;
        Ok(())
    }
    /// Worker-only: native capture/render callbacks never enter this method.
    pub async fn service(
        &mut self,
        session: &mut RtcSession,
    ) -> Result<Vec<RemoteVideoFrame>, String> {
        self.service_sending(session).await?;
        session
            .set_video_suppressed(!self.receiving)
            .map_err(str::to_owned)?;
        for _ in 0..64 {
            let Some(packet) = session.take_video_packet().map_err(str::to_owned)? else {
                break;
            };
            let bytes = packet.as_ref();
            let ssrc = u32::from_be_bytes(
                bytes
                    .get(8..12)
                    .ok_or("invalid video RTP header")?
                    .try_into()
                    .unwrap(),
            );
            if !self.receivers.contains_key(&ssrc) {
                if self.receivers.len() >= 2 {
                    return Err("too many video sources".into());
                }
                self.receivers
                    .insert(ssrc, VideoPlayout::new_at(packet.at)?);
            }
            self.receivers[&ssrc].push(packet)?;
        }
        let mut frames = Vec::with_capacity(4);
        for (&ssrc, decoder) in &self.receivers {
            for _ in 0..2 {
                let Some(frame) = decoder.take_frame()? else {
                    break;
                };
                let at = decoder.frame_time(frame.pts).map_err(str::to_owned)?;
                frames.push(RemoteVideoFrame { ssrc, at, frame });
            }
        }
        Ok(frames)
    }
}
impl Drop for NativeVideo {
    fn drop(&mut self) {
        // Retire outstanding device callback handles before releasing pipelines.
        self.camera.queue.epoch.store(u64::MAX, Ordering::Release);
        self.screen.queue.epoch.store(u64::MAX, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rgba_and_bgra_use_the_same_limited_range_yuv_with_padding_and_odd_edges() {
        crate::runtime::test_init();
        let rgba = [
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 99, 99, 99, 99,
        ];
        let bgra = [0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255];
        let a = pack_rgba(
            3,
            1,
            Plane {
                bytes: &rgba,
                row_stride: 16,
                pixel_stride: 4,
            },
            false,
        )
        .unwrap();
        let b = pack_rgba(
            3,
            1,
            Plane {
                bytes: &bgra,
                row_stride: 12,
                pixel_stride: 4,
            },
            true,
        )
        .unwrap();
        assert_eq!(a.pixels, b.pixels);
        // Independent BT.601 limited-range expectations allow one quantization
        // step between integer implementations of the same color matrix.
        for (&actual, expected) in a.pixels[..3].iter().zip([82i16, 144, 41]) {
            assert!((i16::from(actual) - expected).abs() <= 1);
        }
        assert!(
            pack_rgba(
                3,
                1,
                Plane {
                    bytes: &rgba[..11],
                    row_stride: 12,
                    pixel_stride: 4
                },
                false
            )
            .is_err()
        );
    }
    #[test]
    fn padded_interleaved_chroma_and_odd_dimensions_copy_only_visible_pixels() {
        crate::runtime::test_init();
        let y = [10, 11, 12, 99, 13, 14, 15];
        let uv = [20, 30, 21, 31];
        let frame = pack_yuv420(
            3,
            2,
            [
                Plane {
                    bytes: &y,
                    row_stride: 4,
                    pixel_stride: 1,
                },
                Plane {
                    bytes: &uv,
                    row_stride: 4,
                    pixel_stride: 2,
                },
                Plane {
                    bytes: &uv[1..],
                    row_stride: 4,
                    pixel_stride: 2,
                },
            ],
        )
        .unwrap();
        assert_eq!(&frame.pixels[..3], &[10, 11, 12]);
        assert_eq!(
            &frame.pixels[frame.layout.strides[0] as usize..][..3],
            &[13, 14, 15]
        );
        assert_eq!(&frame.pixels[frame.layout.offsets[1]..][..2], &[20, 21]);
        assert_eq!(&frame.pixels[frame.layout.offsets[2]..][..2], &[30, 31]);
        assert!(
            pack_yuv420(
                3,
                2,
                [
                    Plane {
                        bytes: &y[..6],
                        row_stride: 4,
                        pixel_stride: 1
                    },
                    Plane {
                        bytes: &uv,
                        row_stride: 4,
                        pixel_stride: 2
                    },
                    Plane {
                        bytes: &uv[1..],
                        row_stride: 4,
                        pixel_stride: 2
                    }
                ]
            )
            .is_err()
        );
    }
    #[test]
    fn video_callbacks_are_bounded_and_retired_after_owner_drop() {
        crate::runtime::test_init();
        let (mut native, camera, _) = NativeVideo::new();
        native.controls(true, false, false).unwrap();
        for _ in 0..10 {
            let layout = VideoLayout::i420(4, 4).unwrap();
            camera
                .submit(
                    VideoFrame {
                        pixels: vec![128; layout.size],
                        layout,
                        pts: Duration::ZERO,
                    },
                    Instant::now(),
                )
                .unwrap();
        }
        assert_eq!(camera.queue.frames.len(), 2);
        drop(native);
        assert_eq!(camera.queue.epoch.load(Ordering::Acquire), u64::MAX);
    }

    #[tokio::test]
    async fn send_only_service_does_not_require_an_inbound_video_track() {
        let mut session = RtcSession::with_config(lingxi_webrtc_core::SessionConfig {
            audio: false,
            video: true,
            video_direction: lingxi_webrtc_core::VideoDirection::SendOnly,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(session.set_video_suppressed(true).is_err());

        let (mut native, _, _) = NativeVideo::new();
        native.controls(false, false, false).unwrap();
        native.service_sending(&mut session).await.unwrap();
        session.close().await.unwrap();
    }
}
