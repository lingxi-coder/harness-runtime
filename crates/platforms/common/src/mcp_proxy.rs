//! Environment proxy selection for the Native MCP HTTP/SSE fetch path.
//!
//! Claude Code 2.1.295's `jfe(url)` first uses `Rf()`'s process environment
//! proxy (priority `https_proxy`, `HTTPS_PROXY`, `http_proxy`, `HTTP_PROXY`),
//! then falls back to the registered agent-proxy environment for HTTPS URLs.
//! Unlike reqwest's `system-proxy` feature, this deliberately reads only the
//! explicit environment values that Native passes to its fetch options.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::net::IpAddr;
use std::path::PathBuf;

use lingxi_llm_client::transport::http_backend as reqwest;
use lingxi_llm_client::transport::ProxyAuthHelper;
use reqwest::{Certificate, Client, Proxy};
use std::sync::Arc;
use url::{Host, Url};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProxyRoute {
    url: String,
    ssl_cert_file: Option<PathBuf>,
    proxy_auth_helper_allowed: bool,
}

/// Resolve the proxy route observed by Native `jfe(url)` and `ta()`.
///
/// `agent_proxy_env` models `getAgentProxyEnv()`. The current Rust host has no
/// equivalent registered host callback, so production passes the process
/// environment here; keeping the second input explicit makes the source-only
/// fallback behavior testable without inventing a configuration override.
fn resolve_route<S: BuildHasher>(
    url: &str,
    process_env: &HashMap<String, String, S>,
    agent_proxy_env: &HashMap<String, String, S>,
) -> Option<ProxyRoute> {
    // Native qW uses the first truthy key in this exact order. An invalid first
    // value makes Rf() return undefined; lower-priority keys are not tried.
    let configured = ["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY"]
        .into_iter()
        .find_map(|key| {
            process_env
                .get(key)
                .filter(|value| !value.is_empty())
                .map(|value| (key, value.as_str()))
        });

    if let Some((_key, proxy_url)) = configured {
        if has_url_host(proxy_url) {
            let no_proxy = process_no_proxy(process_env);
            if !native_no_proxy_match(url, &no_proxy) {
                return Some(ProxyRoute {
                    url: proxy_url.to_owned(),
                    ssl_cert_file: None,
                    proxy_auth_helper_allowed: true,
                });
            }
            return None;
        }
    }

    // Native jfe's registered-agent fallback is HTTPS-only and requires the
    // uppercase HTTPS_PROXY field. It reads SSL_CERT_FILE before `ta()` checks
    // bypasses, but only the non-bypassed route actually applies that CA file.
    if !is_https_url(url) {
        return None;
    }
    let proxy_url = agent_proxy_env
        .get("HTTPS_PROXY")
        .filter(|value| !value.is_empty())?;
    let no_proxy = agent_proxy_env.get("NO_PROXY").map(String::as_str);
    if no_proxy.is_some_and(|rules| native_fallback_no_proxy_match(url, rules))
        || native_fallback_no_proxy_match(url, &process_no_proxy(process_env))
    {
        return None;
    }

    Some(ProxyRoute {
        url: proxy_url.clone(),
        ssl_cert_file: agent_proxy_env
            .get("SSL_CERT_FILE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        proxy_auth_helper_allowed: false,
    })
}

fn has_url_host(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| url.host().is_some())
}

fn is_https_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| url.scheme() == "https")
}

fn process_no_proxy<S: BuildHasher>(env: &HashMap<String, String, S>) -> String {
    let lower = env.get("no_proxy").filter(|value| !value.is_empty());
    let upper = env.get("NO_PROXY").filter(|value| !value.is_empty());
    if lower.is_some_and(|value| value == "*") || upper.is_some_and(|value| value == "*") {
        return "*".to_owned();
    }
    match (lower, upper) {
        (Some(lower), Some(upper)) if lower != upper => format!("{lower},{upper}"),
        (Some(lower), _) => lower.clone(),
        (_, Some(upper)) => upper.clone(),
        (None, None) => String::new(),
    }
}

