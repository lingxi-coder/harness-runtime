//! Host policy for Anthropic fast-mode admission and rejection.

use crate::model::betas::FAST_MODE;
use crate::model::user_agent::user_agent;
use crate::service::ApiService;
use crate::{LlmError, LlmRequest};

impl ApiService {
    /// Current route/model Fast gate, without authentication or transport.
    pub fn fast_model_allowed(&self, model: &str, profile: Option<&str>) -> Result<bool, LlmError> {
        if !self.client().fast_model_allowed(model, profile)? {
            return Ok(false);
        }
        let account = self.client().fast_account_identity(model, profile)?;
        let policy = (self.anthropic.fast_policy)()?;
        let (org, oauth) = self
            .anthropic
            .fast_availability
            .observed(&account.profile, &policy);
        let inputs = policy.inputs(&account, org, oauth, !self.interactive_session_for_policy());
        Ok(lingxi_core::host::fast_mode::decline(&inputs).is_none())
    }

    fn interactive_session_for_policy(&self) -> bool {
        self.interactive_session.unwrap_or_else(|| {
            !lingxi_core::host::session_flags::effective_non_interactive_session()
        })
    }

    /// Live root settings and managed-policy facts. No model-input field can
    /// manufacture organization admission.
    #[must_use]
    pub fn with_fast_policy_source(
        mut self,
        source: crate::model::fast_admission::PolicySource,
    ) -> Self {
        self.anthropic.fast_policy = source;
        self
    }

