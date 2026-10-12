use super::*;
use crate::connection::{ConfigScope, McpServerConfig};
use async_trait::async_trait;
use lingxi_core::host::{
    ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
    McpRawConnection, McpResourceContentDto, McpResourceDto, McpServerInfo, McpStatus, McpToolDto,
    McpToolResultDto, McpTransport, McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
};
use lingxi_core::types::McpConnectionId as ConnId;
use std::sync::Arc;

/// Minimal in-crate stub transport — only `connections` matters for
/// `snapshot`, so every method panics if called.
struct StubTransport;

#[async_trait]
impl McpTransport for StubTransport {
    async fn connect(&self, _s: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        unreachable!()
    }
    async fn initialize(&self, _c: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError> {
        unreachable!()
    }
    async fn list_tools(&self, _c: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        unreachable!()
    }
    async fn list_resources(&self, _c: &McpRawConnection) -> Result<Vec<McpResourceDto>, McpError> {
        unreachable!()
    }
    async fn list_prompts(&self, _c: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        unreachable!()
    }
    async fn call_tool(
        &self,
        _c: &McpRawConnection,
        _t: &str,
        _i: lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<McpToolResultDto, McpError> {
        unreachable!()
    }
    async fn read_resource(
        &self,
        _c: &McpRawConnection,
        _u: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<McpResourceContentDto, McpError> {
        unreachable!()
    }
    async fn ping(&self, _id: ConnId) -> Result<(), McpError> {
        unreachable!()
    }
    async fn notifications(
        &self,
        _c: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        unreachable!()
    }
    async fn handle_elicitation(
        &self,
        _c: &McpRawConnection,
        _r: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        unreachable!()
    }
    async fn disconnect(&self, _id: ConnId) -> Result<(), McpError> {
        unreachable!()
    }
    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::Stdio]
    }
}

fn stdio_cfg(name: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.into(),
        spec: McpTransportSpec::Stdio {
            command: "echo".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
        },
        scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    }
}

#[test]
fn server_connection_payloads_only_mark_plugin_agent_source_as_plugin() {
    let mut plugin_cfg = stdio_cfg("plugin:demo:srv");
    plugin_cfg.scope = ConfigScope::Dynamic;
    plugin_cfg.metadata.agent_source = Some(crate::connection::McpAgentSource::Plugin);
    let plugin_succeeded = server_connection_succeeded_payload(
        &plugin_cfg,
        12,
        crate::protocol_negotiation::NegotiationMode::Legacy,
        &lingxi_core::host::McpNegotiatedProtocol {
            era: lingxi_core::host::McpProtocolEra::Legacy,
            version: "2025-11-25".into(),
        },
    );
    let plugin_failed = server_connection_failed_payload(
        &plugin_cfg,
        Some(crate::protocol_negotiation::NegotiationMode::Legacy),
        Some(12),
        Some("INVALID_CONFIG"),
    );
    assert!(plugin_succeeded.is_plugin);
    assert!(plugin_failed.is_plugin);

    let dynamic_cfg = McpServerConfig {
        scope: ConfigScope::Dynamic,
        ..stdio_cfg("dynamic")
    };
    let dynamic_succeeded = server_connection_succeeded_payload(
        &dynamic_cfg,
        8,
        crate::protocol_negotiation::NegotiationMode::Legacy,
        &lingxi_core::host::McpNegotiatedProtocol {
            era: lingxi_core::host::McpProtocolEra::Legacy,
            version: "2025-11-25".into(),
        },
    );
    let dynamic_failed = server_connection_failed_payload(
        &dynamic_cfg,
        Some(crate::protocol_negotiation::NegotiationMode::Legacy),
        Some(8),
        Some("INVALID_CONFIG"),
    );
    assert!(!dynamic_succeeded.is_plugin);
    assert!(!dynamic_failed.is_plugin);
}

#[tokio::test]
async fn snapshot_empty_registry() {
    let r = McpRegistry::new(Arc::new(StubTransport));
    assert_eq!(r.snapshot().await, Vec::<McpServerInfo>::new());
}

