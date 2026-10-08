//! Retry ownership for one model call, including stream reopens and the
//! non-streaming fallback. A new API drive must not replenish this budget.
//!
//! Claude Code 2.1.286 `VLt` shares `retriesLeft()` across `PNt` / `LMo`.
//! `Rse` permits one first non-streaming attempt after ordinary exhaustion;
//! credential renewal has its own two-attempt allowance. This is not a hard
//! physical-request limit: deduplicated request repairs and watchdog capacity
//! waits remain separate in the oracle.

use std::sync::{Arc, Mutex};

use super::retry::{DriveStep, RetryControl, RetryState};

#[derive(Debug, Default)]
struct State {
    display_probe: lingxi_llm_client::providers::anthropic::thinking_display::DisplayProbe,
    max_retries: Option<u32>,
    retries: u32,
    overloaded: u32,
    tried_without_streaming: bool,
    credential_renewals: u8,
    no_response_retries: u8,
    stalls: u32,
    truncations: u32,
    after_thinking_only: u32,
    persistent: bool,
    max_overloaded: Option<u32>,
    model_fallback_available: bool,
    model_fallback_reason: Option<&'static str>,
}

/// One logical response's retry ledger. Clone to carry ownership across a
/// stream reopen or transport-mode fallback; create a fresh ledger for the
/// next tool/model turn or an independent side query.
#[derive(Clone, Debug, Default)]
pub struct ModelCallRetryScope(Arc<Mutex<State>>);

fn http_model_fallback_reason(
    status: u16,
    overload_payload: bool,
    available: bool,
    persistent: bool,
    overloaded: u32,
    max_overloaded: u32,
) -> Option<&'static str> {
    if !available {
        return None;
    }
    if !persistent && (500..600).contains(&status) && status != 529 {
        Some("server_error")
    } else if status == 529 || overload_payload {
        (overloaded.saturating_add(1) >= max_overloaded).then_some("overloaded")
    } else {
        None
    }
}

pub(crate) fn http_retry_decline_is_terminal(
    status: u16,
    declined: bool,
    overload_payload: bool,
    persistent: bool,
) -> bool {
    // Native eWo accepts overload payloads and persistent capacity failures
    // before inspecting x-should-retry. The model-switch gate is earlier still.
    declined && !overload_payload && !(persistent && matches!(status, 429 | 529))
}

impl ModelCallRetryScope {
    pub(crate) fn display_probe(
        &self,
    ) -> lingxi_llm_client::providers::anthropic::thinking_display::DisplayProbe {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .display_probe
            .clone()
    }

