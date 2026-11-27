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
    /// beta) on Anthropic routes, or a local estimate for other routes and
    /// output-limited invocations.
    pub async fn count_tokens(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
    ) -> Result<u64, LlmError> {
        let system = custom_system_prompt(system);
        self.count_request_with_accounts(false, || {
            self.build_request(
                model,
                profile,
                system.as_ref(),
                messages,
                tools,
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
        })
        .await
        .map(|count| count.expect("approximate token count always returns a value"))
    }

    /// Return an exact provider token count when the resolved route supports
    /// it. Unlike [`Self::count_tokens`], this never substitutes the generic
    /// text-only approximation, which is unsuitable for ToolSearch's schema
    /// threshold calculation. Output-limited invocations return `None`.
    pub async fn count_tokens_exact(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
    ) -> Result<Option<u64>, LlmError> {
        let system = custom_system_prompt(system);
        self.count_request_with_accounts(true, || {
            self.build_request(
                model,
                profile,
                system.as_ref(),
                messages,
                tools,
                false,
                None,
                false,
                PromptCacheQuerySource::Unspecified,
            )
        })
        .await
    }

    async fn count_request_with_accounts(
        &self,
        exact: bool,
        build: impl FnOnce() -> Result<LlmRequest, LlmError>,
    ) -> Result<Option<u64>, LlmError> {
        if self.scope_output_quota.is_some() {
            let runtime = self.client();
            let mut request = build()?;
            runtime.resolve_available_route(&request)?;
            if exact {
                return Ok(None);
            }
            crate::anthropic_extra_body::capture(&mut request)?;
            return runtime.estimate_input_tokens_for_route(&request).map(Some);
        }
        let runtime = self.client.account_call_snapshot().await?;
        let owner = Arc::as_ptr(&self.client) as usize;
        ACCOUNT_CALL_CLIENT
            .scope((owner, runtime), async move {
                let mut request = build()?;
                crate::anthropic_extra_body::capture(&mut request)?;
                let runtime = self.client();
                let mut coordinator = self.select_count_account(&mut request).await?;
                let result = crate::model::count_tokens::try_count_tokens_exact(
                    runtime.as_ref(),
                    self.transport.clone(),
                    &request,
                )
                .await;
                if let Some(coordinator) = coordinator.as_mut() {
                    self.settle_count_account(&request, coordinator, &result)
                        .await?;
                }
                match result? {
                    None if !exact => runtime.estimate_input_tokens_for_route(&request).map(Some),
                    count => Ok(count),
                }
            })
            .await
    }

    async fn select_count_account(
        &self,
        request: &mut LlmRequest,
    ) -> Result<Option<crate::accounts::AttemptCoordinator>, LlmError> {
        let runtime = self.client();
        let model = &request.input.model;
        let profile = request.profile.as_deref();
        let route = runtime.resolve_available_route(request)?;
        if runtime.protocol_for_model(model, profile)? != crate::ProtocolFamily::AnthropicMessages {
            return Ok(None);
        }
        let Some(mut coordinator) = runtime.account_coordinator(model, profile)? else {
            return Ok(None);
        };
        coordinator.retain_supported(|candidate| {
            (!route.profile_name.starts_with("account.")
                || candidate.profile_name == route.profile_name)
                && runtime.account_supports_request(request, candidate)
                && runtime
                    .protocol_for_model(candidate.selector(), Some(&candidate.profile_name))
                    .is_ok_and(|protocol| protocol == crate::ProtocolFamily::AnthropicMessages)
        });
        coordinator.scope_to_token_count();
        let selected = coordinator.select().await?;
        request.input.model = selected.selector().to_owned();
        request.profile = Some(selected.profile_name);
        request.execution.selected_account = coordinator.active_scope();
        request.execution.account_scope = request
            .execution
            .selected_account
            .as_ref()
            .map(|scope| serde_json::to_string(scope).expect("account scope"));
        request.execution.request_credentials = Some(Default::default());
        Ok(Some(coordinator))
    }

    async fn settle_count_account(
        &self,
        request: &LlmRequest,
        coordinator: &mut crate::accounts::AttemptCoordinator,
        result: &Result<Option<u64>, LlmError>,
    ) -> Result<(), LlmError> {
        match result {
            Ok(Some(_)) => coordinator.succeeded().await,
            Ok(None) | Err(LlmError::ModelUnavailable) => coordinator.release().await,
            Err(error) => {
                if !self
                    .record_pool_failure(request, coordinator, error, None)
                    .await?
                {
                    coordinator.release().await;
                }
            }
        }
        Ok(())
    }

    /// Count exactly one Read source string on its selected route. This
    /// auxiliary request intentionally starts from model input rather than
    /// normal conversation preparation, which may inject effort/cache rows.
    /// Empty text counts as zero locally. Other text in an output-limited
    /// invocation returns `None`.
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
        if self.scope_output_quota.is_some() {
            let mut request = LlmRequest::new(model);
            request.profile = profile.map(str::to_owned);
            self.client().resolve_available_route(&request)?;
            return Ok(None);
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
                let mut coordinator = self.select_count_account(&mut request).await?;
                let result = crate::model::count_tokens::try_count_tokens_exact(
                    runtime.as_ref(),
                    self.transport.clone(),
                    &request,
                )
                .await;
                if let Some(coordinator) = coordinator.as_mut() {
                    self.settle_count_account(&request, coordinator, &result)
                        .await?;
                }
                result
            })
            .await
    }
}
