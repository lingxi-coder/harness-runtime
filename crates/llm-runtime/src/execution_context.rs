//! Host authority and recovery state for one logical model call.
//!
//! Kept outside model input so serialized requests cannot manufacture admission
//! authority or carry account identity across sessions. This context is never
//! forwarded to a provider; selected account scopes are passed as SDK options.

use std::sync::Arc;

type AdmissionCheck = Arc<dyn Fn() -> bool + Send + Sync>;

struct AdmissionChecks {
    global: Vec<AdmissionCheck>,
    by_message: Vec<(lingxi_core::types::MessageId, AdmissionCheck)>,
}

/// Host-owned check evaluated immediately before a logical request reaches
/// the SDK transport. A rejection stops this drive; it does not claim atomicity
/// with network bytes once dispatch has been admitted.
#[derive(Clone)]
pub struct RequestDispatchAdmission {
    checks: Arc<AdmissionChecks>,
    selected_messages: Option<Arc<std::collections::HashSet<lingxi_core::types::MessageId>>>,
}

impl RequestDispatchAdmission {
    /// Capture host-owned admission state for one logical model request.
    #[must_use]
    pub fn new(check: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        let check: AdmissionCheck = Arc::new(check);
        Self::with_global_and_message_sources(
            [check],
            std::iter::empty::<(lingxi_core::types::MessageId, AdmissionCheck)>(),
        )
    }

    /// Capture global checks and source-row checks for one logical request.
    #[must_use]
    pub fn with_global_and_message_sources(
        global: impl IntoIterator<Item = Arc<dyn Fn() -> bool + Send + Sync>>,
        by_message: impl IntoIterator<
            Item = (
                lingxi_core::types::MessageId,
                Arc<dyn Fn() -> bool + Send + Sync>,
            ),
        >,
    ) -> Self {
        Self {
            checks: Arc::new(AdmissionChecks {
                global: global.into_iter().collect(),
                by_message: by_message.into_iter().collect(),
            }),
            selected_messages: None,
        }
    }

    /// Bind checks to the source messages whose content they authorize.
    #[must_use]
    pub fn for_message_sources(
        checks: impl IntoIterator<
            Item = (
                lingxi_core::types::MessageId,
                Arc<dyn Fn() -> bool + Send + Sync>,
            ),
        >,
    ) -> Self {
        Self::with_global_and_message_sources(
            std::iter::empty::<AdmissionCheck>(),
            checks,
        )
    }

    /// Retain row-scoped checks only while their source content contributes to
    /// the final semantic request. Global checks remain active independently.
    #[must_use]
    pub fn retaining_message_sources(
        &self,
        source_ids: &std::collections::HashSet<lingxi_core::types::MessageId>,
    ) -> Option<Self> {
        let selected_messages = self
            .checks
            .by_message
            .iter()
            .filter(|(id, _)| {
                source_ids.contains(id)
                    && self
                        .selected_messages
                        .as_ref()
                        .is_none_or(|selected| selected.contains(id))
            })
            .map(|(id, _)| *id)
            .collect::<std::collections::HashSet<_>>();
        if self.checks.global.is_empty() && selected_messages.is_empty() {
            return None;
        }
        Some(Self {
            checks: Arc::clone(&self.checks),
            selected_messages: Some(Arc::new(selected_messages)),
        })
    }

    /// Return whether this prepared request may cross the SDK dispatch marker.
    #[must_use]
    pub fn is_admitted(&self) -> bool {
        self.checks.global.iter().all(|check| check())
            && self.checks.by_message.iter().all(|(id, check)| {
                match &self.selected_messages {
                    None => check(),
                    Some(selected) => !selected.contains(id) || check(),
                }
            })
    }
}

impl std::fmt::Debug for RequestDispatchAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestDispatchAdmission(..)")
    }
}

impl PartialEq for RequestDispatchAdmission {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.checks, &other.checks)
            && self.selected_messages == other.selected_messages
    }
}

