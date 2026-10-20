#[path = "transport_network_tests.rs"]
mod network;

use super::*;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;
use webrtc::data_channel::RTCDataChannelState;

struct RemoteEvents(mpsc::Sender<Arc<dyn DataChannel>>, Arc<Notify>);

#[tokio::test]
async fn embedded_offer_and_answer_roles_exchange_exact_bytes() {
    timeout(Duration::from_secs(30), async {
        let mut host = Transport::new_with_video().await.unwrap();
        let mut controller = Transport::new_answerer(true).await.unwrap();
        assert_eq!(
            controller.offer().await,
            Err("peer is not an active offerer")
        );
        assert_eq!(
            controller.answer("".into()).await,
            Err("invalid remote offer")
        );
        let answer = controller
            .answer(host.offer().await.unwrap())
            .await
            .unwrap();
        assert_eq!(
            host.apply_answer(answer).await.unwrap(),
            AnswerOutcome::Ready
        );
        controller.wait_ready().await.unwrap();
        let sources = controller.remote_video_sources().await.unwrap();
        assert_eq!(sources.len(), 2);
        assert!(
            sources
                .values()
                .any(|source| *source == crate::VideoSource::Camera)
        );
        assert!(
            sources
                .values()
                .any(|source| *source == crate::VideoSource::Screen)
        );
        let payload: Vec<u8> = (0..16384).map(|n| (n % 256) as u8).collect();
        host.send_bytes(&payload).await.unwrap();
        let received = controller.receive().await.unwrap();
        assert!(!received.is_string);
        assert_eq!(received.data.as_ref(), payload.as_slice());
        controller.send_bytes(&payload).await.unwrap();
        assert_eq!(
            host.receive().await.unwrap().data.as_ref(),
            payload.as_slice()
        );
        controller.close().await.unwrap();
        host.close().await.unwrap();
    })
    .await
    .unwrap();
}

impl PeerConnectionEventHandler for RemoteEvents {
    fn on_data_channel<'a, 'async_trait>(
        &'a self,
        channel: Arc<dyn DataChannel>,
    ) -> BoxFuture<'async_trait, ()>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.0.try_send(channel).unwrap();
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
                self.1.notify_one();
            }
        })
    }
}

#[tokio::test]
async fn negotiates_real_ordered_channel_over_udp_and_tcp() {
    check_negotiation(Arc::new(crate::transport_runtime::VoiceRuntime::default())).await;
}

#[tokio::test]
async fn negotiates_after_early_packet_loss_and_slow_tcp_connect() {
    check_negotiation(Arc::new(network::DelayedNetwork::default())).await;
}