#[tokio::test]
async fn unconfigured_and_invalid_config_short_circuit_before_dialing() {
    // `StubTransport::connect` is `unreachable!()`, so reaching the dial
    // panics — both of `Nxe`'s pre-dial gates have to fire here, and they
    // carry DIFFERENT oracle error codes (UNCONFIGURED vs INVALID_CONFIG)
    // that `mcp list`/`mcp get` render differently.
    let r = McpRegistry::new(Arc::new(StubTransport));

    let mut blank = stdio_cfg("blank");
    blank.spec = McpTransportSpec::Http {
        url: "   ".into(),
        headers: lingxi_core::host::McpHeaders::default(),
        headers_helper: None,
        oauth: None,
    };
    assert!(
        blank.is_unconfigured(),
        "blank url + no configError = `zar`"
    );
    assert_eq!(
        r.connect(blank).await.unwrap_err().to_string(),
        "connection failed: No URL configured for this server"
    );

    let mut broken = stdio_cfg("broken");
    broken.spec = McpTransportSpec::Http {
        url: "${MISSING:-}".into(),
        headers: lingxi_core::host::McpHeaders::default(),
        headers_helper: None,
        oauth: None,
    };
    broken.config_error = Some("'url' \"${MISSING:-}\" expanded to an empty string.".to_string());
    assert!(
        !broken.is_unconfigured(),
        "`url_invalid` is INVALID_CONFIG, never UNCONFIGURED"
    );
    assert_eq!(
        r.connect(broken).await.unwrap_err().to_string(),
        "connection failed: 'url' \"${MISSING:-}\" expanded to an empty string."
    );

    // §18 — the oracle's CONNECT-TIME `new URL(t.url)` re-check
    // (`Ve`/`Ae` @182283839 / @182488092), which fires on a url that is
    // present and non-blank but does not parse. Nothing at load time
    // records a `config_error` for it, so before this gate existed the
    // registry dialed a garbage url and surfaced a raw transport error.
    // `StubTransport::connect` is `unreachable!()`, so reaching the dial
    // panics the test.
    let mut malformed = stdio_cfg("malformed");
    malformed.spec = McpTransportSpec::Http {
        url: "api.example.com/mcp".into(),
        headers: lingxi_core::host::McpHeaders::default(),
        headers_helper: None,
        oauth: None,
    };
    assert!(
        !malformed.is_unconfigured(),
        "a non-blank url is never UNCONFIGURED"
    );
    assert_eq!(
        r.connect(malformed).await.unwrap_err().to_string(),
        "connection failed: 'url' is not a valid URL. Update the server's config and reconnect."
    );
}

#[tokio::test]
async fn servers_with_tools_empty_when_no_clients() {
    // No registered clients (e.g. a server still connecting / awaiting OAuth
    // exposes no tools) → empty. Used by AgentTool's required-MCP gate.
    let r = McpRegistry::new(Arc::new(StubTransport));
    assert!(r.servers_with_tools().await.is_empty());
}

#[tokio::test]
async fn servers_pending_and_failed_classify_internal_states() {
    // The required-MCP poll-wait needs to distinguish a still-connecting
    // server from a failed/absent one — the public `McpStatus` projection
    // collapses these, so `servers_pending`/`servers_failed` read the
    // internal state map. `pending` = Connecting | AwaitingOAuth |
    // Reconnecting; `failed` = Failed. Connected/Disconnected/Stopped are
    // neither.
    let r = McpRegistry::new(Arc::new(StubTransport));
    {
        let mut c = r.connections.write().await;
        c.insert(
            "connecting".into(),
            McpConnectionState::Connecting {
                config: stdio_cfg("connecting"),
                started_at: SystemTime::now(),
            },
        );
        c.insert(
            "awaiting".into(),
            McpConnectionState::AwaitingOAuth {
                config: stdio_cfg("awaiting"),
                callback_port: 7777,
            },
        );
        c.insert(
            "reconnecting".into(),
            McpConnectionState::Reconnecting {
                config: stdio_cfg("reconnecting"),
                retry_count: 2,
                next_retry_at: SystemTime::now(),
            },
        );
        c.insert(
            "boom".into(),
            McpConnectionState::Failed {
                config: stdio_cfg("boom"),
                error: "nope".into(),
                attempts: 5,
            },
        );
        c.insert(
            "idle".into(),
            McpConnectionState::Disconnected {
                config: stdio_cfg("idle"),
                last_error: None,
            },
        );
    }
    let mut pending = r.servers_pending().await;
    pending.sort();
    assert_eq!(pending, vec!["awaiting", "connecting", "reconnecting"]);
    assert_eq!(r.servers_failed().await, vec!["boom".to_string()]);
}

#[tokio::test]
async fn snapshot_disconnected_server_no_error() {
    let r = McpRegistry::new(Arc::new(StubTransport));
    r.connections.write().await.insert(
        "memory".into(),
        McpConnectionState::Disconnected {
            config: stdio_cfg("memory"),
            last_error: None,
        },
    );
    let snap = r.snapshot().await;
    assert_eq!(
        snap,
        vec![McpServerInfo {
            name: "memory".into(),
            status: McpStatus::Disconnected,
            transport: "stdio".into(),
        }]
    );
}

#[tokio::test]
async fn snapshot_disconnected_with_error_becomes_error_status() {
    let r = McpRegistry::new(Arc::new(StubTransport));
    r.connections.write().await.insert(
        "memory".into(),
        McpConnectionState::Disconnected {
            config: stdio_cfg("memory"),
            last_error: Some("boom".into()),
        },
    );
    let snap = r.snapshot().await;
    assert_eq!(snap[0].status, McpStatus::Error("boom".into()));
}

#[tokio::test]
async fn snapshot_sorts_by_name() {
    let r = McpRegistry::new(Arc::new(StubTransport));
    {
        let mut c = r.connections.write().await;
        c.insert(
            "memory".into(),
            McpConnectionState::Disconnected {
                config: stdio_cfg("memory"),
                last_error: None,
            },
        );
        c.insert(
            "filesystem".into(),
            McpConnectionState::Disconnected {
                config: stdio_cfg("filesystem"),
                last_error: None,
            },
        );
    }
    let snap = r.snapshot().await;
    assert_eq!(snap.len(), 2);
    assert_eq!(snap[0].name, "filesystem");
    assert_eq!(snap[1].name, "memory");
}
