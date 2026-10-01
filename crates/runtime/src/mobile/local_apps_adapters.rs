//! The engine's side of the Local App service's host interfaces.
//!
//! `local_app_service::host` says what the service needs from whoever hosts it;
//! the types here provide it from the engine's own components. The service
//! never names them. [`local_apps_wire`](super::local_apps_wire) is the other
//! half of the edge: it maps the service's vocabulary onto the client protocol.

use crate::mobile::local_apps_host::AgentOutputStream;
use async_trait::async_trait;
use lingxi_core::host::{CostSnapshot, OutputStream};
use local_app_contracts::diagnostics::{
    Diagnostic, DiagnosticSeverity, DiagnosticsSettleState, DiagnosticsSettleStatus,
    FileDiagnostics,
};
use local_app_service::host::DiagnosticsProvider;
use local_app_service::publication::{
    Exposure, ManagedApp, ManagedRuntime, McpPublisher, Published,
};
use lsp::diagnostic_registry::{DiagnosticFreshness, DiagnosticSettleState};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::RwLock;

/// The confirmed native target for the host facts the client reported.
///
/// The Local App service names its own host vocabulary ([`local_apps::HostOs`],
/// [`local_apps::HostDeviceClass`]); this is the one place that maps the
/// mobile runtime's environment onto it. Both matches are exhaustive on
/// purpose: a new OS or class in the environment must be decided here, not
/// fall through to a platform nobody chose.
pub(crate) fn device_context_of(
    environment: &lingxi_core::host::MobileHostEnvironment,
) -> Option<local_apps::DeviceContext> {
    use lingxi_core::host::{MobileDeviceClass, MobileHostOs};
    let os = match environment.host_os {
        MobileHostOs::Ios => local_apps::HostOs::Ios,
        MobileHostOs::Android => local_apps::HostOs::Android,
    };
    let class = match environment.device_class {
        MobileDeviceClass::Phone => local_apps::HostDeviceClass::Phone,
        MobileDeviceClass::Tablet => local_apps::HostDeviceClass::Tablet,
        MobileDeviceClass::Unknown => local_apps::HostDeviceClass::Unknown,
    };
    local_apps::DeviceContext::from_host_facts(os, class)
}

/// Stable output sink owned by one live app Agent orchestrator. The broker
/// swaps the request-specific stream target before each serialized turn.
///
/// The engine's agent loop reports text, tool calls and the end of a turn; the
/// app's stream only ever carried the text, so the rest are accepted and
/// dropped here.
pub(crate) struct AgentOutputRouter {
    target: RwLock<Option<Arc<AgentOutputStream>>>,
}

impl AgentOutputRouter {
    pub(crate) fn new() -> Self {
        Self {
            target: RwLock::new(None),
        }
    }

    pub(crate) async fn set_target(&self, output: Arc<AgentOutputStream>) {
        *self.target.write().await = Some(output);
    }

    async fn target(&self) -> Option<Arc<AgentOutputStream>> {
        self.target.read().await.clone()
    }
}

#[async_trait]
impl OutputStream for AgentOutputRouter {
    async fn emit_text(&self, text: &str) {
        if let Some(target) = self.target().await {
            target.emit_text(text).await;
        }
    }

    async fn emit_tool_call(
        &self,
        _id: &lingxi_core::types::ToolUseId,
        _tool: &str,
        _input: &Value,
    ) {
    }

    async fn emit_tool_result(
        &self,
        _id: &lingxi_core::types::ToolUseId,
        _tool: &str,
        _model_text: &str,
        _result: &Value,
    ) {
    }

    async fn emit_end_turn(&self, _stop_reason: &str, _cost: &CostSnapshot) {}
}

/// Diagnostics from the engine's language-server registry.
///
/// Holds the registry weakly, as the broker always did: the registry outlives
/// every build that asks it, and one that is gone simply has nothing to report.
pub(crate) struct LspDiagnostics {
    registry: Weak<lsp::LspRegistry>,
}

impl LspDiagnostics {
    pub(crate) fn new(registry: Weak<lsp::LspRegistry>) -> Arc<dyn DiagnosticsProvider> {
        Arc::new(Self { registry })
    }
}

#[async_trait]
impl DiagnosticsProvider for LspDiagnostics {
    async fn settle(&self, workspace: &Path, timeout: Duration) -> Option<DiagnosticsSettleStatus> {
        let registry = self.registry.upgrade()?;
        let status = registry
            .settle_diagnostics_under_host_root(workspace, timeout)
            .await?;
        Some(DiagnosticsSettleStatus {
            state: match status.state {
                DiagnosticSettleState::NoTrackedDocuments => {
                    DiagnosticsSettleState::NoTrackedDocuments
                }
                DiagnosticSettleState::Settled => DiagnosticsSettleState::Settled,
                DiagnosticSettleState::TimedOut => DiagnosticsSettleState::TimedOut,
            },
            tracked_documents: status.tracked_documents,
        })
    }

