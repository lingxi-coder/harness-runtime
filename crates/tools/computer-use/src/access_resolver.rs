//! `request_access` interactive resolution.
//!
//! Mirrors `tool-ui`'s `AskUserQuestionResolver`: the tool itself owns the
//! interactivity (constructing a request, pausing on a channel, resuming with
//! the user's real answer) rather than routing through the generic
//! `check_permissions` → `PermissionResult::Ask` path, which can only express
//! a title/message/options list — not per-app checkboxes, a tier selector,
//! three independent capability flags, or a TCC missing-permissions panel.

use async_trait::async_trait;
use permission::computer_access::{
    ComputerAccessExchange, ComputerAccessRequest, ComputerAccessResponse,
};

/// Resolves one `request_access` call to the user's actual decision.
#[async_trait]
pub trait ComputerAccessResolver: Send + Sync {
    /// Present `request` to the user (however this resolver is wired) and
    /// return their decision. Never errors — any failure mode (no UI wired,
    /// closed channel, dropped exchange) resolves to the fully-denied
    /// default, since a permission grant with nowhere to ask a human must
    /// fail closed, not silently auto-allow.
    async fn resolve(&self, request: ComputerAccessRequest) -> ComputerAccessResponse;
}

/// Hermetic default: denies everything. Used whenever no live TUI resolver
/// is wired (mobile, headless/non-interactive sessions, tests that don't
/// construct their own resolver) — the safe failure mode for a
/// permission-relevant prompt with no human to show it to.
pub struct DenyAllResolver;

#[async_trait]
impl ComputerAccessResolver for DenyAllResolver {
    async fn resolve(&self, _request: ComputerAccessRequest) -> ComputerAccessResponse {
        ComputerAccessResponse::default()
    }
}

/// Grants exactly what was requested, no human involved — mirrors
/// `tool_ui::ask_user_question::FirstOptionResolver`'s role: a hermetic
/// stand-in for harnesses that want the tool's OWN permission bookkeeping
/// exercised (grant → tier lookup → enforcement) without driving a real
/// interactive dialog. NOT the production default (that's
/// [`DenyAllResolver`] — an unattended grant of computer-control access is
/// never the safe default), but legitimate for tests and offline tooling.
pub struct AutoGrantResolver;

#[async_trait]
impl ComputerAccessResolver for AutoGrantResolver {
    async fn resolve(&self, request: ComputerAccessRequest) -> ComputerAccessResponse {
        ComputerAccessResponse {
            granted_apps: request.apps.into_iter().map(|a| a.label).collect(),
            clipboard_read: request.clipboard_read,
            clipboard_write: request.clipboard_write,
            system_key_combos: request.system_key_combos,
        }
    }
}

/// Live-TUI resolver: sends the request over `event_tx` and awaits the
/// answer on a fresh one-shot channel, exactly the round-trip
/// `tool_ui::ask_user_question::TuiBridgeResolver` performs for
/// `AskUserQuestion`.
pub struct TuiBridgeResolver {
    event_tx: tokio::sync::mpsc::Sender<ComputerAccessExchange>,
}

impl TuiBridgeResolver {
    /// Construct a resolver that sends exchanges over `event_tx`.
    #[must_use]
    pub fn new(event_tx: tokio::sync::mpsc::Sender<ComputerAccessExchange>) -> Self {
        Self { event_tx }
    }
}

#[async_trait]
impl ComputerAccessResolver for TuiBridgeResolver {
    async fn resolve(&self, request: ComputerAccessRequest) -> ComputerAccessResponse {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let exchange = ComputerAccessExchange { request, resp_tx };
        if self.event_tx.send(exchange).await.is_err() {
            // No TUI listening (channel closed) — fail closed.
            return ComputerAccessResponse::default();
        }
        // A dropped `resp_tx` (Esc / view dropped unresolved) resolves to
        // `Err` here, which we also map to fully-denied.
        resp_rx.await.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use permission::computer_access::AccessTier;

    fn sample_request() -> ComputerAccessRequest {
        ComputerAccessRequest {
            reason: "test".into(),
            apps: vec![],
            tier: AccessTier::Full,
            clipboard_read: false,
            clipboard_write: false,
            system_key_combos: false,
            tcc_state: None,
        }
    }

    #[tokio::test]
    async fn deny_all_resolver_always_denies() {
        let resolver = DenyAllResolver;
        let response = resolver.resolve(sample_request()).await;
        assert!(response.granted_apps.is_empty());
        assert!(!response.clipboard_read);
    }

    #[tokio::test]
    async fn tui_bridge_resolver_fails_closed_when_channel_is_closed() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx); // nobody listening
        let resolver = TuiBridgeResolver::new(tx);
        let response = resolver.resolve(sample_request()).await;
        assert!(response.granted_apps.is_empty());
    }

    #[tokio::test]
    async fn tui_bridge_resolver_relays_the_real_response() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let resolver = TuiBridgeResolver::new(tx);
        let handle = tokio::spawn(async move { resolver.resolve(sample_request()).await });
        let exchange = rx.recv().await.expect("exchange sent");
        exchange
            .resp_tx
            .send(ComputerAccessResponse {
                granted_apps: vec!["com.example.app".into()],
                clipboard_read: true,
                clipboard_write: false,
                system_key_combos: false,
            })
            .unwrap();
        let response = handle.await.unwrap();
        assert_eq!(response.granted_apps, vec!["com.example.app".to_string()]);
        assert!(response.clipboard_read);
    }

    #[tokio::test]
    async fn tui_bridge_resolver_fails_closed_when_dropped_unresolved() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let resolver = TuiBridgeResolver::new(tx);
        let handle = tokio::spawn(async move { resolver.resolve(sample_request()).await });
        let exchange = rx.recv().await.expect("exchange sent");
        drop(exchange); // simulates Esc: resp_tx dropped unsent
        let response = handle.await.unwrap();
        assert!(response.granted_apps.is_empty());
    }
}
