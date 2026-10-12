//! Request-scoped Native primary process proxy route for Claude/Foundry calls.
//!
//! The ordinary provider client keeps reqwest's existing proxy setup. This
//! module configures only the separate Native Claude pool with `Rf()`'s four
//! process variables, source order, and `Ny()` bypass semantics.

use lingxi_llm_client::transport::http_backend as reqwest;
use lingxi_llm_client::transport::ProxyAuthHelper;
use std::collections::HashMap;
use std::sync::Arc;
use url::Host;

pub(super) fn install_process_proxy(
    builder: reqwest::ClientBuilder,
    helper: Option<Arc<dyn ProxyAuthHelper>>,
) -> reqwest::ClientBuilder {
    let mut proxy = reqwest::Proxy::custom(|target| {
        primary_process_proxy_url(target.as_str())
            .and_then(|proxy| reqwest::Url::parse(&proxy).ok())
    });
    if let Some(helper) = helper {
        proxy = proxy.custom_http_auth_provider(move || {
            helper
                .cached_proxy_authorization()
                .and_then(|value| reqwest::header::HeaderValue::from_str(&value).ok())
        });
    }
    builder.proxy(proxy)
}

fn primary_process_proxy_url(target_url: &str) -> Option<String> {
    let process_env: HashMap<String, String> = std::env::vars_os()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let proxy_url = ["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY"]
        .into_iter()
        .find_map(|key| process_env.get(key).filter(|value| !value.is_empty()))?;
    let parsed_proxy = reqwest::Url::parse(proxy_url).ok()?;
    if parsed_proxy.host().is_none()
        || !matches!(parsed_proxy.scheme(), "http" | "https")
        || no_proxy_matches(target_url, &process_no_proxy(&process_env))
    {
        return None;
    }
    Some(proxy_url.clone())
}

fn process_no_proxy(env: &HashMap<String, String>) -> String {
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

fn no_proxy_matches(target_url: &str, rules: &str) -> bool {
    if rules.is_empty() {
        return false;
    }
    if rules == "*" {
        return true;
    }
    let Ok(url) = reqwest::Url::parse(target_url) else {
        return false;
    };
    let Some(host) = url.host() else {
        return false;
    };
    let host = match host {
        Host::Domain(host) => host.to_lowercase(),
        Host::Ipv4(host) => host.to_string(),
        Host::Ipv6(host) => format!("[{host}]"),
    };
    let secure = matches!(url.scheme(), "https" | "wss");
    let port = url.port().unwrap_or(if secure { 443 } else { 80 });
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

#[cfg(test)]
mod tests {
    use super::{no_proxy_matches, primary_process_proxy_url};
    use std::sync::{Mutex, OnceLock};

    fn environment_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn source_priority_stops_at_an_invalid_selected_proxy() {
        let _guard = environment_lock();
        let keys = ["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY"];
        let old = keys.map(|key| (key, std::env::var_os(key)));
        std::env::set_var("https_proxy", "not a url");
        std::env::set_var("HTTPS_PROXY", "http://upper.example:3128");
        std::env::remove_var("http_proxy");
        std::env::remove_var("HTTP_PROXY");
        assert_eq!(primary_process_proxy_url("https://api.example/v1"), None);
        for (key, value) in old {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }

    #[test]
    fn bypass_rules_use_native_exact_host_port_and_ecmascript_whitespace() {
        assert!(no_proxy_matches(
            "https://API.Example:8443/v1",
            "other\u{feff}.example:443,api.example:8443"
        ));
        assert!(no_proxy_matches("https://sub.example/v1", ".example"));
        assert!(!no_proxy_matches("https://sub.example/v1", "example"));
        assert!(no_proxy_matches("https://sub.example/v1", "*"));
    }
}