    async fn latest(&self, workspace: &Path) -> Vec<FileDiagnostics> {
        let Some(registry) = self.registry.upgrade() else {
            return Vec::new();
        };
        registry
            .latest_diagnostics_under_host_root(workspace)
            .await
            .into_iter()
            .map(|snapshot| FileDiagnostics {
                path: snapshot.host_path,
                fresh: matches!(snapshot.freshness, DiagnosticFreshness::Fresh),
                diagnostics: snapshot.diagnostics.into_iter().map(lower).collect(),
            })
            .collect()
    }
}

fn lower(diagnostic: lsp_types::Diagnostic) -> Diagnostic {
    Diagnostic {
        severity: diagnostic.severity.map(|severity| {
            if severity == lsp_types::DiagnosticSeverity::ERROR {
                DiagnosticSeverity::Error
            } else if severity == lsp_types::DiagnosticSeverity::WARNING {
                DiagnosticSeverity::Warning
            } else if severity == lsp_types::DiagnosticSeverity::INFORMATION {
                DiagnosticSeverity::Information
            } else {
                DiagnosticSeverity::Hint
            }
        }),
        code: diagnostic.code.map(|code| match code {
            lsp_types::NumberOrString::Number(number) => number.to_string(),
            lsp_types::NumberOrString::String(text) => text,
        }),
        line: diagnostic.range.start.line,
        character: diagnostic.range.start.character,
        message: diagnostic.message,
    }
}

/// The engine's MCP registry as the place an app's tools are published.
///
/// Holds the registry weakly, as the broker always did: the registry outlives
/// every operation that reaches it, and one that is gone leaves publication
/// unavailable rather than failing.
pub(crate) struct RegistryPublisher {
    registry: Weak<mcp::McpRegistry>,
}

impl RegistryPublisher {
    pub(crate) fn new(registry: Weak<mcp::McpRegistry>) -> Arc<dyn McpPublisher> {
        Arc::new(Self { registry })
    }

    fn registry(&self) -> Result<Arc<mcp::McpRegistry>, String> {
        self.registry
            .upgrade()
            .ok_or_else(|| "the MCP registry is gone".to_string())
    }

    /// The logical server configuration that connects one conversation to one
    /// app's in-process MCP server.
    fn managed_config(
        scope: &mcp::registry::ConversationExport,
        conversation_id: &str,
    ) -> Result<mcp::McpServerConfig, String> {
        Ok(mcp::McpServerConfig {
            name: scope.server_name(),
            spec: lingxi_core::host::McpTransportSpec::InProcess {
                registry_key: scope
                    .scoped_registry_key(conversation_id)
                    .map_err(|error| error.to_string())?,
            },
            scope: mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed),
            disabled: false,
            timeout_ms: Some(crate::mobile::host::LOCAL_APPS_MCP_TIMEOUT_MS),
            always_load: true,
            discovery_cache: None,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: Default::default(),
        })
    }
}

fn scope_of(app: &ManagedApp) -> Result<mcp::registry::ConversationExport, String> {
    mcp::registry::ConversationExport::new(app.app_id.clone(), app.effective_surface_sha256.clone())
        .map_err(|error| error.to_string())
}

#[async_trait]
impl McpPublisher for RegistryPublisher {
    fn available(&self) -> bool {
        self.registry.strong_count() > 0
    }