/// Native `Ny`: exact host/host:port matches, leading-dot suffixes, `*`, and
/// comma-or-whitespace-separated entries. It intentionally does not treat a
/// bare domain as a suffix or accept CIDR; only the fallback path uses Qe.
fn native_no_proxy_match(url: &str, rules: &str) -> bool {
    let Some((host, port)) = host_and_port(url) else {
        return false;
    };
    if rules == "*" {
        return true;
    }
    let host_port = format!("{host}:{port}");
    rules
        .split(|character| character == ',' || is_ecmascript_whitespace(character))
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            let entry = entry.to_lowercase();
            if entry.contains(':') {
                return host_port == entry;
            }
            if let Some(suffix) = entry.strip_prefix('.') {
                return host == suffix || host.ends_with(&entry);
            }
            host == entry
        })
}

/// Native `Qe`: `Ny` first, then exact IP and CIDR matching.
fn native_fallback_no_proxy_match(url: &str, rules: &str) -> bool {
    if native_no_proxy_match(url, rules) {
        return true;
    }
    let Some(host) = host_ip(url) else {
        return false;
    };
    rules
        .split(|character| character == ',' || is_ecmascript_whitespace(character))
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if let Some((network, prefix)) = entry.split_once('/') {
                let Ok(network) = network.parse::<IpAddr>() else {
                    return false;
                };
                let Ok(prefix) = prefix.parse::<u8>() else {
                    return false;
                };
                ip_in_network(host, network, prefix)
            } else if let Ok(network) = entry.parse::<IpAddr>() {
                ip_in_network(
                    host,
                    network,
                    match network {
                        IpAddr::V4(_) => 32,
                        IpAddr::V6(_) => 128,
                    },
                )
            } else {
                false
            }
        })
}

fn is_ecmascript_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

fn host_and_port(value: &str) -> Option<(String, u16)> {
    let url = Url::parse(value).ok()?;
    let host = match url.host()? {
        Host::Domain(host) => host.to_ascii_lowercase(),
        Host::Ipv4(host) => host.to_string(),
        Host::Ipv6(host) => format!("[{host}]"),
    };
    let default_port = if matches!(url.scheme(), "https" | "wss") {
        443
    } else {
        80
    };
    // WHATWG URL exposes a default port when no non-default port is present.
    // An explicit port zero is falsy in Native's `port || defaultPort` too.
    let port = url.port().filter(|port| *port != 0).unwrap_or(default_port);
    Some((host, port))
}

fn host_ip(value: &str) -> Option<IpAddr> {
    let url = Url::parse(value).ok()?;
    match url.host()? {
        Host::Ipv4(host) => Some(IpAddr::V4(host)),
        Host::Ipv6(host) => Some(IpAddr::V6(host)),
        Host::Domain(_) => None,
    }
}

fn ip_in_network(host: IpAddr, network: IpAddr, prefix: u8) -> bool {
    match (host, network) {
        (IpAddr::V4(host), IpAddr::V4(network)) if prefix <= 32 => {
            prefix_matches(&host.octets(), &network.octets(), prefix)
        }
        (IpAddr::V6(host), IpAddr::V6(network)) if prefix <= 128 => {
            if let (Some(host), Some(network)) = (host.to_ipv4_mapped(), network.to_ipv4_mapped()) {
                return prefix <= 32 && prefix_matches(&host.octets(), &network.octets(), prefix);
            }
            prefix_matches(&host.octets(), &network.octets(), prefix)
        }
        // Native maps v4-mapped IPv6 addresses before comparing byte lengths.
        (IpAddr::V6(host), IpAddr::V4(network)) => host.to_ipv4_mapped().is_some_and(|host| {
            prefix <= 32 && prefix_matches(&host.octets(), &network.octets(), prefix)
        }),
        (IpAddr::V4(host), IpAddr::V6(network)) => {
            network.to_ipv4_mapped().is_some_and(|network| {
                prefix <= 32 && prefix_matches(&host.octets(), &network.octets(), prefix)
            })
        }
        _ => false,
    }
}

