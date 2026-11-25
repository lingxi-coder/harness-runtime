//! Request correlation and quota observations exposed to the host.

use crate::model::rate_limit::{
    formatted_reset_times_from_decoded, rate_limit_error_message, RateLimitInfo, RawUtilization,
    SubscriptionContext,
};
use crate::service::retry::{is_free_tier_model, openrouter_free_rate_limit_message};
use crate::service::ApiService;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Subscription flags — gates the 429 retry policy.
///
/// Task 8 wires real values from auth; default is both false (conservative:
/// over-retries 429s slightly, but never breaks).
#[derive(Debug, Clone, Copy, Default)]
pub struct SubscriberState {
    /// `true` when the configured credential is a Claude.ai OAuth subscriber.
    pub is_subscriber: bool,
    /// `true` when the subscriber is an enterprise account.
    pub is_enterprise: bool,
}

/// Origin of the request id recorded by [`ApiService::last_request_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RequestIdOrigin {
    /// The provider returned its own id in a known response header
    /// (authoritative — valid for provider-side log/support lookups).
    Server,
    /// No server id header was present, so the outgoing `x-client-request-id`
    /// we sent is used as a fallback. Correlation-only: a provider will NOT find
    /// this id in its logs.
    Client,
}

pub(super) fn response_request_id(
    server: Option<String>,
    client: Option<&str>,
) -> Option<(String, RequestIdOrigin)> {
    server.map(|id| (id, RequestIdOrigin::Server)).or_else(|| {
        client
            .filter(|id| !id.is_empty())
            .map(|id| (id.to_owned(), RequestIdOrigin::Client))
    })
}

pub(super) struct Observed429 {
    pub(super) pending: Option<Pending429>,
    pub(super) message: Option<String>,
}

const NEAR_LIMIT_WRAP_UP_DEFAULT_THRESHOLD: f64 = 0.95;
const NEAR_LIMIT_WRAP_UP_MAX5X_THRESHOLD: f64 = 0.99;
const NEAR_LIMIT_WRAP_UP_MAX20X_THRESHOLD: f64 = 0.9975;

/// 429-attempt state held until the retry loop declares the error TERMINAL —
/// TS updates module state only in the terminal catch handler
/// (`extractQuotaStatusFromError`, claudeAiLimits.ts:487), never on retried
/// attempts. Each response owns its snapshot until its terminal decision.
pub(super) struct Pending429 {
    /// Forced-rejected limits snapshot (`from_429_error_headers`); `None`
    /// when the unified-header limits gate did not pass but raw windows did.
    pub(super) info: Option<RateLimitInfo>,
    /// Raw per-window utilization from the SAME error headers, computed
    /// UNCONDITIONALLY (`extractRawUtilization`, ts:500 — independent of the
    /// limits gate).
    pub(super) raw: RawUtilization,
    observation: RateLimitHeaderObservation,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct RateLimitAccount {
    scope: crate::CredentialScope,
    request_auth: crate::AuthStrategy,
    uses_session_subscription: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct RateLimitOwner {
    account: RateLimitAccount,
    credential_revision: Option<u64>,
    unversioned_credential_identity: Option<String>,
}

impl RateLimitOwner {
    fn from_capture(
        scope: &crate::CredentialScope,
        request_auth: crate::AuthStrategy,
        uses_session_subscription: bool,
        authenticator: &crate::execution::HostAuthenticator,
    ) -> Self {
        let revision = authenticator.captured_credential_revision();
        Self {
            account: RateLimitAccount {
                scope: scope.clone(),
                request_auth,
                uses_session_subscription,
            },
            credential_revision: revision,
            unversioned_credential_identity: revision
                .is_none()
                .then(|| authenticator.captured_unversioned_credential_identity())
                .flatten(),
        }
    }
}

/// A child runner's notice and checkpoint callback for a single logical request.
#[derive(Clone)]
pub struct SubagentWrapUpHint {
    /// Exact model-visible checkpoint instruction.
    pub text: &'static str,
    /// Source identity shared by the wire projection and the child transcript.
    pub message_id: lingxi_core::types::MessageId,
    publish: Arc<dyn Fn() -> crate::BoxFuture<'static, bool> + Send + Sync>,
    claimed: Arc<tokio::sync::Mutex<Option<(u64, RateLimitOwner)>>>,
}

impl std::fmt::Debug for SubagentWrapUpHint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentWrapUpHint")
            .field("message_id", &self.message_id)
            .finish_non_exhaustive()
    }
}