    async fn publish(&self, app: &ManagedApp, runtime: ManagedRuntime) -> Result<(), String> {
        let registry = self.registry()?;
        registry
            .register_managed_local_app(scope_of(app)?, app.catalog_sha256.clone(), false)
            .await
            .map_err(|error| error.to_string())?;
        registry
            .set_managed_local_app_runtime(
                &app.app_id,
                runtime.enabled,
                Some(runtime.enabled_tools),
                runtime
                    .widget
                    .map(|widget| mcp::registry::ManagedLocalAppResource {
                        uri: widget.uri,
                        name: widget.name,
                        description: widget.description,
                        mime_type: widget.mime_type,
                        meta: widget.meta,
                    }),
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn unregister(&self, app_id: &str) -> Result<(), String> {
        self.registry()?
            .unregister_managed_local_app(app_id)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn disconnect_app(&self, app_id: &str) -> Result<(), String> {
        self.registry()?
            .disconnect(&format!("local_app_{app_id}"))
            .await
            .map_err(|error| error.to_string())
    }

    async fn disconnect_all(&self) -> Result<(), String> {
        let registry = self.registry()?;
        for managed in registry.managed_local_apps().await {
            registry
                .disconnect(&managed.scope.server_name())
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    async fn expose(
        &self,
        conversation_id: &str,
        app: &ManagedApp,
        pin: bool,
    ) -> Result<(), String> {
        let registry = self.registry()?;
        let scope = scope_of(app)?;
        let update = registry
            .expose_managed_local_app_with_diff(conversation_id, &app.app_id, pin)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(evicted_app_id) = update.evicted_app_id {
            registry
                .disconnect(&format!("local_app_{evicted_app_id}"))
                .await
                .map_err(|error| error.to_string())?;
        }

        let desired = Self::managed_config(&scope, conversation_id)?;
        let desired_registry_key = match &desired.spec {
            lingxi_core::host::McpTransportSpec::InProcess { registry_key } => registry_key,
            _ => unreachable!("managed Local App MCP is always in-process"),
        };
        let existing = registry.get_config(&scope.server_name()).await;
        let same_conversation_route =
            existing
                .as_ref()
                .is_some_and(|current| match &current.spec {
                    lingxi_core::host::McpTransportSpec::InProcess { registry_key } => {
                        let current_route = registry_key.rsplit_once(':').map(|(route, _)| route);
                        let desired_route = desired_registry_key
                            .rsplit_once(':')
                            .map(|(route, _)| route);
                        current_route == desired_route
                    }
                    _ => false,
                });
        if same_conversation_route {
            return Ok(());
        }
        if existing.is_some() {
            registry
                .disconnect(&scope.server_name())
                .await
                .map_err(|error| error.to_string())?;
        }
        registry
            .connect(desired)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn unpin(&self, conversation_id: &str, app_id: &str) -> Result<(), String> {
        match self
            .registry()?
            .pin_local_app_exposure(conversation_id, app_id, false)
            .await
        {
            Ok(_) | Err(lingxi_core::host::McpError::ToolNotFound(_)) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn published(&self, app_id: &str) -> Published {
        let Ok(registry) = self.registry() else {
            return Published::default();
        };
        let runtime = registry.managed_local_app_runtime(app_id).await;
        Published {
            server_name: registry
                .managed_local_app(app_id)
                .await
                .map(|managed| managed.scope.server_name()),
            enabled: runtime.as_ref().map(|runtime| runtime.enabled),
            enabled_tools: runtime.and_then(|runtime| runtime.enabled_tools),
        }
    }

    async fn exposures(&self, conversation_id: &str) -> Vec<Exposure> {
        let Ok(registry) = self.registry() else {
            return Vec::new();
        };
        registry
            .local_app_exposures(conversation_id)
            .await
            .into_iter()
            .map(|entry| Exposure {
                app_id: entry.app_id,
                pinned: entry.pinned,
                last_used: entry.last_used,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lsp_diagnostic(
        severity: Option<lsp_types::DiagnosticSeverity>,
        code: Option<lsp_types::NumberOrString>,
    ) -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: lsp_types::Range::new(
                lsp_types::Position::new(3, 7),
                lsp_types::Position::new(3, 9),
            ),
            severity,
            code,
            message: "boom".into(),
            ..Default::default()
        }
    }

    #[test]
    fn every_severity_lowers_to_its_own_and_a_missing_one_stays_missing() {
        for (lsp, expected) in [
            (
                lsp_types::DiagnosticSeverity::ERROR,
                DiagnosticSeverity::Error,
            ),
            (
                lsp_types::DiagnosticSeverity::WARNING,
                DiagnosticSeverity::Warning,
            ),
            (
                lsp_types::DiagnosticSeverity::INFORMATION,
                DiagnosticSeverity::Information,
            ),
            (
                lsp_types::DiagnosticSeverity::HINT,
                DiagnosticSeverity::Hint,
            ),
        ] {
            assert_eq!(
                lower(lsp_diagnostic(Some(lsp), None)).severity,
                Some(expected)
            );
        }
        assert_eq!(lower(lsp_diagnostic(None, None)).severity, None);
    }

    #[test]
    fn the_position_is_the_start_of_the_range_and_codes_become_text() {
        let lowered = lower(lsp_diagnostic(
            Some(lsp_types::DiagnosticSeverity::ERROR),
            Some(lsp_types::NumberOrString::Number(2304)),
        ));
        assert_eq!((lowered.line, lowered.character), (3, 7));
        assert_eq!(lowered.code.as_deref(), Some("2304"));
        assert_eq!(lowered.message, "boom");
        let named = lower(lsp_diagnostic(
            None,
            Some(lsp_types::NumberOrString::String("TS2304".into())),
        ));
        assert_eq!(named.code.as_deref(), Some("TS2304"));
    }

    fn registry() -> Arc<mcp::McpRegistry> {
        Arc::new(mcp::McpRegistry::new(Arc::new(
            crate::mobile::local_apps_mcp::LocalAppsMcpTransport::new(std::path::PathBuf::from(
                "/nonexistent-local-apps-root",
            )),
        )))
    }

    fn managed_app(app_id: &str) -> ManagedApp {
        ManagedApp {
            app_id: app_id.into(),
            catalog_sha256: "a".repeat(64),
            effective_surface_sha256: "b".repeat(64),
        }
    }

    fn runtime(enabled: bool, tools: &[&str]) -> ManagedRuntime {
        ManagedRuntime {
            enabled,
            enabled_tools: tools.iter().map(|tool| (*tool).to_string()).collect(),
            widget: None,
        }
    }

    #[tokio::test]
    async fn a_publisher_is_available_exactly_as_long_as_its_registry() {
        let registry = registry();
        let publisher = RegistryPublisher::new(Arc::downgrade(&registry));
        assert!(publisher.available());
        drop(registry);
        assert!(!publisher.available());
        // Asking an unavailable publisher to do something is a failure the
        // service reports, not a panic; reads degrade to "nothing".
        assert!(publisher
            .publish(&managed_app("abc12345"), runtime(true, &["read_value"]))
            .await
            .is_err());
        assert!(publisher.exposures("conversation-1").await.is_empty());
        assert_eq!(publisher.published("abc12345").await, Published::default());
    }

    #[tokio::test]
    async fn publishing_records_the_server_and_its_live_tools_and_unregistering_forgets_them() {
        let registry = registry();
        let publisher = RegistryPublisher::new(Arc::downgrade(&registry));
        assert_eq!(publisher.published("abc12345").await, Published::default());

        publisher
            .publish(
                &managed_app("abc12345"),
                runtime(true, &["read_value", "write_value"]),
            )
            .await
            .expect("publish");
        let published = publisher.published("abc12345").await;
        assert_eq!(published.server_name.as_deref(), Some("local_app_abc12345"));
        assert_eq!(published.enabled, Some(true));
        assert_eq!(
            published.enabled_tools,
            Some(vec!["read_value".to_string(), "write_value".to_string()])
        );

        // Publishing again with new runtime state updates it in place.
        publisher
            .publish(&managed_app("abc12345"), runtime(false, &["read_value"]))
            .await
            .expect("republish");
        let published = publisher.published("abc12345").await;
        assert_eq!(published.enabled, Some(false));
        assert_eq!(
            published.enabled_tools,
            Some(vec!["read_value".to_string()])
        );

        publisher.unregister("abc12345").await.expect("unregister");
        assert_eq!(publisher.published("abc12345").await, Published::default());
        // Unregistering what is not there is not an error.
        publisher
            .unregister("abc12345")
            .await
            .expect("unregister again");
    }

    #[tokio::test]
    async fn a_catalog_that_is_not_a_digest_is_refused() {
        let publisher = RegistryPublisher::new(Arc::downgrade(&registry()));
        let mut app = managed_app("abc12345");
        app.catalog_sha256 = "not-a-digest".into();
        assert!(publisher.publish(&app, runtime(true, &[])).await.is_err());
    }

    #[tokio::test]
    async fn exposures_are_listed_and_unpinning_an_unexposed_app_is_not_an_error() {
        let registry = registry();
        let publisher = RegistryPublisher::new(Arc::downgrade(&registry));
        publisher
            .publish(&managed_app("abc12345"), runtime(true, &["read_value"]))
            .await
            .expect("publish");
        registry
            .expose_managed_local_app("conversation-1", "abc12345", true)
            .await
            .expect("expose");

        let exposures = publisher.exposures("conversation-1").await;
        assert_eq!(exposures.len(), 1);
        assert_eq!(exposures[0].app_id, "abc12345");
        assert!(exposures[0].pinned);
        assert!(publisher.exposures("conversation-2").await.is_empty());

        publisher
            .unpin("conversation-1", "abc12345")
            .await
            .expect("unpin");
        assert!(!publisher.exposures("conversation-1").await[0].pinned);
        publisher
            .unpin("conversation-1", "zzzz9999")
            .await
            .expect("unpinning an app that is not exposed");
    }

    #[tokio::test]
    async fn disconnecting_with_nothing_connected_succeeds() {
        let registry = registry();
        let publisher = RegistryPublisher::new(Arc::downgrade(&registry));
        publisher
            .publish(&managed_app("abc12345"), runtime(true, &["read_value"]))
            .await
            .expect("publish");
        publisher.disconnect_app("abc12345").await.expect("one");
        publisher.disconnect_all().await.expect("all");
        // Disconnecting does not forget the app.
        assert!(publisher.published("abc12345").await.server_name.is_some());
    }

    #[tokio::test]
    async fn a_registry_that_is_gone_reports_nothing() {
        let provider = LspDiagnostics::new(Weak::new());
        assert!(provider
            .settle(Path::new("/workspace"), Duration::from_millis(1))
            .await
            .is_none());
        assert!(provider.latest(Path::new("/workspace")).await.is_empty());
    }
}
