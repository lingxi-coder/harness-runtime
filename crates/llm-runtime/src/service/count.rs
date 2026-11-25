//! Token-counting queries.

use crate::service::request::custom_system_prompt;
use crate::service::{ApiService, ACCOUNT_CALL_CLIENT};
use crate::{LlmError, LlmRequest, PromptCacheQuerySource};
use lingxi_core::types::ConversationMessage;
use std::sync::Arc;

impl ApiService {
    /// Count the input tokens a non-streaming `messages.create` for
    /// `(model, profile, system, messages, tools)` would consume on its resolved
    /// route. The drive logic of the orchestrator's
    /// `OrchestratorApiClient::count_tokens`: build the same non-streaming request
    /// shape `messages_create` sends, then delegate to the count_tokens facade —
    /// the real `/v1/messages/count_tokens` endpoint (with the `count_tokens`
    /// beta) on Anthropic routes, byte-length/4 approximation elsewhere.
    pub async fn count_tokens(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
    ) -> Result<u64, LlmError> {
        let system = custom_system_prompt(system);
        let req = self.build_request(
            model,
            profile,
            system.as_ref(),
            messages,
            tools,
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )?;
        self.count_request_with_accounts(req, false)
            .await
            .map(|count| count.expect("approximate token count always returns a value"))
    }

    /// Return an exact provider token count when the resolved route supports
    /// it. Unlike [`Self::count_tokens`], this never substitutes the generic
    /// text-only approximation, which is unsuitable for ToolSearch's schema
    /// threshold calculation.
    pub async fn count_tokens_exact(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
    ) -> Result<Option<u64>, LlmError> {
        let system = custom_system_prompt(system);
        let req = self.build_request(
            model,
            profile,
            system.as_ref(),
            messages,
            tools,
            false,
            None,
            false,
            PromptCacheQuerySource::Unspecified,
        )?;
        self.count_request_with_accounts(req, true).await
    }

    async fn count_request_with_accounts(
        &self,
        mut request: LlmRequest,
        exact: bool,
    ) -> Result<Option<u64>, LlmError> {
        crate::anthropic_extra_body::capture(&mut request)?;
        let runtime = self.client.account_call_snapshot().await?;
        let owner = Arc::as_ptr(&self.client) as usize;
        ACCOUNT_CALL_CLIENT
            .scope((owner, runtime), async move {
                let runtime = self.client();
                let model = request.input.model.clone();
                let profile = request.profile.clone();
                let mut coordinator = None;
                // Other protocols use a pure approximation/unsupported result and
                // need no credential or account health mutation.
                if runtime.protocol_for_model(&model, profile.as_deref())?
                    == crate::ProtocolFamily::AnthropicMessages
                {
                    let route = runtime
                        .resolve_media_route(&model, profile.as_deref())?
                        .main;
                    coordinator = runtime.account_coordinator(&model, profile.as_deref())?;
                    if let Some(coordinator) = coordinator.as_mut() {
                        coordinator.retain_supported(|candidate| {
                            (!route.profile_name.starts_with("account.")
                                || candidate.profile_name == route.profile_name)
                                && runtime.account_supports_request(&request, candidate)
                                && runtime
                                    .protocol_for_model(&model, Some(&candidate.profile_name))
                                    .is_ok_and(|protocol| {
                                        protocol == crate::ProtocolFamily::AnthropicMessages
                                    })
                        });
                        coordinator.map_candidate_models(|_| {
                            format!("tokens/count/{}", route.request_model)
                        });
                        let selected = coordinator.select().await?;
                        request.input.model = runtime
                            .resolve_media_route(&model, Some(&selected.profile_name))?
                            .main
                            .request_model;
                        request.profile = Some(selected.profile_name);
                        request.execution.selected_account = coordinator.active_scope();
                        request.execution.account_scope = request
                            .execution
                            .selected_account
                            .as_ref()
                            .map(|scope| serde_json::to_string(scope).expect("account scope"));
                        request.execution.request_credentials = Some(Default::default());
                    }
                }
                let result = if exact {
                    crate::model::count_tokens::try_count_tokens_exact(
                        runtime.as_ref(),
                        self.transport.clone(),
                        &request,
                    )
                    .await
                } else {
                    crate::model::count_tokens::count_tokens(
                        runtime.as_ref(),
                        self.transport.clone(),
                        &request,
                    )
                    .await
                    .map(Some)
                };
                if let Some(coordinator) = coordinator.as_mut() {
                    match &result {
                        Ok(_) => coordinator.succeeded().await,
                        Err(error) => {
                            self.record_pool_failure(&request, coordinator, error, None)
                                .await?;
                        }
                    }
                }
                result
            })
            .await
    }

