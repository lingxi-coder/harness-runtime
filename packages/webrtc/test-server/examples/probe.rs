//! Real host-to-Docker ICE/DataChannel smoke test. Uses only local dummy credentials.
use lingxi_webrtc_core::RtcSession;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
async fn post(path: &str, body: &str, token: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let endpoint =
        std::env::var("LINGXI_TEST_ENDPOINT").unwrap_or_else(|_| "127.0.0.1:18080".into());
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
    let mut peer = RtcSession::new_answerer(false).await?;
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
    peer.close().await?;
    println!(
        "PASS: 16384 exact DataChannel bytes; microphone is not echoed; explicit speaker test returns exactly 50 Opus packets"
    );
    Ok(())
}
