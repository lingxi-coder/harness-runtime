//! Main-request cache attribution using finalized provider-request evidence.

use cost::prompt_cache_ledger::{CacheTtl, MissAttribution, MissCause, RequestFacts};
use llm_runtime::prompt_cache::RequestSnapshot;

use super::ConversationOrchestrator;

#[derive(Default)]
pub(crate) struct PromptCacheDiagnostics {
    session_id: Option<lingxi_core::types::SessionId>,
    baseline: Option<RequestSnapshot>,
    pub(crate) ledger: cost::prompt_cache_ledger::PromptCacheLedger,
}

impl PromptCacheDiagnostics {
    pub(crate) fn model_switch_cache_state(
        &self,
        now_ms: u64,
        fallback_ttl: CacheTtl,
    ) -> (bool, CacheTtl) {
        let summary = self.ledger.summary(now_ms);
        let ttl = summary
            .last_request
            .as_ref()
            .map_or(fallback_ttl, |entry| entry.facts.ttl);
        (summary.warm, ttl)
    }

    pub(crate) fn ledger_for_session(
        &self,
        session_id: lingxi_core::types::SessionId,
    ) -> Option<&cost::prompt_cache_ledger::PromptCacheLedger> {
        (self.session_id == Some(session_id)).then_some(&self.ledger)
    }

    fn select_session(&mut self, session_id: lingxi_core::types::SessionId) {
        if self.session_id != Some(session_id) {
            *self = Self {
                session_id: Some(session_id),
                ..Self::default()
            };
        }
    }
}

fn changes(previous: &RequestSnapshot, current: &RequestSnapshot) -> MissAttribution {
    let mut attribution = MissAttribution::default();
    let comparisons = [
        (
            previous.system != current.system,
            MissCause::SystemPromptChanged,
        ),
        (previous.tools != current.tools, MissCause::ToolsChanged),
        (previous.model != current.model, MissCause::ModelChanged),
        (
            previous.fast_mode != current.fast_mode,
            MissCause::FastModeChanged,
        ),
        (
            previous.cache_policy != current.cache_policy,
            MissCause::CacheScopeOrTtlChanged,
        ),
        (previous.betas != current.betas, MissCause::BetasChanged),
        (previous.effort != current.effort, MissCause::EffortChanged),
        (
            !previous.thinking_mode.is_empty()
                && !current.thinking_mode.is_empty()
                && previous.thinking_mode != current.thinking_mode,
            MissCause::ThinkingModeChanged,
        ),
        (
            !previous.thinking_mode.is_empty()
                && previous.thinking_mode == current.thinking_mode
                && previous.thinking_display != current.thinking_display,
            MissCause::ThinkingDisplayChanged,
        ),
        (
            previous.extra_body != current.extra_body,
            MissCause::ExtraBodyChanged,
        ),
        (
            previous.defer_loading != current.defer_loading,
            MissCause::DeferLoadingChanged,
        ),
        (
            previous
                .messages
                .iter()
                .enumerate()
                .any(|(index, hash)| current.messages.get(index) != Some(hash)),
            MissCause::MessagesRewritten,
        ),
    ];
    attribution.causes.extend(
        comparisons
            .into_iter()
            .filter_map(|(changed, cause)| changed.then_some(cause)),
    );
    if previous.tools != current.tools {
        let added = current.tool_names.difference(&previous.tool_names).count();
        let removed = previous.tool_names.difference(&current.tool_names).count();
        if added > 0 || removed > 0 {
            attribution.tools_added = Some(u32::try_from(added).unwrap_or(u32::MAX));
            attribution.tools_removed = Some(u32::try_from(removed).unwrap_or(u32::MAX));
        }
    }
    if previous.system != current.system {
        attribution.system_char_delta =
            Some(current.system_chars.saturating_sub(previous.system_chars));
    }
    attribution
}

