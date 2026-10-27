//! Host-supplied ICE configuration; credentials are never included in Debug.
use std::time::Duration;
use webrtc::peer_connection::{
    RTCConfiguration, RTCConfigurationBuilder, RTCIceServer, RTCIceTransportPolicy,
};

#[derive(Clone, Default)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}
impl std::fmt::Debug for IceServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IceServer")
            .field("url_count", &self.urls.len())
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Default)]
pub struct IceConfig {
    pub servers: Vec<IceServer>,
    /// Require relayed local candidates; failed TURN never falls back to host ICE.
    pub relay_only: bool,
}
/// Whether remote mDNS (`<uuid>.local`) host candidates are resolved.
/// Browsers hide LAN addresses behind mDNS names, so same-LAN direct paths to a
/// Chromium peer need `QueryOnly`. Our own candidates are never published as mDNS names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MdnsMode {
    /// Resolve remote `.local` candidates; local host candidates keep their IPs.
    /// This is the pinned upstream default and the existing voice behaviour.
    #[default]
    QueryOnly,
    /// Discard remote `.local` candidates.
    Disabled,
}

/// Negotiated direction of enabled video media. This affects RTP transceivers
/// and never opens a local camera or screen capture device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoDirection {
    /// Send local camera/screen video and receive peer video.
    #[default]
    SendReceive,
    /// Send selected local video sources without receiving video.
    SendOnly,
    /// Receive peer video without advertising local video senders.
    ReceiveOnly,
}

impl VideoDirection {
    pub(crate) const fn sends(self) -> bool {
        matches!(self, Self::SendReceive | Self::SendOnly)
    }

    pub(crate) const fn receives(self) -> bool {
        matches!(self, Self::SendReceive | Self::ReceiveOnly)
    }
}

