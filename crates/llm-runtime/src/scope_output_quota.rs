//! A host-owned output ceiling shared by every service in one invocation.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::execution_context::ApiAttemptReservation;
use crate::{ApiAttemptsLeft, LlmError, PreparedLlmCall, ProtocolFamily};

/// Limits generation requests to `dispatches * max_output_tokens_per_request`.
///
/// This is an output-token ceiling, not an input-token or monetary budget. All
/// retries and auxiliary services must share the same instance. A dispatched
/// request is never refunded, even when its result or usage is unknown. The
/// provider must honor its wire limit; unsupported routes fail before dispatch.
/// This capability is intentionally not serializable or workspace configurable.
#[derive(Debug)]
pub struct ScopeOutputQuota {
    max_output_tokens_per_request: NonZeroU32,
    attempts: ApiAttemptsLeft,
    revoked: AtomicBool,
}

impl ScopeOutputQuota {
    #[must_use]
    pub fn new(max_output_tokens_per_request: NonZeroU32, dispatches: NonZeroU32) -> Self {
        Self {
            max_output_tokens_per_request,
            attempts: ApiAttemptsLeft::new(dispatches.get()),
            revoked: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn max_output_tokens_per_request(&self) -> u32 {
        self.max_output_tokens_per_request.get()
    }

    #[must_use]
    pub fn remaining_dispatches(&self) -> u32 {
        self.attempts.remaining()
    }

    /// Permanently deny future generation dispatches. The host must separately
    /// stop already admitted provider calls and tool processes.
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    pub(crate) fn clamp(&self, requested: Option<u32>) -> u32 {
        requested
            .unwrap_or(self.max_output_tokens_per_request.get())
            .min(self.max_output_tokens_per_request.get())
    }

    // Admission can be checked repeatedly without charging. Reserve only at
    // the SDK's physical dispatch marker and commit once the host admits it.
    pub(crate) fn reserve(&self) -> Option<ApiAttemptReservation> {
        self.attempts.reserve()
    }

    /// Check the final, sealed request as well as the SDK's resolved budget.
    /// Deferred credential/body policies may remove a previously resolved cap.
    /// Never edit a sealed (possibly signed) request to try to repair that.
    pub(crate) fn validate_prepared(&self, prepared: &PreparedLlmCall) -> Result<(), LlmError> {
        let unsupported = || LlmError::UnsupportedCapability {
            capability: "scope requires an enforceable provider output-token limit".into(),
        };
        let call = prepared.wire_call.as_ref().ok_or_else(unsupported)?;
        if !call
            .output_budget()
            .and_then(|budget| budget.value)
            .is_some_and(|value| value > 0 && value <= self.max_output_tokens_per_request.get())
        {
            return Err(unsupported());
        }
        let body: lingxi_llm_client::protocol::JsonPayload =
            serde_json::from_slice(&call.request().body).map_err(|_| unsupported())?;
        for pointer in [
            "/n",
            "/best_of",
            "/generationConfig/candidateCount",
            "/generation_config/candidate_count",
        ] {
            if let Some(raw) = body.raw_source_at(pointer) {
                if serde_json::from_str::<u64>(&raw).ok() != Some(1) {
                    return Err(unsupported());
                }
            }
        }
        let pointers: &[&str] = match prepared.route.protocol {
            ProtocolFamily::AnthropicMessages
            | ProtocolFamily::VertexClaude
            | ProtocolFamily::BedrockClaude
            | ProtocolFamily::FoundryClaude => &["/max_tokens"],
            ProtocolFamily::OpenAiChat | ProtocolFamily::AzureOpenAi => {
                &["/max_tokens", "/max_completion_tokens"]
            }
            ProtocolFamily::OpenAiResponses => &["/max_output_tokens"],
            ProtocolFamily::GeminiGenerateContent | ProtocolFamily::VertexGemini => {
                &["/generationConfig/maxOutputTokens"]
            }
            ProtocolFamily::GeminiInteractions => &["/generation_config/max_output_tokens"],
        };
        let mut found = false;
        for pointer in pointers {
            if let Some(raw) = body.raw_source_at(pointer) {
                let value = serde_json::from_str::<u64>(&raw).map_err(|_| unsupported())?;
                if value == 0 || value > u64::from(self.max_output_tokens_per_request.get()) {
                    return Err(unsupported());
                }
                found = true;
            }
        }
        if !found {
            return Err(unsupported());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn quota(dispatches: u32) -> ScopeOutputQuota {
        ScopeOutputQuota::new(
            NonZeroU32::new(1024).unwrap(),
            NonZeroU32::new(dispatches).unwrap(),
        )
    }

    #[test]
    fn explicit_values_cannot_raise_the_ceiling() {
        let quota = quota(2);
        assert_eq!(quota.clamp(None), 1024);
        assert_eq!(quota.clamp(Some(u32::MAX)), 1024);
        assert_eq!(quota.clamp(Some(32)), 32);
        assert_eq!(quota.clamp(Some(0)), 0); // Remains invalid for the SDK.
    }

    #[test]
    fn admission_checks_do_not_spend_and_uncommitted_reservations_roll_back() {
        let quota = quota(1);
        for _ in 0..10 {
            assert!(!quota.is_revoked());
        }
        let reservation = quota.reserve().unwrap();
        assert!(quota.reserve().is_none());
        drop(reservation);
        assert_eq!(quota.remaining_dispatches(), 1);
        quota.reserve().unwrap().commit();
        assert_eq!(quota.remaining_dispatches(), 0);
    }

    #[test]
    fn concurrent_services_share_one_finite_counter() {
        let quota = Arc::new(quota(7));
        let threads = (0..32)
            .map(|_| {
                let quota = quota.clone();
                std::thread::spawn(move || quota.reserve().map(|r| r.commit()).is_some())
            })
            .collect::<Vec<_>>();
        let admitted = threads
            .into_iter()
            .map(|t| u32::from(t.join().unwrap()))
            .sum::<u32>();
        assert_eq!(admitted, 7);
        assert_eq!(quota.remaining_dispatches(), 0);
        quota.revoke();
        quota.revoke();
        assert!(quota.is_revoked());
    }
}
