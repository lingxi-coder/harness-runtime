//! Claude-specific repairs and conversation latches.

use crate::model::retry_scope::ModelCallRetryScope;
use crate::model::telemetry;
use crate::service::anthropic::endpoint::native_context_1m_model_key;
use crate::service::ApiService;
use crate::{LlmError, LlmRequest};

/// Claude Code gates cross-model signature stripping on its internal account
/// class `T2o()`. In the shipped 2.1.218 binary that predicate is a compile-time
/// constant `"external"` (there is NO `process.env.USER_TYPE` read in the CLI
/// bundle for this) — so the strip never runs in the released build. Modeled
/// here as the same inert constant rather than reading `USER_TYPE`, which the
/// oracle does not do for this path. The pure SDK signature stripping
/// transform stays separately tested and ready for when the internal account
/// class is actually plumbed.
pub(in crate::service) fn strip_signature_blocks_for_fallback(
    messages: &mut [lingxi_llm_client::protocol::ConversationMessage],
) {
    if is_internal_account_class() {
        lingxi_llm_client::providers::anthropic::response_policy::strip_assistant_signature_blocks(
            messages,
        );
    }
}

/// claude-code `T2o()` — the account class, a compile-time `"external"` constant
/// in the shipped binary. `false` until an internal account class is plumbed.
fn is_internal_account_class() -> bool {
    false
}

