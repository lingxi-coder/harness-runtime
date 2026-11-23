//! Real host-to-Docker ICE/DataChannel smoke test. Uses only local dummy credentials.
use lingxi_webrtc_core::{RtcSession, VideoSource};
use rtc::{
    rtp::{Packet, codec::vp8::Vp8Packet, packetizer::Depacketizer},
    shared::marshal::Unmarshal,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
async fn post(path: &str, body: &str, token: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let endpoint =
        std::env::var("WEBRTC_TEST_ENDPOINT").unwrap_or_else(|_| "127.0.0.1:18080".into());
    let mut socket = tokio::net::TcpStream::connect(&endpoint).await?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(request.as_bytes()).await?;
    let mut reply = Vec::new();
    socket.read_to_end(&mut reply).await?;
    let split = reply
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("invalid HTTP response")?;
    let header = std::str::from_utf8(&reply[..split])?;
    if !header.starts_with("HTTP/1.1 2") {
        return Err(format!(
            "HTTP request failed: {}",
            header.lines().next().unwrap_or("")
        )
        .into());
    }
    Ok(reply[split + 4..].to_vec())
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::time::timeout(Duration::from_secs(45), run()).await??;
    Ok(())
}
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("LINGXI_TEST_TOKEN")?;
    let offer: serde_json::Value = serde_json::from_slice(&post("/offer", "{}", &token).await?)?;
    let mut peer = RtcSession::new_answerer(true).await?;
    let answer = peer
        .answer(offer["sdp"].as_str().ok_or("no SDP")?.into())
        .await?;
    let body = serde_json::json!({"id":offer["id"],"sdp":answer}).to_string();
    post("/answer", &body, &token).await?;
    peer.wait_ready().await?;
    let payload: Vec<u8> = (0..16384).map(|n| (n % 256) as u8).collect();
    peer.send_bytes(&payload).await?;
    let message = peer.receive().await.ok_or("channel closed")?;
    if message.data.as_ref() != payload.as_slice() || message.is_string {
        return Err("binary payload changed".into());
    }
    peer.set_playback_suppressed(false)?;
    let fixture = include_bytes!("../fixtures/speaker-200hz.opus-packets");
    let first_length = u16::from_le_bytes(fixture[4..6].try_into()?) as usize;
    // A captured microphone packet must not be returned to this peer's speaker.
    peer.send_opus(
        fixture[6..6 + first_length].to_vec(),
        std::time::Instant::now(),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    if peer.take_audio_packet()?.is_some() {
        return Err("unsafe microphone loopback".into());
    }
    peer.send_bytes(b"lingxi-webrtc-speaker-test-v1").await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut packets = 0;
    while std::time::Instant::now() < deadline {
        while peer.take_audio_packet()?.is_some() {
            packets += 1;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    if packets != 50 {
        return Err(format!("expected 50 finite speaker packets, got {packets}").into());
    }
    peer.set_video_suppressed(false)?;
    let sources = peer.remote_video_sources().await?;
    let camera_frame = vec![1, 2, 3];
    let screen_frame = vec![4, 5, 6];
    peer.send_video(
        VideoSource::Camera,
        camera_frame.clone(),
        Duration::from_millis(33),
    )
    .await?;
    peer.send_video(
        VideoSource::Screen,
        screen_frame.clone(),
        Duration::from_millis(33),
    )
    .await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let (mut camera, mut screen) = (false, false);
    while !(camera && screen) && std::time::Instant::now() < deadline {
        if let Some(received) = peer.take_video_packet()? {
            let mut bytes = received.as_ref();
            let packet = Packet::unmarshal(&mut bytes)?;
            let mut vp8 = Vp8Packet::default();
            if packet.header.payload_type != 96
                || !packet.header.marker
                || !vp8.is_partition_head(&packet.payload)
            {
                return Err("unexpected video RTP packet".into());
            }
            let payload = vp8.depacketize(&packet.payload)?;
            match sources.get(&packet.header.ssrc) {
                Some(VideoSource::Camera) if payload.as_ref() == camera_frame => camera = true,
                Some(VideoSource::Screen) if payload.as_ref() == screen_frame => screen = true,
                _ => return Err("video source or payload changed".into()),
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    if !(camera && screen) {
        return Err("did not receive both video sources".into());
    }
    peer.close().await?;
    println!(
        "PASS: 16384 exact DataChannel bytes; microphone is not echoed; 50 Opus packets; camera and screen VP8 payloads echoed separately"
    );
    Ok(())
}
