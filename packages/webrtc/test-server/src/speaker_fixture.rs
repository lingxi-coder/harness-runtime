//! Explicit finite downlink fixture. Microphone RTP must never feed this source.
use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};
pub const REQUEST: &[u8] = b"lingxi-webrtc-speaker-test-v1";
pub fn packets() -> &'static [Vec<u8>] {
    static PACKETS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    PACKETS.get_or_init(|| {
        let bytes = include_bytes!("../fixtures/speaker-200hz.opus-packets");
        assert_eq!(&bytes[..4], b"LXOP");
        let mut at = 4;
        let mut packets = Vec::new();
        while at < bytes.len() {
            let len = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize;
            at += 2;
            assert!((1..=1275).contains(&len));
            packets.push(bytes[at..at + len].to_vec());
            at += len;
        }
        assert_eq!(packets.len(), 50);
        packets
    })
}
#[derive(Default)]
pub struct Playback {
    next: Option<Instant>,
    index: usize,
    cooldown: Option<Instant>,
}
impl Playback {
    pub fn request(&mut self, now: Instant) {
        if self.next.is_none() && self.cooldown.is_none_or(|deadline| now >= deadline) {
            self.index = 0;
            self.next = Some(now);
            self.cooldown = Some(now + Duration::from_secs(5));
        }
    }
    pub fn take(&mut self, now: Instant) -> Option<(&'static [u8], Instant)> {
        let due = self.next?;
        if now < due {
            return None;
        }
        if now.duration_since(due) > Duration::from_millis(100) {
            self.next = None;
            return None;
        }
        let packet = &packets()[self.index];
        self.index += 1;
        self.next = if self.index < packets().len() {
            Some(due + Duration::from_millis(20))
        } else {
            None
        };
        Some((packet, due))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finite_clip_is_silent_until_requested_and_cannot_be_extended_by_repeated_requests() {
        let mut player = Playback::default();
        let start = Instant::now();
        assert!(player.take(start).is_none());
        player.request(start);
        for n in 0..50 {
            let now = start + Duration::from_millis(n * 20);
            player.request(now);
            assert!(player.take(now).is_some());
            assert!(player.take(now).is_none());
        }
        assert!(player.take(start + Duration::from_secs(1)).is_none());
        player.request(start + Duration::from_secs(1));
        assert!(player.take(start + Duration::from_secs(1)).is_none());
        player.request(start + Duration::from_secs(5));
        assert!(player.take(start + Duration::from_secs(5)).is_some());
    }
    #[test]
    fn late_playback_is_dropped_instead_of_bursting() {
        let mut player = Playback::default();
        let start = Instant::now();
        player.request(start);
        assert!(player.take(start + Duration::from_secs(1)).is_none());
    }
}
