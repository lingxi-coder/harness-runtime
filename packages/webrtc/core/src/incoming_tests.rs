use super::*;
use pretty_assertions::assert_eq;
use rtc::rtp::header::Header;
use rtc::sansio::Protocol;

fn pair() -> (Incoming, Ingress) {
    let (incoming, ingress) = Incoming::new();
    incoming.set_suppressed(/*suppressed*/ false).unwrap();
    (incoming, ingress)
}

fn packet(size: usize, at: Instant) -> TaggedPacket {
    TaggedPacket {
        now: at,
        transport: Default::default(),
        message: Packet::Rtp(rtc::rtp::Packet {
            header: Header {
                version: 2,
                payload_type: 111,
                ssrc: 7,
                ..Default::default()
            },
            payload: vec![5; size - 12].into(),
        })
        .into(),
    }
}

#[test]
fn arrival_and_rtp_bytes_survive_the_adapter_without_an_upstream_copy() {
    let (mut incoming, mut ingress) = pair();
    let at = Instant::now() - Duration::from_millis(/*millis*/ 20);
    let input = packet(/*size*/ 32, at);
    let Packet::Rtp(expected) = &input.message.packet else {
        panic!()
    };
    let expected = expected.marshal().unwrap();
    ingress.handle_read(input).unwrap();
    let received = incoming.take().unwrap().unwrap();
    assert_eq!((received.at, received.as_ref()), (at, expected.as_ref()));
    assert!(ingress.poll_read().is_none());
}

#[test]
fn exhausted_packet_or_byte_budget_drops_audio_then_accepts_fresh_packets() {
    for size in [32, PACKET_BYTES] {
        let (mut incoming, mut ingress) = pair();
        let count = PACKETS.min(BYTES / size);
        let mut held = Vec::new();
        for _ in 0..count {
            ingress.handle_read(packet(size, Instant::now())).unwrap();
            held.push(incoming.take().unwrap().unwrap());
        }
        drop(held.pop());
        ingress.handle_read(packet(size, Instant::now())).unwrap();
        held.push(incoming.take().unwrap().unwrap());
        ingress.handle_read(packet(size, Instant::now())).unwrap();
        assert!(incoming.take().unwrap().is_none());
        assert!(!incoming.state.failed.load(Ordering::Acquire));
        drop(held);
        ingress.handle_read(packet(size, Instant::now())).unwrap();
        assert!(incoming.take().unwrap().is_some());
        assert_eq!(
            (
                incoming.state.packets.available_permits(),
                incoming.state.bytes.available_permits()
            ),
            (PACKETS, BYTES)
        );
    }
}

#[test]
fn old_speaker_packets_are_discarded_without_poisoning_fresh_audio() {
    let (mut incoming, mut ingress) = pair();
    ingress
        .handle_read(packet(/*size*/ 32, Instant::now()))
        .unwrap();
    let mut queued = incoming.receiver.try_recv().unwrap();
    queued.at -= Duration::from_secs(/*secs*/ 2);
    ingress.sender.try_send(queued).unwrap();
    assert!(incoming.take().unwrap().is_none());
    ingress
        .handle_read(packet(/*size*/ 32, Instant::now()))
        .unwrap();
    assert!(incoming.take().unwrap().is_some());
}

#[test]
fn suppression_discards_queued_and_in_flight_old_epochs() {
    let (mut incoming, mut ingress) = pair();
    ingress
        .handle_read(packet(/*size*/ 32, Instant::now()))
        .unwrap();
    incoming.set_suppressed(/*suppressed*/ true).unwrap();
    ingress
        .handle_read(packet(/*size*/ 32, Instant::now()))
        .unwrap();
    incoming.set_suppressed(/*suppressed*/ false).unwrap();
    assert!(incoming.take().unwrap().is_none());
    ingress
        .handle_read(packet(/*size*/ 32, Instant::now()))
        .unwrap();
    assert!(incoming.take().unwrap().is_some());
}

#[test]
fn invalid_stream_and_oversize_packets_fail_without_returning_media() {
    for size in [32, PACKET_BYTES + 1] {
        let (mut incoming, mut ingress) = pair();
        let mut input = packet(size, Instant::now());
        if size == 32
            && let Packet::Rtp(packet) = &mut input.message.packet
        {
            packet.header.payload_type = 96;
        }
        ingress.handle_read(input).unwrap();
        assert_eq!(incoming.take().err(), Some("incoming audio failed"));
    }
}

#[test]
fn registered_ingress_consumes_inbound_and_preserves_outbound_packets() {
    let (mut incoming, ingress) = pair();
    let mut chain = rtc::interceptor::Registry::new()
        .with(rtc::interceptor::Slot::from(INGRESS_SLOT), ingress)
        .build();
    let now = Instant::now();
    let mut outbound = packet(32, now);
    outbound
        .message
        .add(rtc::interceptor::Attribute::TargetBitrateChanged {
            bits_per_second: 750_000.0,
        });
    let expected = outbound.message.packet.clone();

    chain.handle_write(outbound).unwrap();
    let sent = chain.poll_write().unwrap();
    assert_eq!(sent.message.packet, expected);
    assert_eq!(sent.now, now);
    assert!(
        sent.message
            .has(&rtc::interceptor::Attribute::TargetBitrateChanged {
                bits_per_second: 0.0,
            })
    );
    assert!(chain.poll_write().is_none());

    let rtcp = TaggedPacket {
        now,
        transport: Default::default(),
        message: Packet::Rtcp(vec![]).into(),
    };
    chain.handle_write(rtcp).unwrap();
    assert!(matches!(
        chain.poll_write().unwrap().message.packet,
        Packet::Rtcp(_)
    ));

    chain.handle_read(packet(32, now)).unwrap();
    assert!(incoming.take().unwrap().is_some());
    assert!(chain.poll_read().is_none());
}
