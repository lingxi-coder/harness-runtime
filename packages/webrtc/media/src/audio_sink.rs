//! Forward decoded PCM to an epoch-owned device queue or bounded host-readable queue.
//! Device playback is paced by its native callback; decoded host frames retain PTS.
//! Unexpected device resets fail the current output path closed.

use super::playback::PlaybackWriter;
use crate::ports::PcmOutput;
use audio::gst_base::subclass::prelude::*;
use audio::prelude::*;
use gst::glib;
use gstreamer as gst;
use gstreamer_audio as audio;
use std::sync::OnceLock;

mod imp {
    use super::*;
    #[derive(Default)]
    pub struct Sink {
        pub(super) target: OnceLock<SinkTarget>,
    }
    #[glib::object_subclass]
    impl ObjectSubclass for Sink {
        const NAME: &'static str = "CodexPrivateAudioSink";
        type Type = super::Sink;
        type ParentType = audio::gst_base::BaseSink;
    }
    impl ObjectImpl for Sink {}
    impl GstObjectImpl for Sink {}
    impl ElementImpl for Sink {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: OnceLock<gst::subclass::ElementMetadata> = OnceLock::new();
            Some(META.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "Private audio",
                    "Sink/Audio",
                    "Owned speaker output",
                    "OpenAI",
                )
            }))
        }
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PADS: OnceLock<Vec<gst::PadTemplate>> = OnceLock::new();
            PADS.get_or_init(|| {
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &gst::Caps::builder("audio/x-raw")
                        .field("format", "F32LE")
                        .field("layout", "interleaved")
                        .field("channels", 1i32)
                        .field(
                            "rate",
                            gst::IntRange::<i32>::new(/*min*/ 8000, /*max*/ 384000),
                        )
                        .build(),
                )
                .into_iter()
                .collect()
            })
        }
    }
    impl BaseSinkImpl for Sink {
        fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
            let target = self
                .target
                .get()
                .ok_or_else(|| gst::loggable_error!(gst::CAT_RUST, "speaker not bound"))?;
            let info = audio::AudioInfo::from_caps(caps)
                .map_err(|_| gst::loggable_error!(gst::CAT_RUST, "invalid speaker caps"))?;
            if info.rate() != target.rate()
                || info.channels() != 1
                || info.format() != audio::AudioFormat::F32le
                || info.layout() != audio::AudioLayout::Interleaved
            {
                return Err(gst::loggable_error!(
                    gst::CAT_RUST,
                    "unsupported speaker format"
                ));
            }
            Ok(())
        }
        fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            let target = self.target.get().ok_or(gst::FlowError::Error)?;
            let result = match target {
                SinkTarget::Playback(writer) => (|| {
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let mut bytes = map.as_slice();
                    while !bytes.is_empty() {
                        let written = writer.write(bytes).map_err(|_| gst::FlowError::Error)?;
                        bytes = &bytes[written..];
                    }
                    Ok(gst::FlowSuccess::Ok)
                })(),
                SinkTarget::Pcm(output) => {
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    output
                        .push(map.as_slice(), buffer.pts())
                        .map(|()| gst::FlowSuccess::Ok)
                        .map_err(|_| gst::FlowError::Error)
                }
            };
            if result.is_err() {
                target.fail();
            }
            result
        }
        fn unlock(&self) -> Result<(), gst::ErrorMessage> {
            // Flushes cancel blocked writes. Device writers are epoch-bound, and
            // each decoded PCM queue belongs to one playout instance.
            if let Some(target) = self.target.get() {
                target.fail();
            }
            self.parent_unlock()
        }
    }
}

glib::wrapper! {
    pub struct Sink(ObjectSubclass<imp::Sink>) @extends audio::gst_base::BaseSink, gst::Element, gst::Object;
}

impl Sink {
    pub(super) fn new_playback(writer: PlaybackWriter) -> Self {
        let sink: Self = glib::Object::new();
        assert!(sink.imp().target.set(SinkTarget::Playback(writer)).is_ok());
        sink.set_sync(false);
        sink.set_async(false);
        sink.set_enable_last_sample(false);
        sink
    }

    pub(super) fn new_pcm(output: PcmOutput) -> Self {
        let sink: Self = glib::Object::new();
        assert!(sink.imp().target.set(SinkTarget::Pcm(output)).is_ok());
        sink.set_sync(false);
        sink.set_async(false);
        sink.set_enable_last_sample(false);
        sink
    }
}

enum SinkTarget {
    Playback(PlaybackWriter),
    Pcm(PcmOutput),
}

impl SinkTarget {
    fn rate(&self) -> u32 {
        match self {
            Self::Playback(writer) => writer.rate(),
            Self::Pcm(output) => output.rate(),
        }
    }

    fn fail(&self) {
        match self {
            Self::Playback(writer) => writer.fail_if_current(),
            Self::Pcm(output) => output.fail(),
        }
    }
}

#[cfg(test)]
#[path = "audio_sink_tests.rs"]
mod tests;
