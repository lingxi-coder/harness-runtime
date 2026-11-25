//! Service construction and session configuration.

use crate::model::user_agent::UserAgentEnv;
use crate::service::accounts::ApiServiceAccountObserver;
use crate::service::anthropic;
use crate::service::anthropic::prompt::CacheEditingInputs;
use crate::service::conversation::ConversationSlot;
use crate::service::request::parse_fallback_chain;
use crate::service::response::{PromptCacheOverageState, SubscriberState};
use crate::service::{ApiService, ACCOUNT_CALL_CLIENT};
use crate::{CostEstimator, LlmError, LlmRequest, ModelRuntime, PromptCacheQuerySource, Transport};
use std::sync::{Arc, Mutex, RwLock};
#[cfg(test)]
use std::time::Duration;

pub(super) type EffortSettingsSource = Arc<
    dyn Fn() -> Result<
            Vec<lingxi_core::host::effort::EffortSettingsLayer>,
            lingxi_core::settings::SettingsError,
        > + Send
        + Sync,
>;
pub(super) type PromptCacheTtlSettingsSource = Arc<
    dyn Fn() -> Result<
            lingxi_core::settings::schema::PromptCacheTtlSettings,
            lingxi_core::settings::SettingsError,
        > + Send
        + Sync,
>;

impl ApiService {
    /// Publish the resolved provider gate without changing another live session.
    pub fn set_tool_search_enabled(&self, enabled: bool) {
        self.tool_search_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn transport(&self) -> Arc<dyn Transport> {
        self.transport.clone()
    }

    /// Construct the service.  Called by Task 10 host constructors.
    ///
    /// `version` is the build version string embedded in the User-Agent header.
    ///
    /// `estimator` — when `Some`, a successful response decode populates
    /// `HistoryResponse.cost` via the llm-runtime `CostEstimator`.  Pass
    /// `None` to leave ordinary cost estimation disabled. Registered attempts
    /// always retain a frozen quote for their host accounting hooks.
    #[must_use]
    pub fn new(
        client: Arc<ModelRuntime>,
        transport: Arc<dyn Transport>,
        subscriber: SubscriberState,
        ua: UserAgentEnv,
        version: impl Into<String>,
        analytics: Option<Arc<::telemetry::AnalyticsBus>>,
        fallback_model: Option<String>,
    ) -> Self {
        Self::new_with_estimator(
            client,
            transport,
            subscriber,
            ua,
            version,
            analytics,
            fallback_model,
            None,
        )
    }

    /// Construct the service with an explicit cost estimator.
    ///
    /// Hosts that have the `cost::PricingCatalog` available (desktop + mobile)
    /// call this instead of [`Self::new`] to get live `HistoryResponse.cost` values.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_estimator(
        client: Arc<ModelRuntime>,
        transport: Arc<dyn Transport>,
        subscriber: SubscriberState,
        ua: UserAgentEnv,
        version: impl Into<String>,
        analytics: Option<Arc<::telemetry::AnalyticsBus>>,
        fallback_model: Option<String>,
        estimator: Option<Arc<CostEstimator>>,
    ) -> Self {
        Self::new_with_routing(
            client,
            transport,
            subscriber,
            ua,
            version,
            analytics,
            fallback_model,
            estimator,
            std::collections::BTreeMap::new(),
            None,
            None,
        )
    }

