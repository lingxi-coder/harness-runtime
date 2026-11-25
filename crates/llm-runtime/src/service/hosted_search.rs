//! Hosted search and fetch operations.

use crate::service::ApiService;
use crate::{LlmError, ModelRuntime};
use std::sync::Arc;

struct DeepSeekWebOperation {
    client: lingxi_llm_client::providers::DeepSeekClient,
    options: lingxi_llm_client::RequestOptions,
    source: Arc<ModelRuntime>,
    profile: String,
    credential_revision: Option<u64>,
    coordinator: Option<crate::accounts::AttemptCoordinator>,
}

impl DeepSeekWebOperation {
    async fn settle<T>(&mut self, result: &Result<T, LlmError>) {
        let Some(coordinator) = self.coordinator.as_mut() else {
            return;
        };
        match result {
            Ok(_) => coordinator.succeeded().await,
            Err(error) => {
                if matches!(
                    error.classification_source(),
                    LlmError::Authentication { .. } | LlmError::OAuthRefreshDead
                ) && self
                    .source
                    .current_service_credential_revision(&self.profile)
                    .await
                    .ok()
                    != Some(self.credential_revision)
                {
                    return;
                }
                coordinator.set_rejected_credential_revision(self.credential_revision);
                let _ = coordinator.failed(error).await;
            }
        }
    }
}

impl ApiService {
    pub fn uses_deepseek_web(&self, model: &str, profile: Option<&str>) -> bool {
        self.client().deepseek_web_profile(model, profile).is_some()
    }

    async fn deepseek_web_client(
        &self,
        model: &str,
        profile: Option<&str>,
        operation: &'static str,
    ) -> Result<DeepSeekWebOperation, LlmError> {
        if self.scope_output_quota.is_some() {
            return Err(LlmError::UnsupportedCapability {
                capability: "independent web requests are disabled for output-limited scopes"
                    .into(),
            });
        }
        let started = tokio::time::Instant::now();
        let timeout = std::time::Duration::from_secs(60);
        // One operation owns one current directory, route and credential source.
        // The service lifetime can span account/catalog publications.
        let operation_client = self.client.account_call_snapshot().await?;
        let profile = operation_client
            .deepseek_web_profile(model, profile)
            .ok_or_else(|| LlmError::UnsupportedCapability {
                capability: "independent DeepSeek web APIs require a first-party route".into(),
            })?;
        let mut coordinator = operation_client.service_account_coordinator(&profile, operation)?;
        if let Some(coordinator) = coordinator.as_mut() {
            coordinator.retain_supported(|candidate| {
                (!profile.starts_with("account.") || candidate.profile_name == profile)
                    && operation_client
                        .deepseek_web_profile(model, Some(&candidate.profile_name))
                        .is_some()
            });
        }
        let profile = match coordinator.as_mut() {
            Some(coordinator) => {
                tokio::time::timeout(
                    timeout.saturating_sub(started.elapsed()),
                    coordinator.select(),
                )
                .await
                .map_err(|_| LlmError::TransportTimeout {
                    message: "DeepSeek web account deadline elapsed".into(),
                })??
                .profile_name
            }
            None => profile,
        };
        let services = operation_client
            .provider_services(
                lingxi_llm_client::protocol::Region::International,
                self.transport.clone(),
            )
            .map_err(|error| LlmError::InvalidRequest {
                message: error.to_string(),
            })?;
        let client = services
            .client()
            .provider::<lingxi_llm_client::providers::DeepSeekClient>(&profile)
            .map_err(|error| LlmError::InvalidRequest {
                message: error.to_string(),
            })?;
        let credential = tokio::time::timeout(
            timeout.saturating_sub(started.elapsed()),
            operation_client.service_credential_versioned(&profile),
        )
        .await
        .map_err(|_| LlmError::TransportTimeout {
            message: "DeepSeek web credential deadline elapsed".into(),
        })?;
        let (credential, credential_revision) = match credential {
            Ok(credential) => credential,
            Err(failure) => {
                let mut failed = DeepSeekWebOperation {
                    client,
                    options: Default::default(),
                    source: operation_client,
                    profile,
                    credential_revision: failure.credential_revision,
                    coordinator,
                };
                failed.settle::<()>(&Err(failure.error.clone())).await;
                return Err(failure.error);
            }
        };
        let options = lingxi_llm_client::RequestOptions {
            credential: Some(credential),
            total_timeout: Some(timeout.saturating_sub(started.elapsed())),
            ..Default::default()
        };
        Ok(DeepSeekWebOperation {
            client,
            options,
            source: operation_client,
            profile,
            credential_revision,
            coordinator,
        })
    }