    pub(crate) fn display_probe_error(
        &self,
        status: u16,
        admission: lingxi_llm_client::providers::anthropic::thinking_display::ProbeAdmission,
        budget: &lingxi_llm_client::providers::anthropic::thinking_display::DisplayProbeBudget,
    ) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .display_probe
            .on_error(status, admission, budget)
    }

    pub(crate) fn display_probe_succeeded(
        &self,
        conversation: &lingxi_llm_client::providers::anthropic::beta_repair::ConversationBetaState,
    ) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .display_probe
            .on_success(conversation)
    }

    /// The main query owns its next model. This authority never enters input.
    #[must_use]
    pub fn with_model_fallback(self) -> Self {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .model_fallback_available = true;
        self
    }

    /// Current native HTTP server/overload gates, before ordinary retry spending.
    pub(crate) fn request_http_model_fallback(&self, status: u16, overload_payload: bool) -> bool {
        let persistent = super::retry::retry_watchdog_from_env();
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        // Ordinary retry handling records unsuccessful overloads. Peek at the
        // next count here so the threshold can switch before spending a retry.
        let reason = http_model_fallback_reason(
            status,
            overload_payload,
            state.model_fallback_available,
            persistent,
            state.overloaded,
            state
                .max_overloaded
                .unwrap_or(u32::from(super::retry::MAX_529_RETRIES)),
        );
        state.model_fallback_reason = reason;
        reason.is_some()
    }

    /// Consume the query-owned directive after its API drive has settled.
    pub fn take_model_fallback_request(&self) -> Option<&'static str> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .model_fallback_reason
            .take()
    }

    /// Run only the model API future in this scope. Do not scope tool execution
    /// or auxiliary queries, which own independent retry budgets.
    pub async fn run<F: std::future::Future>(&self, future: F) -> F::Output {
        MODEL_CALL_RETRIES.scope(self.clone(), future).await
    }

    pub(crate) fn current_or_new() -> Self {
        MODEL_CALL_RETRIES
            .try_with(Clone::clone)
            .unwrap_or_default()
    }

    pub(crate) fn configure(&self, control: &RetryControl, state: &mut RetryState) -> RetryControl {
        let mut shared = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let max = *shared.max_retries.get_or_insert(control.max_retries);
        shared.persistent = control.watchdog;
        shared.max_overloaded = Some(u32::from(control.max_529_retries));
        shared.overloaded = shared
            .overloaded
            .max(u32::from(state.consecutive_overloaded));
        state.consecutive_overloaded = u8::try_from(shared.overloaded).unwrap_or(u8::MAX);
        // Ese starts its local $t/B ladders afresh; VLt supplies only the
        // remaining ordinary budget and accumulated overload count.
        state.attempt = 0;
        state.watchdog_capacity_waits = 0;
        let mut control = control.clone();
        control.max_retries = max.saturating_sub(shared.retries);
        control
    }

    pub(crate) fn next_step(
        &self,
        state: &mut RetryState,
        control: &RetryControl,
        error: &crate::LlmError,
        thinking_budget: u32,
        backoff_ms: Option<u64>,
    ) -> DriveStep {
        let mut shared = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let mut control = control.clone();
        let max = *shared.max_retries.get_or_insert(control.max_retries);
        control.max_retries = state
            .attempt
            .saturating_add(max.saturating_sub(shared.retries));
        state.consecutive_overloaded = u8::try_from(shared.overloaded).unwrap_or(u8::MAX);
        let before = state.attempt;
        let step = super::retry::next_step_with_backoff(
            state,
            &control,
            error,
            thinking_budget,
            backoff_ms,
        );
        shared.retries = shared
            .retries
            .saturating_add(state.attempt.saturating_sub(before));
        if matches!(error, crate::LlmError::Overloaded { .. }) {
            shared.overloaded = shared.overloaded.saturating_add(1);
        }
        step
    }

    /// A body-phase overload belongs to the same ledger as HTTP 529s, even
    /// though the caller, rather than the HTTP driver, decides its fallback.
    pub fn record_stream_overload(&self) {
        let mut shared = self.0.lock().unwrap_or_else(|error| error.into_inner());
        shared.overloaded = shared.overloaded.saturating_add(1);
    }

    pub(crate) fn reset_model_overloads(&self) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .overloaded = 0;
    }

    pub(crate) fn retry_count(&self) -> u32 {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retries
    }

    /// Apply the current native body-phase decision to the same ledger that
    /// the HTTP driver configured. Reopening a stream never resets these facts.
    pub fn decide_stream_failure(
        &self,
        failure: super::stream_recovery::Failure,
        has_fallback_model: bool,
        background: bool,
        allow_non_streaming: bool,
    ) -> super::stream_recovery::Outcome {
        use super::stream_recovery::{Counts, Policy};
        let mut shared = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let policy = Policy {
            max_retries: shared.max_retries.unwrap_or_else(|| {
                super::retry::resolve_max_retries(
                    super::retry::retry_watchdog_from_env(),
                    std::env::var("LINGXI_MAX_RETRIES").ok().as_deref(),
                )
            }),
            max_overloaded: shared
                .max_overloaded
                .unwrap_or(u32::from(super::retry::MAX_529_RETRIES)),
            has_fallback_model,
            persistent: shared.persistent
                || (shared.max_retries.is_none() && super::retry::retry_watchdog_from_env()),
            background,
        };
        let counts = Counts {
            retries: shared.retries,
            overloaded: shared.overloaded,
            stalls: shared.stalls,
            truncations: shared.truncations,
            after_thinking_only: shared.after_thinking_only,
            tried_without_streaming: shared.tried_without_streaming,
        };
        let outcome = super::stream_recovery::decide(failure, counts, policy, allow_non_streaming);
        shared.retries = outcome.counts.retries;
        shared.overloaded = outcome.counts.overloaded;
        shared.stalls = outcome.counts.stalls;
        shared.truncations = outcome.counts.truncations;
        shared.after_thinking_only = outcome.counts.after_thinking_only;
        shared.tried_without_streaming = outcome.counts.tried_without_streaming;
        outcome
    }

    /// Spend an ordinary retry for a failed stream body. The service's live
    /// settings establish the limit before yielding the first stream.
    pub fn take_stream_retry(&self) -> bool {
        let mut shared = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let max = shared
            .max_retries
            .unwrap_or_else(super::retry::max_retries_from_env);
        if shared.retries >= max {
            return false;
        }
        shared.retries += 1;
        true
    }

    /// Oracle `Rse`: the first switch to non-streaming is allowed even when
    /// ordinary retries are exhausted; subsequent switches need a retry left.
    pub fn take_non_streaming_fallback(&self) -> bool {
        let mut shared = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let max = shared
            .max_retries
            .unwrap_or_else(super::retry::max_retries_from_env);
        if shared.tried_without_streaming && shared.retries >= max {
            return false;
        }
        shared.tried_without_streaming = true;
        shared.retries = shared.retries.saturating_add(1).min(max);
        true
    }

    pub(crate) fn take_credential_renewal(&self) -> bool {
        let mut shared = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if shared.credential_renewals >= 2 {
            return false;
        }
        shared.credential_renewals += 1;
        true
    }

    pub(crate) fn take_no_response_retry(&self) -> bool {
        let mut shared = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if shared.no_response_retries >= 1 {
            return false;
        }
        shared.no_response_retries += 1;
        true
    }
}

