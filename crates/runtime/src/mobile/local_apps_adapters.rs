//! The engine's side of the Local App service's host interfaces.
//!
//! `local_app_service::host` says what the service needs from whoever hosts it;
//! the types here provide it from the engine's own components. The service
//! never names them. [`local_apps_wire`](super::local_apps_wire) is the other
//! half of the edge: it maps the service's vocabulary onto the client protocol.

use async_trait::async_trait;
use local_app_contracts::diagnostics::{
    Diagnostic, DiagnosticSeverity, DiagnosticsSettleState, DiagnosticsSettleStatus,
    FileDiagnostics,
};
use local_app_service::host::DiagnosticsProvider;
use lsp::diagnostic_registry::{DiagnosticFreshness, DiagnosticSettleState};
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;

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