    /// One independent request; retries and turn generation are not involved.
    pub async fn deepseek_search(
        &self,
        model: &str,
        profile: Option<&str>,
        request: &lingxi_llm_client::providers::deepseek::web::DeepSeekSearchRequest,
    ) -> Result<lingxi_llm_client::providers::deepseek::web::DeepSeekSearchResponse, LlmError> {
        request.validate().map_err(crate::upstream::error)?;
        let mut operation = self
            .deepseek_web_client(model, profile, "web/search")
            .await?;
        let result = operation
            .client
            .web()
            .search(request, &operation.options)
            .await
            .map_err(crate::upstream::error);
        operation.settle(&result).await;
        result
    }

    pub async fn deepseek_fetch(
        &self,
        model: &str,
        profile: Option<&str>,
        request: &lingxi_llm_client::providers::deepseek::web::DeepSeekFetchRequest,
    ) -> Result<lingxi_llm_client::providers::deepseek::web::DeepSeekFetchResponse, LlmError> {
        request.validate().map_err(crate::upstream::error)?;
        let mut operation = self
            .deepseek_web_client(model, profile, "web/fetch")
            .await?;
        let result = operation
            .client
            .web()
            .fetch(request, &operation.options)
            .await
            .map_err(crate::upstream::error);
        operation.settle(&result).await;
        result
    }

    pub fn hosted_search_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<crate::ResolvedRoute, LlmError> {
        self.client().hosted_search_route(model, profile)
    }

    pub fn uses_codex_standalone_search(&self, model: &str, profile: Option<&str>) -> bool {
        self.client()
            .search_profile(model, profile)
            .is_some_and(lingxi_llm_client::providers::openai::codex_search::supports)
    }

    pub fn supports_hosted_search(&self, model: &str, profile: Option<&str>) -> bool {
        self.uses_deepseek_web(model, profile)
            || self.supports_hosted_search_config(model, profile, &Default::default())
    }

    pub fn supports_hosted_search_config(
        &self,
        model: &str,
        profile: Option<&str>,
        config: &lingxi_llm_client::protocol::WebSearchConfig,
    ) -> bool {
        if self.uses_deepseek_web(model, profile) {
            return config.max_uses.is_none();
        }
        let Ok(route) = self.hosted_search_route(model, profile) else {
            return false;
        };
        self.client()
            .search_profile(model, profile)
            .is_some_and(|profile| {
                lingxi_llm_client::hosted_search::supports_model_with_config(
                    profile,
                    &route.request_model,
                    config,
                )
            })
    }

    pub fn hosted_search_max_uses(&self, model: &str, profile: Option<&str>) -> Option<u32> {
        self.client()
            .search_profile(model, profile)
            .filter(|p| {
                p.extra
                    .get("web_search")
                    .and_then(serde_json::Value::as_str)
                    == Some("anthropic")
            })
            .map(|_| 8)
    }

    pub(super) fn log_deepseek_prepared_request(
        model: &str,
        prepared: &crate::PreparedLlmCall,
        stream: bool,
    ) {
        let body = &prepared.provider_request.body_json;
        let is_deepseek = model.contains("deepseek")
            || body
                .string_at("/model")
                .is_some_and(|model| model.contains("deepseek"))
            || prepared.provider_request.url.contains("api.deepseek.com");
        if !is_deepseek {
            return;
        }
        // The message walk exists only for this debug event. A disabled
        // target must not scan every prepared message.
        if !tracing::enabled!(target: "llm_runtime::service", tracing::Level::DEBUG) {
            return;
        }

        let message_count = body.array_len("/messages").unwrap_or_default();
        let messages_with_reasoning_content = (0..message_count)
            .filter(|index| {
                body.array_item_at("/messages", *index)
                    .is_some_and(|message| {
                        lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
                            &message,
                            &["reasoning_content"],
                        )
                    })
            })
            .count();
        let last_assistant = (0..message_count)
            .rev()
            .find(|index| {
                body.array_item_at("/messages", *index)
                    .is_some_and(|message| {
                        message.string_at("/role").as_deref() == Some("assistant")
                    })
            })
            .and_then(|index| body.array_item_at("/messages", index));
        let last_assistant_reasoning_len = last_assistant
            .and_then(|message| message.string_at("/reasoning_content"))
            .map_or(0, |text| text.len());
        tracing::debug!(
            target = "llm_runtime::service",
            event = "deepseek_prepared_request",
            stream = stream,
            model = %model,
            provider_url = %prepared.provider_request.url,
            message_count = message_count,
            messages_with_reasoning_content = messages_with_reasoning_content,
            last_assistant_reasoning_len = last_assistant_reasoning_len,
            tool_choice = ?body.raw_source_at("/tool_choice"),
            tools = body.array_len("/tools").unwrap_or_default(),
            thinking = ?body.raw_source_at("/thinking"),
        );
    }
}