impl ConversationOrchestrator {
    pub(crate) async fn record_prompt_cache_usage(&self, usage: &llm_runtime::ExecutionUsage) {
        let current = self.model_runtime.prompt_cache_capture.take();
        let now_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        let at_ms = current.as_ref().map_or(now_ms, |request| request.at_ms);
        let counts = usage.counts();
        let ttl = if counts.cache_write_1h_tokens > 0 {
            CacheTtl::OneHour
        } else if usage
            .provider_metadata
            .pointer("/cache_creation/ephemeral_5m_input_tokens")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|tokens| tokens > 0)
        {
            CacheTtl::FiveMinutes
        } else if current.as_ref().is_some_and(|request| request.ttl_1h) {
            CacheTtl::OneHour
        } else {
            CacheTtl::FiveMinutes
        };
        let mut diagnostics = self.current_prompt_cache_ledger().await;
        let previous = std::mem::replace(&mut diagnostics.baseline, current.clone());
        let ledger = &mut diagnostics.ledger;
        let summary = ledger.summary(at_ms);
        // The oracle's diagnosis gate compares the previous CACHE READ,
        // independently of the ledger's total-prefix miss classifier. A cold
        // write followed by another cold write is a miss without attribution.
        let previous_read = summary
            .last_request
            .as_ref()
            .map_or(0, |entry| entry.facts.cache_read_tokens);
        #[allow(clippy::cast_precision_loss)]
        let diagnose = (counts.cache_read_tokens as f64) < (previous_read as f64) * 0.95
            && previous_read.saturating_sub(counts.cache_read_tokens) >= 2_000;
        if diagnose {
            let mut attribution = match (previous.as_ref(), current.as_ref()) {
                (Some(previous), Some(current)) => changes(previous, current),
                _ => MissAttribution {
                    causes: vec![MissCause::Unknown],
                    ..Default::default()
                },
            };
            if attribution.causes.is_empty() {
                let expired = summary.expires_at.is_some_and(|at| at_ms >= at);
                let prior_ttl = summary
                    .last_request
                    .as_ref()
                    .map_or(ttl, |entry| entry.facts.ttl);
                attribution.causes.push(if expired {
                    match prior_ttl {
                        CacheTtl::FiveMinutes => MissCause::TtlExpired5m,
                        CacheTtl::OneHour => MissCause::TtlExpired1h,
                    }
                } else {
                    MissCause::LikelySeverSide
                });
            }
            ledger.attribute(attribution);
        }
        ledger.record(RequestFacts {
            at_ms,
            input_tokens: counts.input_tokens,
            cache_read_tokens: counts.cache_read_tokens,
            cache_creation_tokens: counts.cache_write_tokens,
            ttl,
        });
    }

    pub(crate) async fn expect_prompt_cache_rebuild(&self) {
        let now_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        self.current_prompt_cache_ledger()
            .await
            .ledger
            .expect_drop(now_ms);
    }

    pub(crate) async fn current_prompt_cache_ledger(
        &self,
    ) -> tokio::sync::MutexGuard<'_, PromptCacheDiagnostics> {
        let session_id = self.session.lock().await.session_id;
        let mut diagnostics = self.model_runtime.prompt_cache_ledger.lock().await;
        diagnostics.select_session(session_id);
        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> RequestSnapshot {
        RequestSnapshot {
            at_ms: 0,
            system: [0; 32],
            system_chars: 2,
            tools: [0; 32],
            tool_names: ["old".to_string()].into(),
            model: [0; 32],
            cache_policy: [0; 32],
            ttl_1h: false,
            betas: [0; 32],
            effort: [0; 32],
            fast_mode: false,
            thinking_mode: "on",
            thinking_display: [0; 32],
            extra_body: [0; 32],
            defer_loading: false,
            messages: vec![[0; 32]],
        }
    }

    #[test]
    fn prompt_cache_changes_preserve_oracle_cause_order_and_counts() {
        let previous = snapshot();
        let mut current = previous.clone();
        current.system = [1; 32];
        current.system_chars = 4;
        current.tools = [1; 32];
        current.tool_names = ["new".to_string()].into();
        current.model = [1; 32];
        current.fast_mode = true;
        current.cache_policy = [1; 32];
        current.betas = [1; 32];
        current.effort = [1; 32];
        current.thinking_mode = "off";
        current.thinking_display = [1; 32];
        current.extra_body = [1; 32];
        current.defer_loading = true;
        current.messages[0] = [1; 32];
        let attribution = changes(&previous, &current);
        assert_eq!(
            attribution.causes,
            vec![
                MissCause::SystemPromptChanged,
                MissCause::ToolsChanged,
                MissCause::ModelChanged,
                MissCause::FastModeChanged,
                MissCause::CacheScopeOrTtlChanged,
                MissCause::BetasChanged,
                MissCause::EffortChanged,
                MissCause::ThinkingModeChanged,
                MissCause::ExtraBodyChanged,
                MissCause::DeferLoadingChanged,
                MissCause::MessagesRewritten,
            ]
        );
        assert_eq!(
            (attribution.tools_added, attribution.tools_removed),
            (Some(1), Some(1))
        );
        assert_eq!(attribution.system_char_delta, Some(2));
    }

    #[test]
    fn prompt_cache_append_is_not_rewrite_and_thinking_display_needs_stable_mode() {
        let previous = snapshot();
        let mut current = previous.clone();
        current.messages.push([1; 32]);
        assert!(changes(&previous, &current).causes.is_empty());
        current.thinking_display = [1; 32];
        assert_eq!(
            changes(&previous, &current).causes,
            vec![MissCause::ThinkingDisplayChanged]
        );
        current.thinking_mode = "";
        assert!(changes(&previous, &current).causes.is_empty());
        current.messages.clear();
        assert_eq!(
            changes(&previous, &current).causes,
            vec![MissCause::MessagesRewritten]
        );
    }

    #[test]
    fn prompt_cache_model_switch_uses_observed_ttl_and_dispatch_clock() {
        let mut diagnostics = PromptCacheDiagnostics::default();
        diagnostics.ledger.record(RequestFacts {
            at_ms: 1_000,
            input_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 10_000,
            ttl: CacheTtl::OneHour,
        });
        assert_eq!(
            diagnostics.model_switch_cache_state(301_000, CacheTtl::FiveMinutes),
            (true, CacheTtl::OneHour)
        );
        assert_eq!(
            diagnostics.model_switch_cache_state(3_601_000, CacheTtl::FiveMinutes),
            (false, CacheTtl::OneHour)
        );
        diagnostics.ledger.record(RequestFacts {
            at_ms: 4_000_000,
            input_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 10_000,
            ttl: CacheTtl::FiveMinutes,
        });
        assert_eq!(
            diagnostics.model_switch_cache_state(4_300_000, CacheTtl::OneHour),
            (false, CacheTtl::FiveMinutes)
        );
    }

    #[test]
    fn prompt_cache_model_switch_without_observation_is_cold_with_configured_ttl() {
        let diagnostics = PromptCacheDiagnostics::default();
        assert_eq!(
            diagnostics.model_switch_cache_state(0, CacheTtl::OneHour),
            (false, CacheTtl::OneHour)
        );
        assert_eq!(
            diagnostics.model_switch_cache_state(0, CacheTtl::FiveMinutes),
            (false, CacheTtl::FiveMinutes)
        );
    }
}