fn prefix_matches(host: &[u8], network: &[u8], prefix: u8) -> bool {
    if host.len() != network.len() || prefix as usize > host.len() * 8 {
        return false;
    }
    let whole_bytes = usize::from(prefix / 8);
    if host[..whole_bytes] != network[..whole_bytes] {
        return false;
    }
    let remaining_bits = prefix % 8;
    if remaining_bits == 0 {
        return true;
    }
    let mask = u8::MAX << (8 - remaining_bits);
    (host[whole_bytes] & mask) == (network[whole_bytes] & mask)
}

pub(crate) fn build_client_with_proxy_auth_helper(
    url: &str,
    proxy_auth_helper: Option<Arc<dyn ProxyAuthHelper>>,
) -> Result<Client, reqwest::Error> {
    let process_env: HashMap<String, String> = std::env::vars().collect();
    // The source's non-agent `getAgentProxyEnv()` fallback exports process
    // proxy values only when both uppercase HTTPS_PROXY and SSL_CERT_FILE are
    // present. Active remote agent-proxy state is a registered Host callback,
    // which this transport does not currently expose.
    let agent_proxy_env = process_agent_proxy_env(&process_env);
    let route = resolve_route(url, &process_env, &agent_proxy_env);

    // reqwest's `default-features = false` still leaves automatic env-proxy
    // matching on by default. Disable it so only the Native-resolved route can
    // be used, including the direct/no-proxy case.
    let mut builder = Client::builder().no_proxy();
    if let Some(route) = route {
        let mut proxy = Proxy::all(&route.url)?;
        if route.proxy_auth_helper_allowed {
            if let Some(helper) = proxy_auth_helper {
                proxy = proxy.custom_http_auth_provider(move || {
                    helper
                        .cached_proxy_authorization()
                        .and_then(|value| reqwest::header::HeaderValue::from_str(&value).ok())
                });
            }
        }
        builder = builder.proxy(proxy);
        if let Some(path) = route.ssl_cert_file {
            // Native catches file-read failure and proceeds with its other TLS
            // settings. A readable SSL_CERT_FILE is supplied as the fallback
            // proxy's CA set; it replaces the built-in roots in that path.
            if let Ok(pem) = std::fs::read(path) {
                let certificates = Certificate::from_pem_bundle(&pem)?;
                builder = builder.tls_built_in_root_certs(false);
                for certificate in certificates {
                    builder = builder.add_root_certificate(certificate);
                }
            }
        }
    }
    builder.build()
}

