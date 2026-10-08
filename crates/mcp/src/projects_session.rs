//! Projects authority from Native 2.1.289's NSn, Sbt, Jkn, qS and Le.
//! A host owns one shared context: its startup URL is captured by the
//! composition root, while the sticky latch starts false. The current-session
//! client predicate reads live registry configs separately.

use crate::{ConfigScope, McpServerConfig};
use lingxi_core::host::McpTransportSpec;
use std::sync::atomic::{AtomicBool, Ordering};
use url::Url;

/// Host-owned inputs for the Native Projects-session predicates.
///
/// Construct this in the composition root from the host's startup facts and
/// share the same `Arc` with every registry and session that belongs to that
/// host. The URL is a startup ingress fact, not the live model API route. The
/// Native host latch starts false; no generic MCP connect path may set it.
pub struct ProjectsSessionHostContext {
    startup_url: Option<String>,
    enabled: AtomicBool,
}

impl ProjectsSessionHostContext {
    /// Construct host state from an already-captured startup URL. This method
    /// never reads process environment or current-directory state.
    #[must_use]
    pub fn new(startup_url: Option<String>) -> Self {
        Self {
            startup_url,
            enabled: AtomicBool::new(false),
        }
    }

    /// Startup ingress URL used by Native `qS`/`Jkn` matching.
    #[must_use]
    pub fn startup_url(&self) -> Option<&str> {
        self.startup_url.as_deref()
    }

    /// Native `Is(el)` sticky host latch. It is initialized to false and stays
    /// false until a trusted first-party remote-config ingress is implemented.
    #[must_use]
    pub fn default_host_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }
}

/// Le parses process env strings using JavaScript trim and ASCII true tokens.
#[must_use]
pub fn parse_env_bool(value: Option<&str>) -> bool {
    let Some(value) = value else { return false };
    let value = value.trim_matches(|c| {
        matches!(c,
        '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
        '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' |
        '\u{205f}' | '\u{3000}' | '\u{feff}')
    });
    ["1", "true", "yes", "on"]
        .iter()
        .any(|token| value.eq_ignore_ascii_case(token))
}

/// Native gzt: a dynamic, non-CLI-owned hearthbot client at a Projects URL.
#[must_use]
pub fn client_config_matches(config: &McpServerConfig, startup: Option<&str>) -> bool {
    config.scope == ConfigScope::Dynamic && projects_config_matches(config, startup)
}

fn projects_config_matches(config: &McpServerConfig, startup: Option<&str>) -> bool {
    if config.name != "hearthbot" || config.metadata.cli_owned {
        return false;
    }
    let address = match &config.spec {
        McpTransportSpec::Http { url, .. }
        | McpTransportSpec::Sse { url, .. }
        | McpTransportSpec::WebSocket { url, .. }
        | McpTransportSpec::SseIde { url, .. }
        | McpTransportSpec::WsIde { url, .. } => url,
        _ => return false,
    };
    projects_url_matches(address, startup)
}