pub(crate) const DEFAULT_SCTP_BUFFER: usize = 64 * 1024;
pub(crate) const DEFAULT_RECEIVE_QUEUE: usize = 32;
const MIN_SCTP_BUFFER: usize = 16 * 1024;
const MAX_SCTP_BUFFER: usize = 16 * 1024 * 1024;
const MAX_RECEIVE_QUEUE: usize = 65_536;

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub bind_addresses: Vec<String>,
    /// Negotiate the independent `lingxi-agent` DataChannel. Defaults to true
    /// for existing RPC sessions; media-only peers must set false.
    pub data_channel: bool,
    /// Negotiate and send an Opus audio track. Defaults to true to preserve the
    /// existing voice-session behavior; Video-only sessions must set false.
    pub audio: bool,
    /// Whether enabled video sends, receives, or does both directions.
    pub video_direction: VideoDirection,
    pub video: bool,
    pub answerer: bool,
    pub ice: IceConfig,
    /// Negotiate only the `lingxi-agent` DataChannel: no audio or video track, no
    /// media codecs in the SDP. Incompatible with `audio` or `video`.
    pub data_only: bool,
    /// Bytes of outbound DataChannel data in flight before `send_bytes` waits (16 KiB..16 MiB).
    pub sctp_send_buffer: usize,
    /// SCTP receive window in bytes (16 KiB..16 MiB). Keep it at least one fragment.
    pub sctp_receive_buffer: u32,
    /// Messages queued between the network and `receive()`. If a consumer lets this
    /// fill, the channel is closed (never silently dropped); see `receive_overflowed`.
    pub receive_queue: usize,
    /// `None`: `offer`/`answer` fail if gathering is incomplete after 15 s (voice behaviour).
    /// `Some(d)`: wait at most `d` for gathering, then return the SDP with the candidates
    /// gathered so far (non-trickle), so an unreachable STUN server cannot stall pairing.
    pub gather_timeout: Option<Duration>,
    pub mdns: MdnsMode,
}
impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            bind_addresses: vec!["0.0.0.0:0".into(), "[::]:0".into()],
            data_channel: true,
            audio: true,
            video_direction: VideoDirection::SendReceive,
            video: false,
            answerer: false,
            ice: IceConfig::default(),
            data_only: false,
            sctp_send_buffer: DEFAULT_SCTP_BUFFER,
            sctp_receive_buffer: DEFAULT_SCTP_BUFFER as u32,
            receive_queue: DEFAULT_RECEIVE_QUEUE,
            gather_timeout: None,
            mdns: MdnsMode::default(),
        }
    }
}
impl SessionConfig {
    /// Offerer-side defaults for a Remote Control viewer: DataChannel only, a
    /// 3 s bounded gathering wait, 1 MiB buffers and a 1024-message receive queue.
    /// Set `answerer` for the accepting side; `ice.servers` takes the STUN list.
    pub fn data_only(ice: IceConfig) -> Self {
        Self {
            data_only: true,
            data_channel: true,
            audio: false,
            ice,
            sctp_send_buffer: 1024 * 1024,
            sctp_receive_buffer: 1024 * 1024,
            receive_queue: 1024,
            gather_timeout: Some(Duration::from_secs(3)),
            ..Default::default()
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.data_only && (self.audio || self.video) {
            return Err("data-only sessions cannot carry audio or video");
        }
        if self.data_only && !self.data_channel {
            return Err("data-only sessions require a data channel");
        }
        if !self.data_channel && !self.audio && !self.video {
            return Err("session must enable media or a data channel");
        }
        if !(MIN_SCTP_BUFFER..=MAX_SCTP_BUFFER).contains(&self.sctp_send_buffer)
            || !(MIN_SCTP_BUFFER..=MAX_SCTP_BUFFER).contains(&(self.sctp_receive_buffer as usize))
        {
            return Err("SCTP buffer size out of range");
        }
        if !(1..=MAX_RECEIVE_QUEUE).contains(&self.receive_queue) {
            return Err("receive queue size out of range");
        }
        if self
            .gather_timeout
            .is_some_and(|d| d.is_zero() || d > Duration::from_secs(15))
        {
            return Err("gather timeout out of range");
        }
        self.ice.validate()
    }
}
impl IceConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.clone().configuration().map(|_| ())
    }

    pub(crate) fn configuration(self) -> Result<RTCConfiguration, &'static str> {
        if self.servers.len() > 8 {
            return Err("too many ICE servers");
        }
        let mut total = 0;
        let mut turn = false;
        let mut servers = Vec::new();
        for server in self.servers {
            total += server.urls.len();
            if server.urls.is_empty()
                || total > 16
                || server.username.len() > 1024
                || server.credential.len() > 1024
                || server.urls.iter().any(|url| url.len() > 2048)
            {
                return Err("invalid ICE server bounds");
            }
            let server = RTCIceServer {
                urls: server.urls,
                username: server.username,
                credential: server.credential,
            };
            for url in server
                .urls()
                .map_err(|_| "invalid ICE URL or TURN credentials")?
            {
                // The pinned async driver currently implements UDP relay only.
                // Reject unsupported transports instead of silently skipping
                // them upstream and later reporting a gathering timeout.
                if url.is_secure() || url.proto.to_string() != "udp" {
                    return Err("only UDP STUN/TURN is supported");
                }
                turn |= matches!(
                    url.scheme,
                    rtc::ice::url::SchemeType::Turn | rtc::ice::url::SchemeType::Turns
                );
            }
            servers.push(server);
        }
        if self.relay_only && !turn {
            return Err("relay-only ICE requires TURN");
        }
        Ok(RTCConfigurationBuilder::new()
            .with_ice_servers(servers)
            .with_ice_transport_policy(if self.relay_only {
                RTCIceTransportPolicy::Relay
            } else {
                RTCIceTransportPolicy::All
            })
            .build())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relay_requires_valid_bounded_turn_and_debug_redacts_secrets() {
        assert!(
            IceConfig {
                relay_only: true,
                ..Default::default()
            }
            .configuration()
            .is_err()
        );
        let server = IceServer {
            urls: vec!["turn:127.0.0.1:3478?transport=udp".into()],
            username: "private-user".into(),
            credential: "private-secret".into(),
        };
        let config = IceConfig {
            servers: vec![server.clone()],
            relay_only: true,
        };
        let debug = format!("{config:?}");
        for secret in ["private-user", "private-secret", "127.0.0.1"] {
            assert!(!debug.contains(secret));
        }
        assert_eq!(
            config.configuration().unwrap().ice_transport_policy(),
            RTCIceTransportPolicy::Relay
        );
        let mut bad = server.clone();
        bad.credential.clear();
        assert!(
            IceConfig {
                servers: vec![bad],
                relay_only: false
            }
            .configuration()
            .is_err()
        );
        let mut bad = server.clone();
        bad.urls = vec!["https://invalid.example".into()];
        assert!(
            IceConfig {
                servers: vec![bad],
                relay_only: false
            }
            .configuration()
            .is_err()
        );
        assert!(
            IceConfig {
                servers: vec![server; 9],
                relay_only: false
            }
            .configuration()
            .is_err()
        );
        for url in ["turns:127.0.0.1:5349", "turn:127.0.0.1:3478?transport=tcp"] {
            assert_eq!(
                IceConfig {
                    servers: vec![IceServer {
                        urls: vec![url.into()],
                        username: "user".into(),
                        credential: "secret".into()
                    }],
                    relay_only: true
                }
                .configuration()
                .err(),
                Some("only UDP STUN/TURN is supported")
            );
        }
        let stun = IceServer {
            urls: vec!["stun:127.0.0.1:3478".into()],
            ..Default::default()
        };
        assert!(
            IceConfig {
                servers: vec![stun],
                relay_only: true
            }
            .configuration()
            .is_err()
        );
    }
}