async fn check_negotiation(runtime: Arc<dyn webrtc::runtime::Runtime>) {
    for tcp in [false, true] {
        timeout(Duration::from_secs(/*secs*/ 30), async {
            let (sender, mut channels) = mpsc::channel(/*buffer*/ 1);
            let gathered = Arc::new(Notify::new());
            let mut settings = webrtc::peer_connection::SettingEngine::default();
            settings.set_lite(/*lite*/ true);
            let (media, mut remote_audio) = crate::audio_track::AudioTrack::new().unwrap();
            let builder = PeerConnectionBuilder::new()
                .with_media_engine(media)
                .with_setting_engine(settings)
                .with_handler(Arc::new(RemoteEvents(sender, gathered.clone())));
            let remote = if tcp {
                builder.with_tcp_addrs(vec!["0.0.0.0:0"])
            } else {
                builder.with_udp_addrs(vec!["0.0.0.0:0"])
            }
            .build()
            .await
            .unwrap();
            remote.add_track(remote_audio.track.clone()).await.unwrap();
            let mut local = Transport::with_runtime(runtime.clone()).await.unwrap();
            let offer = RTCSessionDescription::offer(local.offer().await.unwrap()).unwrap();
            remote.set_remote_description(offer).await.unwrap();
            let answer = remote.create_answer(/*options*/ None).await.unwrap();
            remote.set_local_description(answer).await.unwrap();
            gathered.notified().await;
            let mut answer = remote.local_description().await.unwrap().sdp;
            let candidates: Vec<_> = answer
                .lines()
                .filter(|line| line.starts_with("a=candidate:"))
                .map(str::to_owned)
                .collect();
            // Normal generated UDP/TCP answers still connect at the admission boundary.
            assert!(!candidates.is_empty() && candidates.len() <= MAX_REMOTE_CANDIDATES);
            for _ in candidates.len()..MAX_REMOTE_CANDIDATES {
                answer.push_str(&format!("{}\r\n", candidates[0]));
            }
            let outcome = local
                .apply_answer(answer)
                .await
                .unwrap_or_else(|error| panic!("tcp={tcp}: {error}"));
            assert_eq!(outcome, super::AnswerOutcome::Ready);
            let channel = channels.recv().await.unwrap();
            assert_eq!(
                (
                    channel.label().await.unwrap(),
                    channel.ordered().await.unwrap()
                ),
                ("lingxi-agent".into(), true)
            );
            let unexpected = remote
                .create_data_channel("unexpected", /*options*/ None)
                .await
                .unwrap();
            loop {
                match unexpected.poll().await {
                    Some(DataChannelEvent::OnClose) => break,
                    Some(
                        DataChannelEvent::OnOpen
                        | DataChannelEvent::OnClosing
                        | DataChannelEvent::OnBufferedAmountLow
                        | DataChannelEvent::OnBufferedAmountHigh,
                    ) => {}
                    event => panic!("unexpected channel did not close: {event:?}"),
                }
            }
            assert_eq!(
                (channel.ready_state().await.unwrap(), *local.ready.borrow()),
                (RTCDataChannelState::Open, true)
            );
            channel.send_text("synthetic sideband event").await.unwrap();
            let received_message = local.receive().await.unwrap();
            assert_eq!(received_message.data.as_ref(), b"synthetic sideband event");
            let raw = [0, 255, 128, 10, 13];
            local.send_bytes(&raw).await.unwrap();
            loop {
                if let Some(DataChannelEvent::OnMessage(message)) = channel.poll().await {
                    assert_eq!(message.data.as_ref(), &raw);
                    assert!(!message.is_string);
                    break;
                }
            }
            local.incoming.set_suppressed(/*suppressed*/ false).unwrap();
            let sent = std::time::Instant::now();
            remote_audio
                .send(crate::audio_track::EncodedAudio {
                    data: vec![0xf8, 0xff, 0xfe],
                    at: sent,
                })
                .await
                .unwrap();
            let received = loop {
                if let Some(packet) = local.incoming.take().unwrap() {
                    break packet;
                }
                tokio::time::sleep(Duration::from_millis(/*millis*/ 5)).await;
            };
            assert!(received.at >= sent);
            assert_eq!(
                &received.as_ref()[received.as_ref().len() - 3..],
                &[0xf8, 0xff, 0xfe]
            );
            local.close().await.unwrap();
            assert!(local.ready.has_changed().is_err());
            remote.close().await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn rejects_excess_or_invalid_candidates_before_mutating_peer() {
    let mut peer = Transport::new().await.unwrap();
    let offer = peer.offer().await.unwrap();
    let header = offer
        .lines()
        .filter(|line| !line.starts_with("a=candidate:"))
        .map(|line| format!("{line}\r\n"))
        .collect::<String>();
    for transport in ["udp", "tcp"] {
        let mut answer = header.clone();
        for index in 0..=MAX_REMOTE_CANDIDATES {
            let suffix = if transport == "tcp" {
                " tcptype passive"
            } else {
                ""
            };
            answer.push_str(&format!(
                "a=candidate:{index} 1 {transport} 1 192.0.2.1 9 typ host{suffix}\r\n"
            ));
        }
        assert_eq!(
            peer.apply_answer(answer).await,
            Err("too many voice candidates")
        );
        assert!(peer.connection.remote_description().await.is_none());
    }
    // The budget is global, before duplicate/component/inactive-media filtering.
    let candidate = "a=candidate:duplicate 2 udp 1 192.0.2.1 9 typ host\r\n";
    let answer = format!(
        "{header}{}m=application 0 UDP/DTLS/SCTP webrtc-datachannel\r\na=inactive\r\n{}",
        candidate.repeat(16),
        candidate.repeat(17),
    );
    assert_eq!(
        peer.apply_answer(answer).await,
        Err("too many voice candidates")
    );
    assert!(peer.connection.remote_description().await.is_none());
    let answer = format!("{header}{candidate}a=candidate:synthetic-secret-invalid\r\n");
    assert_eq!(
        peer.apply_answer(answer).await,
        Err("invalid voice candidate")
    );
    assert!(peer.connection.remote_description().await.is_none());
    peer.close().await.unwrap();
}

#[tokio::test]
async fn invalid_answer_is_redacted_and_peer_can_close() {
    let mut peer = Transport::new().await.unwrap();
    peer.offer().await.unwrap();
    assert_eq!(
        peer.apply_answer("synthetic-secret-invalid-sdp".into())
            .await,
        Err("invalid voice answer")
    );
    peer.close().await.unwrap();
}

#[tokio::test]
async fn video_candidate_budget_rejects_excess_before_offer_or_answer_mutates_peer() {
    let mut offerer = Transport::new_with_video().await.unwrap();
    let sdp = offerer.offer().await.unwrap();
    let mut answerer = Transport::new_answerer(true).await.unwrap();
    let mut excessive = sdp
        .lines()
        .filter(|line| !line.starts_with("a=candidate:"))
        .map(|line| format!("{line}\r\n"))
        .collect::<String>();
    // Count duplicates too, even in the inactive/final media section.
    excessive.push_str(
        &"a=candidate:duplicate 2 udp 1 192.0.2.1 9 typ host\r\n"
            .repeat(MAX_VIDEO_REMOTE_CANDIDATES + 1),
    );
    assert_eq!(
        offerer.apply_answer(excessive.clone()).await,
        Err("too many voice candidates")
    );
    assert_eq!(
        answerer.answer(excessive).await,
        Err("too many remote candidates")
    );
    assert!(offerer.connection.remote_description().await.is_none());
    assert!(answerer.connection.remote_description().await.is_none());
    let candidates: Vec<_> = sdp
        .lines()
        .filter(|line| line.starts_with("a=candidate:"))
        .map(str::to_owned)
        .collect();
    assert!(!candidates.is_empty() && candidates.len() <= MAX_VIDEO_REMOTE_CANDIDATES);
    let mut boundary = sdp;
    for _ in candidates.len()..MAX_VIDEO_REMOTE_CANDIDATES {
        boundary.push_str(&format!("{}\r\n", candidates[0]));
    }
    let answer = answerer.answer(boundary).await.unwrap();
    assert_eq!(
        offerer.apply_answer(answer).await.unwrap(),
        AnswerOutcome::Ready
    );
    answerer.wait_ready().await.unwrap();
    offerer.close().await.unwrap();
    answerer.close().await.unwrap();
}

#[tokio::test]
async fn embedded_close_is_idempotent_and_sends_cannot_outlive_it() {
    let mut peer = Transport::new().await.unwrap();
    assert_eq!(
        peer.send_bytes(&vec![0; 16385]).await,
        Err("closed peer or oversized data fragment")
    );
    peer.close().await.unwrap();
    peer.close().await.unwrap();
    assert!(!peer.is_ready());
    assert_eq!(
        peer.send_bytes(b"late").await,
        Err("closed peer or oversized data fragment")
    );
    assert_eq!(
        peer.apply_answer("not-sdp".into()).await,
        Err("invalid voice answer")
    );
    assert!(peer.receive().await.is_none());
}

#[tokio::test]
async fn camera_and_screen_rtp_do_not_poison_audio_ingress() {
    timeout(Duration::from_secs(15), async {
        let (sender, _channels) = mpsc::channel(1);
        let gathered = Arc::new(Notify::new());
        let mut settings = webrtc::peer_connection::SettingEngine::default();
        settings.set_lite(true);
        let (mut media, mut audio) = crate::audio_track::AudioTrack::new().unwrap();
        let camera =
            crate::video_track::VideoTrack::new(&mut media, crate::VideoSource::Camera).unwrap();
        let screen =
            crate::video_track::VideoTrack::new(&mut media, crate::VideoSource::Screen).unwrap();
        let remote = PeerConnectionBuilder::new()
            .with_media_engine(media)
            .with_setting_engine(settings)
            .with_handler(Arc::new(RemoteEvents(sender, gathered.clone())))
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .build()
            .await
            .unwrap();
        remote.add_track(audio.track.clone()).await.unwrap();
        remote.add_track(camera.track.clone()).await.unwrap();
        remote.add_track(screen.track.clone()).await.unwrap();
        let mut local = Transport::new_with_video().await.unwrap();
        remote
            .set_remote_description(
                RTCSessionDescription::offer(local.offer().await.unwrap()).unwrap(),
            )
            .await
            .unwrap();
        let answer = remote.create_answer(None).await.unwrap();
        remote.set_local_description(answer).await.unwrap();
        gathered.notified().await;
        let mut answer = remote.local_description().await.unwrap().sdp;
        let candidates: Vec<_> = answer
            .lines()
            .filter(|line| line.starts_with("a=candidate:"))
            .map(str::to_owned)
            .collect();
        assert!(!candidates.is_empty() && candidates.len() <= MAX_VIDEO_REMOTE_CANDIDATES);
        for _ in candidates.len()..MAX_VIDEO_REMOTE_CANDIDATES {
            answer.push_str(&format!("{}\r\n", candidates[0]));
        }
        assert_eq!(
            local.apply_answer(answer).await.unwrap(),
            AnswerOutcome::Ready
        );
        local.set_playback_suppressed(false).unwrap();
        local.set_video_suppressed(false).unwrap();
        camera
            .send(vec![1, 2, 3], Duration::from_millis(67))
            .await
            .unwrap();
        screen
            .send(vec![4, 5, 6], Duration::from_millis(67))
            .await
            .unwrap();
        audio
            .send(crate::audio_track::EncodedAudio {
                data: vec![0xf8, 0xff, 0xfe],
                at: std::time::Instant::now(),
            })
            .await
            .unwrap();
        let mut streams = std::collections::HashSet::new();
        let mut heard_audio = false;
        while streams.len() < 2 || !heard_audio {
            if let Some(packet) = local.take_video_packet().unwrap() {
                let data = packet.as_ref();
                assert_eq!(data[1] & 127, 96);
                streams.insert(u32::from_be_bytes(data[8..12].try_into().unwrap()));
            }
            if local.take_audio_packet().unwrap().is_some() {
                heard_audio = true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        local.close().await.unwrap();
        remote.close().await.unwrap();
    })
    .await
    .unwrap();
}

/// Requires the explicit local Coturn fixture; ignored in ordinary offline CI.
#[tokio::test]
#[ignore = "start compose.turn.yaml before running the forced TURN acceptance"]
async fn forced_turn_exchanges_binary_audio_and_two_video_tracks_without_host_fallback() {
    timeout(Duration::from_secs(60), forced_turn_case())
        .await
        .unwrap();
}

async fn forced_turn_case() {
    fn config(answerer: bool, password: &str) -> crate::SessionConfig {
        crate::SessionConfig {
            // This Coturn publishes only loopback; do not gather through
            // unrelated Wi-Fi/VPN interfaces in a host-only fixture.
            bind_addresses: vec!["127.0.0.1:0".into()],
            video: true,
            answerer,
            ice: crate::IceConfig {
                relay_only: true,
                servers: vec![crate::IceServer {
                    urls: vec!["turn:127.0.0.1:3478?transport=udp".into()],
                    username: "lingxi-test".into(),
                    credential: password.into(),
                }],
            },
            ..Default::default()
        }
    }
    fn relay_candidates(sdp: &str) -> usize {
        let candidates: Vec<_> = sdp
            .lines()
            .filter(|s| s.starts_with("a=candidate:"))
            .collect();
        assert!(
            !candidates.is_empty(),
            "TURN must allocate real relay candidates"
        );
        for candidate in &candidates {
            assert_eq!(
                candidate.split_whitespace().nth(7),
                Some("relay"),
                "non-relay candidate in relay-only SDP"
            );
        }
        candidates.len()
    }
    let mut host = Transport::with_config(config(false, "local-turn-test-password"))
        .await
        .unwrap();
    let mut controller = Transport::with_config(config(true, "local-turn-test-password"))
        .await
        .unwrap();
    let offer = host.offer().await.unwrap();
    let offer_count = relay_candidates(&offer);
    let answer = controller.answer(offer).await.unwrap();
    let answer_count = relay_candidates(&answer);
    assert_eq!(
        host.apply_answer(answer).await.unwrap(),
        AnswerOutcome::Ready
    );
    controller.wait_ready().await.unwrap();
    let bytes: Vec<u8> = (0..16384).map(|n| (n % 256) as u8).collect();
    host.send_bytes(&bytes).await.unwrap();
    let received = controller.receive().await.unwrap();
    assert!(!received.is_string);
    assert_eq!(received.data.as_ref(), bytes);
    controller.send_bytes(&bytes).await.unwrap();
    assert_eq!(host.receive().await.unwrap().data.as_ref(), bytes);
    controller.set_playback_suppressed(false).unwrap();
    controller.set_video_suppressed(false).unwrap();
    // Transport acceptance: Opus silence and separate VP8 RTP packetization.
    // Full encoder/decoder acceptance is exercised by the media/device suite.
    host.send_opus(vec![0xf8, 0xff, 0xfe], std::time::Instant::now())
        .await
        .unwrap();
    host.send_video(
        crate::VideoSource::Camera,
        vec![1, 2, 3],
        Duration::from_millis(33),
    )
    .await
    .unwrap();
    host.send_video(
        crate::VideoSource::Screen,
        vec![4, 5, 6],
        Duration::from_millis(33),
    )
    .await
    .unwrap();
    let mut video = HashSet::new();
    let mut audio = false;
    timeout(Duration::from_secs(10), async {
        while video.len() < 2 || !audio {
            if let Some(packet) = controller.take_video_packet().unwrap() {
                let data = packet.as_ref();
                assert_eq!(data[1] & 127, 96);
                video.insert(u32::from_be_bytes(data[8..12].try_into().unwrap()));
            }
            if let Some(packet) = controller.take_audio_packet().unwrap() {
                assert_eq!(packet.as_ref()[1] & 127, 111);
                audio = true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    controller.close().await.unwrap();
    host.close().await.unwrap();
    // A rejected credential must not silently expose direct host candidates.
    let mut rejected = Transport::with_config(config(false, "incorrect-local-test-password"))
        .await
        .unwrap();
    if let Ok(sdp) = rejected.offer().await {
        assert!(
            !sdp.lines().any(|s| s.starts_with("a=candidate:")),
            "failed TURN fell back to another candidate"
        );
    }
    rejected.close().await.unwrap();
    println!(
        "forced TURN PASS: relay-only candidates {offer_count}/{answer_count}, exact 16384-byte binary roundtrip, Opus RTP, two video SSRCs; bad credentials have no direct fallback"
    );
}

#[tokio::test]
#[ignore = "requires local Coturn; set LINGXI_TURN_TEST_BIND_IP for a LAN fixture"]
async fn turn_close_releases_allocation_before_reusing_source_port() {
    timeout(Duration::from_secs(60), async {
        let ip = std::env::var("LINGXI_TURN_TEST_BIND_IP").unwrap_or_else(|_| "127.0.0.1".into());
        for _ in 0..20 {
            let mut peer = Transport::with_config(crate::SessionConfig {
                bind_addresses: vec![format!("{ip}:50202")],
                ice: crate::IceConfig {
                    relay_only: true,
                    servers: vec![crate::IceServer {
                        urls: vec![format!("turn:{ip}:3478?transport=udp")],
                        username: "lingxi-test".into(),
                        credential: "local-turn-test-password".into(),
                    }],
                },
                ..Default::default()
            })
            .await
            .unwrap();
            let offer = peer.offer().await.unwrap();
            assert!(
                offer
                    .lines()
                    .any(|line| line.starts_with("a=candidate:") && line.contains(" typ relay"))
            );
            peer.close().await.unwrap();
            // Coturn 4.18 clamps zero-lifetime cleanup to a one-second timer
            // (refresh_relay_connection). Wait for that server-side drain
            // before deliberately reusing exactly the same source port.
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
    })
    .await
    .unwrap();
}
