//! Embedded peer with bounded, lossless-or-closed data delivery.
//! The host owns the executor; dropping a peer schedules connection teardown.
//! Candidate admission is bounded before an answer can mutate the peer or start ICE work.
//! The offerer creates the event channel; the answerer admits exactly that channel.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::Notify;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use webrtc::data_channel::DataChannel;
use webrtc::data_channel::DataChannelEvent;
use webrtc::peer_connection::PeerConnection;
use webrtc::peer_connection::PeerConnectionBuilder;
use webrtc::peer_connection::PeerConnectionEventHandler;
use webrtc::peer_connection::RTCIceCandidateInit;
use webrtc::peer_connection::RTCIceGatheringState;
use webrtc::peer_connection::RTCPeerConnectionState;
use webrtc::peer_connection::RTCSessionDescription;

const WAIT: Duration = Duration::from_secs(/*secs*/ 15);
const MAX_REMOTE_CANDIDATES: usize = 32;
// The mobile video SDP repeats interface candidates across audio, camera,
// screen and DataChannel media sections. Count every occurrence (including
// duplicates), but allow a bounded four-section video session.
const MAX_VIDEO_REMOTE_CANDIDATES: usize = 4 * MAX_REMOTE_CANDIDATES;
pub type Result<T> = std::result::Result<T, &'static str>;

struct Events(
    Arc<Notify>,
    Option<tokio::sync::mpsc::Sender<Arc<dyn DataChannel>>>,
    AtomicBool,
    watch::Sender<RTCPeerConnectionState>,
);

// The upstream trait requires async-trait's boxed-future ABI.
impl PeerConnectionEventHandler for Events {
    fn on_data_channel<'a, 'async_trait>(
        &'a self,
        channel: Arc<dyn DataChannel>,
    ) -> BoxFuture<'async_trait, ()>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if let Some(sender) = &self.1 {
                if channel.label().await.as_deref() == Ok("lingxi-agent")
                    && channel.ordered().await == Ok(true)
                    && !self.2.swap(true, Ordering::AcqRel)
                    && sender.try_send(channel.clone()).is_ok()
                {
                    return;
                }
            }
            let _ = channel.close().await;
        })
    }

    fn on_ice_gathering_state_change<'a, 'async_trait>(
        &'a self,
        state: RTCIceGatheringState,
    ) -> BoxFuture<'async_trait, ()>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if state == RTCIceGatheringState::Complete {
                self.0.notify_one();
            }
        })
    }

    fn on_connection_state_change<'a, 'async_trait>(
        &'a self,
        state: RTCPeerConnectionState,
    ) -> BoxFuture<'async_trait, ()>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.3.send_replace(state);
        })
    }
}

#[derive(Debug, PartialEq)]
pub enum AnswerOutcome {
    Ready,
    TimedOut,
}

pub struct Transport {
    pub(crate) incoming: crate::incoming::Incoming,
    pub(crate) audio: Option<crate::audio_track::AudioTrack>,
    connection: Arc<dyn PeerConnection>,
    gathered: Arc<Notify>,
    observer: JoinHandle<()>,
    pub(crate) ready: watch::Receiver<bool>,
    channel: Arc<OnceLock<Arc<dyn DataChannel>>>,
    answerer: bool,
    messages: Option<tokio::sync::mpsc::Receiver<webrtc::data_channel::RTCDataChannelMessage>>,
    closed: bool,
    gather_timeout: Option<Duration>,
    data_only: bool,
    overflowed: Arc<AtomicBool>,
    state: watch::Receiver<RTCPeerConnectionState>,
    video_incoming: Option<crate::incoming::Incoming>,
    camera: Option<crate::video_track::VideoTrack>,
    screen: Option<crate::video_track::VideoTrack>,
}

impl Transport {
    fn remote_candidate_limit(&self) -> usize {
        if self.camera.is_some() {
            MAX_VIDEO_REMOTE_CANDIDATES
        } else {
            MAX_REMOTE_CANDIDATES
        }
    }
    pub async fn new() -> Result<Self> {
        Self::with_bind_addresses(vec!["0.0.0.0:0".into(), "[::]:0".into()]).await
    }