/// Host-owned execution state. Cloning preserves the logical attempt and its
/// recovery scope; it does not authorize another physical dispatch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutionContext {
    /// Native prompt source and typed cache inputs, projected after the
    /// selected request credential is captured in `ModelRuntime`. The Host
    /// account generation comes from a separate live getter, not from the SDK
    /// credential snapshot.
    pub prompt_cache: Option<PromptCacheRequestContext>,
    /// Host-admitted native server fallback policy, outside provider input.
    pub server_fallback:
        Option<lingxi_llm_client::providers::anthropic::fallback_request::RequestPolicy>,
    /// Host query routing and live-latch facts. Never serialized as model input.
    pub refusal_fallback_context: Option<lingxi_core::host::refusal_driver::FallbackTargetContext>,
    /// Trusted native effort inputs, independent of model input and persistence.
    pub effort_state: lingxi_core::host::effort::EffortState,
    /// Explicit original host settings sources. Some(empty) is authoritative.
    pub effort_settings: Option<Vec<lingxi_core::host::effort::EffortSettingsLayer>>,
    /// Native service feature resolution; raw ModelRuntime callers keep their
    /// provider-neutral SDK controls unless the host selects this policy.
    pub resolve_native_effort: bool,
    /// Native inherited defaults are a boot snapshot, independently of live caps.
    pub inherited_effort_settings: Option<Vec<lingxi_core::host::effort::EffortSettingsLayer>>,
    pub effort_table_options: lingxi_core::host::effort_table::TableOptions,
    pub session_effort: lingxi_core::host::effort_table::SessionEffort,
    /// Side-query caller explicitly selected disabled thinking.
    pub side_thinking_disabled: bool,
    /// Native main/side-query body policy, independent of dispatch category.
    pub anthropic_request_kind:
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind,
    /// Wire family used to prepare canonical history. Route changes adapt a
    /// clone through the SDK; absent for inputs authored directly for a route.
    pub input_protocol: Option<lingxi_llm_client::protocol::ProtocolFamily>,
    /// Exact UTF-16 strings in canonical SDK message/content coordinates.
    /// The selected llm-client codec reindexes them onto its encoded JSON body
    /// before final request serialization.
    pub message_json_string_overrides: std::collections::BTreeMap<String, Vec<u16>>,
    /// Registered admission authority, never forwarded to the provider.
    pub model_attempt: Option<lingxi_core::host::ModelAttemptContext>,
    /// Host-owned logical dispatch check, never serialized into provider input.
    pub request_dispatch_admission: Option<RequestDispatchAdmission>,
    /// Host message sources whose content contributes to the canonical request.
    /// Used to project row-scoped admission only after all normalization passes.
    pub request_message_source_ids: std::collections::HashSet<lingxi_core::types::MessageId>,
    /// Historical identities used by signature recovery; never provider wire.
    pub thinking_source_message_ids: Vec<::lingxi_core::types::MessageId>,
    /// Query ownership for retries and lazy streams; never provider wire.
    pub thinking_recovery_scope: Option<crate::thinking_scope::ThinkingRecoveryScope>,
    /// Trusted account identity for provider continuations and hosted resources.
    pub account_scope: Option<String>,
    /// Capture live request credentials for host-selected native computer state.
    pub computer_request: bool,
    /// This request declares native computer controls rather than management functions.
    pub computer_native: bool,
    /// Original native receipt binding that the captured current request must match.
    pub expected_computer_binding: Option<lingxi_core::host::NativeContinuationBinding>,
    /// Durable receipt submission immediately before a prepared SDK dispatch.
    pub computer_submission: Option<Arc<crate::computer::ComputerReceiptSubmission>>,
    /// Trusted host account identity for provider-owned file references.
    pub file_account_scope: Option<String>,
    /// Whether the host response should include per-call retry accounting.
    pub capture_retry_count: bool,
    /// Internal side-query purpose for telemetry; never sent to providers.
    pub query_source: Option<String>,
    /// Activate the context-hint beta even when the offer omits its body.
    /// Consumed before sealing; never serialized into model input.
    pub context_hint_beta: bool,
    /// Native server-error fallback outlasted the current nonstream threshold.
    /// A watchdog timeout ceiling consumes this trusted fact; never provider wire.
    pub failed_stream_outlasted_timeout: bool,
    /// This request replaces an abandoned stream. Selects final SDK fallback
    /// parameters independently of whether that stream was long.
    pub stream_fallback: bool,
}

