//! Model-facing Mods `agent.offer` filtering.

use futures::future::join_all;
use hooks::mods::{ModError, ModHost, ModSessionContext};
use lingxi_core::host::subagent_spawn::{AgentOfferCandidate, SubagentListingEntry};
use serde_json::{Value, json};
use std::sync::Arc;

/// The call context inputs used by native `Brt(context)` to bypass offer
/// filtering only for a local top-level hook caller. The current Rust prompt
/// and reminder seams do not expose these per-call values, so production
/// callers use the default context instead of inferring them from session
/// identity or the static tool context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentOfferContext {
    pub agent_id_present: bool,
    pub for_remote_execution: bool,
    pub hook_caller_present: bool,
}

impl AgentOfferContext {
    /// Native `Enr(context) !== undefined`:
    /// `agentId === undefined && forRemoteExecution !== true && hookCaller`.
    #[must_use]
    pub fn bypasses_filter(self) -> bool {
        !self.agent_id_present && !self.for_remote_execution && self.hook_caller_present
    }
}

/// Apply the native per-agent `agent.offer` filter to model-facing rows.
/// Missing provider provenance and dispatch errors fail open to the core
/// default (`{ isOffered: true }`); only the literal boolean `true` retains a
/// successful answer, matching `MGn`'s `result?.isOffered === true`.
pub async fn filter_agent_offer_candidates(
    candidates: Vec<AgentOfferCandidate>,
    host: Option<Arc<ModHost>>,
    context: AgentOfferContext,
) -> Vec<SubagentListingEntry> {
    let Some(host) = host.filter(|host| host.has_event("agent.offer")) else {
        return candidates
            .into_iter()
            .map(|candidate| candidate.listing)
            .collect();
    };
    if context.bypasses_filter() {
        return candidates
            .into_iter()
            .map(|candidate| candidate.listing)
            .collect();
    }

    join_all(candidates.into_iter().map(|candidate| {
        let host = Arc::clone(&host);
        async move {
            let offered = match agent_offer_input(&candidate) {
                None => {
                    tracing::debug!(
                        agent_type = %candidate.listing.agent_type,
                        source = %candidate.source,
                        "keeping agent without recoverable provider provenance"
                    );
                    true
                }
                Some(input) => match dispatch_agent_offer(&host, input).await {
                    Ok(result) => successful_offer_result(&result),
                    Err(error) => {
                        tracing::warn!(
                            agent_type = %candidate.listing.agent_type,
                            %error,
                            "agent.offer Mod failed; using core offer default"
                        );
                        true
                    }
                },
            };
            (candidate.listing, offered)
        }
    }))
    .await
    .into_iter()
    .filter_map(|(listing, offered)| offered.then_some(listing))
    .collect()
}

fn agent_offer_input(candidate: &AgentOfferCandidate) -> Option<Value> {
    let provider = candidate.provider.as_ref()?;
    Some(json!({
        "agent": candidate.listing.agent_type,
        "description": candidate.listing.when_to_use,
        "source": candidate.source,
        "provider": provider,
    }))
}

fn successful_offer_result(result: &Value) -> bool {
    result.get("isOffered") == Some(&Value::Bool(true))
}

