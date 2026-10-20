//! Reuse GStreamer's synchronous color converter rather than converting every
//! RGBA pixel twice in scalar Rust. Native callbacks retain no foreign memory.
use crate::{
    video::{VideoFrame, VideoLayout},
    video_ports::Plane,
};
use gstreamer as gst;
use gstreamer_video::{VideoConverter, VideoConverterConfig, VideoFrameRef, VideoInfo};
use std::{cell::RefCell, time::Duration};

struct Converter {
    input_info: VideoInfo,
    output_info: VideoInfo,
    converter: VideoConverter,
    input: gst::Buffer,
    output: gst::Buffer,
    bgra: bool,
}
thread_local! {
    // One bounded format cache per capture thread, replaced on size/format
    // changes and released with that thread. No cross-device lock or pipeline.
    static CONVERTER: RefCell<Option<Converter>> = const { RefCell::new(None) };
}
impl Converter {
    fn new(width: u32, height: u32, bgra: bool) -> Result<Self, String> {
        crate::runtime::ensure_initialized().map_err(str::to_owned)?;
        let format = if bgra {
            gstreamer_video::VideoFormat::Bgra
        } else {
            gstreamer_video::VideoFormat::Rgba
        };
        let input_info = VideoInfo::builder(format, width, height)
            .build()
            .map_err(|_| "invalid RGBA format")?;
        if input_info.size() > 16 * 1024 * 1024 {
            return Err("RGBA frame exceeds memory budget".into());
        }
        let color = "bt601".parse().map_err(|_| "invalid output colorimetry")?;
        let output_info = VideoInfo::builder(gstreamer_video::VideoFormat::I420, width, height)
            .colorimetry(&color)
            .build()
            .map_err(|_| "invalid I420 format")?;
        let mut config = VideoConverterConfig::new();
        config.set_threads(1);
        config.set_dither_method(gstreamer_video::VideoDitherMethod::None);
        let converter = VideoConverter::new(&input_info, &output_info, Some(config))
            .map_err(|_| "RGBA converter unavailable")?;
        let mut input =
            gst::Buffer::with_size(input_info.size()).map_err(|_| "RGBA allocation failed")?;
        let mut output =
            gst::Buffer::with_size(output_info.size()).map_err(|_| "I420 allocation failed")?;
        // Conversion may leave alignment padding untouched. Never export
        // allocator contents with the visible pixels of an owned frame.
        for buffer in [&mut input, &mut output] {
            buffer
                .get_mut()
                .ok_or("video buffer ownership failed")?
                .map_writable()
                .map_err(|_| "video buffer initialization failed")?
                .as_mut_slice()
                .fill(0);
        }
        Ok(Self {
            input_info,
            output_info,
            converter,
            input,
            output,
            bgra,
        })
    }
    fn convert(&mut self, layout: VideoLayout, plane: Plane<'_>) -> Result<VideoFrame, String> {
        {
            let buffer = self.input.get_mut().ok_or("RGBA buffer ownership failed")?;
            let mut mapped = buffer
                .map_writable()
                .map_err(|_| "RGBA buffer mapping failed")?;
            let width = layout.width as usize;
            for y in 0..layout.height as usize {
                let out = &mut mapped[y * self.input_info.stride()[0] as usize..][..width * 4];
                let source = y * plane.row_stride;
                if plane.pixel_stride == 4 {
                    out.copy_from_slice(&plane.bytes[source..source + width * 4]);
                } else {
                    for (x, pixel) in out.chunks_exact_mut(4).enumerate() {
                        let at = source + x * plane.pixel_stride;
                        pixel.copy_from_slice(&plane.bytes[at..at + 4]);
                    }
                }
            }
        }
        {
            let source = VideoFrameRef::from_buffer_ref_readable(&self.input, &self.input_info)
                .map_err(|_| "RGBA frame mapping failed")?;
            let output = self
                .output
                .get_mut()
                .ok_or("I420 buffer ownership failed")?;
            let mut destination =
                VideoFrameRef::from_buffer_ref_writable(output, &self.output_info)
                    .map_err(|_| "I420 frame mapping failed")?;
            self.converter.frame_ref(&source, &mut destination);
        }
        let pixels = self
            .output
            .map_readable()
            .map_err(|_| "I420 buffer mapping failed")?
            .as_slice()
            .to_vec();
        if pixels.len() != layout.size {
            return Err("I420 conversion layout changed".into());
        }
        Ok(VideoFrame {
            layout,
            pixels,
            pts: Duration::ZERO,
        })
    }
}
pub(crate) fn pack(
    width: u32,
    height: u32,
    plane: Plane<'_>,
    bgra: bool,
) -> Result<VideoFrame, String> {
    let layout = VideoLayout::i420(width, height)?;
    let visible = (width as usize - 1)
        .checked_mul(plane.pixel_stride)
        .and_then(|n| n.checked_add(4))
        .ok_or("RGBA plane overflow")?;
    let needed = (height as usize - 1)
        .checked_mul(plane.row_stride)
        .and_then(|n| n.checked_add(visible))
        .ok_or("RGBA plane overflow")?;
    if plane.pixel_stride < 4 || plane.row_stride < visible || needed > plane.bytes.len() {
        return Err("invalid RGBA plane bounds".into());
    }
    CONVERTER.with(|cache| {
        let mut cache = cache
            .try_borrow_mut()
            .map_err(|_| "recursive RGBA capture")?;
        if cache.as_ref().is_none_or(|c| {
            c.input_info.width() != width || c.input_info.height() != height || c.bgra != bgra
        }) {
            *cache = Some(Converter::new(width, height, bgra)?);
        }
        cache.as_mut().unwrap().convert(layout, plane)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn padded_rgba_and_bgra_preserve_bt601_color_and_owned_frames_across_reconfiguration() {
        crate::runtime::test_init();
        let mut retained = None;
        for (rgb, expected) in [
            ([255, 0, 0], [81i16, 90, 240]),
            ([0, 255, 0], [145, 54, 34]),
            ([0, 0, 255], [41, 240, 110]),
            ([255, 255, 255], [235, 128, 128]),
            ([0, 0, 0], [16, 128, 128]),
        ] {
            for bgra in [false, true] {
                let mut bytes = [212u8; 84];
                for y in 0..3 {
                    for x in 0..3 {
                        let at = y * 28 + x * 8;
                        let rgb = if bgra { [rgb[2], rgb[1], rgb[0]] } else { rgb };
                        bytes[at..at + 3].copy_from_slice(&rgb);
                        bytes[at + 3] = 255;
                    }
                }
                let frame = pack(
                    3,
                    3,
                    Plane {
                        bytes: &bytes,
                        row_stride: 28,
                        pixel_stride: 8,
                    },
                    bgra,
                )
                .unwrap();
                for (plane, expected) in expected.into_iter().enumerate() {
                    let value = i16::from(frame.pixels[frame.layout.offsets[plane]]);
                    assert!(
                        (value - expected).abs() <= 2,
                        "plane {plane}: {value} vs {expected}"
                    );
                }
                // No padding from either the external plane or the allocator.
                assert_eq!(frame.pixels[3], 0);
                if retained.is_none() {
                    retained = Some(frame);
                }
                assert!(
                    pack(
                        3,
                        3,
                        Plane {
                            bytes: &bytes[..75],
                            row_stride: 28,
                            pixel_stride: 8
                        },
                        bgra
                    )
                    .is_err()
                );
            }
        }
        let bytes = [0u8; 20];
        pack(
            5,
            1,
            Plane {
                bytes: &bytes,
                row_stride: 20,
                pixel_stride: 4,
            },
            false,
        )
        .unwrap();
        let first = retained.unwrap();
        assert!((i16::from(first.pixels[0]) - 81).abs() <= 2);
        assert_eq!(first.layout.width, 3);
    }
}
