//! Host-supplied ICE configuration; credentials are never included in Debug.
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
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub bind_addresses: Vec<String>,
    pub video: bool,
    pub answerer: bool,
    pub ice: IceConfig,
}
impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            bind_addresses: vec!["0.0.0.0:0".into(), "[::]:0".into()],
            video: false,
            answerer: false,
            ice: IceConfig::default(),
        }
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