tokio::task_local! {
    static MODEL_CALL_RETRIES: ModelCallRetryScope;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_fallback_matches_current_native_gates() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/http_fallback_2_1_288.json"
        ))
        .unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 5600);
        for case in cases {
            let reason = http_model_fallback_reason(
                case["status"].as_u64().unwrap() as u16,
                case["overload_payload"].as_bool().unwrap(),
                case["available"].as_bool().unwrap(),
                case["persistent"].as_bool().unwrap(),
                case["overloaded"].as_u64().unwrap() as u32,
                3,
            );
            assert_eq!(reason, case["expected"].as_str(), "{case}");
        }
    }

    #[test]
    fn fallback_directive_is_consumed_once_and_does_not_spend_the_retry_budget() {
        let scope = ModelCallRetryScope::default().with_model_fallback();
        scope.record_stream_overload();
        scope.record_stream_overload();
        assert!(scope.request_http_model_fallback(529, false));
        assert_eq!(scope.take_model_fallback_request(), Some("overloaded"));
        assert_eq!(scope.take_model_fallback_request(), None);
        assert_eq!(scope.retry_count(), 0);
        assert!(!ModelCallRetryScope::default().request_http_model_fallback(529, false));
    }

    #[test]
    fn overload_and_persistent_capacity_precede_retry_declines() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/http_fallback_2_1_288.json"
        ))
        .unwrap();
        let cases = fixture["declineCases"].as_array().unwrap();
        assert_eq!(cases.len(), 32);
        for case in cases {
            assert_eq!(
                http_retry_decline_is_terminal(
                    case["status"].as_u64().unwrap() as u16,
                    case["declined"].as_bool().unwrap(),
                    case["overload_payload"].as_bool().unwrap(),
                    case["persistent"].as_bool().unwrap(),
                ),
                case["expected"].as_bool().unwrap(),
                "{case}"
            );
        }
    }
    #[test]
    fn current_stream_decisions_use_and_update_the_shared_ledger() {
        use super::super::stream_recovery::{Counts, Failure, Policy};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/stream_recovery_2_1_288.json"
        ))
        .unwrap();
        for row in fixture["cases"].as_array().unwrap() {
            let counts: Counts = serde_json::from_value(row["counts"].clone()).unwrap();
            let policy: Policy = serde_json::from_value(row["policy"].clone()).unwrap();
            let scope = ModelCallRetryScope(Arc::new(Mutex::new(State {
                max_retries: Some(policy.max_retries),
                max_overloaded: Some(policy.max_overloaded),
                persistent: policy.persistent,
                retries: counts.retries,
                overloaded: counts.overloaded,
                stalls: counts.stalls,
                truncations: counts.truncations,
                after_thinking_only: counts.after_thinking_only,
                tried_without_streaming: counts.tried_without_streaming,
                ..Default::default()
            })));
            let failure: Failure = serde_json::from_value(row["failure"].clone()).unwrap();
            let result = scope.decide_stream_failure(
                failure,
                policy.has_fallback_model,
                policy.background,
                row["allowNonStreaming"].as_bool().unwrap(),
            );
            assert_eq!(
                serde_json::to_value(result).unwrap(),
                row["expected"],
                "{row}"
            );
            let shared = scope.0.lock().unwrap();
            assert_eq!(
                (
                    shared.retries,
                    shared.overloaded,
                    shared.stalls,
                    shared.truncations,
                    shared.after_thinking_only,
                    shared.tried_without_streaming
                ),
                (
                    result.counts.retries,
                    result.counts.overloaded,
                    result.counts.stalls,
                    result.counts.truncations,
                    result.counts.after_thinking_only,
                    result.counts.tried_without_streaming
                )
            );
        }
    }

    #[test]
    fn http_and_thinking_reopens_spend_the_same_budget() {
        use super::super::stream_recovery::{Cause, Decision, Failure, Progress};
        let scope = ModelCallRetryScope::default();
        let ctl = RetryControl {
            max_retries: 2,
            ..Default::default()
        };
        let mut state = RetryState::default();
        let ctl = scope.configure(&ctl, &mut state);
        assert!(matches!(
            scope.next_step(
                &mut state,
                &ctl,
                &crate::LlmError::ProviderInternal,
                0,
                Some(1)
            ),
            DriveStep::RetryAfter(_)
        ));
        let failure = Failure {
            cause: Cause::ServerError,
            progress: Progress::ThinkingOnly,
            stop_reason_received: false,
            outlasted_non_streaming_timeout: false,
        };
        assert_eq!(
            scope
                .decide_stream_failure(failure, false, false, false)
                .decision,
            Decision::Retry
        );
        let mut reopened = RetryState::default();
        let ctl = scope.configure(&ctl, &mut reopened);
        assert_eq!(ctl.max_retries, 0);
        assert_eq!(
            scope
                .decide_stream_failure(failure, false, false, false)
                .decision,
            Decision::Fail
        );
        assert!(matches!(
            scope.next_step(
                &mut reopened,
                &ctl,
                &crate::LlmError::ProviderInternal,
                0,
                Some(1)
            ),
            DriveStep::Terminal
        ));
    }

    #[test]
    fn mixed_branches_share_remaining_retries_and_first_fallback_allowance() {
        let scope = ModelCallRetryScope::default();
        let control = RetryControl {
            max_retries: 3,
            ..RetryControl::default()
        };
        let mut connect = RetryState::default();
        scope.configure(&control, &mut connect);
        assert!(matches!(
            scope.next_step(
                &mut connect,
                &control,
                &crate::LlmError::ProviderInternal,
                0,
                Some(1)
            ),
            DriveStep::RetryAfter(_)
        ));
        assert!(scope.take_stream_retry());
        let mut reopened = RetryState::default();
        scope.configure(&control, &mut reopened);
        assert_eq!(reopened.attempt, 0);
        assert_eq!(scope.retry_count(), 2);
        assert!(matches!(
            scope.next_step(
                &mut reopened,
                &control,
                &crate::LlmError::ProviderInternal,
                0,
                Some(1)
            ),
            DriveStep::RetryAfter(_)
        ));
        assert!(!scope.take_stream_retry());
        assert!(scope.take_non_streaming_fallback());
        let mut fallback = RetryState::default();
        scope.configure(&control, &mut fallback);
        assert!(matches!(
            scope.next_step(
                &mut fallback,
                &control,
                &crate::LlmError::ProviderInternal,
                0,
                Some(1)
            ),
            DriveStep::Terminal
        ));
        assert!(!scope.take_non_streaming_fallback());
        assert!(scope.take_credential_renewal());
        assert!(scope.take_credential_renewal());
        assert!(!scope.take_credential_renewal());
    }

    #[test]
    fn watchdog_capacity_waits_leave_the_stream_retry_budget_available() {
        for error in [
            crate::LlmError::Overloaded { repeated: false },
            crate::LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
        ] {
            let scope = ModelCallRetryScope::default();
            let control = RetryControl {
                max_retries: 1,
                watchdog: true,
                ..RetryControl::default()
            };
            let mut state = RetryState::default();
            scope.configure(&control, &mut state);
            for (wait, base_ms) in [(1, 10), (2, 20), (3, 40)] {
                let step = scope.next_step(&mut state, &control, &error, 0, Some(10));
                let DriveStep::RetryAfter(delay) = step else {
                    panic!("capacity must keep waiting");
                };
                assert!(delay.as_millis() >= base_ms && delay.as_millis() * 4 < base_ms * 5);
                assert_eq!(state.watchdog_capacity_waits, wait);
                assert_eq!(
                    state.attempt, 0,
                    "capacity waits do not spend ordinary retries"
                );
            }
            // A lost stream body still has its one ordinary retry, even after
            // the capacity ladder has gone beyond that ordinary limit.
            assert!(scope.take_stream_retry());
            assert!(!scope.take_stream_retry());
            let mut reopened = RetryState::default();
            scope.configure(&control, &mut reopened);
            assert_eq!(reopened.attempt, 0);
            assert_eq!(scope.retry_count(), 1);
            assert_eq!(
                reopened.watchdog_capacity_waits, 0,
                "each HTTP drive owns its wait ladder"
            );
            assert_eq!(
                scope.next_step(
                    &mut reopened,
                    &control,
                    &crate::LlmError::ProviderInternal,
                    0,
                    Some(10)
                ),
                DriveStep::Terminal
            );
        }
    }

    #[test]
    fn stream_overload_seeds_fallback_with_the_whole_call_count() {
        let scope = ModelCallRetryScope::default();
        let control = RetryControl {
            max_retries: 10,
            allow_fallback: true,
            fallback_model: Some("fallback-model".into()),
            ..RetryControl::default()
        };
        let mut first = RetryState::default();
        let first_control = scope.configure(&control, &mut first);
        assert!(matches!(
            scope.next_step(
                &mut first,
                &first_control,
                &crate::LlmError::Overloaded { repeated: false },
                0,
                Some(1)
            ),
            DriveStep::RetryAfter(_)
        ));
        // VLt's overload ledger is cumulative, even when another error occurs
        // before the body-phase overload which switches transport modes.
        assert!(matches!(
            scope.next_step(
                &mut first,
                &first_control,
                &crate::LlmError::ProviderInternal,
                0,
                Some(1)
            ),
            DriveStep::RetryAfter(_)
        ));
        scope.record_stream_overload();
        assert!(scope.take_non_streaming_fallback());
        let mut fallback = RetryState {
            consecutive_overloaded: 1,
            ..RetryState::default()
        };
        let fallback_control = scope.configure(&control, &mut fallback);
        assert_eq!(fallback.consecutive_overloaded, 2);
        assert_eq!(fallback_control.max_retries, 7);
        assert_eq!(
            scope.next_step(
                &mut fallback,
                &fallback_control,
                &crate::LlmError::Overloaded { repeated: false },
                0,
                Some(1)
            ),
            DriveStep::Fallback {
                fallback_model: "fallback-model".into()
            }
        );
        // A configured different model owns a fresh overload threshold while
        // the ordinary whole-call retry budget remains spent.
        scope.reset_model_overloads();
        let mut next_model = RetryState::default();
        let next_control = scope.configure(&control, &mut next_model);
        assert_eq!(next_model.consecutive_overloaded, 0);
        assert_eq!(next_control.max_retries, 7);
    }

    #[test]
    fn reopened_drive_starts_its_backoff_with_only_the_remaining_budget() {
        let scope = ModelCallRetryScope::default();
        let control = RetryControl {
            max_retries: 6,
            ..RetryControl::default()
        };
        let mut first = RetryState::default();
        let first_control = scope.configure(&control, &mut first);
        for _ in 0..2 {
            assert!(matches!(
                scope.next_step(
                    &mut first,
                    &first_control,
                    &crate::LlmError::ProviderInternal,
                    0,
                    Some(10)
                ),
                DriveStep::RetryAfter(_)
            ));
        }
        assert!(scope.take_stream_retry());
        let mut reopened = RetryState::default();
        let reopened_control = scope.configure(&control, &mut reopened);
        assert_eq!(reopened_control.max_retries, 3);
        let DriveStep::RetryAfter(delay) = scope.next_step(
            &mut reopened,
            &reopened_control,
            &crate::LlmError::ProviderInternal,
            0,
            Some(10),
        ) else {
            panic!("remaining retries must be usable");
        };
        assert!(
            delay.as_millis() >= 10 && delay.as_millis() * 4 < 50,
            "the new drive starts with the first backoff rung"
        );
        assert_eq!(reopened.attempt, 1);
        assert_eq!(scope.retry_count(), 4);
    }

    #[tokio::test]
    async fn independent_scopes_do_not_share_renewal_or_no_response_allowance() {
        let first = ModelCallRetryScope::default();
        first
            .run(async {
                assert!(ModelCallRetryScope::current_or_new().take_no_response_retry());
                assert!(!ModelCallRetryScope::current_or_new().take_no_response_retry());
                let independent = ModelCallRetryScope::default();
                independent
                    .run(async {
                        assert!(ModelCallRetryScope::current_or_new().take_no_response_retry());
                    })
                    .await;
                assert!(!ModelCallRetryScope::current_or_new().take_no_response_retry());
            })
            .await;
    }
}
