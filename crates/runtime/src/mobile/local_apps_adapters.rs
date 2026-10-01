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
