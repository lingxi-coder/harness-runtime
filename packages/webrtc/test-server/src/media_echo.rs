//! Bounded VP8 reassembly for the development echo fixture, not a media server.
use rtc::rtp::{Packet, codec::vp8::Vp8Packet, packetizer::Depacketizer};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
#[derive(Default)]
pub struct VideoAssembly {
    timestamp: Option<u32>,
    head: Option<u16>,
    tail: Option<u16>,
    payloads: HashMap<u16, Vec<u8>>,
    bytes: usize,
    started: Option<Instant>,
    previous: Option<u32>,
}
impl VideoAssembly {
    pub fn push(&mut self, packet: Packet) -> Option<(Vec<u8>, Duration)> {
        let now = Instant::now();
        if self.timestamp != Some(packet.header.timestamp)
            || self
                .started
                .is_some_and(|at| now.duration_since(at) > Duration::from_millis(150))
        {
            let previous = self.previous;
            *self = Self {
                timestamp: Some(packet.header.timestamp),
                started: Some(now),
                previous,
                ..Default::default()
            };
        }
        if self.payloads.contains_key(&packet.header.sequence_number) {
            return None;
        }
        if self.payloads.len() >= 64 || self.bytes + packet.payload.len() > 2 * 1024 * 1024 {
            self.payloads.clear();
            self.bytes = 0;
            self.head = None;
            self.tail = None;
            return None;
        }
        let mut depay = Vp8Packet::default();
        let head = depay.is_partition_head(&packet.payload);
        let decoded = depay.depacketize(&packet.payload).ok()?;
        if head {
            self.head = Some(packet.header.sequence_number);
        }
        if packet.header.marker {
            self.tail = Some(packet.header.sequence_number);
        }
        self.bytes += decoded.len();
        self.payloads
            .insert(packet.header.sequence_number, decoded.to_vec());
        let (head, tail) = (self.head?, self.tail?);
        let count = usize::from(tail.wrapping_sub(head)) + 1;
        if count > 64
            || (0..count).any(|n| !self.payloads.contains_key(&head.wrapping_add(n as u16)))
        {
            return None;
        }
        let mut data = Vec::with_capacity(self.bytes);
        for n in 0..count {
            data.extend(self.payloads.remove(&head.wrapping_add(n as u16))?);
        }
        let ticks = self
            .previous
            .map(|last| packet.header.timestamp.wrapping_sub(last))
            .filter(|ticks| *ticks > 0 && *ticks <= 90000)
            .unwrap_or(6000);
        self.previous = self.timestamp;
        self.head = None;
        self.tail = None;
        self.bytes = 0;
        Some((data, Duration::from_secs_f64(f64::from(ticks) / 90000.)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::rtp::{codec::vp8::Vp8Payloader, packetizer::Payloader};
    #[test]
    fn reassembly_preserves_bytes_across_sequence_wrap_and_reordering() {
        let payload: Vec<_> = (0..100).collect();
        let fragments = Vp8Payloader::default()
            .payload(24, &payload.clone().into())
            .unwrap();
        let count = fragments.len();
        let mut packets: Vec<_> = fragments
            .into_iter()
            .enumerate()
            .map(|(n, payload)| Packet {
                header: rtc::rtp::header::Header {
                    version: 2,
                    payload_type: 96,
                    sequence_number: 65534u16.wrapping_add(n as u16),
                    timestamp: 90000,
                    marker: n + 1 == count,
                    ..Default::default()
                },
                payload,
            })
            .collect();
        packets.swap(1, 2);
        let mut assembly = VideoAssembly::default();
        let frames: Vec<_> = packets
            .into_iter()
            .filter_map(|packet| assembly.push(packet))
            .collect();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, payload);
    }
}
