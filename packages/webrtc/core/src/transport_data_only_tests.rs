//! Data-only mode: a viewer-style offerer and an answerer exchange binary fragments
//! over real loopback ICE/DTLS/SCTP with no audio or video negotiated.

use super::*;
use crate::{MdnsMode, SessionConfig};
use pretty_assertions::assert_eq;

const FRAGMENT: usize = 16 * 1024;

fn data_only(answerer: bool) -> SessionConfig {
    SessionConfig {
        answerer,
        ..SessionConfig::data_only(crate::IceConfig::default())
    }
}

async fn pair(offerer: SessionConfig, answerer: SessionConfig) -> (Transport, Transport) {
    let mut viewer = Transport::with_config(offerer).await.unwrap();
    let mut desktop = Transport::with_config(answerer).await.unwrap();
    let answer = desktop.answer(viewer.offer().await.unwrap()).await.unwrap();
    assert_eq!(
        viewer.apply_answer(answer).await.unwrap(),
        AnswerOutcome::Ready
    );
    desktop.wait_ready().await.unwrap();
    assert!(viewer.is_ready() && desktop.is_ready());
    // Keep `mut` bindings meaningful for callers that close the peers.
    let _ = (&mut viewer, &mut desktop);
    (viewer, desktop)
}

fn fragment(side: u8, index: usize) -> Vec<u8> {
    (0..FRAGMENT)
        .map(|n| (n as u8) ^ (index as u8) ^ side)
        .collect()
}

#[tokio::test]
async fn data_only_offer_has_no_media_and_voice_offer_keeps_audio() {
    let mut viewer = Transport::with_config(data_only(false)).await.unwrap();
    let offer = viewer.offer().await.unwrap();
    assert!(offer.contains("m=application"));
    assert!(!offer.contains("m=audio") && !offer.contains("m=video"));
    assert!(!offer.contains("opus"));
    assert_eq!(
        viewer
            .send_opus(vec![0xf8, 0xff, 0xfe], std::time::Instant::now())
            .await,
        Err("audio not enabled")
    );
    viewer.close().await.unwrap();

    let mut voice = Transport::new().await.unwrap();
    let offer = voice.offer().await.unwrap();
    assert!(offer.contains("m=audio") && offer.contains("opus"));
    voice.close().await.unwrap();
}

