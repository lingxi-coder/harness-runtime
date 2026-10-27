use rtc::{
    media::Sample,
    rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
        RtpCodecKind,
    },
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use webrtc::{
    media_stream::{MediaStreamTrack, track_local::static_sample::TrackLocalStaticSample},
    peer_connection::MediaEngine,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoSource {
    Camera,
    Screen,
}
pub(crate) struct VideoTrack {
    pub track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
}
impl VideoTrack {
    pub fn new(
        media: &mut MediaEngine,
        source: VideoSource,
        now: Instant,
    ) -> Result<Self, &'static str> {
        let codec = video_codec();
        Self::register_codec(media)?;
        let ssrc = rand::random();
        let label = match source {
            VideoSource::Camera => "camera",
            VideoSource::Screen => "screen",
        };
        let track = TrackLocalStaticSample::new(
            now,
            MediaStreamTrack::new(
                "lingxi".into(),
                format!("{label}-{ssrc}"),
                label.into(),
                RtpCodecKind::Video,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec,
                    active: true,
                    ..Default::default()
                }],
            ),
        )
        .map_err(|_| "video track creation failed")?;
        Ok(Self {
            track: Arc::new(track),
            ssrc,
        })
    }

    pub fn register_codec(media: &mut MediaEngine) -> Result<(), &'static str> {
        let codec = video_codec();
        media
            .register_codec(
                RTCRtpCodecParameters {
                    rtp_codec: codec,
                    payload_type: 96,
                },
                RtpCodecKind::Video,
            )
            .map_err(|_| "video codec registration failed")
    }

    pub async fn send(&self, data: Vec<u8>, duration: Duration) -> Result<(), &'static str> {
        if data.is_empty()
            || data.len() > 4 * 1024 * 1024
            || duration.is_zero()
            || duration > Duration::from_secs(1)
        {
            return Err("invalid encoded video frame");
        }
        tokio::time::timeout(
            Duration::from_millis(100),
            self.track.write_sample(
                self.ssrc,
                96,
                &Sample {
                    data: data.into(),
                    duration,
                    ..Sample::new(Instant::now())
                },
                &[],
            ),
        )
        .await
        .map_err(|_| "video send timed out")?
        .map_err(|_| "video send failed")
    }
}

fn video_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: "video/VP8".into(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: String::new(),
        rtcp_feedback: vec![],
    }
}
