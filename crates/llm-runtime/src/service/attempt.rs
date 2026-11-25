//! Host admission, timeouts and accounting for physical attempts.

use crate::dispatch_header::{DispatchAttempt, DispatchHeaderState};
use crate::model::retry_scope::ModelCallRetryScope;
use crate::service::ApiService;
use crate::{LlmError, LlmRequest, ModelRuntime};
use std::sync::Arc;

// Keep provider state machines out of the generic trace/retry scope futures.
// Construct through a factory so their large temporaries stay off the caller's
// poll stack as well as the final boxed future.
#[inline(never)]
pub(super) fn boxed_model_future<'a, T, F>(create: impl FnOnce() -> F) -> crate::BoxFuture<'a, T>
where
    F: std::future::Future<Output = T> + Send + 'a,
{
    Box::pin(create())
}

/// Live session owner for calls that begin outside a scoped turn.
pub type RequestSessionIdSource =
    Arc<dyn Fn() -> crate::BoxFuture<'static, Option<String>> + Send + Sync>;

/// Generate a local Host telemetry ID; provider request IDs belong to the SDK.
#[must_use]
pub(super) fn new_telemetry_id() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-";
    let mut rng = rand::rng();
    (0..16)
        .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
        .collect()
}

/// Host authority shared by both SDK dispatch paths.
pub(super) struct AttemptAdmission<'a> {
    request: &'a crate::ExecutionContext,
    quota: Option<&'a crate::ScopeOutputQuota>,
    pub(super) rejected: bool,
}

impl<'a> AttemptAdmission<'a> {
    pub(super) fn new(service: &'a ApiService, request: &'a LlmRequest) -> Self {
        Self {
            request: &request.execution,
            quota: service.scope_output_quota.as_deref(),
            rejected: false,
        }
    }

    pub(super) fn check_computer_submission(&mut self) -> Result<(), LlmError> {
        if self.request.computer_submission.is_some() && !self.host_admitted() {
            return Err(self.reject());
        }
        Ok(())
    }

    pub(super) async fn authorize_quota(&mut self) -> Result<(), LlmError> {
        if let Some(quota) = self.quota {
            if let Err(error) = quota.authorize_dispatch().await {
                self.rejected |= quota.is_revoked();
                return Err(error);
            }
        }
        Ok(())
    }

    /// Runs immediately before the SDK writes to the selected transport. Both
    /// reservations roll back if either budget or accounting rejects the send.
    pub(super) fn mark_dispatch(
        &mut self,
        attempt: &mut crate::model_attempt::WireAttempt,
        observations: &DispatchObservations,
        any_dispatched: &mut bool,
    ) -> Result<(), LlmError> {
        if !self.host_admitted() || self.quota.is_some_and(|quota| quota.is_revoked()) {
            return Err(self.reject());
        }
        let api_reservation = self
            .request
            .api_attempts_left
            .as_ref()
            .map(|budget| budget.reserve().ok_or(LlmError::ApiAttemptsExhausted))
            .transpose()?;
        let quota_reservation = self
            .quota
            .map(|quota| quota.reserve().ok_or(LlmError::ApiAttemptsExhausted))
            .transpose()?;
        attempt.mark_dispatched()?;
        if let Some(reservation) = api_reservation {
            reservation.commit();
        }
        if let Some(reservation) = quota_reservation {
            reservation.commit();
        }
        observations.publish_tool_change();
        if let Some(observer) = &self.request.request_dispatch_observer {
            observer.mark_dispatched();
        }
        *any_dispatched = true;
        observations.publish_cache();
        Ok(())
    }

    fn host_admitted(&self) -> bool {
        self.request
            .request_dispatch_admission
            .as_ref()
            .is_none_or(|admission| admission.is_admitted())
    }

    fn reject(&mut self) -> LlmError {
        self.rejected = true;
        LlmError::InvalidRequest {
            message: "host rejected request dispatch admission".into(),
        }
    }
}

/// Observations are frozen from the sealed request and published only on
/// physical dispatch, including when the SDK switches transports.
pub(super) struct DispatchObservations {
    cache: Option<crate::prompt_cache::RequestSnapshot>,
    openai_cache: Option<crate::prompt_cache::OpenAiPrefixSnapshot>,
    tool_change_scope: Option<crate::thinking_scope::ThinkingRecoveryScope>,
}

impl DispatchObservations {
    pub(super) fn capture(prepared: &crate::PreparedLlmCall) -> Self {
        Self {
            cache: crate::prompt_cache::snapshot_prepared(prepared),
            openai_cache: crate::prompt_cache::openai_snapshot_prepared(prepared),
            tool_change_scope: ApiService::tool_change_dispatch_observation(prepared),
        }
    }

    fn publish_tool_change(&self) {
        if let Some(scope) = &self.tool_change_scope {
            scope.mark_mid_conversation_tool_changes_sent();
        }
    }

    fn publish_cache(&self) {
        crate::prompt_cache::observe_snapshot(self.cache.clone());
        crate::prompt_cache::observe_openai_snapshot(self.openai_cache.clone());
    }
}