fn projects_url_matches(address: &str, startup: Option<&str>) -> bool {
    let Some(startup) = startup.filter(|value| !value.is_empty()) else {
        return false;
    };
    let (Ok(mut target), Ok(startup)) = (Url::parse(address), Url::parse(startup)) else {
        return false;
    };
    let path = target.path();
    let session_path = path
        .strip_prefix("/v2/ccr-sessions/")
        .and_then(|suffix| suffix.strip_suffix("/hearthbot/mcp"))
        .is_some_and(|id| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        });
    if path != "/v1/code/mcp/hearthbot" && !session_path {
        return false;
    }
    if !target.username().is_empty()
        || target
            .password()
            .is_some_and(|password| !password.is_empty())
        || target
            .fragment()
            .is_some_and(|fragment| !fragment.is_empty())
        || target.query().is_some_and(|query| !query.is_empty())
    {
        return false;
    }
    match target.scheme() {
        "ws" => {
            if target.set_scheme("http").is_err() {
                return false;
            }
        }
        "wss" => {
            if target.set_scheme("https").is_err() {
                return false;
            }
        }
        _ => {}
    }
    target.origin().ascii_serialization() == startup.origin().ascii_serialization()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> McpServerConfig {
        McpServerConfig {
            name: "hearthbot".into(),
            spec: McpTransportSpec::Http {
                url: "https://example.test/v1/code/mcp/hearthbot".into(),
                headers: Default::default(),
                headers_helper: None,
                oauth: None,
            },
            scope: ConfigScope::Dynamic,
            disabled: false,
            timeout_ms: None,
            always_load: false,
            discovery_cache: None,
            tools: Vec::new(),
            tool_permissions: Default::default(),
            config_error: None,
            metadata: Default::default(),
        }
    }

    #[test]
    fn host_context_uses_only_explicit_startup_input_and_starts_false() {
        let empty = ProjectsSessionHostContext::new(None);
        assert_eq!(empty.startup_url(), None);
        assert!(!empty.default_host_enabled());

        let supplied =
            ProjectsSessionHostContext::new(Some("https://example.test/v1/code/".into()));
        assert_eq!(
            supplied.startup_url(),
            Some("https://example.test/v1/code/")
        );
        assert!(!supplied.default_host_enabled());
    }

    #[test]
    fn live_client_predicate_observes_name_scope_and_cli_ownership() {
        let startup = Some("https://example.test/");
        let mut config = config();
        assert!(client_config_matches(&config, startup));
        config.metadata.cli_owned = true;
        assert!(!client_config_matches(&config, startup));
        config.metadata.cli_owned = false;
        config.scope = ConfigScope::Agent;
        assert!(!client_config_matches(&config, startup));
        config.scope = ConfigScope::Dynamic;
        config.name = "hearthbot-other".into();
        assert!(!client_config_matches(&config, startup));
    }

    #[test]
    fn env_bool_uses_ecmascript_whitespace_and_only_native_true_tokens() {
        for value in ["1", "TRUE", "yes", " on ", "\u{feff}true\u{a0}"] {
            assert!(parse_env_bool(Some(value)), "{value:?}");
        }
        for value in ["", "0", "false", "enabled", "\u{85}true", "\u{200b}true"] {
            assert!(!parse_env_bool(Some(value)), "{value:?}");
        }
        assert!(!parse_env_bool(None));
    }

    #[test]
    fn projects_urls_match_native_origin_path_and_empty_url_components() {
        let startup = Some("https://example.test/v2/session_ingress/shttp/mcp/s");
        for address in [
            "https://EXAMPLE.test/v1/code/mcp/hearthbot",
            "wss://example.test/v2/ccr-sessions/-/hearthbot/mcp",
            "https://example.test/v2/ccr-sessions/a_1-2/hearthbot/mcp?",
            "https://example.test/v1/code/mcp/hearthbot#",
        ] {
            assert!(projects_url_matches(address, startup), "{address}");
        }
        for address in [
            "https://other.test/v1/code/mcp/hearthbot",
            "https://example.test/v1/code/mcp/hearthbot/",
            "https://u@example.test/v1/code/mcp/hearthbot",
            "https://example.test/v2/ccr-sessions//hearthbot/mcp",
            "https://example.test/v2/ccr-sessions/a.b/hearthbot/mcp",
            "https://example.test/v1/code/mcp/hearthbot?q=1",
            "https://example.test/v1/code/mcp/hearthbot#fragment",
        ] {
            assert!(!projects_url_matches(address, startup), "{address}");
        }
        assert!(!projects_url_matches(
            "https://example.test/v1/code/mcp/hearthbot",
            None
        ));
        assert!(projects_url_matches(
            "file:///v1/code/mcp/hearthbot",
            Some("file:///startup")
        ));
        assert!(projects_url_matches(
            "data:/v1/code/mcp/hearthbot",
            Some("file:///startup")
        ));
        assert!(!projects_url_matches(
            "wss://example.test/v1/code/mcp/hearthbot",
            Some("wss://example.test/")
        ));
    }
}
