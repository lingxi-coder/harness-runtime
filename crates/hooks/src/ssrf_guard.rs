//! SSRF guard for `HookExecutor::Http` requests (spec §9.7).
//!
//! Blocks loopback, private/link-local ranges, and non-`http(s)` schemes by
//! default. The async validation path resolves hostnames before dispatch so a
//! name that resolves to a protected address cannot bypass literal-IP checks.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use platform_api::ResolvedAddressOverride;
use thiserror::Error;

/// Outbound URL validator used by the HTTP hook executor.
///
/// Use [`Self::with_defaults`] for the standard configuration (HTTP/HTTPS
/// only, loopback + RFC1918 blocked).
#[derive(Clone)]
pub struct SsrfGuard {
    allowed_schemes: HashSet<String>,
    blocked_cidrs: Vec<IpRange>,
    allowed_hosts: Option<HashSet<String>>,
    resolver: Arc<dyn DnsResolver>,
}

#[async_trait]
/// DNS resolution seam used to validate every candidate address before an
/// outbound HTTP hook is dispatched.
pub trait DnsResolver: Send + Sync {
    /// Resolve `host:port` to the candidate socket addresses the HTTP client
    /// may use for a request.
    async fn lookup_host(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String>;
}

#[derive(Debug, Default)]
struct TokioDnsResolver;

#[async_trait]
impl DnsResolver for TokioDnsResolver {
    async fn lookup_host(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        tokio::net::lookup_host((host, port))
            .await
            .map(|iter| iter.collect())
            .map_err(|err| err.to_string())
    }
}

/// An inclusive IPv4/IPv6 address range used to express blocked CIDR-like
/// rules without depending on a CIDR parsing crate.
#[derive(Debug, Clone)]
pub struct IpRange {
    /// Inclusive lower bound.
    pub start: IpAddr,
    /// Inclusive upper bound.
    pub end: IpAddr,
}

/// Reasons a URL might be rejected by [`SsrfGuard::check_url`].
#[derive(Debug, Clone, Error)]
pub enum SsrfError {
    /// URL used a scheme not in the allow-list (e.g. `file://`).
    #[error("disallowed scheme: {0}")]
    DisallowedScheme(String),
    /// URL host failed the allow-list check (when one is configured).
    #[error("host blocked: {0}")]
    HostBlocked(String),
    /// URL contained an IP literal that fell inside a blocked range.
    #[error("ip blocked: {0}")]
    IpBlocked(IpAddr),
    /// DNS resolution failed.
    #[error("dns resolution failed: {0}")]
    DnsFailed(String),
    /// URL string could not be parsed.
    #[error("url parse failed: {0}")]
    UrlParseFailed(String),
}

impl SsrfGuard {
    /// Default safe configuration: HTTP + HTTPS only; loopback + RFC1918
    /// blocked; no host allow-list.
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut allowed_schemes = HashSet::new();
        allowed_schemes.insert("http".into());
        allowed_schemes.insert("https".into());

        // Block RFC1918 + loopback (10/8, 172.16/12, 192.168/16, 127/8) +
        // IPv4 link-local (169.254/16) which includes the cloud-metadata
        // service at 169.254.169.254. M5-06 added 169.254/16 because M5-06
        // routes hooks to public endpoints — link-local must be blocked.
        let blocked = vec![
            IpRange {
                start: "10.0.0.0".parse().unwrap(),
                end: "10.255.255.255".parse().unwrap(),
            },
            IpRange {
                start: "172.16.0.0".parse().unwrap(),
                end: "172.31.255.255".parse().unwrap(),
            },
            IpRange {
                start: "192.168.0.0".parse().unwrap(),
                end: "192.168.255.255".parse().unwrap(),
            },
            IpRange {
                start: "127.0.0.0".parse().unwrap(),
                end: "127.255.255.255".parse().unwrap(),
            },
            // M5-06: IPv4 link-local 169.254/16 — cloud metadata at
            // 169.254.169.254 is the canonical target.
            IpRange {
                start: "169.254.0.0".parse().unwrap(),
                end: "169.254.255.255".parse().unwrap(),
            },
            IpRange {
                start: "::1".parse().unwrap(),
                end: "::1".parse().unwrap(),
            },
            IpRange {
                start: "fc00::".parse().unwrap(),
                end: "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap(),
            },
            IpRange {
                start: "fe80::".parse().unwrap(),
                end: "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap(),
            },
        ];