    /// Bind explicit UDP endpoints (e.g. a Docker-published port). TCP remains ephemeral.
    pub async fn with_bind_addresses(addresses: Vec<String>) -> Result<Self> {
        Self::with_config(crate::SessionConfig {
            bind_addresses: addresses,
            ..Default::default()
        })
        .await
    }

    #[cfg(test)]
    async fn with_runtime(runtime: Arc<dyn webrtc::runtime::Runtime>) -> Result<Self> {
        Self::build(runtime, crate::SessionConfig::default()).await
    }

    /// Configure STUN/TURN and role before creating sockets or negotiating.
    pub async fn with_config(config: crate::SessionConfig) -> Result<Self> {
        Self::build(
            Arc::new(crate::transport_runtime::VoiceRuntime::default()),
            config,
        )
        .await
    }

    async fn build(
        runtime: Arc<dyn webrtc::runtime::Runtime>,
        config: crate::SessionConfig,
    ) -> Result<Self> {
        config.validate()?;
        let crate::SessionConfig {
            bind_addresses: addresses,
            video,
            answerer,
            ice,
            data_only,
            sctp_send_buffer,
            sctp_receive_buffer,
            receive_queue,
            gather_timeout,
            mdns,
        } = config;
        let configuration = ice.configuration()?;
        let (mut media, audio) = if data_only {
            (webrtc::peer_connection::MediaEngine::default(), None)
        } else {
            let (media, audio) = crate::audio_track::AudioTrack::new()?;
            (media, Some(audio))
        };
        let (incoming, ingress, video_incoming) = if video {
            let (audio, ingress, video) = crate::incoming::Incoming::with_video();
            (audio, ingress, Some(video))
        } else {
            let (audio, ingress) = crate::incoming::Incoming::new();
            (audio, ingress, None)
        };
        let camera = if video {
            Some(crate::video_track::VideoTrack::new(
                &mut media,
                crate::VideoSource::Camera,
            )?)
        } else {
            None
        };
        let screen = if video {
            Some(crate::video_track::VideoTrack::new(
                &mut media,
                crate::VideoSource::Screen,
            )?)
        } else {
            None
        };
        let gathered = Arc::new(Notify::new());
        let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::channel(1);
        // Upstream defaults exhaust checks after 1.4s, including checks sent before
        // a TCP connection exists. Keep probing throughout our negotiation deadline.
        let check_interval = Duration::from_millis(/*millis*/ 200);
        let attempts = u16::try_from(WAIT.as_millis() / check_interval.as_millis())
            .map_err(|_| "invalid voice ICE attempt budget")?;
        let mut settings = webrtc::peer_connection::SettingEngine::default();
        settings.set_ice_connection_attempts(Some(check_interval), Some(attempts));
        // Joining the mDNS group can be refused (e.g. iOS without the multicast
        // entitlement); a data-only peer then connects without resolving `.local` names.
        let query_mdns = mdns == crate::MdnsMode::QueryOnly
            && (!data_only || rtc::mdns::MulticastSocket::new().into_std().is_ok());
        settings.set_multicast_dns_mode(if query_mdns {
            rtc::ice::mdns::MulticastDnsMode::QueryOnly
        } else {
            rtc::ice::mdns::MulticastDnsMode::Disabled
        });
        let (state_tx, state) = watch::channel(RTCPeerConnectionState::New);
        let connection: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(configuration)
                .with_interceptor_registry(rtc::interceptor::Registry::from(ingress))
                .with_media_engine(media)
                .with_runtime(runtime)
                .with_setting_engine(settings)
                .with_handler(Arc::new(Events(
                    gathered.clone(),
                    answerer.then_some(accepted_tx),
                    AtomicBool::new(false),
                    state_tx,
                )))
                .with_udp_addrs(addresses)
                .with_tcp_addrs(vec!["0.0.0.0:0".into(), "[::]:0".into()])
                .with_data_channel_send_buffer_limit(sctp_send_buffer)
                .with_sctp_receive_buffer_size(sctp_receive_buffer)
                .build()
                .await
                .map_err(|_| "failed to create voice peer")?,
        );
        if let Some(audio) = &audio
            && connection.add_track(audio.track.clone()).await.is_err()
        {
            let _ = timeout(Duration::from_secs(/*secs*/ 2), connection.close()).await;
            return Err("failed to attach voice track");
        }
        for track in [&camera, &screen].into_iter().flatten() {
            if connection.add_track(track.track.clone()).await.is_err() {
                let _ = connection.close().await;
                return Err("video track attachment failed");
            }
        }
        let channel = Arc::new(OnceLock::new());
        if !answerer {
            let created = match connection
                .create_data_channel("lingxi-agent", /*options*/ None)
                .await
            {
                Ok(channel) => channel,
                Err(_) => {
                    let _ = timeout(Duration::from_secs(/*secs*/ 2), connection.close()).await;
                    return Err("failed to create voice event channel");
                }
            };
            let _ = channel.set(created);
        }
        let (sender, ready) = watch::channel(false);
        let (message_tx, messages) = tokio::sync::mpsc::channel(receive_queue);
        let overflowed = Arc::new(AtomicBool::new(false));
        let overflow_flag = overflowed.clone();
        let mut peer_state = state.clone();
        let channel_slot = channel.clone();
        let observer = tokio::spawn(async move {
            let observed = if let Some(channel) = channel_slot.get() {
                channel.clone()
            } else {
                let Some(channel) = accepted_rx.recv().await else {
                    return;
                };
                let _ = channel_slot.set(channel.clone());
                channel
            };
            loop {
                // A data-only peer also ends when the connection itself fails or closes,
                // since the channel may never report it. Voice behaviour is unchanged.
                let event = tokio::select! {
                    event = observed.poll() => event,
                    _ = async {
                        if data_only {
                            let _ = peer_state
                                .wait_for(|s| {
                                    matches!(
                                        s,
                                        RTCPeerConnectionState::Failed
                                            | RTCPeerConnectionState::Closed
                                    )
                                })
                                .await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => None,
                };
                let Some(event) = event else { break };
                match event {
                    DataChannelEvent::OnOpen => {
                        sender.send_replace(true);
                    }
                    DataChannelEvent::OnError
                    | DataChannelEvent::OnClosing
                    | DataChannelEvent::OnClose => break,
                    DataChannelEvent::OnMessage(message) => {
                        if let Err(error) = message_tx.try_send(message) {
                            // Closing is observable; never silently lose an Agent message.
                            // The upstream driver also drops events once its own 256-event
                            // queue fills, so this loop must keep draining it: awaiting
                            // here would turn backpressure into silent loss. Senders pace
                            // themselves with protocol acknowledgements.
                            overflow_flag.store(
                                matches!(error, tokio::sync::mpsc::error::TrySendError::Full(_)),
                                Ordering::Release,
                            );
                            let _ = observed.close().await;
                            break;
                        }
                    }
                    DataChannelEvent::OnBufferedAmountLow
                    | DataChannelEvent::OnBufferedAmountHigh => {}
                }
            }
            sender.send_replace(false);
        });
        Ok(Self {
            incoming,
            audio,
            connection,
            gathered,
            observer,
            ready,
            channel,
            answerer,
            messages: Some(messages),
            closed: false,
            gather_timeout,
            data_only,
            overflowed,
            state,
            video_incoming,
            camera,
            screen,
        })
    }

    pub async fn offer(&self) -> Result<String> {
        if self.answerer || self.closed {
            return Err("peer is not an active offerer");
        }
        timeout(WAIT, async {
            let offer = self
                .connection
                .create_offer(/*options*/ None)
                .await
                .map_err(|_| "failed to create voice offer")?;
            self.connection
                .set_local_description(offer)
                .await
                .map_err(|_| "failed to set voice offer")?;
            self.wait_gathered().await;
            self.connection
                .local_description()
                .await
                .map(|offer| offer.sdp)
                .ok_or("missing voice offer")
        })
        .await
        .map_err(|_| "timed out gathering voice candidates")?
    }

    /// Waits for ICE gathering. With a configured bound, a timeout is not an error:
    /// the non-trickle SDP carries whatever candidates were gathered so far.
    async fn wait_gathered(&self) {
        match self.gather_timeout {
            Some(bound) => {
                let _ = timeout(bound, self.gathered.notified()).await;
            }
            None => self.gathered.notified().await,
        }
    }

    pub async fn apply_answer(&self, sdp: String) -> Result<AnswerOutcome> {
        if self.answerer || self.closed || sdp.is_empty() || sdp.len() > 64 * 1024 {
            return Err("invalid voice answer");
        }
        let answer = RTCSessionDescription::answer(sdp).map_err(|_| "invalid voice answer")?;
        let parsed = answer.unmarshal().map_err(|_| "invalid voice answer")?;
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        // Count all occurrences, including duplicates and other components/media sections.
        // set_remote_description itself installs ICE candidates, so validate before that call.
        for (index, attribute) in parsed
            .media_descriptions
            .iter()
            .flat_map(|media| &media.attributes)
            .filter(|attribute| attribute.key == "candidate")
            .enumerate()
        {
            if index >= self.remote_candidate_limit() {
                return Err("too many voice candidates");
            }
            let candidate = attribute
                .value
                .as_deref()
                .ok_or("invalid voice candidate")?;
            let parsed = rtc::ice::candidate::unmarshal_candidate(candidate)
                .map_err(|_| "invalid voice candidate")?;
            if parsed.component() == 1 && seen.insert(candidate) {
                candidates.push(RTCIceCandidateInit {
                    candidate: format!("candidate:{candidate}"),
                    ..Default::default()
                });
            }
        }
        timeout(WAIT, async {
            self.connection
                .set_remote_description(answer)
                .await
                .map_err(|_| "failed to apply voice answer")?;
            // The async driver requires explicit candidates to start TCP dialing,
            // even when they were bundled in the non-trickle answer.
            for candidate in candidates {
                self.connection
                    .add_ice_candidate(candidate)
                    .await
                    .map_err(|_| "failed to apply voice candidate")?;
            }
            self.ready
                .clone()
                .wait_for(|ready| *ready)
                .await
                .map(|_| ())
                .map_err(|_| "voice event channel closed")
        })
        .await
        .map_or(Ok(AnswerOutcome::TimedOut), |result| {
            result.map(|()| AnswerOutcome::Ready)
        })
    }

    /// Sends a binary fragment without JSON or UTF-8 conversion. Maximum 16 KiB.
    pub async fn send_bytes(&self, bytes: &[u8]) -> Result<()> {
        if self.closed || bytes.len() > 16 * 1024 {
            return Err("closed peer or oversized data fragment");
        }
        let channel = self.channel.get().ok_or("data channel not negotiated")?;
        timeout(WAIT, channel.send(bytes::BytesMut::from(bytes)))
            .await
            .map_err(|_| "data send timed out")?
            .map_err(|_| "data send failed")
    }

    /// Receives the exact message bytes and whether the sender used text framing.
    pub async fn receive(&mut self) -> Option<webrtc::data_channel::RTCDataChannelMessage> {
        match self.messages.as_mut() {
            Some(messages) => messages.recv().await,
            None => None,
        }
    }

    pub fn try_receive(&mut self) -> Option<webrtc::data_channel::RTCDataChannelMessage> {
        self.messages.as_mut()?.try_recv().ok()
    }

    /// Move the DataChannel receiver to a separately polled ingress loop.
    /// This allows receiving while `send_bytes` waits for SCTP backpressure.
    /// Can be taken only once; afterwards `receive` and `try_receive` return None.
    /// Closing or dropping the session ends this receiver after queued messages.
    pub fn take_data_receiver(
        &mut self,
    ) -> Option<tokio::sync::mpsc::Receiver<webrtc::data_channel::RTCDataChannelMessage>> {
        self.messages.take()
    }

    /// Answering role uses the same transport and packet admission as the host.
    pub async fn new_answerer(video: bool) -> Result<Self> {
        Self::with_config(crate::SessionConfig {
            video,
            answerer: true,
            ..Default::default()
        })
        .await
    }

    pub async fn answer(&self, sdp: String) -> Result<String> {
        if !self.answerer || self.closed || sdp.is_empty() || sdp.len() > 64 * 1024 {
            return Err("invalid remote offer");
        }
        let offer = RTCSessionDescription::offer(sdp).map_err(|_| "invalid remote offer")?;
        let parsed = offer.unmarshal().map_err(|_| "invalid remote offer")?;
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        for (index, attribute) in parsed
            .media_descriptions
            .iter()
            .flat_map(|media| &media.attributes)
            .filter(|a| a.key == "candidate")
            .enumerate()
        {
            if index >= self.remote_candidate_limit() {
                return Err("too many remote candidates");
            }
            let candidate = attribute
                .value
                .as_deref()
                .ok_or("invalid remote candidate")?;
            let parsed = rtc::ice::candidate::unmarshal_candidate(candidate)
                .map_err(|_| "invalid remote candidate")?;
            if parsed.component() == 1 && seen.insert(candidate) {
                candidates.push(RTCIceCandidateInit {
                    candidate: format!("candidate:{candidate}"),
                    ..Default::default()
                });
            }
        }
        timeout(WAIT, async {
            self.connection
                .set_remote_description(offer)
                .await
                .map_err(|_| "failed to apply remote offer")?;
            for candidate in candidates {
                self.connection
                    .add_ice_candidate(candidate)
                    .await
                    .map_err(|_| "failed to apply remote candidate")?;
            }
            let answer = self
                .connection
                .create_answer(None)
                .await
                .map_err(|_| "failed to create answer")?;
            self.connection
                .set_local_description(answer)
                .await
                .map_err(|_| "failed to set answer")?;
            self.wait_gathered().await;
            self.connection
                .local_description()
                .await
                .map(|answer| answer.sdp)
                .ok_or("missing local answer")
        })
        .await
        .map_err(|_| "answer gathering timed out")?
    }

    pub async fn wait_ready(&self) -> Result<()> {
        timeout(WAIT, self.ready.clone().wait_for(|ready| *ready))
            .await
            .map_err(|_| "channel readiness timed out")?
            .map(|_| ())
            .map_err(|_| "channel closed")
    }

    pub fn is_ready(&self) -> bool {
        !self.closed && *self.ready.borrow()
    }

    /// Channel readiness: `true` once `lingxi-agent` is open, `false` after it closes,
    /// errors, overflows its receive queue or (data-only) the connection fails.
    pub fn subscribe_ready(&self) -> watch::Receiver<bool> {
        self.ready.clone()
    }

    /// Peer-connection state (new, connecting, connected, disconnected, failed, closed).
    pub fn subscribe_connection_state(&self) -> watch::Receiver<RTCPeerConnectionState> {
        self.state.clone()
    }

    /// True when the channel was closed because the consumer let the receive queue fill.
    pub fn receive_overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Acquire)
    }

    /// Sends a host-encoded 20 ms Opus frame using the shared RTP source clock.
    pub async fn send_opus(&mut self, bytes: Vec<u8>, at: std::time::Instant) -> Result<()> {
        if self.closed {
            return Err("peer closed");
        }
        self.audio
            .as_mut()
            .ok_or("audio not enabled")?
            .send(crate::audio_track::EncodedAudio { data: bytes, at })
            .await
    }

    /// Send a synthetic silence packet only when the RTP source clock is due.
    pub async fn send_opus_if_due(&mut self, bytes: Vec<u8>, at: std::time::Instant) -> Result<()> {
        if self
            .audio
            .as_ref()
            .and_then(|audio| audio.end)
            .is_some_and(|end| at < end)
        {
            return Ok(());
        }
        self.send_opus(bytes, at).await
    }

    /// Keep the owned packet permits until the media decoder releases its buffer.
    pub fn take_audio_packet(&mut self) -> Result<Option<crate::ReceivedRtp>> {
        self.incoming.take()
    }

    /// Enable independent camera and screen tracks before negotiation.
    pub async fn new_with_video() -> Result<Self> {
        Self::with_config(crate::SessionConfig {
            video: true,
            ..Default::default()
        })
        .await
    }
    pub async fn with_bind_addresses_and_video(addresses: Vec<String>) -> Result<Self> {
        Self::with_config(crate::SessionConfig {
            bind_addresses: addresses,
            video: true,
            ..Default::default()
        })
        .await
    }
    /// Native renderers identify camera/screen from negotiated track metadata,
    /// never from whichever source happens to send its first packet first.
    pub async fn remote_video_sources(
        &self,
    ) -> Result<std::collections::HashMap<u32, crate::VideoSource>> {
        let remote = self
            .connection
            .remote_description()
            .await
            .ok_or("remote description unavailable")?;
        let parsed = remote
            .unmarshal()
            .map_err(|_| "invalid remote description")?;
        let mut sources = std::collections::HashMap::new();
        for media in &parsed.media_descriptions {
            let label = media
                .attributes
                .iter()
                .find(|a| a.key == "msid")
                .and_then(|a| a.value.as_deref())
                .and_then(|value| value.split_whitespace().nth(1));
            let source = match label {
                Some(label) if label.starts_with("camera-") => crate::VideoSource::Camera,
                Some(label) if label.starts_with("screen-") => crate::VideoSource::Screen,
                _ => continue,
            };
            for attribute in media.attributes.iter().filter(|a| a.key == "ssrc") {
                if let Some(ssrc) = attribute
                    .value
                    .as_deref()
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|v| v.parse::<u32>().ok())
                {
                    sources.insert(ssrc, source);
                }
            }
        }
        if sources.len() > 2 {
            return Err("too many remote video sources");
        }
        Ok(sources)
    }
    pub async fn send_video(
        &self,
        source: crate::VideoSource,
        bytes: Vec<u8>,
        duration: Duration,
    ) -> Result<()> {
        if self.closed {
            return Err("peer closed");
        }
        let track = match source {
            crate::VideoSource::Camera => &self.camera,
            crate::VideoSource::Screen => &self.screen,
        };
        track
            .as_ref()
            .ok_or("video not enabled")?
            .send(bytes, duration)
            .await
    }
    pub fn set_video_suppressed(&self, suppressed: bool) -> Result<()> {
        self.video_incoming
            .as_ref()
            .ok_or("video not enabled")?
            .set_suppressed(suppressed)
    }
    pub fn take_video_packet(&mut self) -> Result<Option<crate::ReceivedRtp>> {
        self.video_incoming
            .as_mut()
            .ok_or("video not enabled")?
            .take()
    }

    pub fn set_playback_suppressed(&self, suppressed: bool) -> Result<()> {
        self.incoming.set_suppressed(suppressed)
    }

    pub fn take_audio_rtp(&mut self) -> Result<Option<(std::time::Instant, Vec<u8>)>> {
        self.incoming
            .take()
            .map(|packet| packet.map(|p| (p.at, p.as_ref().to_vec())))
    }

    pub async fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        if self.data_only {
            // The pinned stack sends no close notification when the connection is torn
            // down, so the remote would only notice after ICE fails (~30 s). Resetting the
            // SCTP stream first lets it see the channel close promptly.
            if let Some(channel) = self.channel.get()
                && channel.close().await.is_ok()
            {
                let _ = timeout(
                    Duration::from_millis(/*millis*/ 500),
                    self.ready.clone().wait_for(|ready| !*ready),
                )
                .await;
            }
        }
        if let Some(messages) = self.messages.as_mut() {
            messages.close();
        }
        self.observer.abort();
        let _ = (&mut self.observer).await;
        timeout(Duration::from_secs(/*secs*/ 2), self.connection.close())
            .await
            .map_err(|_| "timed out closing voice peer")?
            .map_err(|_| "failed to close voice peer")
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.observer.abort();
        {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let connection = self.connection.clone();
                runtime.spawn(async move {
                    let _ = timeout(Duration::from_secs(2), connection.close()).await;
                });
            }
        }
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
