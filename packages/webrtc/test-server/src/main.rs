//! Local development echo peer. Not a production Remote relay.
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use lingxi_webrtc_core::{IceConfig, IceServer, RtcSession, SessionConfig};
use rtc::shared::marshal::Unmarshal;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, oneshot};
mod media_echo;
mod speaker_fixture;
type Reply = Result<(), &'static str>;
type Answer = (String, oneshot::Sender<Reply>);
struct App {
    pending: Mutex<HashMap<u16, oneshot::Sender<Answer>>>,
    token: String,
    advertised_ip: String,
    ice: IceConfig,
}
fn authorized(headers: &HeaderMap, app: &App) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(app.token.as_str())
}
#[derive(Serialize)]
struct Offer {
    id: u16,
    sdp: String,
}
async fn offer(State(app): State<Arc<App>>, headers: HeaderMap) -> Result<Json<Offer>, StatusCode> {
    if !authorized(&headers, &app) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut pending = app.pending.lock().await;
    let id = (50000..50016)
        .find(|id| !pending.contains_key(id))
        .ok_or(StatusCode::TOO_MANY_REQUESTS)?;
    let mut peer = RtcSession::with_config(SessionConfig {
        // Relay traffic is outbound and needs no fixed Docker UDP mapping.
        // Fresh sockets also avoid reusing a live TURN allocation's 5-tuple.
        bind_addresses: vec![format!(
            "0.0.0.0:{}",
            if app.ice.relay_only { 0 } else { id }
        )],
        video: true,
        ice: app.ice.clone(),
        ..Default::default()
    })
    .await
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let raw = match peer.offer().await {
        Ok(sdp) => sdp,
        Err(_) => {
            let _ = peer.close().await;
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    // Development Docker port mapping: rewrite host candidate addresses only.
    // The bound UDP port stays unchanged and is published 1:1 by Compose.
    let sdp = docker_sdp(&raw, &app.advertised_ip);
    let (tx, rx) = oneshot::channel::<Answer>();
    pending.insert(id, tx);
    drop(pending);
    tokio::spawn(async move {
        if let Ok(Ok((sdp, done))) = tokio::time::timeout(Duration::from_secs(30), rx).await {
            let outcome = peer.apply_answer(sdp).await;
            if !matches!(outcome, Ok(lingxi_webrtc_core::AnswerOutcome::Ready)) {
                eprintln!("fixture answer {id}: {outcome:?}");
            }
            let ready = matches!(outcome, Ok(lingxi_webrtc_core::AnswerOutcome::Ready));
            let _ = done.send(if ready {
                Ok(())
            } else {
                Err("negotiation failed")
            });
            if ready {
                let _ = peer.set_playback_suppressed(false);
                let _ = peer.set_video_suppressed(false);
                let sources = peer.remote_video_sources().await.unwrap_or_default();
                let mut video = HashMap::<u32, media_echo::VideoAssembly>::new();
                let mut tick = tokio::time::interval(Duration::from_millis(5));
                let mut activity = std::time::Instant::now();
                let mut speaker = speaker_fixture::Playback::default();
                loop {
                    tokio::select! {
                        message = peer.receive() => {
                            let Some(message) = message else { break; };
                            if message.data.as_ref()==speaker_fixture::REQUEST {
                                speaker.request(std::time::Instant::now());
                            }else if peer.send_bytes(&message.data).await.is_err() {
                                break;
                            }
                            activity = std::time::Instant::now();
                        },
                        _ = tick.tick() => {},
                    }
                    if activity.elapsed() >= Duration::from_secs(120) {
                        break;
                    }
                    let mut failed = false;
                    if let Some((packet, at)) = speaker.take(std::time::Instant::now()) {
                        failed |= peer.send_opus(packet.to_vec(), at).await.is_err();
                    }
                    for _ in 0..64 {
                        let Ok(Some(packet)) = peer.take_audio_packet() else {
                            break;
                        };
                        // Consume uplink for capture testing. Never route it to
                        // the same device's speaker: that forms acoustic feedback.
                        drop(packet);
                        activity = std::time::Instant::now();
                    }
                    for _ in 0..64 {
                        let Ok(Some(packet)) = peer.take_video_packet() else {
                            break;
                        };
                        let mut bytes = packet.as_ref();
                        if let Ok(rtp) = rtc::rtp::Packet::unmarshal(&mut bytes) {
                            let Some(&source) = sources.get(&rtp.header.ssrc) else {
                                continue;
                            };
                            if let Some((frame, duration)) =
                                video.entry(rtp.header.ssrc).or_default().push(rtp)
                            {
                                failed |= peer.send_video(source, frame, duration).await.is_err();
                            }
                            activity = std::time::Instant::now();
                        }
                    }
                    if failed {
                        break;
                    }
                }
            }
        }
        let _ = peer.close().await;
        app.pending.lock().await.remove(&id);
    });
    Ok(Json(Offer { id, sdp }))
}
// Port mapping applies to our own host sockets, never to a TURN allocation.
fn docker_sdp(raw: &str, advertised_ip: &str) -> String {
    raw.lines()
        .filter(|line| !line.starts_with("a=candidate:") || line.contains(" udp "))
        .map(|line| {
            let mut fields: Vec<_> = line.split_whitespace().collect();
            if line.starts_with("a=candidate:") && fields.get(7) == Some(&"host") {
                fields[4] = advertised_ip;
                fields.join(" ")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n"
}
#[cfg(test)]
mod signaling_tests {
    #[test]
    fn docker_mapping_preserves_turn_allocation_address() {
        let input = "a=candidate:a 1 udp 1 172.18.0.3 50000 typ host\r\na=candidate:b 1 udp 1 203.0.113.2 50100 typ relay\r\n";
        let out = super::docker_sdp(input, "192.168.0.2");
        assert!(out.contains("192.168.0.2 50000 typ host"));
        assert!(out.contains("203.0.113.2 50100 typ relay"));
    }
}
#[derive(Deserialize)]
struct AnswerRequest {
    id: u16,
    sdp: String,
}
async fn answer(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(input): Json<AnswerRequest>,
) -> Result<StatusCode, StatusCode> {
    if !authorized(&headers, &app) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let (done, result) = oneshot::channel();
    let mut pending = app.pending.lock().await;
    let entry = pending.get_mut(&input.id).ok_or(StatusCode::NOT_FOUND)?;
    // Keep a closed placeholder until the peer task releases its port.
    let (placeholder, _) = oneshot::channel();
    let tx = std::mem::replace(entry, placeholder);
    drop(pending);
    tx.send((input.sdp, done))
        .map_err(|_| StatusCode::CONFLICT)?;
    match result.await {
        Ok(Ok(())) => Ok(StatusCode::NO_CONTENT),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("LINGXI_TEST_TOKEN")?;
    if token.len() < 16 {
        return Err("test token must be at least 16 bytes".into());
    }
    let advertised_ip =
        std::env::var("LINGXI_ADVERTISED_IP").unwrap_or_else(|_| "127.0.0.1".into());
    let _: std::net::IpAddr = advertised_ip.parse()?;
    let ice = if let Some(url) = std::env::var("LINGXI_TURN_URL")
        .ok()
        .filter(|s| !s.is_empty())
    {
        IceConfig {
            relay_only: true,
            servers: vec![IceServer {
                urls: vec![url],
                username: std::env::var("LINGXI_TURN_USERNAME")?,
                credential: std::env::var("LINGXI_TURN_CREDENTIAL")?,
            }],
        }
    } else {
        IceConfig::default()
    };
    ice.validate()?;
    let app = Arc::new(App {
        pending: Mutex::new(HashMap::new()),
        token: format!("Bearer {token}"),
        advertised_ip,
        ice,
    });
    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/offer", post(offer))
        .route("/answer", post(answer))
        .layer(DefaultBodyLimit::max(70 * 1024))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    axum::serve(listener, router).await?;
    Ok(())
}