impl ExecutionContext {
    /// Install request admission after the canonical message vector is built.
    /// Row-scoped checks for pruned content disappear; global checks remain.
    pub fn set_request_dispatch_admission(&mut self, admission: Option<RequestDispatchAdmission>) {
        self.request_dispatch_admission = admission.and_then(|admission| {
            admission.retaining_message_sources(&self.request_message_source_ids)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::RequestDispatchAdmission;
    use std::collections::HashSet;
    use std::sync::Arc;

    type Check = Arc<dyn Fn() -> bool + Send + Sync>;

    fn check(value: bool) -> Check {
        Arc::new(move || value)
    }

    #[test]
    fn unprojected_source_checks_fail_closed() {
        let stale_id = lingxi_core::types::MessageId::new();
        let admission = RequestDispatchAdmission::for_message_sources([(
            stale_id,
            check(false),
        )]);
        assert!(!admission.is_admitted());
    }

    #[test]
    fn projection_prunes_row_checks_but_keeps_global_checks() {
        let source_id = lingxi_core::types::MessageId::new();
        let message_only = RequestDispatchAdmission::for_message_sources([(
            source_id,
            check(false),
        )]);
        assert!(message_only
            .retaining_message_sources(&HashSet::new())
            .is_none());

        let mixed = RequestDispatchAdmission::with_global_and_message_sources(
            [check(true)],
            [(source_id, check(false))],
        );
        assert!(!mixed.is_admitted(), "unprojected stale row rejects");
        assert!(mixed
            .retaining_message_sources(&HashSet::new())
            .expect("global admission remains after row pruning")
            .is_admitted());

        let global_rejection = RequestDispatchAdmission::with_global_and_message_sources(
            [check(false)],
            [(source_id, check(true))],
        )
        .retaining_message_sources(&HashSet::new())
        .expect("global admission remains after row pruning");
        assert!(!global_rejection.is_admitted());
    }
}

/// Host-only inputs needed to project a Native system prompt at provider
/// egress. The service supplies a live account-generation getter and a
/// scope-keyed overage lookup; `ModelRuntime` fences that generation around the
/// selected credential snapshot used by the request.
#[derive(Clone)]
pub struct PromptCacheRequestContext {
    /// The current Native system-prompt input, before provider block projection.
    pub system: Option<lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    /// SDK-owned process and host settings supplied by this request's builder.
    pub policy: lingxi_llm_client::providers::anthropic::system_prompt::CachePolicy,
    /// Read the current Host auth generation. ModelRuntime samples it before
    /// and after the one request-scoped credential load so a generation change
    /// during that await cannot pair old credential material with a new epoch.
    pub current_account_epoch: std::sync::Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Native's current `CLAUDE_CODE_SIMPLE` / `--bare` subscriber disablement.
    pub native_bare_mode: bool,
    /// Native's Unix-socket OAuth source is outside this Host credential seam.
    pub native_unix_socket: bool,
    /// Reads an overage observation only for the exact credential scope and
    /// generation captured by the request.
    pub overage_for_scope:
        std::sync::Arc<dyn Fn(&crate::CredentialScope, u64) -> bool + Send + Sync>,
    /// Per-logical-request 429 observation; retried attempts share the slot.
    pub pending_overage: std::sync::Arc<std::sync::Mutex<Option<PendingPromptCacheObservation>>>,
}

/// Decoded Anthropic 429 quota state waiting for the retry driver to decide
/// whether that error became terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPromptCacheObservation {
    /// Exact selected Host credential scope for this physical request.
    pub scope: crate::CredentialScope,
    /// Auth generation bound to the prepared credential attempt. If that
    /// attempt observed a generation change during credential loading, this
    /// remains the pre-await generation and its responses are rejected.
    pub account_epoch: u64,
    /// `Iet` overage boolean decoded by the SDK from the 429 headers.
    pub is_using_overage: bool,
    /// Host response observation order, used to discard stale concurrent data.
    pub observed_at_ms: u128,
}

impl std::fmt::Debug for PromptCacheRequestContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PromptCacheRequestContext")
            .field("system", &self.system)
            .field("policy", &self.policy)
            .field("current_account_epoch", &"<host generation getter>")
            .field("native_bare_mode", &self.native_bare_mode)
            .field("native_unix_socket", &self.native_unix_socket)
            .field("overage_for_scope", &"<host state lookup>")
            .field("pending_overage", &"<request-local state>")
            .finish()
    }
}

impl PartialEq for PromptCacheRequestContext {
    fn eq(&self, other: &Self) -> bool {
        let left = &self.policy;
        let right = &other.policy;
        self.system == other.system
            && self.native_bare_mode == other.native_bare_mode
            && self.native_unix_socket == other.native_unix_socket
            && left.hipaa_tainted == right.hipaa_tainted
            && left.agent_prompt_cache_ttl_override == right.agent_prompt_cache_ttl_override
            && left.query_source_is_main == right.query_source_is_main
            && left.query_source == right.query_source
            && left.prompt_cache_ttl_settings == right.prompt_cache_ttl_settings
            && left.prompt_cache_ttl_inputs.subscriber == right.prompt_cache_ttl_inputs.subscriber
            && left.prompt_cache_ttl_inputs.is_using_overage
                == right.prompt_cache_ttl_inputs.is_using_overage
            && left.experimental_betas_disabled == right.experimental_betas_disabled
            && left.process_base_url_allowed == right.process_base_url_allowed
            && left.assume_first_party_base_url == right.assume_first_party_base_url
            && left.skip_global_cache_for_system_prompt == right.skip_global_cache_for_system_prompt
    }
}
