//! Orchestrator-side [`mcp::HookDispatcher`] implementation.
//!
//! The `mcp` crate is deliberately decoupled from the `hooks` crate (see
//! `mcp::hook_dispatch`). This adapter closes that seam from the orchestrator
//! side: it owns an `Arc<hooks::HookExecutorImpl>` (the engine's `orch.hooks`)
//! plus the engine cwd, translates an incoming [`mcp::ElicitationHookRequest`]
//! into a `hooks::HookEvent::Elicitation`, fires the registry, and folds the
//! resulting [`hooks::AggregateHookResult`] into an
//! [`mcp::ElicitationHookOutcome`].
//!
//! This reproduces claude-code's `runElicitationHooks`
//! (`services/mcp/elicitationHandler.ts:214-257`):
//!
//! * a `blockingError` (top-level `decision: block`, exit-2, or
//!   `action: 'decline'`) => `{ action: 'decline' }` ([`Deny`]);
//! * an `elicitationResponse` => `{ action, content }` ([`Respond`]);
//! * neither => fall through to the host default ([`Pass`]).
//!
//! [`Deny`]: mcp::ElicitationHookOutcome::Deny
//! [`Respond`]: mcp::ElicitationHookOutcome::Respond
//! [`Pass`]: mcp::ElicitationHookOutcome::Pass
//!
//! Wiring: the composition root builds an [`OrchestratorHookDispatcher`] over
//! the SAME `Arc<hooks::HookExecutorImpl>` it hands the orchestrator, then
//! injects it via `McpRegistry::with_hook_dispatcher` (an `Option`, default
//! `None` => the inbound handler's current `{"action":"cancel"}` behavior).
//!
//! Best-effort: the executor never errors out of `execute`, so a misbehaving /
//! absent hook always degrades to [`Pass`] and never breaks the elicitation
//! flow.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use hooks::events::{ElicitationMode, HookEvent};
use hooks::registry::HookContext;
use hooks::response::HookDecision;
use hooks::HookExecutorImpl;
use mcp::{ElicitationHookOutcome, ElicitationHookRequest, HookDispatcher};
use serde_json::json;

/// Adapts the engine's hook executor to the MCP inbound dispatch seam.
pub struct OrchestratorHookDispatcher {
    /// The SAME executor the orchestrator fires its other hooks through.
    hooks: Arc<HookExecutorImpl>,
    /// Engine cwd, threaded into the `Elicitation` hook payload (`cwd`) and the
    /// per-hook Command-arm `LINGXI_PROJECT_DIR` fallback.
    cwd: PathBuf,
    /// The MAIN orchestrator session's transcript path
    /// (`<config_home>/projects/<sanitize(cwd)>/<uuid>.jsonl`, claude-code
    /// `getTranscriptPathForSession`), stamped on the `Elicitation` hook payload's
    /// `transcript_path` (FIX B). Empty for builds wiring neither.
    transcript_path: PathBuf,
}

impl OrchestratorHookDispatcher {
    /// Build a dispatcher over the shared hook executor, engine cwd, and the main
    /// session's transcript path. Pass the SAME `Arc<HookExecutorImpl>` handed to
    /// the orchestrator so the `Elicitation` hook rides the identical registry /
    /// async / sandbox plumbing.
    #[must_use]
    pub fn new(hooks: Arc<HookExecutorImpl>, cwd: PathBuf, transcript_path: PathBuf) -> Self {
        Self {
            hooks,
            cwd,
            transcript_path,
        }
    }

    /// Map the raw wire `mode` string onto the hooks `ElicitationMode`.
    /// claude-code normalizes anything that is not exactly `"url"` to `"form"`
    /// (`runElicitationHooks`: `params.mode === 'url' ? 'url' : 'form'`).
    fn map_mode(mode: Option<&str>) -> Option<ElicitationMode> {
        match mode {
            Some("url") => Some(ElicitationMode::Url),
            Some(_) => Some(ElicitationMode::Form),
            None => None,
        }
    }
}

#[async_trait]
impl HookDispatcher for OrchestratorHookDispatcher {
    async fn dispatch_elicitation(
        &self,
        request: ElicitationHookRequest,
    ) -> ElicitationHookOutcome {
        let event = HookEvent::Elicitation {
            server_name: request.server_name,
            message: request.message,
            mode: Self::map_mode(request.mode.as_deref()),
            url: request.url,
            elicitation_id: request.elicitation_id,
            requested_schema: request.requested_schema,
        };
        // Minimal context: the inbound elicitation path has no live per-turn
        // session, so we thread the engine cwd (also used as the LINGXI_PROJECT_DIR
        // fallback) and the main session's `transcript_path` (FIX B). Everything
        // else defaults — matching the orchestrator's other "context-light" fires.
        let ctx = HookContext {
            cwd: self.cwd.clone(),
            transcript_path: self.transcript_path.clone(),
            ..Default::default()
        };

        let agg = self.hooks.execute(event, ctx).await;

        // Mapping mirrors `runElicitationHooks` (elicitationHandler.ts:241-252).
        // A Block decision is the Rust surface of claude-code's `blockingError`
        // (top-level `decision: block`, exit-2, or `action: 'decline'`).
        if matches!(agg.decision, Some(HookDecision::Block)) {
            return ElicitationHookOutcome::Deny;
        }
        if let Some(er) = agg.elicitation_response {
            // Build the answer object the handler returns verbatim: always an
            // `action`, plus `content` only when present (skip-if-none keeps the
            // wire bytes minimal, matching the optional `content` field).
            let answer = match er.content {
                Some(content) => json!({ "action": er.action, "content": content }),
                None => json!({ "action": er.action }),
            };
            return ElicitationHookOutcome::Respond(answer);
        }
        ElicitationHookOutcome::Pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::noop_hook_executor;

    #[tokio::test]
    async fn no_matching_hook_passes() {
        // An executor with an empty registry never intervenes => Pass, so the
        // handler keeps its default behavior (the "no-op" contract).
        let dispatcher = OrchestratorHookDispatcher::new(
            noop_hook_executor(),
            PathBuf::from("/work"),
            PathBuf::from("/work/.t.jsonl"),
        );
        let outcome = dispatcher
            .dispatch_elicitation(ElicitationHookRequest {
                server_name: "github".into(),
                message: "Authorize?".into(),
                ..Default::default()
            })
            .await;
        assert_eq!(outcome, ElicitationHookOutcome::Pass);
    }

    #[test]
    fn map_mode_normalizes_non_url_to_form() {
        assert_eq!(
            OrchestratorHookDispatcher::map_mode(Some("url")),
            Some(ElicitationMode::Url)
        );
        assert_eq!(
            OrchestratorHookDispatcher::map_mode(Some("form")),
            Some(ElicitationMode::Form)
        );
        assert_eq!(
            OrchestratorHookDispatcher::map_mode(Some("anything")),
            Some(ElicitationMode::Form)
        );
        assert_eq!(OrchestratorHookDispatcher::map_mode(None), None);
    }
}