fn process_agent_proxy_env<S: BuildHasher>(
    process_env: &HashMap<String, String, S>,
) -> HashMap<String, String> {
    let Some(proxy) = process_env
        .get("HTTPS_PROXY")
        .filter(|value| !value.is_empty())
    else {
        return HashMap::new();
    };
    let Some(ca_file) = process_env
        .get("SSL_CERT_FILE")
        .filter(|value| !value.is_empty())
    else {
        return HashMap::new();
    };
    let mut result = HashMap::from([
        ("HTTPS_PROXY".to_owned(), proxy.clone()),
        ("SSL_CERT_FILE".to_owned(), ca_file.clone()),
    ]);
    if let Some(no_proxy) = process_env.get("NO_PROXY") {
        result.insert("NO_PROXY".to_owned(), no_proxy.clone());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{process_agent_proxy_env, resolve_route, ProxyRoute};
    use std::collections::HashMap;

    fn env(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn native_process_proxy_priority_and_ny_bypass_match_jfe() {
        let vars = env(&[
            ("https_proxy", "http://lower.example:3128"),
            ("HTTPS_PROXY", "http://upper.example:3128"),
            ("http_proxy", "http://http-lower.example:3128"),
            ("NO_PROXY", ".example.test"),
        ]);
        assert_eq!(
            resolve_route("https://mcp.example.test/", &vars, &HashMap::new()),
            None,
            "leading-dot rules bypass both the bare name and subdomains"
        );
        assert_eq!(
            resolve_route("https://outside.test/", &vars, &HashMap::new()),
            Some(ProxyRoute {
                url: "http://lower.example:3128".into(),
                ssl_cert_file: None,
                proxy_auth_helper_allowed: true,
            })
        );
        let exact_only = env(&[
            ("HTTPS_PROXY", "http://proxy.test"),
            ("NO_PROXY", "example.test"),
        ]);
        assert!(resolve_route("https://sub.example.test/", &exact_only, &HashMap::new()).is_some());
    }

    #[test]
    fn native_fallback_is_https_only_uses_no_proxy_cidr_and_cert_file() {
        let process = env(&[
            ("HTTPS_PROXY", "http://bad-proxy.test"),
            ("SSL_CERT_FILE", "/proxy/ca.pem"),
        ]);
        let agent = env(&[
            ("HTTPS_PROXY", "http://agent-proxy.test:3128"),
            ("NO_PROXY", "10.0.0.0/8,internal.test"),
            ("SSL_CERT_FILE", "/agent/ca.pem"),
        ]);

        assert_eq!(
            resolve_route("http://mcp.test/", &HashMap::new(), &agent),
            None,
            "registered-agent fallback is only for HTTPS destinations"
        );
        assert_eq!(
            resolve_route("https://10.5.4.3/mcp", &HashMap::new(), &agent),
            None,
            "Qe applies CIDR entries only on the fallback path"
        );
        assert_eq!(
            resolve_route("https://outside.test/", &HashMap::new(), &agent),
            Some(ProxyRoute {
                url: "http://agent-proxy.test:3128".into(),
                ssl_cert_file: Some("/agent/ca.pem".into()),
                proxy_auth_helper_allowed: false,
            })
        );
        assert_eq!(
            resolve_route("https://outside.test/", &process, &agent),
            Some(ProxyRoute {
                url: "http://bad-proxy.test".into(),
                ssl_cert_file: None,
                proxy_auth_helper_allowed: true,
            }),
            "a valid Rf process proxy wins and does not read SSL_CERT_FILE"
        );
    }

    #[test]
    fn invalid_first_process_proxy_does_not_fall_through_to_lower_priority_key() {
        let vars = env(&[
            ("https_proxy", "not-a-url"),
            ("HTTPS_PROXY", "http://upper.example:3128"),
        ]);
        assert_eq!(
            resolve_route("https://outside.test/", &vars, &HashMap::new()),
            None,
            "Rf returns undefined after the first truthy but invalid value"
        );
    }

    #[test]
    fn native_process_agent_proxy_fallback_requires_uppercase_proxy_and_ca_pair() {
        let proxy_only = env(&[("HTTPS_PROXY", "http://proxy.test")]);
        assert!(process_agent_proxy_env(&proxy_only).is_empty());

        let lowercase_only = env(&[
            ("https_proxy", "http://proxy.test"),
            ("SSL_CERT_FILE", "/proxy/ca.pem"),
        ]);
        assert!(process_agent_proxy_env(&lowercase_only).is_empty());

        let full = env(&[
            ("HTTPS_PROXY", "http://proxy.test"),
            ("SSL_CERT_FILE", "/proxy/ca.pem"),
            ("NO_PROXY", ".internal"),
        ]);
        assert_eq!(
            process_agent_proxy_env(&full),
            env(&[
                ("HTTPS_PROXY", "http://proxy.test"),
                ("SSL_CERT_FILE", "/proxy/ca.pem"),
                ("NO_PROXY", ".internal"),
            ])
        );
    }
}