impl ApiService {
    /// Install the host's registered-attempt authority after composition.
    /// Ordinary requests without context never invoke this hook.
    pub fn set_model_attempt_hooks(&self, hooks: Arc<dyn crate::ModelAttemptHooks>) {
        let retired = self
            .model_attempt_hooks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(hooks);
        drop(retired);
    }

    /// Retire host authority after the host has drained all producers and
    /// receipts. Registered requests then fail closed; ordinary requests are
    /// unchanged. Other service owners must not extend a closed session lease.
    pub fn clear_model_attempt_hooks(&self) {
        let retired = self
            .model_attempt_hooks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        // Host destructors can release their own graphs or reenter the service.
        // Never run them under the service's hook lock.
        drop(retired);
    }

    /// Attach a live session owner for calls that begin outside a scoped turn.
    /// The callback is queried per logical request and must not retain the
    /// service or orchestrator strongly.
    pub fn set_request_session_id_source(&self, source: RequestSessionIdSource) {
        *self
            .request_session_id_source
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(source);
    }

    pub(super) async fn capture_request_session_id(&self, request: &mut LlmRequest) {
        if request.execution.request_session_id.is_none() {
            if request.execution.model_attempt.is_some() {
                request.execution.request_session_id = request
                    .execution
                    .model_attempt
                    .as_ref()
                    .and_then(|context| {
                        self.model_attempt_hooks
                            .read()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .as_ref()
                            .and_then(|hooks| hooks.request_session_id(context))
                    })
                    .map(|session_id| session_id.as_uuid().to_string());
            } else if let Some(session_id) =
                lingxi_core::host::session_flags::current_request_session_id()
            {
                request.execution.request_session_id = Some(session_id.as_uuid().to_string());
            } else if !lingxi_core::host::session_flags::current_request_is_subagent() {
                let source = self
                    .request_session_id_source
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let Some(source) = source {
                    request.execution.request_session_id = source().await;
                }
            }
        }

        self.update_anthropic_session_metadata(request);
    }

    pub(super) async fn begin_model_attempt(
        &self,
        request: &LlmRequest,
        prepared: &crate::PreparedLlmCall,
    ) -> Result<crate::model_attempt::WireAttempt, LlmError> {
        let Some(context) = request.execution.model_attempt.as_ref() else {
            return Ok(
                crate::model_attempt::WireAttempt::new(None).with_classifier(request, prepared)
            );
        };
        let hooks = self
            .model_attempt_hooks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(crate::model_attempt::missing_hooks_error)?;
        let lease = hooks
            .begin(context, request, prepared)
            .await
            .map_err(crate::model_attempt::accounting_error)?;
        Ok(crate::model_attempt::WireAttempt::new(Some(lease)).with_classifier(request, prepared))
    }

    /// Bind estimation and admission to the selected physical route.
    pub(super) async fn bind_attempt_route(
        &self,
        request: &mut LlmRequest,
        inherit_effort: bool,
    ) -> Result<Arc<ModelRuntime>, LlmError> {
        if inherit_effort {
            self.refresh_effort_settings(request)?;
            request.execution.resolve_native_effort = true;
        }
        let client = self.client();
        client.bind_context_usage_for_route(request)?;
        self.ensure_context_fit(request).await?;
        Ok(client)
    }

    /// Apply the selected provider's host policy before the SDK seals the call.
    pub(super) async fn finish_attempt_preparation(
        &self,
        request: &LlmRequest,
        prepared: &mut crate::PreparedLlmCall,
        scope: &ModelCallRetryScope,
        dispatch: DispatchHeaderState,
        stream: bool,
    ) -> Result<(DispatchAttempt, bool), LlmError> {
        if !prepared.route.protocol.is_anthropic_family() {
            self.apply_user_agent(prepared);
            Self::log_deepseek_prepared_request(&request.input.model, prepared, stream);
            return Ok((DispatchAttempt::default(), false));
        }
        self.capture_fast_account(prepared).await?;
        prepared.thinking_display_probe = scope.display_probe();
        Self::log_deepseek_prepared_request(&request.input.model, prepared, stream);
        if stream {
            self.inject_stream_headers(prepared, dispatch)
        } else {
            self.inject_headers(prepared, dispatch)
        }
    }

    /// Attach one trusted invocation quota. Forked services retain the same
    /// capability; request JSON, explicit max_tokens and retries cannot reset it.
    #[must_use]
    pub fn with_scope_output_quota(mut self, quota: Option<Arc<crate::ScopeOutputQuota>>) -> Self {
        self.scope_output_quota = quota;
        self
    }

    pub(super) fn clamp_scope_output(
        &self,
        request: &mut LlmRequest,
        prior_dispatch: bool,
    ) -> Result<(), LlmError> {
        if let Some(quota) = &self.scope_output_quota {
            if quota.is_revoked() {
                return Err(LlmError::RequestDispatchRejected { prior_dispatch });
            }
            request.input.max_tokens = Some(quota.clamp(request.input.max_tokens));
        }
        Ok(())
    }

    pub(super) fn validate_scope_output(
        &self,
        prepared: &crate::PreparedLlmCall,
    ) -> Result<(), LlmError> {
        if let Some(quota) = &self.scope_output_quota {
            quota.validate_prepared(prepared)?;
        }
        Ok(())
    }
}