impl SubagentWrapUpHint {
    /// Create a request-local callback. The signing account determines whether it runs.
    pub fn new(
        text: &'static str,
        message_id: lingxi_core::types::MessageId,
        publish: impl Fn() -> crate::BoxFuture<'static, bool> + Send + Sync + 'static,
    ) -> Self {
        Self {
            text,
            message_id,
            publish: Arc::new(publish),
            claimed: Default::default(),
        }
    }

    /// Check the live rollout flag and publish the host notice when enabled.
    pub async fn publish(&self) -> bool {
        (self.publish)().await
    }
}

#[derive(Clone)]
pub(crate) struct BoundWrapUpHint {
    hint: SubagentWrapUpHint,
    limits: Arc<RateLimitState>,
    epoch: u64,
}

tokio::task_local! {
    static SUBAGENT_WRAP_UP: Option<BoundWrapUpHint>;
}

impl BoundWrapUpHint {
    pub(super) fn current(service: &ApiService) -> Option<Self> {
        SUBAGENT_WRAP_UP
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .filter(|bound| Arc::ptr_eq(&bound.limits, &service.rate_limits))
    }

    pub(crate) fn may_match(&self, scope: &crate::CredentialScope) -> bool {
        let Ok(claimed) = self.hint.claimed.try_lock() else {
            // A concurrent Mod invocation is publishing. Wait for its exact
            // owner below before deciding whether this attempt can reuse it.
            return true;
        };
        let state = self.limits.lock();
        state.account_epoch == self.epoch
            && claimed.as_ref().map_or_else(
                || {
                    state.accounts.iter().any(|(account, history)| {
                        &account.scope == scope && history.pending_wrap_up
                    })
                },
                |(epoch, owner)| *epoch == self.epoch && &owner.account.scope == scope,
            )
    }

    pub(crate) async fn note_for(
        &self,
        scope: &crate::CredentialScope,
        request_auth: crate::AuthStrategy,
        uses_session_subscription: bool,
        authenticator: &crate::execution::HostAuthenticator,
    ) -> Option<(&'static str, lingxi_core::types::MessageId)> {
        let owner = RateLimitOwner::from_capture(
            scope,
            request_auth,
            uses_session_subscription,
            authenticator,
        );
        // Pooled backends which omit revisions must still identify their exact
        // captured credential. A profile label alone cannot own a pending note.
        if scope.expected_identity_generation.is_some()
            && owner.credential_revision.is_none()
            && owner.unversioned_credential_identity.is_none()
        {
            return None;
        }
        let note = Some((self.hint.text, self.hint.message_id));
        let mut claimed = self.hint.claimed.lock().await;
        {
            let mut state = self.limits.lock();
            if state.account_epoch != self.epoch {
                return None;
            }
            if let Some((epoch, previous)) = claimed.as_ref() {
                return if *epoch == self.epoch
                    && previous == &owner
                    && state
                        .accounts
                        .get(&owner.account)
                        .is_some_and(|history| history.matches(&owner))
                {
                    note
                } else {
                    None
                };
            }
            if !state.consume_for(&owner, ApiService::rate_limit_now_secs()) {
                return None;
            }
        }
        if !self.hint.publish().await {
            return None;
        }
        let state = self.limits.lock();
        if state.account_epoch != self.epoch
            || !state
                .accounts
                .get(&owner.account)
                .is_some_and(|history| history.matches(&owner))
        {
            return None;
        }
        *claimed = Some((self.epoch, owner));
        note
    }
}

#[derive(Clone, Copy)]
pub(super) struct RateLimitHeaderObservation {
    binding: u64,
    order: u128,
}

#[derive(Default)]
pub(super) struct RateLimitState {
    snapshot: Mutex<RateLimitSnapshot>,
}