#[tokio::test]
async fn explicit_audio_video_modes_match_the_requested_media_tracks() {
    let mut audio = Transport::with_config(SessionConfig {
        data_channel: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let audio_offer = audio.offer().await.unwrap();
    assert!(audio_offer.contains("m=audio") && audio_offer.contains("opus"));
    assert!(!audio_offer.contains("m=video"));
    assert!(!audio_offer.contains("m=application") && !audio_offer.contains("lingxi-agent"));
    audio.close().await.unwrap();

    // A video-only session must not create the native Opus audio track. This
    // is the transport-level guard against opening an unrequested microphone.
    let mut video = Transport::with_config(SessionConfig {
        audio: false,
        video: true,
        data_channel: false,
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(video.audio.is_none());
    let video_offer = video.offer().await.unwrap();
    assert!(video_offer.contains("m=video"));
    assert!(!video_offer.contains("m=audio") && !video_offer.contains("opus"));
    assert!(!video_offer.contains("m=application") && !video_offer.contains("lingxi-agent"));
    assert_eq!(
        video
            .send_opus(vec![0xf8, 0xff, 0xfe], std::time::Instant::now())
            .await,
        Err("audio not enabled")
    );
    video.close().await.unwrap();

    let mut audio_video = Transport::with_config(SessionConfig {
        audio: true,
        video: true,
        data_channel: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let audio_video_offer = audio_video.offer().await.unwrap();
    assert!(audio_video_offer.contains("m=audio") && audio_video_offer.contains("opus"));
    assert!(audio_video_offer.contains("m=video"));
    assert!(
        !audio_video_offer.contains("m=application") && !audio_video_offer.contains("lingxi-agent")
    );
    audio_video.close().await.unwrap();
}

#[tokio::test]
async fn media_only_peer_pair_connects_without_agent_data_channel() {
    let mut viewer = Transport::with_config(SessionConfig {
        audio: false,
        video: true,
        data_channel: false,
        video_direction: crate::VideoDirection::ReceiveOnly,
        ..Default::default()
    })
    .await
    .unwrap();
    let mut desktop = Transport::with_config(SessionConfig {
        audio: false,
        video: true,
        data_channel: false,
        video_direction: crate::VideoDirection::SendOnly,
        answerer: true,
        ..Default::default()
    })
    .await
    .unwrap();

    let offer = viewer.offer().await.unwrap();
    assert!(offer.contains("m=video"));
    assert!(!offer.contains("m=application") && !offer.contains("lingxi-agent"));
    let answer = desktop.answer(offer).await.unwrap();
    assert!(answer.contains("m=video"));
    assert!(!answer.contains("m=application") && !answer.contains("lingxi-agent"));
    assert_eq!(
        viewer.apply_answer(answer).await.unwrap(),
        AnswerOutcome::Ready
    );
    desktop.wait_ready().await.unwrap();
    assert!(viewer.is_ready() && desktop.is_ready());
    assert_eq!(
        viewer.send_bytes(b"rpc").await,
        Err("data channel not negotiated")
    );
    viewer.close().await.unwrap();
    desktop.close().await.unwrap();
}

#[tokio::test]
async fn video_direction_does_not_negotiate_an_unrequested_send_or_receive_path() {
    let mut send_only = Transport::with_config(SessionConfig {
        audio: false,
        video: true,
        data_channel: false,
        video_direction: crate::VideoDirection::SendOnly,
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(send_only.video_incoming.is_none());
    assert!(send_only.camera.is_some() && send_only.screen.is_some());
    let send_offer = send_only.offer().await.unwrap();
    let send_video_section = send_offer
        .split("m=video")
        .nth(1)
        .expect("send-only session has a video m-line");
    assert!(send_video_section.contains("a=sendonly"));
    assert!(!send_video_section.contains("a=recvonly"));
    assert!(!send_offer.contains("m=audio"));
    assert!(send_only.set_video_suppressed(true).is_err());
    send_only.close().await.unwrap();

    let mut receive_only = Transport::with_config(SessionConfig {
        audio: false,
        video: true,
        data_channel: false,
        video_direction: crate::VideoDirection::ReceiveOnly,
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(receive_only.video_incoming.is_some());
    assert!(receive_only.camera.is_none() && receive_only.screen.is_none());
    let receive_offer = receive_only.offer().await.unwrap();
    let receive_video_section = receive_offer
        .split("m=video")
        .nth(1)
        .expect("receive-only session has a video m-line");
    assert!(receive_video_section.contains("a=recvonly"));
    assert!(!receive_video_section.contains("a=sendonly"));
    assert!(!receive_offer.contains("m=audio"));
    assert!(
        receive_only
            .send_video(
                crate::VideoSource::Camera,
                vec![1],
                Duration::from_millis(33)
            )
            .await
            .is_err()
    );
    receive_only.close().await.unwrap();

    let mut send_receive = Transport::with_config(SessionConfig {
        audio: false,
        video: true,
        data_channel: false,
        video_direction: crate::VideoDirection::SendReceive,
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(send_receive.video_incoming.is_some());
    assert!(send_receive.camera.is_some() && send_receive.screen.is_some());
    let send_receive_offer = send_receive.offer().await.unwrap();
    let send_receive_video_section = send_receive_offer
        .split("m=video")
        .nth(1)
        .expect("send/receive session has a video m-line");
    assert!(send_receive_video_section.contains("a=sendrecv"));
    send_receive.close().await.unwrap();
}

#[tokio::test]
async fn data_only_pair_exchanges_one_mebibyte_each_way_in_order() {
    timeout(Duration::from_secs(60), async {
        let (mut viewer, mut desktop) = pair(data_only(false), data_only(true)).await;
        let mut ready = viewer.subscribe_ready();
        assert!(*ready.borrow());
        let count = 64; // 64 x 16 KiB = 1 MiB per direction
        let send = async {
            for index in 0..count {
                viewer.send_bytes(&fragment(1, index)).await.unwrap();
            }
        };
        let send_back = async {
            for index in 0..count {
                desktop.send_bytes(&fragment(2, index)).await.unwrap();
            }
        };
        // Concurrent receivers need exclusive access, so drain after sending in
        // lock-step with the sender tasks below.
        let (_, _) = tokio::join!(send, send_back);
        for index in 0..count {
            let message = desktop.receive().await.unwrap();
            assert!(!message.is_string);
            assert_eq!(message.data.as_ref(), fragment(1, index).as_slice());
            let message = viewer.receive().await.unwrap();
            assert_eq!(message.data.as_ref(), fragment(2, index).as_slice());
        }
        assert!(!viewer.receive_overflowed() && !desktop.receive_overflowed());
        assert_eq!(
            viewer.send_bytes(&vec![0; FRAGMENT + 1]).await,
            Err("closed peer or oversized data fragment")
        );
        let state = viewer.subscribe_connection_state();
        assert_eq!(*state.borrow(), RTCPeerConnectionState::Connected);

        desktop.close().await.unwrap();
        // The remote close is observable by the viewer promptly, not after ICE fails.
        timeout(Duration::from_secs(5), ready.wait_for(|ready| !*ready))
            .await
            .unwrap()
            .unwrap();
        assert!(viewer.receive().await.is_none());
        viewer.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn full_receive_queue_closes_the_channel_and_is_reported() {
    timeout(Duration::from_secs(30), async {
        let (mut viewer, mut desktop) = pair(
            data_only(false),
            SessionConfig {
                receive_queue: 2,
                ..data_only(true)
            },
        )
        .await;
        let mut ready = desktop.subscribe_ready();
        for index in 0..16 {
            // The channel may already be closing; only the overflow outcome matters.
            if viewer.send_bytes(&fragment(1, index)).await.is_err() {
                break;
            }
        }
        ready.wait_for(|ready| !*ready).await.unwrap();
        assert!(desktop.receive_overflowed());
        assert!(!viewer.receive_overflowed());
        // Queued messages stay readable before the end of stream.
        assert_eq!(
            desktop.receive().await.unwrap().data.as_ref(),
            fragment(1, 0).as_slice()
        );
        viewer.close().await.unwrap();
        desktop.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn small_sctp_buffers_apply_backpressure_without_losing_data() {
    timeout(Duration::from_secs(60), async {
        let small = |answerer| SessionConfig {
            sctp_send_buffer: FRAGMENT,
            sctp_receive_buffer: FRAGMENT as u32,
            ..data_only(answerer)
        };
        let (mut viewer, mut desktop) = pair(small(false), small(true)).await;
        let send = async {
            for index in 0..32 {
                viewer.send_bytes(&fragment(1, index)).await.unwrap();
            }
        };
        let receive = async {
            for index in 0..32 {
                let message = desktop.receive().await.unwrap();
                assert_eq!(message.data.as_ref(), fragment(1, index).as_slice());
            }
        };
        tokio::join!(send, receive);
        viewer.close().await.unwrap();
        desktop.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn remote_mdns_candidates_are_accepted_and_configuration_is_validated() {
    timeout(Duration::from_secs(30), async {
        for mdns in [MdnsMode::QueryOnly, MdnsMode::Disabled] {
            let mut viewer = Transport::with_config(SessionConfig {
                mdns,
                ..data_only(false)
            })
            .await
            .unwrap();
            let mut desktop = Transport::with_config(SessionConfig {
                mdns,
                ..data_only(true)
            })
            .await
            .unwrap();
            let answer = desktop.answer(viewer.offer().await.unwrap()).await.unwrap();
            // Chromium answers with `<uuid>.local` host candidates in place of LAN IPs.
            let mut lines: Vec<_> = answer.lines().map(str::to_owned).collect();
            let first = lines.iter().position(|l| l.starts_with("a=candidate:")).unwrap();
            lines.insert(
                first,
                "a=candidate:7 1 udp 2113937151 0b3d0c9e-6a1f-4d61-8f64-5f1a0e6c2f10.local 51234 typ host generation 0"
                    .into(),
            );
            let answer = lines.join("\r\n") + "\r\n";
            assert_eq!(
                viewer.apply_answer(answer).await.unwrap(),
                AnswerOutcome::Ready,
                "mdns={mdns:?}"
            );
            desktop.close().await.unwrap();
            viewer.close().await.unwrap();
        }
    })
    .await
    .unwrap();

    let bad = |config: SessionConfig| config.validate().unwrap_err();
    assert_eq!(
        bad(SessionConfig {
            video: true,
            ..data_only(false)
        }),
        "data-only sessions cannot carry audio or video"
    );
    assert_eq!(
        bad(SessionConfig {
            audio: true,
            ..SessionConfig::data_only(crate::IceConfig::default())
        }),
        "data-only sessions cannot carry audio or video"
    );
    assert_eq!(
        bad(SessionConfig {
            data_channel: false,
            audio: false,
            video: false,
            ..Default::default()
        }),
        "session must enable media or a data channel"
    );
    assert_eq!(
        bad(SessionConfig {
            data_channel: false,
            ..SessionConfig::data_only(crate::IceConfig::default())
        }),
        "data-only sessions require a data channel"
    );
    assert_eq!(
        bad(SessionConfig {
            sctp_send_buffer: 0,
            ..data_only(false)
        }),
        "SCTP buffer size out of range"
    );
    assert_eq!(
        bad(SessionConfig {
            sctp_receive_buffer: 1024,
            ..data_only(false)
        }),
        "SCTP buffer size out of range"
    );
    assert_eq!(
        bad(SessionConfig {
            receive_queue: 0,
            ..data_only(false)
        }),
        "receive queue size out of range"
    );
    assert_eq!(
        bad(SessionConfig {
            gather_timeout: Some(Duration::ZERO),
            ..data_only(false)
        }),
        "gather timeout out of range"
    );
    // Defaults keep the voice values.
    let voice = SessionConfig::default();
    assert_eq!(
        (
            voice.sctp_send_buffer,
            voice.sctp_receive_buffer,
            voice.receive_queue
        ),
        (64 * 1024, 64 * 1024, 32)
    );
    assert_eq!(
        (
            voice.audio,
            voice.video,
            voice.data_only,
            voice.gather_timeout,
            voice.mdns
        ),
        (true, false, false, None, MdnsMode::QueryOnly)
    );
    voice.validate().unwrap();
}

#[tokio::test]
async fn stun_list_is_configured_for_a_data_only_offerer() {
    let ice = crate::IceConfig {
        servers: vec![crate::IceServer {
            urls: vec!["stun:192.0.2.1:3478".into()],
            ..Default::default()
        }],
        relay_only: false,
    };
    let mut viewer = Transport::with_config(SessionConfig::data_only(ice))
        .await
        .unwrap();
    // The unreachable STUN server must not stall pairing past the bounded gathering wait.
    let started = std::time::Instant::now();
    let offer = timeout(Duration::from_secs(10), viewer.offer())
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(8));
    assert!(offer.contains("a=candidate:") && !offer.contains("m=audio"));
    viewer.close().await.unwrap();
}

#[tokio::test]
async fn extracted_receivers_drain_concurrently_with_backpressured_sends() {
    timeout(Duration::from_secs(60), async {
        let config = |answerer| SessionConfig {
            receive_queue: 4,
            sctp_send_buffer: FRAGMENT,
            sctp_receive_buffer: FRAGMENT as u32,
            ..data_only(answerer)
        };
        let (mut viewer, mut desktop) = pair(config(false), config(true)).await;
        let mut incoming_viewer = viewer.take_data_receiver().unwrap();
        let mut incoming_desktop = desktop.take_data_receiver().unwrap();
        assert!(viewer.take_data_receiver().is_none());
        assert!(viewer.receive().await.is_none());
        assert!(viewer.try_receive().is_none());
        let send_viewer = async {
            for i in 0..128 {
                viewer.send_bytes(&fragment(1, i)).await.unwrap();
            }
        };
        let send_desktop = async {
            for i in 0..128 {
                desktop.send_bytes(&fragment(2, i)).await.unwrap();
            }
        };
        let receive_viewer = async {
            for i in 0..128 {
                assert_eq!(
                    incoming_viewer.recv().await.unwrap().data.as_ref(),
                    fragment(2, i).as_slice()
                );
            }
        };
        let receive_desktop = async {
            for i in 0..128 {
                assert_eq!(
                    incoming_desktop.recv().await.unwrap().data.as_ref(),
                    fragment(1, i).as_slice()
                );
            }
        };
        tokio::join!(send_viewer, send_desktop, receive_viewer, receive_desktop);
        assert!(!viewer.receive_overflowed() && !desktop.receive_overflowed());
        viewer.close().await.unwrap();
        assert!(incoming_viewer.recv().await.is_none());
        assert!(incoming_desktop.recv().await.is_none());
        desktop.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn dropping_extracted_receiver_closes_without_reporting_overflow() {
    timeout(Duration::from_secs(30), async {
        let (mut viewer, mut desktop) = pair(data_only(false), data_only(true)).await;
        drop(desktop.take_data_receiver().unwrap());
        let mut ready = desktop.subscribe_ready();
        viewer.send_bytes(&fragment(1, 0)).await.unwrap();
        ready.wait_for(|ready| !*ready).await.unwrap();
        assert!(!desktop.receive_overflowed());
        desktop.close().await.unwrap();
        viewer.close().await.unwrap();
    })
    .await
    .unwrap();
}
