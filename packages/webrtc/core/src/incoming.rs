//! Takes incoming Opus RTP before the upstream track queue, preserving network arrival time.
//! Packet and byte permits follow owned data through the consumer, not merely through this queue.
//! Stale or saturated media is discarded so a slow worker can resume with fresh speaker audio.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use rtc::interceptor::Interceptor;
use rtc::interceptor::NoopInterceptor;
use rtc::interceptor::Packet;
use rtc::interceptor::StreamInfo;
use rtc::interceptor::TaggedPacket;
use rtc::interceptor::interceptor;
use rtc::sansio;
use rtc::shared::error::Error;
use rtc::shared::marshal::Marshal;
use rtc::shared::marshal::MarshalSize;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

const PACKETS: usize = 64;
const BYTES: usize = 2 * 1024 * 1024;
const PACKET_BYTES: usize = 64 * 1024;

struct State {
    epoch: AtomicU64,
    failed: AtomicBool,
    packets: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
}

pub struct ReceivedRtp {
    data: Vec<u8>,
    pub at: Instant,
    epoch: u64,
    _packet: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

impl AsRef<[u8]> for ReceivedRtp {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

pub struct Incoming {
    state: Arc<State>,
    receiver: mpsc::Receiver<ReceivedRtp>,
}

#[derive(Interceptor)]
pub struct Ingress {
    #[next]
    next: NoopInterceptor,
    state: Arc<State>,
    sender: mpsc::Sender<ReceivedRtp>,
    payload_type: u8,
    streams: std::collections::HashSet<u32>,
    stream_limit: usize,
    video: Option<Box<Ingress>>,
}

impl Incoming {
    pub fn new() -> (Self, Ingress) {
        let state = Arc::new(State {
            epoch: AtomicU64::new(/*v*/ 1),
            failed: AtomicBool::new(false),
            packets: Arc::new(Semaphore::new(/*permits*/ PACKETS)),
            bytes: Arc::new(Semaphore::new(/*permits*/ BYTES)),
        });
        let (sender, receiver) = mpsc::channel(/*buffer*/ PACKETS);
        (
            Self {
                state: state.clone(),
                receiver,
            },
            Ingress {
                next: NoopInterceptor::new(),
                state,
                sender,
                payload_type: 111,
                streams: Default::default(),
                stream_limit: 1,
                video: None,
            },
        )
    }

    /// Separate admission budgets for audio and the two negotiated VP8 tracks.
    pub fn with_video() -> (Self, Ingress, Self) {
        let (audio, mut ingress) = Self::new();
        let (video, mut video_ingress) = Self::new();
        video_ingress.payload_type = 96;
        video_ingress.stream_limit = 2;
        ingress.video = Some(Box::new(video_ingress));
        (audio, ingress, video)
    }

    pub fn set_suppressed(&self, suppressed: bool) -> Result<(), &'static str> {
        let epoch = self.state.epoch.load(Ordering::Acquire);
        if (epoch % 2 == 1) != suppressed {
            self.state.epoch.store(
                epoch.checked_add(1).ok_or("audio epoch exhausted")?,
                Ordering::Release,
            );
        }
        Ok(())
    }

    pub fn take(&mut self) -> Result<Option<ReceivedRtp>, &'static str> {
        if self.state.failed.load(Ordering::Acquire) {
            return Err("incoming audio failed");
        }
        let epoch = self.state.epoch.load(Ordering::Acquire);
        for _ in 0..PACKETS {
            let Ok(packet) = self.receiver.try_recv() else {
                return Ok(None);
            };
            if epoch.is_multiple_of(2) && packet.epoch == epoch {
                if packet.at.elapsed() > Duration::from_secs(/*secs*/ 1) {
                    continue;
                }
                return Ok(Some(packet));
            }
        }
        Ok(None)
    }
}

#[interceptor]
impl Ingress {
    #[overrides]
    fn handle_read(&mut self, message: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtp(packet) = &message.message {
            if packet.header.payload_type == 96 {
                if let Some(video) = &mut self.video {
                    return video.handle_read(message);
                }
            }
        }
        let Packet::Rtp(packet) = message.message else {
            return Ok(());
        };
        let epoch = self.state.epoch.load(Ordering::Acquire);
        if epoch % 2 == 1 || self.state.failed.load(Ordering::Acquire) {
            return Ok(());
        }
        let size = packet.marshal_size();
        if size > PACKET_BYTES
            || packet.header.payload_type != self.payload_type
            || (!self.streams.contains(&packet.header.ssrc)
                && self.streams.len() >= self.stream_limit)
        {
            self.state.failed.store(true, Ordering::Release);
            return Ok(());
        }
        self.streams.insert(packet.header.ssrc);
        if message.now.elapsed() > Duration::from_secs(/*secs*/ 1) {
            return Ok(());
        }
        let Ok(packet_permit) = self.state.packets.clone().try_acquire_owned() else {
            return Ok(());
        };
        let Ok(bytes_permit) = self.state.bytes.clone().try_acquire_many_owned(size as u32) else {
            return Ok(());
        };
        let mut data = vec![0; size];
        if packet.marshal_to(&mut data).ok() != Some(size) {
            self.state.failed.store(true, Ordering::Release);
            return Ok(());
        }
        let _ = self.sender.try_send(ReceivedRtp {
            data,
            at: message.now,
            epoch,
            _packet: packet_permit,
            _bytes: bytes_permit,
        });
        // Consume here: no second copy is retained by the upstream track-event queue.
        Ok(())
    }
}

#[cfg(test)]
#[path = "incoming_tests.rs"]
mod tests;
