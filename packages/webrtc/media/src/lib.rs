//! Shared Codex-derived resampling, echo/noise processing and Opus encoding.
//! Platform adapters provide raw PCM and the actual playback reference/timing.
use std::time::{Duration, Instant};
const DEVICE_SERVICE_INTERVAL: Duration = Duration::from_millis(5);
#[path = "device_buffers.rs"]
mod buffers;
mod playback;
mod processing;
mod audio_track {
    pub const OPUS_PAYLOAD_TYPE: u8 = 111;
    pub const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);
}
#[derive(Debug, Default, PartialEq)]
pub struct AudioState {
    pub microphone_peak: u16,
    pub speaker_peak: u16,
}
#[derive(Debug)]
pub struct EncodedAudio {
    pub data: Vec<u8>,
    pub at: Instant,
}

/// One media worker owns this processor; device callbacks only write bounded queues.
pub struct AudioProcessor {
    inner: processing::Processor,
}
impl AudioProcessor {
    pub fn new(capture_rate: u32, render_rate: u32) -> Result<Self, &'static str> {
        if !(8000..=192000).contains(&capture_rate) || !(8000..=192000).contains(&render_rate) {
            return Err("invalid audio sample rate");
        }
        Ok(Self {
            inner: processing::Processor::new(capture_rate, render_rate)?,
        })
    }
    /// Create a processor for an already-admitted first capture. Its first
    /// sample may precede processor construction, for example across IPC.
    /// Later resets continue to discard capture from before the reset.
    pub fn new_with_capture_start(
        capture_rate: u32,
        render_rate: u32,
        first_sample_at: Instant,
    ) -> Result<Self, &'static str> {
        let mut processor = Self::new(capture_rate, render_rate)?;
        processor.inner.set_initial_capture_cutoff(first_sample_at);
        Ok(processor)
    }

    pub fn reset(&mut self) -> Result<(), &'static str> {
        self.inner.reset()
    }
    pub fn set_render_rate(&mut self, rate: u32) -> Result<(), &'static str> {
        if !(8000..=192000).contains(&rate) {
            return Err("invalid audio sample rate");
        }
        self.inner.set_render_rate(rate)
    }
    /// Validate device callback latency before opening capture/playback.
    pub fn validate_callback_timing(
        &self,
        capture_frames: u32,
        render_frames: u32,
    ) -> Result<(), &'static str> {
        self.inner
            .validate_callback_timing(capture_frames, render_frames)
    }
    /// Up to 256 mono samples with the first sample's monotonic capture timestamp.
    pub fn capture(&mut self, pcm: &[f32], at: Instant) -> Result<Vec<EncodedAudio>, &'static str> {
        let frame = frame(pcm, at)?;
        self.inner.capture(&frame, Instant::now)
    }
    /// Feed samples actually rendered, with the device's predicted playback time.
    pub fn rendered(&mut self, pcm: &[f32], at: Instant) -> Result<(), &'static str> {
        self.inner.render(&frame(pcm, at)?)
    }
}
fn frame(pcm: &[f32], at: Instant) -> Result<buffers::Frame, &'static str> {
    if pcm.is_empty() || pcm.len() > buffers::BLOCK || pcm.iter().any(|v| !v.is_finite()) {
        return Err("invalid PCM frame");
    }
    let mut frame = buffers::Frame {
        samples: [0.; buffers::BLOCK],
        len: pcm.len(),
        at,
        generation: 0,
    };
    frame.samples[..pcm.len()].copy_from_slice(pcm);
    Ok(frame)
}

const MAX_CAPTURE_AGE: Duration = Duration::from_secs(1);
#[derive(Clone, Copy, Debug)]
pub enum AudioDeviceKind {
    Input,
    Output,
}
#[derive(Clone, Debug, Default)]
pub struct AudioDeviceSelection {
    pub microphone: Option<String>,
    pub speaker: Option<String>,
    pub channel: Option<Vec<std::num::NonZeroU16>>,
}
#[derive(Clone, Debug)]
pub struct AudioDevice {
    pub name: String,
    pub channels: u16,
    pub is_default: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct AudioControls {
    pub microphone_muted: bool,
    pub speaker_suppressed: bool,
}
#[cfg(feature = "gstreamer-media")]
mod audio_sink;
#[cfg(feature = "gstreamer-media")]
mod capture_worker;
#[cfg(feature = "desktop-devices")]
pub mod devices;
#[cfg(feature = "desktop-devices")]
mod input_channel;
#[cfg(feature = "gstreamer-media")]
mod playout;
#[cfg(feature = "gstreamer-media")]
mod service_failure;

#[cfg(feature = "gstreamer-media")]
pub mod ports;

#[cfg(feature = "gstreamer-media")]
pub mod runtime;

#[cfg(feature = "gstreamer-media")]
pub mod video;

#[cfg(feature = "gstreamer-media")]
pub mod video_playout;

#[cfg(feature = "gstreamer-media")]
mod rgba;

#[cfg(feature = "gstreamer-media")]
pub mod video_ports;

#[cfg(feature = "diagnostics")]
pub mod diagnostics;

/// Static plugin builds have no dynamic plugin directory requirement.
#[cfg(feature = "gstreamer-media")]
pub const fn codec_uses_static_plugins() -> bool {
    cfg!(feature = "static-plugins")
}

/// Resolve the host-selected dynamic plugin directory, if one is required.
#[cfg(feature = "gstreamer-media")]
pub fn configured_codec_plugin_directory() -> Result<Option<std::path::PathBuf>, &'static str> {
    if codec_uses_static_plugins() {
        return Ok(None);
    }
    match std::env::var_os("LINGXI_GSTREAMER_PLUGIN_DIR") {
        Some(value) if !value.is_empty() => Ok(Some(value.into())),
        Some(_) => Err("configured media plugin directory is empty"),
        None => {
            let path = env!("LINGXI_DEV_GST_PLUGIN_DIR");
            if path.is_empty() {
                Err("the host must configure its media plugin directory")
            } else {
                Ok(Some(path.into()))
            }
        }
    }
}

/// Resolve a host SDK library directory separately from plugin registration.
/// Even static-plugin builds may link the shared GStreamer core dynamically.
#[cfg(feature = "gstreamer-media")]
pub fn configured_codec_loader_directory() -> Option<std::path::PathBuf> {
    let plugin_dir = std::env::var_os("LINGXI_GSTREAMER_PLUGIN_DIR")
        .filter(|path| !path.is_empty())
        .or_else(|| {
            let path = env!("LINGXI_DEV_GST_PLUGIN_DIR");
            (!path.is_empty()).then(|| path.into())
        })?;
    let plugin_dir = std::path::PathBuf::from(plugin_dir);
    #[cfg(windows)]
    let loader = plugin_dir.parent()?.parent()?.join("bin");
    #[cfg(not(windows))]
    let loader = plugin_dir.parent()?.to_path_buf();
    loader.is_dir().then_some(loader)
}