#[derive(Default)]
struct RateLimitSnapshot {
    account_epoch: u64,
    binding: u64,
    owner: Option<RateLimitOwner>,
    latest_header_order: Option<u128>,
    info: Option<RateLimitInfo>,
    raw: Option<RawUtilization>,
    accounts: HashMap<RateLimitAccount, RateLimitAccountHistory>,
}

#[derive(Default)]
struct RateLimitAccountHistory {
    credential_revision: Option<u64>,
    unversioned_credential_identity: Option<String>,
    wrap_up_window: Option<u64>,
    pending_wrap_up: bool,
}

impl RateLimitAccountHistory {
    fn matches(&self, owner: &RateLimitOwner) -> bool {
        self.credential_revision == owner.credential_revision
            && self.unversioned_credential_identity == owner.unversioned_credential_identity
    }
}

impl RateLimitSnapshot {
    fn rebind(&mut self, owner: Option<RateLimitOwner>) {
        self.binding = self.binding.wrapping_add(1);
        self.owner = owner;
        self.latest_header_order = None;
        self.info = None;
        self.raw = None;
    }

    fn observe_headers(
        &mut self,
        binding: u64,
        order: Option<u128>,
    ) -> Option<RateLimitHeaderObservation> {
        if self.binding != binding {
            return None;
        }
        let order = order.unwrap_or_else(|| self.latest_header_order.map_or(0, |last| last + 1));
        if self.latest_header_order.is_some_and(|last| order < last) {
            return None;
        }
        self.latest_header_order = Some(order);
        Some(RateLimitHeaderObservation { binding, order })
    }

    fn is_current(&self, observation: RateLimitHeaderObservation) -> bool {
        self.binding == observation.binding && self.latest_header_order == Some(observation.order)
    }

    fn consume_for(&mut self, owner: &RateLimitOwner, now_secs: u64) -> bool {
        let Some(history) = self.accounts.get_mut(&owner.account) else {
            return false;
        };
        history.matches(owner)
            && std::mem::take(&mut history.pending_wrap_up)
            && history.wrap_up_window.is_some_and(|reset| reset > now_secs)
    }

    #[cfg(test)]
    fn consume_wrap_up_hint(&mut self, now_secs: u64) -> bool {
        self.owner
            .clone()
            .is_some_and(|owner| self.consume_for(&owner, now_secs))
    }
}

impl RateLimitState {
    fn lock(&self) -> std::sync::MutexGuard<'_, RateLimitSnapshot> {
        self.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn reset_for_account_change(&self) {
        let mut state = self.lock();
        state.account_epoch = state.account_epoch.wrapping_add(1);
        state.accounts.clear();
        state.rebind(None);
    }

    fn admit(&self, account_epoch: u64, owner: RateLimitOwner) -> Option<u64> {
        let mut state = self.lock();
        if state.account_epoch != account_epoch {
            return None;
        }
        let history = state.accounts.entry(owner.account.clone()).or_default();
        if let Some(revision) = owner.credential_revision {
            if history
                .credential_revision
                .is_some_and(|known| revision < known)
            {
                return None;
            }
            if history.credential_revision != Some(revision) && history.pending_wrap_up {
                // A rotated credential must supply its own near-limit observation.
                history.pending_wrap_up = false;
                history.wrap_up_window = None;
            }
            history.credential_revision = Some(revision);
        }
        if history.unversioned_credential_identity != owner.unversioned_credential_identity {
            history
                .unversioned_credential_identity
                .clone_from(&owner.unversioned_credential_identity);
            if history.pending_wrap_up || owner.account.scope.expected_identity_generation.is_none()
            {
                history.wrap_up_window = None;
            }
            history.pending_wrap_up = false;
        }
        if state.owner.as_ref() != Some(&owner) {
            state.rebind(Some(owner));
        }
        Some(state.binding)
    }
}

#[derive(Default)]
pub(super) struct PromptCacheOverageState {
    next_header_order: std::sync::atomic::AtomicU64,
    state: Mutex<PromptCacheOverageSnapshot>,
}