    /// Construct the service with routing overrides from `routing.fallback` /
    /// `routing.retry` settings.
    ///
    /// ## Constructor choice
    ///
    /// Hosts that parse `routing` settings call this after
    /// `parse_routing_overrides`; the older [`Self::new`] and
    /// [`Self::new_with_estimator`] paths delegate here with empty overrides so
    /// they continue to compile unchanged.
    ///
    /// ## Fallback precedence (per-request)
    ///
    /// Per-model `fallback_overrides` entry for the request's display model **wins**
    /// over the global `fallback_model` field.  When neither is set, no fallback
    /// is configured.
    ///
    /// ## Retry precedence
    ///
    /// `LINGXI_MAX_RETRIES` env > `settings_max_retries` > `DEFAULT_MAX_RETRIES` (10).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_routing(
        client: Arc<ModelRuntime>,
        transport: Arc<dyn Transport>,
        subscriber: SubscriberState,
        ua: UserAgentEnv,
        version: impl Into<String>,
        analytics: Option<Arc<::telemetry::AnalyticsBus>>,
        fallback_model: Option<String>,
        estimator: Option<Arc<CostEstimator>>,
        fallback_overrides: std::collections::BTreeMap<String, Vec<String>>,
        settings_max_retries: Option<u32>,
        settings_backoff_ms: Option<u64>,
    ) -> Self {
        let models = client.available_models();
        // Build alias→display map once so configured model fallback can
        // normalize an alias request to the display model before looking up
        // per-model fallback overrides (whose keys are display-normalized).
        let mut alias_to_display = std::collections::BTreeMap::new();
        for m in &models {
            for alias in &m.aliases {
                alias_to_display.insert(alias.clone(), m.display_model.clone());
            }
            // Map display_model → itself so the lookup is always correct
            // whether the caller used the canonical name or an alias.
            alias_to_display.insert(m.display_model.clone(), m.display_model.clone());
        }
        let fallback_models = fallback_model
            .as_deref()
            .map(parse_fallback_chain)
            .unwrap_or_default();
        let fast_availability = Arc::new(crate::model::fast_admission::Availability::default());
        let prompt_cache_overage = Arc::new(PromptCacheOverageState::default());
        let account_change_observer = Arc::new(ApiServiceAccountObserver {
            fast_availability: fast_availability.clone(),
            prompt_cache_overage: prompt_cache_overage.clone(),
        });
        Self {
            anthropic: anthropic::State {
                fast_policy: Arc::new(|| Ok(crate::model::fast_admission::Policy::default())),
                fast_availability,
                prompt_cache_ttl_settings_source: None,
                native_thinking_type_overrides: Mutex::new(Default::default()),
                native_model_option_capabilities: Mutex::new(Default::default()),
                native_thinking_resumption_refused: Arc::new(Default::default()),
                request_metadata: None,
                cache_editing_inputs: CacheEditingInputs::default(),
                anthropic_client_metadata: None,
                native_anthropic_api_base_url: None,
                anthropic_compatible_version: None,
                native_thinking_display: None,
                native_system_prefix: None,
                custom_cli_betas: Vec::new(),
                native_mid_conversation_system_cache: Mutex::new(Default::default()),
                prompt_cache_overage,
                aws_auth: None,
                foundry_capabilities: Mutex::new(Default::default()),
            },
            tool_search_enabled: std::sync::atomic::AtomicBool::new(false),
            account_routing_reporter: RwLock::new(None),
            scope_output_quota: None,
            account_change_observer: account_change_observer.clone(),
            account_change_observer_keepalive: vec![account_change_observer],
            client,
            model_attempt_hooks: RwLock::new(None),
            request_session_id_source: RwLock::new(None),
            effort_settings_source: None,
            inherited_effort_settings: None,
            effort_table_options: Default::default(),
            session_effort: RwLock::new(Default::default()),
            transport,
            subscriber,
            subscription: None,
            retry_reporter: None,
            forced_tool_choice: None,
            thinking: RwLock::new(crate::model::thinking_state::ThinkingState::default()),
            ua,
            version: version.into(),
            analytics,
            fallback_models,
            interactive_session: None,
            fallback_overrides,
            alias_to_display,
            settings_max_retries,
            settings_backoff_ms,
            estimator,
            last_rate_limit: Mutex::new(None),
            last_request_id: Mutex::new(None),
            last_retry_count: Mutex::new(0),
            thinking_recovery: crate::thinking_scope::ThinkingRecoveryScope::default(),
            last_raw_utilization: Mutex::new(None),
            last_429_message: Mutex::new(None),
            pending_near_limit_wrap_up_hint: Mutex::new(false),
            near_limit_wrap_up_window_key: Mutex::new(None),
            last_rate_limit_record_ts_ms: Mutex::new(None),
            stream_idle_timeout_override: None,
            stream_first_byte_timeout_override: None,
            responses_ws_session: Arc::new(Mutex::new(ConversationSlot {
                generation: 0,
                session: Some(Default::default()),
            })),
            history_projection_cache: Mutex::default(),
            side_sessions: Arc::default(),
        }
    }

    /// Fork a conversation adapter over the ready route/transport recipe.
    /// Request IDs, retries, thinking recovery, websocket sessions, cache
    /// overage and effort storage are newly allocated for this child.
    #[must_use]
    pub fn fork_independent(&self) -> Self {
        let mut child = Self::new_with_routing(
            self.client.clone(),
            self.transport.clone(),
            self.subscriber,
            self.ua.clone(),
            self.version.clone(),
            None,
            None,
            self.estimator.clone(),
            self.fallback_overrides.clone(),
            self.settings_max_retries,
            self.settings_backoff_ms,
        );
        child.set_tool_search_enabled(
            self.tool_search_enabled
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        *child
            .account_routing_reporter
            .write()
            .unwrap_or_else(|e| e.into_inner()) = self
            .account_routing_reporter
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        child.fallback_models = self.fallback_models.clone();
        child.scope_output_quota = self.scope_output_quota.clone();
        child.subscription = self.subscription.clone();
        child.anthropic.aws_auth = self.anthropic.aws_auth.clone();
        child.anthropic.custom_cli_betas = self.anthropic.custom_cli_betas.clone();
        child.interactive_session = self.interactive_session;
        child.anthropic.anthropic_compatible_version =
            self.anthropic.anthropic_compatible_version.clone();
        child.anthropic.native_system_prefix = self.anthropic.native_system_prefix.clone();
        child.anthropic.native_thinking_display = self.anthropic.native_thinking_display.clone();
        child.anthropic.anthropic_client_metadata =
            self.anthropic.anthropic_client_metadata.clone();
        child.anthropic.native_anthropic_api_base_url =
            self.anthropic.native_anthropic_api_base_url.clone();
        child.anthropic.request_metadata = self.anthropic.request_metadata.clone();
        child.effort_settings_source = self.effort_settings_source.clone();
        child.inherited_effort_settings = self.inherited_effort_settings.clone();
        child.anthropic.prompt_cache_ttl_settings_source =
            self.anthropic.prompt_cache_ttl_settings_source.clone();
        child.effort_table_options = self.effort_table_options.clone();
        child.anthropic.fast_policy = self.anthropic.fast_policy.clone();
        child.thinking = RwLock::new(self.thinking_state());
        child
    }

    /// Fork an app task without inheriting a conversation's fallback models.
    /// The host has authorized one provider/model; failure must stay explicit.
    #[must_use]
    pub fn fork_app_invocation(&self) -> Self {
        let mut child = self.fork_independent();
        child.fallback_models.clear();
        child.fallback_overrides.clear();
        child
    }

    /// Attach the AWS auth-refresh driver (2.1.198 `awsAuthRefresh` flow).
    /// Builder-style; the default is `None` (no refresh, errors stay terminal).
    #[must_use]
    pub fn with_aws_auth(
        mut self,
        aws_auth: Arc<dyn crate::auth::external_aws::AwsAuthRefresh>,
    ) -> Self {
        self.anthropic.aws_auth = Some(aws_auth);
        self
    }

    /// Test-only: force the streaming idle-watchdog timeout (the env floor of
    /// 5 min is otherwise untestable). Builder-style; default `None`.
    #[cfg(test)]
    #[must_use]
    pub fn with_stream_idle_timeout_override(mut self, timeout: Option<Duration>) -> Self {
        self.stream_idle_timeout_override = timeout;
        self
    }

    /// Test-only: force the streaming first-byte timeout. Builder-style;
    /// default `None` uses the provider/body-aware environment resolver.
    #[cfg(test)]
    #[must_use]
    pub fn with_stream_first_byte_timeout_override(mut self, timeout: Duration) -> Self {
        self.stream_first_byte_timeout_override = Some(timeout);
        self
    }

    /// Attach the live subscription slot (batch-5 Task 3). When present and
    /// resolved, the drive loops read subscriber/enterprise state from it at
    /// call time instead of the build-time [`SubscriberState`] copy.
    #[must_use]
    pub fn with_subscription(
        mut self,
        slot: lingxi_core::host::subscription::SharedSubscription,
    ) -> Self {
        self.subscription = Some(slot);
        self
    }

    /// Attach host-validated, stable-deduplicated CLI beta additions.
    #[must_use]
    pub fn with_custom_cli_betas(mut self, betas: Vec<String>) -> Self {
        self.anthropic.custom_cli_betas = betas;
        self
    }

    /// Attach the session's interaction mode for beta-header decisions.
    #[must_use]
    pub fn with_interactive_session(mut self, interactive: bool) -> Self {
        self.interactive_session = Some(interactive);
        self
    }

    /// Host-validated beta additions active for this service. Orchestrator
    /// context-window and compaction math must use the same list as request
    /// assembly (notably for the 1M-context beta).
    #[must_use]
    pub fn active_custom_betas(&self) -> &[String] {
        &self.anthropic.custom_cli_betas
    }

    pub fn with_forced_tool_choice(mut self, choice: crate::ToolChoice) -> Self {
        self.forced_tool_choice = Some(choice);
        self
    }

    /// Install the host's admitted settings source. It is sampled before each
    /// physical preparation, so edits and route fallback use current caps.
    /// An empty result clears caps and never triggers ambient settings loading.
    pub fn with_effort_settings_source(
        mut self,
        source: EffortSettingsSource,
    ) -> Result<Self, lingxi_core::settings::SettingsError> {
        self.inherited_effort_settings = Some(source()?);
        self.effort_settings_source = Some(source);
        Ok(self)
    }

    /// Install the host's merged current main/subagent cache-TTL settings.
    /// The source is sampled while building each request so file, CLI, managed,
    /// and mobile settings updates reach the SDK's Native TTL resolver.
    #[must_use]
    pub fn with_prompt_cache_ttl_settings_source(
        mut self,
        source: PromptCacheTtlSettingsSource,
    ) -> Self {
        self.anthropic.prompt_cache_ttl_settings_source = Some(source);
        self
    }

    /// Supply the native first-start/managed override state without ambient discovery.
    #[must_use]
    pub fn with_effort_table_options(
        mut self,
        options: lingxi_core::host::effort_table::TableOptions,
    ) -> Self {
        self.effort_table_options = options;
        self
    }

    pub fn set_session_effort(&self, effort: lingxi_core::host::effort_table::SessionEffort) {
        *self
            .session_effort
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = effort;
    }

    /// Current command state uses the admitted boot defaults and live caps,
    /// exactly like the next main preparation, without invoking authentication.
    pub fn effort_command_snapshot(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<Option<lingxi_core::host::effort::EffortCommandSnapshot>, LlmError> {
        let mut request = LlmRequest::new(model);
        request.profile = profile.map(str::to_owned);
        request.execution.resolve_native_effort = true;
        self.refresh_effort_settings(&mut request)?;
        self.client().effort_command_snapshot(&request)
    }

    pub(super) fn refresh_effort_settings(&self, request: &mut LlmRequest) -> Result<(), LlmError> {
        request
            .execution
            .inherited_effort_settings
            .clone_from(&self.inherited_effort_settings);
        request
            .execution
            .effort_table_options
            .clone_from(&self.effort_table_options);
        request.execution.session_effort = self
            .session_effort
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(source) = &self.effort_settings_source {
            request.execution.effort_settings = Some(source()?);
        }
        Ok(())
    }

    /// Set the session thinking configuration. Builder-style; the default is
    /// [`ThinkingConfig::Adaptive`](crate::model::thinking::ThinkingConfig::Adaptive).
    #[must_use]
    pub fn with_thinking(mut self, thinking: crate::model::thinking::ThinkingConfig) -> Self {
        self.thinking = RwLock::new(crate::model::thinking_state::ThinkingState::new(thinking));
        self
    }

    /// Replace the live session thinking policy. The next request observes the
    /// new value; an already-open response stream is intentionally unaffected.
    pub fn set_thinking(&self, thinking: crate::model::thinking::ThinkingConfig) {
        self.thinking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_base(thinking);
    }

    /// Replace or clear the Native max-thinking-token overlay. `None` restores
    /// the current base policy; changing the base does not clear this overlay.
    pub fn set_max_thinking_tokens_override(
        &self,
        value: Option<crate::model::thinking_state::NativeIntegerNumber>,
    ) {
        self.thinking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_max_thinking_tokens_override(value);
    }

    fn generic_thinking_config(
        state: crate::model::thinking_state::ThinkingState,
    ) -> Result<crate::model::thinking::ThinkingConfig, LlmError> {
        state.generic_request_config().map_err(|value| LlmError::InvalidRequest {
            message: format!(
                "max thinking token override must be zero or a positive integer no greater than {} for this route (received {})",
                u32::MAX,
                value.get()
            ),
        })
    }

    /// Resolve the session policy through the provider-neutral thinking API.
    /// Native Main requests use their route-specific Lsn policy during request
    /// preparation; this getter is for generic consumers such as side queries.
    pub fn thinking(&self) -> Result<crate::model::thinking::ThinkingConfig, LlmError> {
        Self::generic_thinking_config(self.thinking_state())
    }

    pub(super) fn main_request_thinking_config(
        &self,
        model: &str,
        profile: Option<&str>,
        query_source: PromptCacheQuerySource<'_>,
        state: crate::model::thinking_state::ThinkingState,
    ) -> Result<crate::model::thinking::ThinkingConfig, LlmError> {
        if self.native_main_thinking_policy_route(model, profile, query_source) {
            // Native Main applies Lsn after the exact route and output budget
            // are selected, so do not reject or narrow its wider JS number here.
            Ok(state.base())
        } else {
            Self::generic_thinking_config(state)
        }
    }

    pub(super) fn thinking_state(&self) -> crate::model::thinking_state::ThinkingState {
        *self
            .thinking
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn client(&self) -> Arc<ModelRuntime> {
        let owner = Arc::as_ptr(&self.client) as usize;
        ACCOUNT_CALL_CLIENT
            .try_with(|(selected_owner, client)| (*selected_owner == owner).then(|| client.clone()))
            .ok()
            .flatten()
            .unwrap_or_else(|| crate::catalog_task::client(&self.client))
    }
}
