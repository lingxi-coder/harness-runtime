use super::*;
use crate::connection::{ConfigScope, McpServerConfig};
use async_trait::async_trait;
use platform_api::{
    ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
    McpRawConnection, McpResourceContentDto, McpResourceDto, McpServerInfo, McpStatus, McpToolDto,
    McpToolResultDto, McpTransport, McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
};
use protocol::McpConnectionId as ConnId;
use serde_json::Value;
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
        _i: Value,
    ) -> Result<McpToolResultDto, McpError> {
        unreachable!()
    }
    async fn read_resource(
        &self,
        _c: &McpRawConnection,
        _u: &str,
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
        scope: ConfigScope::Settings(protocol::SettingsScope::Project),
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
        &platform_api::McpNegotiatedProtocol {
            era: platform_api::McpProtocolEra::Legacy,
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
        &platform_api::McpNegotiatedProtocol {
            era: platform_api::McpProtocolEra::Legacy,
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
        headers: platform_api::McpHeaders::default(),
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
        headers: platform_api::McpHeaders::default(),
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
        headers: platform_api::McpHeaders::default(),
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

#[test]
fn conversation_export_identity_preserves_hyphens_and_split_boundaries() {
    let scope = ConversationExport::new("abc--1", "0".repeat(64)).unwrap();
    assert_eq!(scope.server_name(), "local_app_abc--1");
    assert_eq!(scope.server_info_name(), "lingxi-local-app");
    assert_eq!(
        scope.registry_key(),
        "local_apps:conversation-export:abc--1"
    );
    assert_eq!(
        scope.tool_full_name("read_value").unwrap(),
        "mcp__local_app_abc--1__read_value"
    );
    assert!(ConversationExport::new("abc_1", "0".repeat(64)).is_err());
    assert!(scope.tool_full_name("bad__name").is_err());
}

#[test]
fn conversation_export_uses_schema_v3_app_id_boundaries() {
    let id_54 = format!("a{}", "b".repeat(53));
    let id_55 = format!("a{}", "b".repeat(54));
    assert!(ConversationExport::new(id_54, "0".repeat(64)).is_ok());
    assert!(ConversationExport::new(id_55, "0".repeat(64)).is_err());
    assert!(ConversationExport::new("A123", "0".repeat(64)).is_err());
    assert!(ConversationExport::new("-leading", "0".repeat(64)).is_err());
}

#[tokio::test]
async fn managed_local_apps_share_one_physical_hub_and_notify_changed_catalog_partitions() {
    let registry = McpRegistry::new(Arc::new(StubTransport));
    let mut events = registry.subscribe_catalog_changes();
    for index in 0..100 {
        let scope = ConversationExport::new(format!("app-{index}"), "0".repeat(64)).unwrap();
        registry
            .register_managed_local_app(scope, "1".repeat(64), true)
            .await
            .unwrap();
        let change = events.recv().await.unwrap();
        assert_eq!(change.server_name, format!("local_app_app-{index}"));
    }
    assert_eq!(registry.managed_local_app_count().await, 100);
    assert_eq!(registry.physical_transport_count(), 1);
    let scope = ConversationExport::new("app-0", "0".repeat(64)).unwrap();
    registry
        .register_managed_local_app(scope, "2".repeat(64), false)
        .await
        .unwrap();
    assert_eq!(events.recv().await.unwrap().kind, McpCatalogKind::Resources);
    // A bad `surface_changed=true` hint cannot duplicate the event when
    // the committed surface digest is unchanged.
    let same_surface = ConversationExport::new("app-0", "0".repeat(64)).unwrap();
    let refreshed = registry
        .register_managed_local_app(same_surface, "3".repeat(64), true)
        .await
        .unwrap();
    assert_eq!(refreshed.surface_generation, 1);
    assert_eq!(events.recv().await.unwrap().kind, McpCatalogKind::Resources);
    let changed_surface = ConversationExport::new("app-0", "f".repeat(64)).unwrap();
    let changed = registry
        .register_managed_local_app(changed_surface, "4".repeat(64), false)
        .await
        .unwrap();
    assert_eq!(changed.surface_generation, 2);
    let tools = events.recv().await.unwrap();
    assert_eq!(tools.server_name, "local_app_app-0");
    assert_eq!(tools.kind, McpCatalogKind::Tools);
    assert_eq!(events.recv().await.unwrap().kind, McpCatalogKind::Resources);
    assert!(registry
        .unregister_managed_local_app("app-0")
        .await
        .unwrap());
}

#[tokio::test]
async fn managed_local_app_catalog_refresh_notifies_resources_only() {
    let registry = McpRegistry::new(Arc::new(StubTransport));
    let mut events = registry.subscribe_catalog_changes();
    let scope = ConversationExport::new("app-0", "0".repeat(64)).unwrap();
    registry
        .register_managed_local_app(scope.clone(), "1".repeat(64), false)
        .await
        .unwrap();
    assert_eq!(events.recv().await.unwrap().kind, McpCatalogKind::Tools);
    registry
        .register_managed_local_app(scope, "2".repeat(64), false)
        .await
        .unwrap();
    let event = events.recv().await.unwrap();
    assert_eq!(event.server_name, "local_app_app-0");
    assert_eq!(event.kind, McpCatalogKind::Resources);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), events.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn local_app_exposure_is_bounded_lru_and_pin_aware() {
    let registry = McpRegistry::new(Arc::new(StubTransport));
    for index in 0..10 {
        let scope = ConversationExport::new(format!("app-{index}"), "0".repeat(64)).unwrap();
        registry
            .register_managed_local_app(scope, "1".repeat(64), false)
            .await
            .unwrap();
    }

    assert!(registry
        .local_app_exposures("conversation")
        .await
        .is_empty());
    for index in 0..8 {
        registry
            .expose_managed_local_app("conversation", &format!("app-{index}"), false)
            .await
            .unwrap();
    }
    assert_eq!(registry.local_app_exposures("conversation").await.len(), 8);

    // app-0 is the oldest unpinned entry and is the only one evicted.
    registry
        .expose_managed_local_app("conversation", "app-8", false)
        .await
        .unwrap();
    let ids: Vec<String> = registry
        .local_app_exposures("conversation")
        .await
        .into_iter()
        .map(|entry| entry.app_id)
        .collect();
    assert!(!ids.iter().any(|id| id == "app-0"));
    assert!(ids.iter().any(|id| id == "app-8"));

    // Pinning is explicit. The next eviction skips app-1 even though it
    // is older than the unpinned entries.
    registry
        .pin_local_app_exposure("conversation", "app-1", true)
        .await
        .unwrap();
    registry
        .expose_managed_local_app("conversation", "app-9", false)
        .await
        .unwrap();
    let ids: Vec<String> = registry
        .local_app_exposures("conversation")
        .await
        .into_iter()
        .map(|entry| entry.app_id)
        .collect();
    assert!(ids.iter().any(|id| id == "app-1"));
    assert!(ids.iter().any(|id| id == "app-9"));
}

#[tokio::test]
async fn local_app_exposure_rejects_ninth_when_all_are_pinned_and_tracks_calls() {
    let registry = McpRegistry::new(Arc::new(StubTransport));
    for index in 0..9 {
        let scope = ConversationExport::new(format!("pin-{index}"), "0".repeat(64)).unwrap();
        registry
            .register_managed_local_app(scope, "1".repeat(64), false)
            .await
            .unwrap();
    }
    for index in 0..8 {
        registry
            .expose_managed_local_app("conversation", &format!("pin-{index}"), true)
            .await
            .unwrap();
    }
    let err = registry
        .expose_managed_local_app("conversation", "pin-8", false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("exposure_capacity_reached"));
    assert!(err.to_string().contains("pin-0"));

    for _ in 0..4 {
        registry
            .begin_local_app_call("conversation", "pin-0")
            .await
            .unwrap();
    }
    let err = registry
        .begin_local_app_call("conversation", "pin-0")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("rate_limited"));
    for _ in 0..5 {
        registry.end_local_app_call("conversation", "pin-0").await;
    }
    assert_eq!(
        registry
            .local_app_exposures("conversation")
            .await
            .iter()
            .find(|entry| entry.app_id == "pin-0")
            .unwrap()
            .in_flight,
        0
    );
}

#[tokio::test]
async fn local_app_exposure_never_evicts_an_inflight_entry() {
    let registry = McpRegistry::new(Arc::new(StubTransport));
    for index in 0..9 {
        let scope = ConversationExport::new(format!("busy-{index}"), "0".repeat(64)).unwrap();
        registry
            .register_managed_local_app(scope, "1".repeat(64), false)
            .await
            .unwrap();
    }
    for index in 0..8 {
        registry
            .expose_managed_local_app("conversation", &format!("busy-{index}"), false)
            .await
            .unwrap();
    }
    registry
        .begin_local_app_call("conversation", "busy-0")
        .await
        .unwrap();
    registry
        .expose_managed_local_app("conversation", "busy-8", false)
        .await
        .unwrap();
    let exposed = registry.local_app_exposures("conversation").await;
    assert!(exposed.iter().any(|entry| entry.app_id == "busy-0"));
    assert!(exposed.iter().any(|entry| entry.app_id == "busy-8"));
    assert_eq!(
        exposed
            .iter()
            .find(|entry| entry.app_id == "busy-0")
            .unwrap()
            .in_flight,
        1
    );
    registry.end_local_app_call("conversation", "busy-0").await;
}

#[tokio::test]
async fn deleting_managed_local_app_removes_all_conversation_exposure() {
    let registry = McpRegistry::new(Arc::new(StubTransport));
    let scope = ConversationExport::new("delete-me", "0".repeat(64)).unwrap();
    registry
        .register_managed_local_app(scope, "1".repeat(64), false)
        .await
        .unwrap();
    registry
        .expose_managed_local_app("conversation", "delete-me", true)
        .await
        .unwrap();
    assert!(registry
        .unregister_managed_local_app("delete-me")
        .await
        .unwrap());
    assert!(registry
        .local_app_exposures("conversation")
        .await
        .is_empty());
}

#[tokio::test]
async fn disabling_managed_local_app_clears_exposure_and_emits_changes() {
    let registry = McpRegistry::new(Arc::new(StubTransport));
    let mut events = registry.subscribe_catalog_changes();
    registry
        .register_managed_local_app(
            ConversationExport::new("toggle-me", "0".repeat(64)).unwrap(),
            "1".repeat(64),
            false,
        )
        .await
        .unwrap();
    let _ = events.recv().await.unwrap();
    registry
        .expose_managed_local_app("conversation", "toggle-me", true)
        .await
        .unwrap();
    let runtime = registry
        .set_managed_local_app_runtime(
            "toggle-me",
            false,
            Some(vec!["read_value".into()]),
            Some(ManagedLocalAppResource {
                uri: "ui://local-app/toggle-me/0fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff/mcp-app.html".into(),
                name: "Toggle".into(),
                description: None,
                mime_type: Some("text/html;profile=mcp-app".into()),
                meta: None,
            }),
        )
        .await
        .unwrap();
    assert!(!runtime.enabled);
    assert!(registry
        .local_app_exposures("conversation")
        .await
        .is_empty());
    assert_eq!(events.recv().await.unwrap().kind, McpCatalogKind::Tools);
    assert_eq!(events.recv().await.unwrap().kind, McpCatalogKind::Resources);
    let err = registry
        .expose_managed_local_app("conversation", "toggle-me", false)
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::ToolNotFound(_)));
}