impl ApiService {
    /// Learn provider limitations from a rejection and repair the current
    /// attempt's input. A terminal prefix-recovery result is returned as false.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::service) async fn recover_provider_rejection(
        &self,
        request: &mut LlmRequest,
        prepared: &mut crate::PreparedLlmCall,
        error: &LlmError,
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
        scope: &ModelCallRetryScope,
        recover_thinking: bool,
    ) -> Option<bool> {
        if self.recover_foundry_tools(request, prepared, error, status, body, scope) {
            return Some(true);
        }
        if !recover_thinking {
            return None;
        }
        if self.native_thinking_binding_controls_hq_unknown_refusal(prepared, error, status, body) {
            Self::refuse_native_thinking_binding_controls(prepared);
            return Some(true);
        }
        self.repair_native_prefix_lock(request, prepared, error, status, scope)
            .await
    }

    fn native_context_1m_planned(prepared: &crate::PreparedLlmCall) -> bool {
        prepared
            .planned_native_beta_context
            .as_ref()
            .is_some_and(|planned| planned.contains(crate::model::betas::CONTEXT_1M))
    }

    pub(in crate::service) fn remove_context_1m_beta(prepared: &mut crate::PreparedLlmCall) {
        let Some((name, value)) = prepared
            .provider_request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(name, value)| (name.clone(), value.clone()))
        else {
            return;
        };
        let kept = value
            .split(',')
            .map(str::trim)
            .filter(|beta| *beta != crate::model::betas::CONTEXT_1M)
            .collect::<Vec<_>>();
        if kept.is_empty() {
            prepared.provider_request.headers.remove(&name);
        } else {
            prepared
                .provider_request
                .headers
                .insert(name, kept.join(","));
        }
    }

    pub(in crate::service) fn native_context_1m_probe_refusal(
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
    ) -> bool {
        if status != 400 {
            return false;
        }
        let Some(message) = body
            .string_at("/error/message")
            .or_else(|| body.string_at("/message"))
        else {
            return false;
        };
        message.contains(crate::model::betas::CONTEXT_1M)
            || message.contains("long context beta")
            || message.to_lowercase().contains("invalid beta flag")
    }

    /// Native RZe: require the context beta in the source-planned lists and
    /// the final wire request. The latter is a safety check; it cannot turn a
    /// custom `extra.body.anthropic_beta` token into planned membership.
    pub(in crate::service) fn native_context_1m_ordinary_refusal(
        &self,
        prepared: &crate::PreparedLlmCall,
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
    ) -> bool {
        const THINKING_BINDING_CONTROLS: &str = "thinking-binding-controls-2026-08-01";
        if status != 400
            || !Self::native_context_1m_planned(prepared)
            || !Self::request_beta_tokens(prepared)
                .iter()
                .any(|beta| beta == crate::model::betas::CONTEXT_1M)
            || !self.native_context_1m_resend_allowed(prepared)
        {
            return false;
        }
        let Some(message) = body
            .string_at("/error/message")
            .or_else(|| body.string_at("/message"))
        else {
            return false;
        };
        let explicit_context_refusal = message.contains(crate::model::betas::CONTEXT_1M)
            || message.contains("long context beta");
        let ambiguous_beta_refusal = message.to_lowercase().contains("invalid beta flag");
        let thinking_binding_controls_sent =
            Self::native_thinking_binding_controls_planned(prepared)
                && Self::native_thinking_binding_controls_u6(prepared) == Some(false)
                && Self::request_header_beta_tokens(prepared)
                    .iter()
                    .any(|beta| beta == THINKING_BINDING_CONTROLS);
        explicit_context_refusal || ambiguous_beta_refusal && !thinking_binding_controls_sent
    }

    /// Consume the SDK's typed server-fallback beta repair fact for this query.
    /// The parameter-presence input is captured before the extra-body spread,
    /// matching native `a1`; the SDK facts helper owns the native cause and
    /// one-shot guards, while the prepared call supplies the conversation-local
    /// state handle.
    pub(in crate::service) fn repair_server_fallback_beta_rejection(
        prepared: &crate::PreparedLlmCall,
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
        server_fallback_parameter_added: bool,
        repair_already_attempted: bool,
    ) -> bool {
        // Native r3e catches this SDK-classified error across all Anthropic
        // Messages transports, including the cloud Claude wrappers.
        if !Self::is_anthropic_family_protocol(&prepared.route.protocol) {
            return false;
        }
        use lingxi_llm_client::providers::anthropic::fallback_request;
        let Some(cause) = fallback_request::classify_server_fallback_rejection(status, body) else {
            return false;
        };
        let Some(facts) = fallback_request::server_fallback_repair_facts(
            cause,
            server_fallback_parameter_added,
            repair_already_attempted,
        ) else {
            return false;
        };
        prepared.server_fallback_betas.reject(facts.rejected_mode);
        true
    }

    /// Capture a connection before preparation, after account/fallback selection.
    pub(in crate::service) fn context_1m_route_key(
        &self,
        request: &LlmRequest,
    ) -> Option<crate::thinking_scope::Context1mRouteKey> {
        let client = self.client();
        let route = client
            .resolve_media_route(&request.input.model, request.profile.as_deref())
            .ok()?
            .main;
        let profile =
            client.prompt_cache_profile(&request.input.model, Some(&route.profile_name))?;
        Some(crate::thinking_scope::Context1mRouteKey {
            provider: serde_json::to_string(&route.provider_id).ok()?,
            profile: route.profile_name,
            endpoint: profile.base_url.trim_end_matches('/').to_owned(),
            protocol: profile.protocol,
            model: native_context_1m_model_key(request),
        })
    }

    /// Native `eRe(cc(model))` gate for the model-validation 1m repair.
    pub(in crate::service) fn native_context_1m_resend_allowed(
        &self,
        prepared: &crate::PreparedLlmCall,
    ) -> bool {
        if prepared.route.resolved_route.provider_id != crate::ProviderId::AnthropicFirstParty {
            return true;
        }
        match self.native_system_prefix_snapshot() {
            Some(prefix) => {
                !prefix.assume_first_party_base_url && !Self::native_first_party_endpoint(prepared)
            }
            None => !Self::dispatch_first_party(prepared),
        }
    }

    pub(in crate::service) fn capture_thinking_recovery(&self, request: &mut LlmRequest) {
        let scope = request
            .execution
            .thinking_recovery_scope
            .get_or_insert_with(|| self.thinking_recovery_scope());
        request
            .execution
            .native_tbc_routes
            .get_or_insert_with(|| scope.native_thinking_binding_controls_routes());
    }

    pub(in crate::service) fn thinking_recovery_scope(
        &self,
    ) -> crate::thinking_scope::ThinkingRecoveryScope {
        crate::thinking_scope::current().unwrap_or_else(|| self.thinking_recovery.clone())
    }

    /// Recovery status for the owning query (legacy direct callers use a private scope).
    #[must_use]
    pub fn thinking_signature_stripped(&self) -> bool {
        self.thinking_recovery_scope().stripped()
    }

    /// Compatibility arm: capture only the next request's historical identities.
    pub fn set_thinking_signature_stripped(&self, stripped: bool) {
        self.thinking_recovery_scope().arm(stripped);
    }

    pub fn thinking_stripped_messages(
        &self,
    ) -> std::collections::HashMap<lingxi_core::types::MessageId, usize> {
        self.thinking_recovery_scope().messages()
    }

    pub fn set_thinking_stripped_messages(
        &self,
        messages: std::collections::HashMap<lingxi_core::types::MessageId, usize>,
    ) {
        self.thinking_recovery_scope().merge(messages);
    }

    /// Apply Native's strict prefix-lock recovery before generic error retry.
    /// Some(true) means the request clone was changed and the logical call
    /// should retry; Some(false) is a terminal same-error decline; None means
    /// the exact error was not marked declined and normal handling continues.
    pub(in crate::service) async fn repair_native_prefix_lock(
        &self,
        req: &mut crate::LlmRequest,
        prepared: &crate::PreparedLlmCall,
        error: &LlmError,
        status: u16,
        retry_scope: &ModelCallRetryScope,
    ) -> Option<bool> {
        let facts = error.native_anthropic_error_facts()?.clone();
        if !crate::prefix_lock_recovery::is_native_prefix_lock_400(status, &facts) {
            return None;
        }
        let attempt = retry_scope.record_prefix_lock_error();
        let stripped_this_call = retry_scope.prefix_lock_stripped_this_call();
        let mismatch = facts
            .response
            .as_ref()
            .and_then(|response| response.thinking_prefix_mismatch.as_ref());
        let plan = crate::prefix_lock_recovery::plan_prefix_lock_recovery(
            status,
            &facts,
            mismatch,
            &req.input,
            &prepared.provider_request.body_json,
            &req.execution.request_content_source_ids,
            &req.execution.source_thinking_messages,
            &req.execution.thinking_source_message_ids,
            &req.execution
                .thinking_recovery_scope
                .as_ref()
                .map(crate::thinking_scope::ThinkingRecoveryScope::messages)
                .unwrap_or_default(),
            attempt,
            stripped_this_call,
            req.execution.resume_incomplete_thinking(),
        )?;

        if let crate::prefix_lock_recovery::PrefixLockPlan::Declined { mark_same_error } = plan {
            let declined_this_error =
                facts
                    .native_hq_hrn15_evidence
                    .as_ref()
                    .is_some_and(|evidence| {
                        evidence.aggregate_claim(Some(mark_same_error)) == Some(false)
                    });
            tracing::warn!(
                attempt,
                mark_same_error,
                declined_this_error,
                "Native prefix-lock error declined by the incomplete-thinking guard"
            );
            return declined_this_error.then_some(false);
        }

        let mut source_ranges = None;
        let mut changed = false;
        if let crate::prefix_lock_recovery::PrefixLockPlan::Partial(target) = plan {
            source_ranges = crate::prefix_lock_recovery::partial_source_ranges(
                target,
                &req.execution.thinking_source_message_ids,
            );
            if source_ranges.is_some() {
                changed = crate::prefix_lock_recovery::strip_partial_request(req, target);
            }
        }
        if source_ranges.is_none() || !changed {
            changed = crate::prefix_lock_recovery::strip_all_request_thinking(req);
            source_ranges = Some(crate::prefix_lock_recovery::all_source_ranges(
                &req.execution.thinking_source_message_ids,
            ));
        }

        if !changed {
            let mark_same_error = !retry_scope.prefix_lock_stripped_this_call();
            let declined_this_error =
                facts
                    .native_hq_hrn15_evidence
                    .as_ref()
                    .is_some_and(|evidence| {
                        evidence.aggregate_claim(Some(mark_same_error)) == Some(false)
                    });
            tracing::warn!(
                attempt,
                mark_same_error,
                declined_this_error,
                "Native prefix-lock error had no thinking blocks to strip"
            );
            return declined_this_error.then_some(false);
        }

        let recovery_scope = req
            .execution
            .thinking_recovery_scope
            .clone()
            .unwrap_or_else(|| self.thinking_recovery_scope());
        recovery_scope.rejected(source_ranges.unwrap_or_default());
        recovery_scope.persist().await;
        retry_scope.mark_prefix_lock_stripped();
        tracing::warn!(
            attempt,
            partial = matches!(
                plan,
                crate::prefix_lock_recovery::PrefixLockPlan::Partial(_)
            ),
            "Native prefix-lock error stripped request-local thinking and will retry"
        );
        Some(true)
    }

    /// Strip thinking blocks after a thinking-signature 400 on any provider.
    /// Returns `true` when the caller should retry immediately.
    pub(in crate::service) async fn handle_thinking_signature_strip(
        &self,
        req: &mut crate::LlmRequest,
    ) -> bool {
        let (signed, unsigned) =
            crate::model::thinking_signature::count_input_thinking(&req.input.messages);
        if !crate::model::thinking_signature::strip_input_thinking(req) {
            return false;
        }
        tracing::warn!(
            "[thinking] server rejected a thinking block; stripping all thinking blocks and retrying."
        );
        telemetry::emit_thinking_signature_strip_retry(
            &self.analytics,
            req.execution.query_source.as_deref(),
            &req.input.model,
            signed,
            unsigned,
        )
        .await;
        let scope = req
            .execution
            .thinking_recovery_scope
            .clone()
            .unwrap_or_else(|| self.thinking_recovery_scope());
        scope.rejected(
            req.execution
                .thinking_source_message_ids
                .iter()
                .map(|id| (*id, 0))
                .collect(),
        );
        scope.persist().await;
        true
    }

    /// Native Anthropic returns some accumulated-request 413s as
    /// `invalid_request_error`, while the HTTP status is the authoritative
    /// `RequestTooLarge` signal. Preserve the provider-text wrapper and its
    /// status, but expose the semantic class expected by the Host's existing
    /// `image_error` surface. Other providers and context-window errors keep
    /// their original classification.
    pub(in crate::service) fn classify_native_invalid_request_413(err: &mut LlmError) {
        if err.http_status() != Some(413)
            || err.native_anthropic_error_facts().is_none()
            || !matches!(err.classification_source(), LlmError::InvalidRequest { .. })
        {
            return;
        }
        *err.classification_source_mut() = LlmError::RequestTooLarge;
    }
}