#[derive(Default)]
struct PromptCacheOverageSnapshot {
    account_epoch: u64,
    by_scope: std::collections::HashMap<crate::CredentialScope, PromptCacheOverageObservation>,
}

#[derive(Clone, Copy)]
struct PromptCacheOverageObservation {
    is_using_overage: bool,
    header_order: u64,
}

impl PromptCacheOverageState {
    /// Shared by streaming and non-streaming attempts, across route and account changes.
    pub(super) fn observe_headers(&self) -> u64 {
        self.next_header_order
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub(super) fn account_epoch(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .account_epoch
    }

    pub(super) fn reset_for_account_change(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.account_epoch = state.account_epoch.wrapping_add(1);
        state.by_scope.clear();
    }

    pub(super) fn is_using_overage(
        &self,
        scope: &crate::CredentialScope,
        account_epoch: u64,
    ) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.account_epoch != account_epoch {
            return false;
        }
        state
            .by_scope
            .get(scope)
            .is_some_and(|observation| observation.is_using_overage)
    }

    pub(super) fn record(
        &self,
        scope: &crate::CredentialScope,
        account_epoch: u64,
        is_using_overage: bool,
        header_order: u64,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.account_epoch != account_epoch
            || state
                .by_scope
                .get(scope)
                .is_some_and(|previous| header_order <= previous.header_order)
        {
            return false;
        }
        state.by_scope.insert(
            scope.clone(),
            PromptCacheOverageObservation {
                is_using_overage,
                header_order,
            },
        );
        true
    }
}

impl ApiService {
    /// Effective subscriber state: the live shared snapshot when provided and
    /// resolved, else the static build-time state. Since M13 the build-time
    /// state (and the seed in the shared slot) already carries the enterprise
    /// tier PERSISTED in the stored credential (claude-code keeps
    /// `subscriptionType` inside `claudeAiOauth`), so the static fallback is
    /// correct from request #1; the slot exists to FRESHEN it once the
    /// background profile fetch lands. Poisoned/empty slot → static fallback
    /// (conservative, pre-batch-5 behavior).
    ///
    /// Granularity: each drive fn hoists this ONCE before its retry loop, so
    /// `RetryState`'s 429/enterprise gate is stable across a request's retry
    /// attempts — the TS-faithful behaviour (`getSubscriptionType()` reads per
    /// attempt-ish but the gate effectively stabilizes per request).
    pub(super) fn effective_subscriber(&self) -> SubscriberState {
        let Some(slot) = &self.subscription else {
            return self.subscriber;
        };
        let Ok(guard) = slot.read() else {
            return self.subscriber;
        };
        let Some(snap) = guard.as_ref() else {
            return self.subscriber;
        };
        SubscriberState {
            is_subscriber: snap.is_subscriber,
            is_enterprise: snap.subscription_type.as_deref() == Some("enterprise"),
        }
    }

    /// Return the most recently observed 2xx rate-limit header snapshot, if any.
    ///
    /// Populated on every successful response from `drive_non_stream` and on the
    /// connect-success path of `drive_stream`.  `None` until the first successful
    /// response is received.
    ///
    /// TUI wiring note: no existing `OrchestratorHandle` surface maps naturally
    /// to per-request rate-limit metadata.  Callers that need this should hold an
    /// `Arc<ApiService>` and call this method directly.  A future task can
    /// wire it through the handle if needed.
    pub fn last_rate_limit_info(&self) -> Option<RateLimitInfo> {
        self.rate_limits.lock().info.clone()
    }

    /// The request id of the most recently recorded response. Backs the
    /// `OrchestratorApiClient::last_request_id` trait override (used to stamp the
    /// persisted assistant line's top-level `requestId`). Returns the server id
    /// when present, else the actual outgoing client-ID fallback (see
    /// [`Self::last_request_id_origin`]). `None` until the first recorded
    /// response (or when both are absent).
    #[must_use]
    pub fn last_request_id(&self) -> Option<String> {
        self.last_request_id
            .lock()
            .unwrap()
            .as_ref()
            .map(|(value, _origin)| value.clone())
    }

