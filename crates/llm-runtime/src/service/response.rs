//! Request correlation and quota observations exposed to the host.

use crate::model::rate_limit::{
    formatted_reset_times_from_decoded, rate_limit_error_message, RateLimitInfo, RawUtilization,
    SubscriptionContext,
};
use crate::service::retry::{is_free_tier_model, openrouter_free_rate_limit_message};
use crate::service::ApiService;
use std::sync::Mutex;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestIdOrigin {
    /// The provider returned its own id in a known response header
    /// (authoritative — valid for provider-side log/support lookups).
    Server,
    /// No server id header was present, so the outgoing `x-client-request-id`
    /// we sent is used as a fallback. Correlation-only: a provider will NOT find
    /// this id in its logs.
    Client,
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
    observed_at_ms: u128,
}

#[derive(Default)]
pub(super) struct PromptCacheOverageState {
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
    observed_at_ms: u128,
}

impl PromptCacheOverageState {
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
        observed_at_ms: u128,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.account_epoch != account_epoch
            || state
                .by_scope
                .get(scope)
                .is_some_and(|previous| observed_at_ms < previous.observed_at_ms)
        {
            return false;
        }
        state.by_scope.insert(
            scope.clone(),
            PromptCacheOverageObservation {
                is_using_overage,
                observed_at_ms,
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
        self.last_rate_limit.lock().unwrap().clone()
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
        *self.last_raw_utilization.lock().unwrap()
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

    /// Consume the pending near-limit wrap-up hint once.
    ///
    /// The dedupe window key intentionally survives the consume: once a
    /// subagent has seen the hint for a five-hour window, later responses in
    /// that same window must not re-arm it. A later reset starts a new window.
    #[must_use]
    pub fn consume_pending_near_limit_wrap_up_hint(&self) -> bool {
        let mut pending = self.pending_near_limit_wrap_up_hint.lock().unwrap();
        if !*pending {
            return false;
        }
        *pending = false;
        true
    }

    /// Parse rate-limit headers from a 2xx response and update the cached snapshot.
    ///
    /// Emits a `tracing::warn!` when the overage status indicates the account is
    /// at or near exhaustion (`overage_status == "rejected"` or `"allowed_warning"`).
    /// Wall-clock milliseconds since the Unix epoch — the record timestamp for
    /// the rate-limit monotonic guard (binary `Date.now()`).
    pub(super) fn now_ms() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    /// Monotonic rate-limit record guard — ports the binary's `Bha`/`Nha`
    /// (@210953352): returns `true` (STALE ⇒ caller skips the snapshot update)
    /// when `ts_ms` is OLDER than the last recorded timestamp; otherwise
    /// records `ts_ms` and returns `false`. Prevents an out-of-order (older)
    /// parallel response from overwriting a newer rate-limit snapshot — the
    /// 2.1.196 flicker fix. Under normal monotonic wall-clock operation this
    /// never drops, so production behaviour is unchanged.
    fn rate_limit_record_stale(&self, ts_ms: u128) -> bool {
        let mut guard = self.last_rate_limit_record_ts_ms.lock().unwrap();
        match *guard {
            Some(prev) if ts_ms < prev => true,
            _ => {
                *guard = Some(ts_ms);
                false
            }
        }
    }

    pub(super) fn record_rate_limit_from_headers_for_route(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: Option<&str>,
        protocol: lingxi_llm_client::protocol::ProtocolFamily,
        provider_id: &str,
    ) {
        self.record_rate_limit_from_headers_at_for_route(
            headers,
            client_request_id,
            protocol,
            provider_id,
            Self::now_ms(),
        );
    }

    fn record_rate_limit_from_headers_at_for_route(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: Option<&str>,
        protocol: lingxi_llm_client::protocol::ProtocolFamily,
        provider_id: &str,
        ts_ms: u128,
    ) {
        let hvec: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Capture the request id on every recorded response — the SDK's
        // `response._request_id`, which claude-code persists as the assistant
        // line's top-level `requestId`. Provider-aware: tries each provider's
        // canonical request-id header for the selected route. If absent, use
        // the actual SDK/caller `x-client-request-id`; without one, keep the
        // Host correlation field absent.
        *self.last_request_id.lock().unwrap() =
            match crate::execution::extract_response_request_id(protocol, provider_id, headers) {
                Some(server_id) => Some((server_id, RequestIdOrigin::Server)),
                None if client_request_id.is_some_and(|id| !id.is_empty()) => {
                    let client_request_id = client_request_id.unwrap_or_default();
                    tracing::debug!(
                        client_request_id,
                        "no provider request-id header on response; \
                         falling back to outgoing client request-id for correlation only"
                    );
                    Some((client_request_id.to_string(), RequestIdOrigin::Client))
                }
                None => None,
            };
        // Monotonic guard (binary `Bha`/`Nha`): a response whose record
        // timestamp is OLDER than the last recorded one is STALE — skip the
        // rate-limit snapshot updates so an out-of-order parallel response can
        // never flip the warning off (2.1.196 flicker fix). request-id capture
        // above and the pending-429 bookkeeping below stay unconditional.
        let stale = self.rate_limit_record_stale(ts_ms);
        // Task 2 (llm-runtime future-work batch 5): track the raw per-window
        // snapshot on EVERY recorded (non-stale) headers pass — `rawUtilization
        // = extractRawUtilization(headersToUse)` (claudeAiLimits.ts:476), NOT
        // gated on `has_unified_headers()` like the limits snapshot below.
        let decoded =
            lingxi_llm_client::providers::response_headers::ProviderResponseHeaders::decode(
                protocol,
                provider_id,
                &hvec,
                std::time::SystemTime::now(),
            )
            .anthropic_rate_limits
            .unwrap_or_default();
        let raw = RawUtilization::from_decoded(&decoded);
        if !stale {
            *self.last_raw_utilization.lock().unwrap() = Some(raw);
        }
        let info = RateLimitInfo::from_decoded_at(&decoded, std::time::SystemTime::now());
        if !stale {
            self.update_near_limit_wrap_up_state(&info, raw, ts_ms);
        }
        if !stale && info.has_unified_headers() {
            // Warn when the account is near or at exhaustion.
            match info.overage_status.as_deref() {
                Some("rejected") => {
                    tracing::warn!(
                        overage_status = "rejected",
                        rate_limit_type = ?info.rate_limit_type,
                        "Rate limit: overage rejected — usage limit exhausted"
                    );
                }
                Some("allowed_warning") => {
                    tracing::warn!(
                        overage_status = "allowed_warning",
                        rate_limit_type = ?info.rate_limit_type,
                        "Rate limit: overage warning — nearing usage limit"
                    );
                }
                _ => {}
            }
            *self.last_rate_limit.lock().unwrap() = Some(info);
        }
        // Task 6 (batch 5): a successful response supersedes any cached 429
        // limits copy — the slot always reflects the most recent response.
        *self.last_429_message.lock().unwrap() = None;
        // B6-T1: a success also discards any staged-but-unpromoted 429 from an
        // earlier retried attempt (TS resets module state to the success's
        // `status`, never leaving a stale `rejected` behind).
    }

    #[cfg(test)]
    pub(super) fn record_rate_limit_from_headers(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: &str,
    ) {
        self.record_rate_limit_from_headers_for_route(
            headers,
            (!client_request_id.is_empty()).then_some(client_request_id),
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
        );
    }

    #[cfg(test)]
    pub(super) fn record_rate_limit_from_headers_at(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        client_request_id: &str,
        ts_ms: u128,
    ) {
        self.record_rate_limit_from_headers_at_for_route(
            headers,
            (!client_request_id.is_empty()).then_some(client_request_id),
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
            ts_ms,
        );
    }

    pub(super) fn record_prompt_cache_overage_from_headers(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        prepared: Option<&crate::client::PreparedPromptCacheContext>,
    ) {
        let Some(prepared) = prepared.filter(|facts| facts.is_subscriber) else {
            return;
        };
        let headers = headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let decoded =
            lingxi_llm_client::providers::anthropic::limits::quota_status_from_headers(&headers);
        if self.anthropic.prompt_cache_overage.record(
            &prepared.scope,
            prepared.account_epoch,
            decoded.is_using_overage,
            Self::now_ms(),
        ) {
            *prepared
                .pending_overage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    pub(super) fn stage_prompt_cache_overage_from_429(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        prepared: Option<&crate::client::PreparedPromptCacheContext>,
    ) {
        let Some(prepared) = prepared.filter(|facts| facts.is_subscriber) else {
            return;
        };
        let headers = headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let decoded =
            lingxi_llm_client::providers::anthropic::limits::quota_status_from_429_headers(
                &headers,
            );
        *prepared
            .pending_overage
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(crate::PendingPromptCacheObservation {
                scope: prepared.scope.clone(),
                account_epoch: prepared.account_epoch,
                is_using_overage: decoded.is_using_overage,
                observed_at_ms: Self::now_ms(),
            });
    }

    pub(super) fn record_prompt_cache_overage_from_quota_wait(
        &self,
        status_code: u16,
        headers: &std::collections::BTreeMap<String, String>,
        retry_watchdog_enabled: bool,
        prepared: Option<&crate::client::PreparedPromptCacheContext>,
    ) {
        let Some(prepared) = prepared.filter(|facts| facts.is_subscriber) else {
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
        if self.anthropic.prompt_cache_overage.record(
            &prepared.scope,
            prepared.account_epoch,
            wait.status.is_using_overage,
            Self::now_ms(),
        ) {
            *prepared
                .pending_overage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
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
    ) -> Option<Pending429> {
        self.record_rate_limit_from_429_at_for_route(
            headers,
            body,
            model,
            client_request_id,
            protocol,
            provider_id,
            Self::now_ms(),
        )
    }

    pub(super) fn record_rate_limit_from_429_at_for_route(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        body: Option<&lingxi_llm_client::protocol::JsonPayload>,
        model: &str,
        client_request_id: Option<&str>,
        protocol: lingxi_llm_client::protocol::ProtocolFamily,
        provider_id: &str,
        ts_ms: u128,
    ) -> Option<Pending429> {
        let hvec: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        *self.last_request_id.lock().unwrap() =
            match crate::execution::extract_response_request_id(protocol, provider_id, headers) {
                Some(server_id) => Some((server_id, RequestIdOrigin::Server)),
                None if client_request_id.is_some_and(|id| !id.is_empty()) => {
                    let client_request_id = client_request_id.unwrap_or_default();
                    Some((client_request_id.to_string(), RequestIdOrigin::Client))
                }
                None => None,
            };
        // `extractRawUtilization(headersToUse)` (claudeAiLimits.ts:500) runs
        // for ANY error headers, independent of the limits gate below.
        let decoded =
            lingxi_llm_client::providers::response_headers::ProviderResponseHeaders::decode(
                protocol,
                provider_id,
                &hvec,
                std::time::SystemTime::now(),
            )
            .anthropic_rate_limits
            .unwrap_or_default();
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

        // MESSAGE composition ports errors.ts:482-516 (a LOCAL limits object
        // built from the error headers) — kept verbatim; it is NOT staged and
        // is set per-attempt because the terminal error re-map reads it
        // directly (the most-recent-429 slot, cleared on success).
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
        *self.last_429_message.lock().unwrap() = composed.flatten().or_else(|| {
            is_free_tier_model(model).then(|| openrouter_free_rate_limit_message(body))
        });

        // Monotonic guard (binary `Bha`/`Nha`): a stale (out-of-order) 429 must
        // not stage a snapshot that could later PROMOTE over a newer response's
        // state. The composed message above is per-attempt (most-recent-429)
        // and stays unconditional; only the promotable staged slot is gated.
        if self.rate_limit_record_stale(ts_ms) {
            return None;
        }
        Some(Pending429 {
            info,
            raw,
            observed_at_ms: ts_ms,
        })
    }

    #[cfg(test)]
    pub(super) fn record_rate_limit_from_429(
        &self,
        headers: &std::collections::BTreeMap<String, String>,
        body: Option<&lingxi_llm_client::protocol::JsonPayload>,
        model: &str,
    ) -> Option<Pending429> {
        self.record_rate_limit_from_429_for_route(
            headers,
            body,
            model,
            None,
            lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
            "anthropic",
        )
    }

    /// Publish this attempt's limits only if it is still the latest response.
    pub(super) fn promote_pending_429(
        &self,
        pending: Option<Pending429>,
        prompt_cache: Option<&crate::client::PreparedPromptCacheContext>,
    ) {
        if let Some(pending) =
            pending.filter(|pending| !self.rate_limit_record_stale(pending.observed_at_ms))
        {
            if let Some(info) = pending.info {
                *self.last_rate_limit.lock().unwrap() = Some(info);
            }
            *self.last_raw_utilization.lock().unwrap() = Some(pending.raw);
        }
        let Some(prepared) = prompt_cache.filter(|facts| facts.is_subscriber) else {
            return;
        };
        let pending = prepared
            .pending_overage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(pending) = pending {
            if pending.scope == prepared.scope && pending.account_epoch == prepared.account_epoch {
                self.anthropic.prompt_cache_overage.record(
                    &pending.scope,
                    pending.account_epoch,
                    pending.is_using_overage,
                    pending.observed_at_ms,
                );
            }
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

    fn update_near_limit_wrap_up_state(
        &self,
        info: &RateLimitInfo,
        raw: RawUtilization,
        _observation_ts_ms: u128,
    ) {
        // Oracle 2.1.252 `extractQuotaStatusFromHeaders` uses `Date.now()` for
        // expiry/future checks, independently of the observation timestamp
        // used by the stale-response guard. `five_hour` names the quota bucket;
        // it is not a "reset within five hours" duration predicate.
        let now_secs = u64::try_from(Self::now_ms() / 1000).unwrap_or(u64::MAX);
        let mut pending = self.pending_near_limit_wrap_up_hint.lock().unwrap();
        let mut window_key = self.near_limit_wrap_up_window_key.lock().unwrap();
        if window_key.is_some_and(|reset| reset < now_secs) {
            *pending = false;
            *window_key = None;
        }

        let Some(window) = raw.five_hour else {
            return;
        };
        if !window.utilization.is_finite()
            || window.resets_at <= now_secs
            || window.utilization < self.near_limit_wrap_up_threshold()
        {
            return;
        }

        // Oracle also requires `!aM()` here, where `aM()` is its separate
        // low-priority/slow-mode controller. LingXi does not implement that
        // subsystem, so every representable runtime state is the inactive
        // branch. If slow mode is added, it must suppress arming here and make
        // `consume_pending_near_limit_wrap_up_hint` clear-then-return-false.

        // Extra usage suppresses an armed hint but deliberately preserves the
        // reset key. If overage later turns off in the same window, the hint
        // must not re-arm (`nearLimitWrapUpWindowKey !== resets_at`).
        if matches!(
            info.overage_status.as_deref(),
            Some("allowed" | "allowed_warning")
        ) {
            *pending = false;
            return;
        }

        if *window_key != Some(window.resets_at) {
            *window_key = Some(window.resets_at);
            *pending = true;
        }
    }
}