async fn dispatch_agent_offer(host: &ModHost, input: Value) -> Result<Value, ModError> {
    let pinned_agent = input.get("agent").cloned();
    let pinned_source = input.get("source").cloned();
    let pinned_provider = input.get("provider").cloned();
    let core = move |forwarded: Value| {
        let pinned_agent = pinned_agent.clone();
        let pinned_source = pinned_source.clone();
        let pinned_provider = pinned_provider.clone();
        async move {
            if forwarded.get("agent") != pinned_agent.as_ref()
                || forwarded.get("source") != pinned_source.as_ref()
                || forwarded.get("provider") != pinned_provider.as_ref()
            {
                return Err(ModError::Hook(
                    "agent.offer agent, source, and provider are pinned".into(),
                ));
            }
            Ok(json!({"isOffered":true}))
        }
    };

    let Some(session) = host.bound_session() else {
        return host.dispatch("agent.offer", input, core).await;
    };
    let log_session: Arc<dyn ModSessionContext> = Arc::clone(&session);
    let toast_session = Arc::clone(&log_session);
    let status_session = Arc::clone(&log_session);
    host.dispatch_with_ui_at_session(
        "agent.offer",
        input,
        session.as_ref(),
        core,
        move |plugin, text| {
            let session = Arc::clone(&log_session);
            async move { session.emit_mod_log(&plugin, &text).await }
        },
        move |plugin, text, timeout_ms| {
            let session = Arc::clone(&toast_session);
            async move { session.emit_mod_toast(&plugin, &text, timeout_ms).await }
        },
        move |plugin, text| {
            let session = Arc::clone(&status_session);
            async move { session.emit_mod_status(&plugin, text.as_deref()).await }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(agent_type: &str, provider: Option<Value>) -> AgentOfferCandidate {
        AgentOfferCandidate {
            listing: SubagentListingEntry {
                agent_type: agent_type.to_string(),
                when_to_use: format!("use {agent_type}"),
                when_to_use_lean: None,
                tools_description: "All tools".into(),
            },
            source: "built-in".into(),
            provider,
        }
    }

    #[test]
    fn agent_offer_uses_literal_true_and_errors_fail_open() {
        assert!(successful_offer_result(&json!({"isOffered":true})));
        assert!(!successful_offer_result(&json!({"isOffered":false})));
        assert!(!successful_offer_result(&json!({"isOffered":"true"})));
        assert!(!successful_offer_result(&json!({"other":true})));
        // Dispatch errors are handled by `filter_agent_offer_candidates` as
        // the native core default, independent of result-shape validation.
        let failed: Result<Value, ModError> = Err(ModError::Hook("failed".into()));
        assert!(failed.map_or(true, |result| successful_offer_result(&result)));
    }

    #[test]
    fn native_hook_caller_bypass_requires_local_top_level_context() {
        let hook = AgentOfferContext {
            hook_caller_present: true,
            ..AgentOfferContext::default()
        };
        assert!(hook.bypasses_filter());
        assert!(
            !AgentOfferContext {
                agent_id_present: true,
                ..hook
            }
            .bypasses_filter()
        );
        assert!(
            !AgentOfferContext {
                for_remote_execution: true,
                ..hook
            }
            .bypasses_filter()
        );
        assert!(!AgentOfferContext::default().bypasses_filter());
    }

    #[test]
    fn agent_offer_input_keeps_native_shape_and_provenance() {
        let explore = candidate("Explore", Some(json!({"plugin":"engine","tier":"core"})));
        assert_eq!(
            agent_offer_input(&explore),
            Some(json!({
                "agent":"Explore",
                "description":"use Explore",
                "source":"built-in",
                "provider":{"plugin":"engine","tier":"core"},
            }))
        );
        assert!(agent_offer_input(&candidate("unknown-provider", None)).is_none());
    }

    #[tokio::test]
    async fn plugin_without_installed_identity_uses_native_source_fallback_in_mod_input() {
        let mut definition = crate::builtins::workflow_subagent_definition();
        definition.agent_type = "unresolved:review".into();
        definition.source = crate::definition::AgentSource::Plugin;
        definition.offer_provider = None;
        let candidates = crate::agent_listing_candidates(&[definition]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].source, "plugin");
        assert_eq!(
            candidates[0].provider,
            Some(json!({"plugin":"plugin","tier":"user"}))
        );

        let directory = tempfile::tempdir().unwrap();
        let module = directory.path().join("plugin-offer.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('agent.offer', { agent: 'unresolved:review' }, ($, event) => ({
                isOffered: event.source === 'plugin'
                  && event.provider.plugin === 'plugin'
                  && event.provider.tier === 'user' ? false : true
              }));
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("plugin-offer", directory.path(), &module, json!({}))
            .await
            .unwrap();

        let visible =
            filter_agent_offer_candidates(candidates, Some(host), AgentOfferContext::default())
                .await;
        assert!(
            visible.is_empty(),
            "the candidate reaches Mod with cQ fallback"
        );
    }

    #[tokio::test]
    async fn mods_hide_candidates_and_hook_errors_use_core_true() {
        let directory = tempfile::tempdir().unwrap();
        let module = directory.path().join("agent-offer.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('agent.offer', { agent: 'hidden' }, () => ({ isOffered: false }));
              on('agent.offer', { agent: 'throws' }, () => { throw new Error('fail open'); });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("agent-offer", directory.path(), &module, json!({}))
            .await
            .unwrap();

        let candidates = vec![
            candidate("hidden", Some(json!({"plugin":"engine","tier":"core"}))),
            candidate("throws", Some(json!({"plugin":"engine","tier":"core"}))),
            candidate("missing-provider", None),
        ];
        let result =
            filter_agent_offer_candidates(candidates, Some(host), AgentOfferContext::default())
                .await;
        let visible: Vec<_> = result
            .iter()
            .map(|entry| entry.agent_type.as_str())
            .collect();
        assert_eq!(visible, vec!["throws", "missing-provider"]);
    }
}