#[tokio::test]
async fn expose_managed_local_app_reports_the_evicted_entry() {
    let registry = McpRegistry::new(Arc::new(StubTransport));
    for index in 0..9 {
        registry
            .register_managed_local_app(
                ConversationExport::new(format!("diff-{index}"), "0".repeat(64)).unwrap(),
                "1".repeat(64),
                false,
            )
            .await
            .unwrap();
    }
    for index in 0..8 {
        registry
            .expose_managed_local_app("conversation", &format!("diff-{index}"), false)
            .await
            .unwrap();
    }
    let update = registry
        .expose_managed_local_app_with_diff("conversation", "diff-8", false)
        .await
        .unwrap();
    assert_eq!(update.exposure.app_id, "diff-8");
    assert_eq!(update.evicted_app_id.as_deref(), Some("diff-0"));
}

#[tokio::test]
async fn managed_local_apps_snapshot_is_sorted() {
    let registry = McpRegistry::new(Arc::new(StubTransport));
    for app_id in ["b-app", "a-app"] {
        registry
            .register_managed_local_app(
                ConversationExport::new(app_id, "0".repeat(64)).unwrap(),
                "1".repeat(64),
                false,
            )
            .await
            .unwrap();
    }
    let apps = registry.managed_local_apps().await;
    assert_eq!(
        apps.iter()
            .map(|server| server.scope.app_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a-app", "b-app"]
    );
}