        Self {
            allowed_schemes,
            blocked_cidrs: blocked,
            allowed_hosts: None,
            resolver: Arc::new(TokioDnsResolver),
        }
    }

    /// Default guard with a caller-supplied DNS resolver.
    ///
    /// This keeps the production policy intact while allowing deterministic
    /// resolution in integration tests or custom pinned-resolution flows.
    #[must_use]
    pub fn with_resolver<R>(resolver: R) -> Self
    where
        R: DnsResolver + 'static,
    {
        let mut guard = Self::with_defaults();
        guard.resolver = Arc::new(resolver);
        guard
    }

    #[cfg(test)]
    pub(crate) fn with_test_resolver<R>(resolver: R) -> Self
    where
        R: DnsResolver + 'static,
    {
        Self::with_resolver(resolver)
    }

    /// Validate `url` against the configured policy.
    ///
    /// Returns `Ok(())` if the URL is safe to dispatch, or an [`SsrfError`]
    /// describing the reason for rejection.
    pub fn check_url(&self, url: &str) -> Result<(), SsrfError> {
        let parsed = url::Url::parse(url).map_err(|e| SsrfError::UrlParseFailed(e.to_string()))?;
        if !self.allowed_schemes.contains(parsed.scheme()) {
            return Err(SsrfError::DisallowedScheme(parsed.scheme().into()));
        }
        if let Some(allowed) = &self.allowed_hosts {
            if !allowed.contains(parsed.host_str().unwrap_or("")) {
                return Err(SsrfError::HostBlocked(
                    parsed.host_str().unwrap_or("").into(),
                ));
            }
        }
        if let Some(host) = parsed.host() {
            match host {
                url::Host::Ipv4(ip) => self.check_ip(IpAddr::V4(ip))?,
                url::Host::Ipv6(ip) => self.check_ip(IpAddr::V6(ip))?,
                url::Host::Domain(_) => {}
            }
        }
        Ok(())
    }

    /// Validate `url` and resolve every hostname address before dispatch.
    ///
    /// A mixed public/private DNS answer is rejected in full rather than
    /// allowing the HTTP transport to pick a protected address.
    pub async fn check_url_resolved(&self, url: &str) -> Result<(), SsrfError> {
        self.resolve_url(url).await.map(|_| ())
    }