    /// Count exactly one Read source string on its selected route. This
    /// auxiliary request intentionally starts from model input rather than
    /// normal conversation preparation, which may inject effort/cache rows.
    pub async fn count_read_text_tokens_exact(
        &self,
        model: &str,
        profile: Option<&str>,
        source: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<Option<u64>, LlmError> {
        let units = source
            .string_units("")
            .ok_or_else(|| LlmError::InvalidRequest {
                message: "Read token counting requires source text".into(),
            })?;
        if units.is_empty() {
            return Ok(Some(0));
        }
        let mut request = LlmRequest::new(model).with_user_text(String::from_utf16_lossy(&units));
        request.profile = profile.map(str::to_owned);
        request.execution.read_token_count = true;
        request.execution.anthropic_request_kind =
            lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        request
            .execution
            .message_json_string_overrides
            .insert("/messages/0/content/0/text".into(), units);
        // Generation selects a named account before credential admission. The
        // visible provider binding cannot authenticate directly in that host.
        // Freeze the same live account catalog for this auxiliary operation.
        let runtime = self.client.account_call_snapshot().await?;
        let owner = Arc::as_ptr(&self.client) as usize;
        ACCOUNT_CALL_CLIENT
            .scope((owner, runtime), async move {
                let runtime = self.client();
                if runtime.protocol_for_model(model, profile)?
                    != crate::ProtocolFamily::AnthropicMessages
                {
                    return Ok(None);
                }
                let route = runtime.resolve_media_route(model, profile)?.main;
                let mut coordinator = if let Some(mut auxiliary) =
                    runtime.account_coordinator(model, profile)?
                {
                    auxiliary.retain_supported(|candidate| {
                        (!route.profile_name.starts_with("account.")
                            || candidate.profile_name == route.profile_name)
                            && runtime.account_supports_request(&request, candidate)
                            && runtime
                                .protocol_for_model(
                                    &candidate.request_model,
                                    Some(&candidate.profile_name),
                                )
                                .is_ok_and(|protocol| {
                                    protocol == crate::ProtocolFamily::AnthropicMessages
                                })
                    });
                    // Keep the model-authorized candidates, including
                    // model billing overrides. Only health's operation key
                    // differs from the generation call's model identity.
                    auxiliary
                        .map_candidate_models(|_| format!("tokens/count/{}", route.request_model));
                    let selected = auxiliary.select().await?;
                    request.profile = Some(selected.profile_name);
                    request.input.model = route.request_model;
                    request.execution.selected_account = auxiliary.active_scope();
                    request.execution.account_scope = request
                        .execution
                        .selected_account
                        .as_ref()
                        .map(|scope| serde_json::to_string(scope).expect("account scope"));
                    request.execution.request_credentials = Some(Default::default());
                    Some(auxiliary)
                } else {
                    None
                };
                let result = crate::model::count_tokens::try_count_tokens_exact(
                    runtime.as_ref(),
                    self.transport.clone(),
                    &request,
                )
                .await;
                if let Some(coordinator) = coordinator.as_mut() {
                    match &result {
                        Ok(_) => coordinator.succeeded().await,
                        Err(error) => {
                            self.record_pool_failure(&request, coordinator, error, None)
                                .await?;
                        }
                    }
                }
                result
            })
            .await
    }
}