    /// Origin of the value returned by [`Self::last_request_id`] —
    /// [`RequestIdOrigin::Server`] when it came from a provider response header,
    /// [`RequestIdOrigin::Client`] when it is the actual outgoing client-ID fallback.
    /// `None` when no request id has been recorded.
    #[must_use]
    pub fn last_request_id_origin(&self) -> Option<RequestIdOrigin> {
        self.last_request_id
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_value, origin)| *origin)
    }

    /// Number of budget-consuming retry attempts the most recent drive performed
    /// before its terminal outcome. Backs the `last_retry_count` trait overrides
    /// (both `OrchestratorApiClient` and `StreamingApiClient`). `0` until the
    /// first drive.
    #[must_use]
    pub fn last_retry_count(&self) -> u32 {
        *self.last_retry_count.lock().unwrap()
    }

    /// The most recently observed RAW per-window utilization snapshot. Backs the
    /// `OrchestratorApiClient::last_raw_utilization` trait override. `None` until
    /// the first recorded response.
    #[must_use]
    pub fn last_raw_utilization(&self) -> Option<RawUtilization> {
        self.rate_limits.lock().raw
    }

    /// The user-facing copy composed from the most recent 429 **error**
    /// response. Backs the
    /// `OrchestratorApiClient::last_rate_limit_error_message` trait override (the
    /// orchestrator's terminal-429 re-map, claude-code `errors.ts:480-524`).
    /// `None` when neither unified Anthropic limits nor an OpenRouter free-model
    /// response supplied actionable context.
    #[must_use]
    pub fn last_rate_limit_error_message(&self) -> Option<String> {
        self.last_429_message.lock().unwrap().clone()
    }

    #[cfg(test)]
    pub(super) fn consume_pending_near_limit_wrap_up_hint(&self) -> bool {
        self.rate_limits
            .lock()
            .consume_wrap_up_hint(Self::rate_limit_now_secs())
    }

    /// Attach a child runner's one-shot notice to its own stream opening.
    pub async fn with_subagent_wrap_up<T>(
        &self,
        hint: Option<SubagentWrapUpHint>,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        let bound = hint.map(|hint| BoundWrapUpHint {
            hint,
            limits: self.rate_limits.clone(),
            epoch: self.rate_limit_account_epoch(),
        });
        SUBAGENT_WRAP_UP.scope(bound, future).await
    }

    /// Wall-clock milliseconds used to interpret provider quota windows.
    pub(super) fn now_ms() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    fn rate_limit_now_secs() -> u64 {
        u64::try_from(Self::now_ms() / 1000).unwrap_or(u64::MAX)
    }

    #[cfg(test)]
    pub(super) fn consume_test_wrap_up_hint_at(&self, now_secs: u64) -> bool {
        self.rate_limits.lock().consume_wrap_up_hint(now_secs)
    }

    pub(super) fn rate_limit_account_epoch(&self) -> u64 {
        self.rate_limits.lock().account_epoch
    }

    pub(super) fn admit_rate_limit_route(
        &self,
        account_epoch: u64,
        prepared: &crate::PreparedLlmCall,
    ) -> Option<u64> {
        self.rate_limits.admit(
            account_epoch,
            RateLimitOwner::from_capture(
                &prepared.credential_scope,
                prepared.request_auth,
                prepared.uses_session_subscription,
                &prepared.authenticator,
            ),
        )
    }

    pub(super) fn observe_rate_limit_headers(
        &self,
        binding: Option<u64>,
        status: u16,
    ) -> Option<RateLimitHeaderObservation> {
        if (200..300).contains(&status) || status == 429 {
            self.rate_limits.lock().observe_headers(binding?, None)
        } else {
            None
        }
    }

    pub(super) fn record_rate_limit_from_headers_for_route(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: Option<&str>,
        protocol: lingxi_llm_client::protocol::ProtocolFamily,
        provider_id: &str,
        observation: Option<RateLimitHeaderObservation>,
    ) -> Option<(String, RequestIdOrigin)> {
        let hvec: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let decoded =
            lingxi_llm_client::providers::response_headers::ProviderResponseHeaders::decode(
                protocol,
                provider_id,
                &hvec,
                std::time::SystemTime::now(),
            );
        let request_id = response_request_id(decoded.request_id, client_request_id);
        *self.last_request_id.lock().unwrap() = request_id.clone();
        *self.last_429_message.lock().unwrap() = None;
        let decoded = decoded.anthropic_rate_limits.unwrap_or_default();
        let raw = RawUtilization::from_decoded(&decoded);
        let info = RateLimitInfo::from_decoded_at(&decoded, std::time::SystemTime::now());
        if let Some(observation) = observation {
            let subscription_threshold = self.near_limit_wrap_up_threshold();
            let mut state = self.rate_limits.lock();
            if state.is_current(observation) {
                state.raw = Some(raw);
                let threshold = state
                    .owner
                    .as_ref()
                    .filter(|owner| owner.account.uses_session_subscription)
                    .map_or(NEAR_LIMIT_WRAP_UP_DEFAULT_THRESHOLD, |_| {
                        subscription_threshold
                    });
                state.update_near_limit_wrap_up_state(&info, raw, threshold);
                if info.has_unified_headers() {
                    match info.overage_status.as_deref() {
                        Some("rejected") => tracing::warn!(
                            overage_status = "rejected",
                            rate_limit_type = ?info.rate_limit_type,
                            "Rate limit: overage rejected — usage limit exhausted"
                        ),
                        Some("allowed_warning") => tracing::warn!(
                            overage_status = "allowed_warning",
                            rate_limit_type = ?info.rate_limit_type,
                            "Rate limit: overage warning — nearing usage limit"
                        ),
                        _ => {}
                    }
                    state.info = Some(info);
                }
            }
        }
        request_id
    }

    #[cfg(test)]
    pub(super) fn bind_test_rate_limit_route(&self, profile: &str, revision: Option<u64>) -> u64 {
        self.rate_limits
            .admit(
                self.rate_limit_account_epoch(),
                RateLimitOwner {
                    account: RateLimitAccount {
                        scope: crate::CredentialScope::new(
                            crate::ProviderId::AnthropicFirstParty,
                            profile,
                        ),
                        request_auth: crate::AuthStrategy::OAuthBearer,
                        uses_session_subscription: true,
                    },
                    credential_revision: revision,
                    unversioned_credential_identity: None,
                },
            )
            .expect("current test credential")
    }

    #[cfg(test)]
    pub(super) fn record_rate_limit_from_headers(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: &str,
    ) {
        let binding = self.bind_test_rate_limit_route("anthropic", None);
        self.record_rate_limit_from_headers_for_route(
            headers,
            (!client_request_id.is_empty()).then_some(client_request_id),
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
            self.observe_rate_limit_headers(Some(binding), 200),
        );
    }

    #[cfg(test)]
    pub(super) fn record_rate_limit_from_headers_at(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: &str,
        ts_ms: u128,
    ) {
        let binding = self.bind_test_rate_limit_route("anthropic", None);
        let observation = self
            .rate_limits
            .lock()
            .observe_headers(binding, Some(ts_ms));
        self.record_rate_limit_from_headers_for_route(
            headers,
            (!client_request_id.is_empty()).then_some(client_request_id),
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
            observation,
        );
    }

    pub(super) fn record_prompt_cache_overage_from_headers(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        prepared: Option<&crate::client::PreparedPromptCacheContext>,
        header_order: Option<u64>,
    ) {
        let Some((prepared, header_order)) = prepared
            .filter(|facts| facts.is_subscriber)
            .zip(header_order)
        else {
            return;
        };
        let headers = headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let decoded =
            lingxi_llm_client::providers::anthropic::limits::quota_status_from_headers(&headers);
        self.anthropic.prompt_cache_overage.record(
            &prepared.scope,
            prepared.account_epoch,
            decoded.is_using_overage,
            header_order,
        );
    }

    pub(super) fn stage_prompt_cache_overage_from_429(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        prepared: Option<&crate::client::PreparedPromptCacheContext>,
        header_order: Option<u64>,
    ) -> Option<crate::PendingPromptCacheObservation> {
        let prepared = prepared.filter(|facts| facts.is_subscriber)?;
        let header_order = header_order?;
        let headers = headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let decoded =
            lingxi_llm_client::providers::anthropic::limits::quota_status_from_429_headers(
                &headers,
            );
        Some(crate::PendingPromptCacheObservation {
            scope: prepared.scope.clone(),
            account_epoch: prepared.account_epoch,
            is_using_overage: decoded.is_using_overage,
            header_order,
        })
    }

    pub(super) fn record_prompt_cache_overage_from_quota_wait(
        &self,
        status_code: u16,
        headers: &std::collections::BTreeMap<String, String>,
        retry_watchdog_enabled: bool,
        prepared: Option<&crate::client::PreparedPromptCacheContext>,
        header_order: Option<u64>,
    ) {
        let Some((prepared, header_order)) = prepared
            .filter(|facts| facts.is_subscriber)
            .zip(header_order)
        else {
            return;
        };
        let headers = headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let Some(wait) =
            lingxi_llm_client::providers::anthropic::limits::retry_quota_window_wait_from_headers(
                status_code,
                &headers,
                retry_watchdog_enabled,
                std::time::SystemTime::now(),
            )
        else {
            return;
        };
        self.anthropic.prompt_cache_overage.record(
            &prepared.scope,
            prepared.account_epoch,
            wait.status.is_using_overage,
            header_order,
        );
    }

    /// Whether the live subscription snapshot is a Pro or Enterprise plan —
    /// the `getSubscriptionType() === 'pro' || 'enterprise'` predicate gating
    /// the `seven_day_sonnet` wording (claude-code
    /// `rateLimitMessages.ts:176-181`).
    ///
    /// Reads the live [`Self::subscription`] slot directly (the snapshot
    /// carries `subscription_type`; [`SubscriberState`] does not). With no
    /// resolved snapshot, falls back to the build-time enterprise bit —
    /// `pro` is unknowable pre-snapshot, matching an unresolved
    /// `getSubscriptionType()` evaluating to neither.
    fn is_pro_or_enterprise(&self) -> bool {
        if let Some(slot) = &self.subscription {
            if let Ok(guard) = slot.read() {
                if let Some(snap) = guard.as_ref() {
                    return matches!(
                        snap.subscription_type.as_deref(),
                        Some("pro" | "enterprise")
                    );
                }
            }
        }
        self.subscriber.is_enterprise
    }

    /// Record the unified rate-limit context from a 429 **error** response —
    /// the Rust seam for claude-code `errors.ts:471-524`, which extracts the
    /// unified headers from the error itself when a turn dies on a 429
    /// (success-path recording never sees them).
    ///
    /// Capture this attempt's rejected limits and raw windows. Only a terminal
    /// decision publishes the resulting snapshot.
    pub(super) fn record_rate_limit_from_429_for_route(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        body: Option<&lingxi_llm_client::protocol::JsonPayload>,
        model: &str,
        client_request_id: Option<&str>,
        protocol: lingxi_llm_client::protocol::ProtocolFamily,
        provider_id: &str,
        observation: Option<RateLimitHeaderObservation>,
    ) -> Observed429 {
        let hvec: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let decoded =
            lingxi_llm_client::providers::response_headers::ProviderResponseHeaders::decode(
                protocol,
                provider_id,
                &hvec,
                std::time::SystemTime::now(),
            );
        *self.last_request_id.lock().unwrap() =
            response_request_id(decoded.request_id, client_request_id);
        let decoded = decoded.anthropic_rate_limits.unwrap_or_default();
        let raw = RawUtilization::from_decoded(&decoded);
        let info = (protocol == lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages)
            .then(|| {
                lingxi_llm_client::providers::response_headers::AnthropicRateLimitHeaders::decode_429_error(
                    &hvec,
                    body,
                )
            })
            .flatten()
            .as_ref()
            .map(RateLimitInfo::from_decoded_429_error);

        // Keep the composed message on this attempt even if its global quota
        // snapshot is stale. Another call can update the service snapshot.
        let composed = info.as_ref().map(|info| {
            // `formatResetTime(…, true)` analogue for both reset headers
            // (`rateLimitMessages.ts:144-148`), formatted at error time.
            let formatted = formatted_reset_times_from_decoded(&decoded);
            rate_limit_error_message(
                info,
                &formatted.as_reset_times(),
                SubscriptionContext {
                    is_pro_or_enterprise: self.is_pro_or_enterprise(),
                },
            )
        });
        let message = composed.flatten().or_else(|| {
            is_free_tier_model(model).then(|| openrouter_free_rate_limit_message(body))
        });
        *self.last_429_message.lock().unwrap() = message.clone();

        let pending = observation.map(|observation| Pending429 {
            info,
            raw,
            observation,
        });
        Observed429 { pending, message }
    }

    #[cfg(test)]
    pub(super) fn record_rate_limit_from_429(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        body: Option<&lingxi_llm_client::protocol::JsonPayload>,
        model: &str,
    ) -> Option<Pending429> {
        let binding = self.bind_test_rate_limit_route("anthropic", None);
        self.record_rate_limit_from_429_for_route(
            headers,
            body,
            model,
            None,
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
            self.observe_rate_limit_headers(Some(binding), 429),
        )
        .pending
    }

    /// Publish this attempt's limits only if it is still the latest response.
    pub(super) fn promote_pending_429(
        &self,
        pending: Option<Pending429>,
        prompt_cache: Option<crate::PendingPromptCacheObservation>,
    ) {
        if let Some(pending) = pending {
            let mut state = self.rate_limits.lock();
            if state.is_current(pending.observation) {
                if let Some(info) = pending.info {
                    state.info = Some(info);
                }
                state.raw = Some(pending.raw);
            }
        }
        if let Some(pending) = prompt_cache {
            self.anthropic.prompt_cache_overage.record(
                &pending.scope,
                pending.account_epoch,
                pending.is_using_overage,
                pending.header_order,
            );
        }
    }

    fn near_limit_wrap_up_threshold(&self) -> f64 {
        if let Some(slot) = &self.subscription {
            if let Ok(guard) = slot.read() {
                if let Some(snapshot) = guard.as_ref() {
                    return match snapshot.rate_limit_tier.as_deref() {
                        Some("default_claude_max_5x") => NEAR_LIMIT_WRAP_UP_MAX5X_THRESHOLD,
                        Some("default_claude_max_20x") => NEAR_LIMIT_WRAP_UP_MAX20X_THRESHOLD,
                        _ => NEAR_LIMIT_WRAP_UP_DEFAULT_THRESHOLD,
                    };
                }
            }
        }
        // The static subscriber seed carries only subscriber/enterprise bits,
        // not `getRateLimitTier()`. The oracle's `OZt(mw())` therefore falls
        // through to the default threshold until the live tier snapshot lands.
        NEAR_LIMIT_WRAP_UP_DEFAULT_THRESHOLD
    }
}

impl RateLimitSnapshot {
    fn update_near_limit_wrap_up_state(
        &mut self,
        info: &RateLimitInfo,
        raw: RawUtilization,
        threshold: f64,
    ) {
        let now_secs = ApiService::rate_limit_now_secs();
        let Some(owner) = self.owner.as_ref() else {
            return;
        };
        let history = self
            .accounts
            .get_mut(&owner.account)
            .expect("admitted quota owner");
        if history.wrap_up_window.is_some_and(|reset| reset < now_secs) {
            history.pending_wrap_up = false;
            history.wrap_up_window = None;
        }

        let Some(window) = raw.five_hour else {
            return;
        };
        if !window.utilization.is_finite()
            || window.resets_at <= now_secs
            || window.utilization < threshold
        {
            return;
        }

        // Extra usage suppresses a pending hint while retaining the reset
        // key, so disabling overage in the same window cannot re-arm it.
        if matches!(
            info.overage_status.as_deref(),
            Some("allowed" | "allowed_warning")
        ) {
            history.pending_wrap_up = false;
            return;
        }

        if history.wrap_up_window != Some(window.resets_at) {
            history.wrap_up_window = Some(window.resets_at);
            history.pending_wrap_up = true;
        }
    }
}