    /// Resolve `url` through the configured DNS policy and return a vetted
    /// transport override for domain hosts.
    ///
    /// IP-literal URLs are still validated, but return `None` because there is
    /// no hostname lookup to pin.
    pub async fn resolve_url(
        &self,
        url: &str,
    ) -> Result<Option<ResolvedAddressOverride>, SsrfError> {
        self.check_url(url)?;
        let parsed = url::Url::parse(url).map_err(|e| SsrfError::UrlParseFailed(e.to_string()))?;
        let Some(url::Host::Domain(host)) = parsed.host() else {
            return Ok(None);
        };
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| SsrfError::DnsFailed("URL has no resolvable port".into()))?;
        let addresses = self
            .resolver
            .lookup_host(host, port)
            .await
            .map_err(SsrfError::DnsFailed)?;
        if addresses.is_empty() {
            return Err(SsrfError::DnsFailed(
                "hostname returned no addresses".into(),
            ));
        }
        for address in &addresses {
            self.check_ip(address.ip())?;
        }
        Ok(Some(ResolvedAddressOverride {
            domain: host.to_ascii_lowercase(),
            addrs: addresses,
        }))
    }

    fn check_ip(&self, ip: IpAddr) -> Result<(), SsrfError> {
        if self
            .blocked_cidrs
            .iter()
            .any(|range| ip >= range.start && ip <= range.end)
        {
            return Err(SsrfError::IpBlocked(ip));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StaticResolver {
        answers: Mutex<HashMap<(String, u16), Result<Vec<SocketAddr>, String>>>,
    }

    impl StaticResolver {
        fn with_answer(host: &str, port: u16, addrs: Vec<SocketAddr>) -> Self {
            let mut answers = HashMap::new();
            answers.insert((host.to_string(), port), Ok(addrs));
            Self {
                answers: Mutex::new(answers),
            }
        }
    }

    #[async_trait]
    impl DnsResolver for StaticResolver {
        async fn lookup_host(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
            self.answers
                .lock()
                .unwrap()
                .get(&(host.to_string(), port))
                .cloned()
                .unwrap_or_else(|| Err(format!("missing resolver answer for {host}:{port}")))
        }
    }

    #[test]
    fn blocks_localhost_ip() {
        let g = SsrfGuard::with_defaults();
        assert!(g.check_url("http://127.0.0.1/").is_err());
    }

    #[test]
    fn blocks_ipv6_local_ranges() {
        let g = SsrfGuard::with_defaults();
        assert!(g.check_url("http://[::1]/").is_err());
        assert!(g.check_url("http://[fc00::1]/").is_err());
        assert!(g.check_url("http://[fe80::1]/").is_err());
    }

    #[tokio::test]
    async fn blocks_hostname_resolving_to_loopback() {
        let g = SsrfGuard::with_defaults();
        assert!(g.check_url_resolved("http://localhost/").await.is_err());
    }

    #[tokio::test]
    async fn documentation_domains_are_not_exempt_from_resolution_policy() {
        let g = SsrfGuard::with_test_resolver(StaticResolver::with_answer(
            "hook.example.com",
            443,
            vec!["127.0.0.1:443".parse().unwrap()],
        ));
        assert!(g
            .check_url_resolved("https://hook.example.com/pre")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn resolve_url_returns_vetted_transport_override() {
        let g = SsrfGuard::with_test_resolver(StaticResolver::with_answer(
            "hooks.example.com",
            8443,
            vec!["93.184.216.34:8443".parse().unwrap()],
        ));
        let override_addrs = g
            .resolve_url("https://hooks.example.com:8443/webhook")
            .await
            .expect("public domain should pass")
            .expect("domain host should return override");
        assert_eq!(override_addrs.domain, "hooks.example.com");
        assert_eq!(
            override_addrs.addrs,
            vec!["93.184.216.34:8443".parse().unwrap()]
        );
    }

    #[tokio::test]
    async fn resolve_url_rejects_mixed_private_answers() {
        let g = SsrfGuard::with_test_resolver(StaticResolver::with_answer(
            "hooks.example.com",
            443,
            vec![
                "93.184.216.34:443".parse().unwrap(),
                "127.0.0.1:443".parse().unwrap(),
            ],
        ));
        assert!(g
            .resolve_url("https://hooks.example.com/webhook")
            .await
            .is_err());
    }

    #[test]
    fn blocks_rfc1918_ip() {
        let g = SsrfGuard::with_defaults();
        assert!(g.check_url("http://192.168.1.1/").is_err());
    }

    #[test]
    fn allows_public_host() {
        let g = SsrfGuard::with_defaults();
        assert!(g.check_url("https://example.com/").is_ok());
    }

    #[test]
    fn blocks_cloud_metadata_169_254() {
        // M5-06: 169.254/16 (IPv4 link-local) was deferred to M2 in M1.4;
        // M5-06 adds it because hooks dispatch to public endpoints — the
        // cloud-metadata service at 169.254.169.254 must be blocked.
        let g = SsrfGuard::with_defaults();
        assert!(g
            .check_url("http://169.254.169.254/latest/meta-data/")
            .is_err());
        assert!(g.check_url("http://169.254.0.1/").is_err());
    }

    #[test]
    fn rejects_file_scheme() {
        let g = SsrfGuard::with_defaults();
        assert!(g.check_url("file:///etc/passwd").is_err());
    }
}