    /// Refresh the selected first-party account before accepting an explicit
    /// enable. Off remains available independently of account status.
    pub async fn validate_fast_enable(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<(), LlmError> {
        let identity = self.client().fast_account_identity(model, profile)?;
        let policy = (self.anthropic.fast_policy)()?;
        if identity.first_party
            && !policy.no_user_account
            && !crate::structured_output::bool_environment(branding::DISABLE_FAST_MODE_ENV)
        {
            let account = self
                .client()
                .load_fast_account(model, profile)
                .await
                .unwrap_or(identity);
            let ua = user_agent(&self.ua, &self.version);
            self.anthropic
                .fast_availability
                .refresh(&account, &policy, self.transport.as_ref(), &ua, || async {
                    let client = self.client();
                    client.refresh_fast_account(&account).await
                })
                .await;
            return self.validate_fast_snapshot(&account, &policy);
        }
        self.validate_fast_snapshot(&identity, &policy)
    }

    fn validate_fast_snapshot(
        &self,
        account: &crate::model::fast_admission::Account,
        policy: &crate::model::fast_admission::Policy,
    ) -> Result<(), LlmError> {
        let (org, oauth) = self
            .anthropic
            .fast_availability
            .observed(&account.profile, policy);
        let mut input = policy.inputs(account, org, oauth, !self.interactive_session_for_policy());
        input.session_only = input.non_interactive;
        if let Some(reason) = lingxi_core::host::fast_mode::decline(&input) {
            let message = reason.message(&input);
            if !message.is_empty() {
                return Err(LlmError::PermissionDenied {
                    message: format!("Fast mode unavailable: {message}"),
                });
            }
        }
        Ok(())
    }

    pub(in crate::service) async fn capture_fast_account(
        &self,
        prepared: &mut crate::PreparedLlmCall,
    ) -> Result<(), LlmError> {
        if !Self::direct_anthropic_api_route(prepared)
            || !prepared.fast_mode_allowed
            || lingxi_core::host::fast_mode::ModelRejections::for_process().blocked(
                &self.fast_rejection_identity(&prepared.route.resolved_route.request_model),
            )
        {
            return Ok(());
        }
        let fast_requested = prepared
            .provider_request
            .body_json
            .string_at("/speed")
            .as_deref()
            == Some("fast")
            || prepared
                .extra_body
                .as_ref()
                .is_some_and(|body| body.string_at("/speed").as_deref() == Some("fast"));
        if fast_requested {
            let generation = self.anthropic.fast_availability.generation();
            let credential = prepared.authenticator.captured_credential().await?;
            prepared.fast_account_binding = Some(crate::model::fast_admission::Binding::new(
                credential.as_ref(),
                generation,
            ));
        }
        Ok(())
    }

    pub(in crate::service) fn enforce_fast_route(
        &self,
        prepared: &mut crate::PreparedLlmCall,
    ) -> Result<(), LlmError> {
        let route = &prepared.route.resolved_route;
        let admitted = match self
            .client()
            .fast_account_identity(&route.request_model, Some(&route.profile_name))
        {
            Ok(account) => {
                let policy = (self.anthropic.fast_policy)()?;
                let (org, oauth) = prepared.fast_account_binding.map_or_else(
                    || {
                        self.anthropic
                            .fast_availability
                            .observed(&account.profile, &policy)
                    },
                    |binding| {
                        self.anthropic.fast_availability.observed_for(
                            &account.profile,
                            &policy,
                            binding,
                        )
                    },
                );
                let mut inputs =
                    policy.inputs(&account, org, oauth, !self.interactive_session_for_policy());
                // Model/global admission was captured before SDK/credential work.
                inputs.disabled = false;
                inputs.model_fast = prepared.fast_mode_allowed;
                lingxi_core::host::fast_mode::decline(&inputs).is_none()
            }
            Err(_) => false,
        };
        let allowed = Self::direct_anthropic_api_route(prepared)
            && prepared.fast_mode_allowed
            && admitted
            && !lingxi_core::host::fast_mode::ModelRejections::for_process().blocked(
                &self.fast_rejection_identity(&prepared.route.resolved_route.request_model),
            );
        if allowed {
            return Ok(());
        }
        lingxi_llm_client::providers::anthropic::request_policy::remove_fast_source(
            &mut prepared.provider_request.body_json,
            &mut prepared.provider_request.headers,
            &mut prepared.provider_request.json_string_overrides,
            FAST_MODE,
        )
        .map_err(crate::upstream::error)?;
        Ok(())
    }

    pub(in crate::service) fn fast_rejection_identity(&self, model: &str) -> String {
        let lower = model.to_lowercase();
        let model = self
            .alias_to_display
            .get(model)
            .or_else(|| self.alias_to_display.get(&lower))
            .map_or(lower.as_str(), String::as_str);
        let canonical = crate::model::betas::beta_canonical(model);
        canonical
            .strip_suffix("[1m]")
            .unwrap_or(&canonical)
            .to_string()
    }

    pub(in crate::service) fn model_unsupported_identity(
        &self,
        prepared: &crate::PreparedLlmCall,
    ) -> lingxi_llm_client::providers::anthropic::beta_repair::ModelUnsupportedIdentity {
        let route = &prepared.route.resolved_route;
        let provider_route = serde_json::to_string(&(
            prepared.route.protocol,
            &route.provider_id,
            &route.profile_name,
        ))
        .expect("resolved protocol route identity is serializable");
        lingxi_llm_client::providers::anthropic::beta_repair::ModelUnsupportedIdentity::new(
            provider_route,
            self.fast_rejection_identity(&route.request_model),
        )
    }

    pub(in crate::service) fn repair_fast_model_rejection(
        &self,
        req: &mut LlmRequest,
        prepared: &crate::PreparedLlmCall,
        status: u16,
        body: &lingxi_llm_client::protocol::JsonPayload,
    ) -> bool {
        if req.input.service_tier != Some(lingxi_llm_client::protocol::ServiceTier::Fast) {
            return false;
        }
        if Self::is_main_thinking_display_call(prepared)
            && prepared
                .provider_request
                .body_json
                .string_at("/speed")
                .as_deref()
                != Some("fast")
        {
            return false;
        }
        if lingxi_llm_client::providers::anthropic::error_recognition::fast_not_enabled(
            status, body,
        ) {
            req.input.service_tier = None;
            return true;
        }
        if !Self::is_main_thinking_display_call(prepared) {
            return false;
        }
        let model = &prepared.route.resolved_route.request_model;
        let identity = self.fast_rejection_identity(model);
        let models = lingxi_core::host::fast_mode::ModelRejections::for_process();
        let target = prepared
            .refusal_fallback_context
            .as_ref()
            .is_some_and(|context| {
                context.is_target(model, |name| self.fast_rejection_identity(name))
            });
        if !(target || models.known_rejected(&identity))
            || !lingxi_llm_client::providers::anthropic::error_recognition::speed_rejected_for(
                status,
                body,
                model,
                |name| self.fast_rejection_identity(name),
            )
        {
            return false;
        }
        models.reject(&identity);
        req.input.service_tier = None;
        true
    }
}
