//! App-owned accounting hooks at the physical model transport boundary.
//!
//! This crate does not own prices or ledgers. A registered request requires an
//! installed host hook; a query-source string is never a substitute. Ordinary
//! requests without a capability keep their existing transport behavior.

use async_trait::async_trait;
use platform_api::ModelAttemptContext;

use crate::{LlmError, LlmRequest, PreparedLlmCall, Usage};

/// Whether normalized usage is a partial observation or an explicitly complete
/// provider report. Successful transport alone does not prove complete usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelAttemptUsageCompleteness {
    /// Preserve known counters while retaining conservative unknown occupancy.
    Partial,
    /// The provider supplied its final usage; schema success is irrelevant.
    Complete,
}

/// One application implementation shared by a service's registered Fusion
/// calls. Admission order is profile permit, queue capacity, atomic budget
/// authorization, durable intent, and live policy recheck before dispatch.
#[async_trait]
pub trait ModelAttemptHooks: Send + Sync {
    /// Prepare exactly one physical attempt after route, body and headers are
    /// final, but before invoking transport. The host must verify this context
    /// belongs to a live registered authority and this exact captured route.
    /// Neither request bodies nor credential headers may be logged by hooks.
    async fn begin(
        &self,
        context: &ModelAttemptContext,
        request: &LlmRequest,
        prepared: &PreparedLlmCall,
    ) -> Result<Box<dyn ModelAttemptLease>, LlmError>;
}

/// One accepted physical attempt. Implementations must retain a conservative
/// settlement when dropped: before dispatch it is proven-not-sent, afterwards
/// it is incomplete unless an actual complete observation has been retained.
/// The lease owns its profile permit and originating-session authority.
pub trait ModelAttemptLease: Send {
    /// Synchronous final live-policy/freeze check and dispatch marker. Invoke
    /// immediately before transport, with no intervening await. A marker is
    /// not proof that the remote service accepted or billed the request.
    fn mark_dispatched(&mut self) -> Result<(), LlmError>;

    /// Retain cumulative normalized facts before yielding them to consumers or
    /// parsing a Fusion/structured-output schema. This method must not await.
    /// Repeated cumulative snapshots replace earlier observations, not add to
    /// them. Content, raw source text and credentials are not part of this seam.
    fn observe_usage(&mut self, usage: &Usage, completeness: ModelAttemptUsageCompleteness);

    /// Record that no provider response was ever accepted for this attempt.
    /// The transport driver calls this when it finishes an attempt it marked
    /// dispatched without ever observing usage. Defaulted so a host that does
    /// not distinguish the case keeps its existing behavior.
    fn mark_no_provider_response(&mut self) {}

    /// Synchronously transfer the observation and permit into a host-owned
    /// finalizer, then return a waiter. Dropping that waiter cannot discard
    /// usage, cancel accepted persistence or release a reservation prematurely.
    fn finish(self: Box<Self>) -> Box<dyn ModelAttemptSettlement>;
}

/// Waiter for an already-owned attempt settlement. A failure must remain
/// visible to the originating session and must not authorize another attempt.
#[async_trait]
pub trait ModelAttemptSettlement: Send {
    /// Observe the retained durable result; the work already has an owner.
    async fn wait(self: Box<Self>) -> Result<(), LlmError>;
}

/// Transport-owned observation state. Dropping it leaves finalization to the
/// host lease's required Drop contract; explicit finish transfers synchronously.
pub(crate) struct WireAttempt {
    lease: Option<Box<dyn ModelAttemptLease>>,
    usage: Usage,
    /// Whether any provider usage was ever observed on this physical attempt.
    /// A dispatched attempt that finishes without one saw no response at all.
    observed: bool,
}

impl WireAttempt {
    pub(crate) fn new(lease: Option<Box<dyn ModelAttemptLease>>) -> Self {
        Self {
            lease,
            usage: Usage::default(),
            observed: false,
        }
    }

    pub(crate) fn mark_dispatched(&mut self) -> Result<(), LlmError> {
        match self.lease.as_mut() {
            Some(lease) => lease.mark_dispatched().map_err(accounting_error),
            None => Ok(()),
        }
    }

    pub(crate) fn observe(&mut self, usage: &Usage, completeness: ModelAttemptUsageCompleteness) {
        self.usage = usage.clone();
        self.observed = true;
        if let Some(lease) = self.lease.as_mut() {
            lease.observe_usage(&self.usage, completeness);
        }
    }

    pub(crate) fn observe_events(&mut self, events: &[crate::LlmEvent]) {
        if self.lease.is_none() {
            return;
        }
        for event in events {
            match event {
                crate::LlmEvent::MessageStart { response } => {
                    self.observe(&response.usage, ModelAttemptUsageCompleteness::Partial);
                }
                crate::LlmEvent::MessageDelta {
                    usage: Some(usage),
                    delta,
                } => {
                    let merged = merge_attempt_usage(&self.usage, usage);
                    self.observe(
                        &merged,
                        if delta.stop_reason.is_some()
                            && complete_usage_for(&merged, "input_tokens", "output_tokens")
                        {
                            ModelAttemptUsageCompleteness::Complete
                        } else {
                            ModelAttemptUsageCompleteness::Partial
                        },
                    );
                }
                crate::LlmEvent::Completed { response } => {
                    if has_usage_report(&response.usage) {
                        self.observe(&response.usage, ModelAttemptUsageCompleteness::Complete);
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) async fn finish(&mut self) -> Result<(), LlmError> {
        if let Some(mut lease) = self.lease.take() {
            // Every `finish` site in the driver is reached with the attempt
            // already marked dispatched, so "never observed" here means the
            // transport produced no provider response at all.
            if !self.observed {
                lease.mark_no_provider_response();
            }
            lease.finish().wait().await.map_err(accounting_error)?;
        }
        Ok(())
    }
}

/// Normalizers retain the actual provider usage object. Empty/default
/// responses are not a complete invoice simply because transport succeeded.
pub(crate) fn has_usage_report(usage: &Usage) -> bool {
    complete_usage_for(usage, "input_tokens", "output_tokens")
        || complete_usage_for(usage, "prompt_tokens", "completion_tokens")
        || complete_usage_for(usage, "promptTokenCount", "candidatesTokenCount")
}

/// Required provider input/output counters must both be explicitly numeric.
/// Normalizer default zero and a total-only report cannot establish Exact.
pub(crate) fn complete_usage_for(usage: &Usage, input: &str, output: &str) -> bool {
    let metadata = &usage.provider_metadata;
    let Some(input_count) = metadata.get(input).and_then(serde_json::Value::as_u64) else {
        return false;
    };
    let Some(output_count) = metadata.get(output).and_then(serde_json::Value::as_u64) else {
        return false;
    };
    for key in [
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "reasoning_output_tokens",
        "thoughtsTokenCount",
        "cached_tokens",
    ] {
        if metadata
            .get(key)
            .is_some_and(|value| value.as_u64().is_none())
        {
            return false;
        }
    }
    // Kimi's top-level cached_tokens is used only when the standard nested
    // count is absent. Match the normalizer's precedence, not both counts.
    if input == "prompt_tokens"
        && metadata
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(serde_json::Value::as_u64)
            .is_none()
        && metadata
            .get("cached_tokens")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|count| count > input_count)
    {
        return false;
    }
    for total_key in ["total_tokens", "totalTokenCount"] {
        if let Some(value) = metadata.get(total_key) {
            let Some(total) = value.as_u64() else {
                return false;
            };
            let Some(mut expected) = input_count.checked_add(output_count) else {
                return false;
            };
            // Gemini's visible and thought output counts are disjoint;
            // OpenAI includes reasoning in its raw completion/output count.
            if input == "promptTokenCount" {
                if let Some(thoughts) = metadata.get("thoughtsTokenCount") {
                    let Some(thoughts) = thoughts.as_u64() else {
                        return false;
                    };
                    let Some(sum) = expected.checked_add(thoughts) else {
                        return false;
                    };
                    expected = sum;
                }
            }
            if total != expected {
                return false;
            }
        }
    }
    // Independent bucket normalization uses subtraction for these subsets;
    // refuse to call a saturated or malformed subtraction complete.
    for (path, ceiling) in [
        ("/prompt_tokens_details/cached_tokens", input_count),
        ("/input_tokens_details/cached_tokens", input_count),
        ("/completion_tokens_details/reasoning_tokens", output_count),
        ("/output_tokens_details/reasoning_tokens", output_count),
        ("/cachedContentTokenCount", input_count),
    ] {
        if metadata
            .pointer(path)
            .is_some_and(|value| value.as_u64().is_none_or(|count| count > ceiling))
        {
            return false;
        }
    }
    if let Some(creation) = metadata.get("cache_creation") {
        if !creation.is_object() {
            return false;
        }
        let Some(total) = metadata
            .get("cache_creation_input_tokens")
            .and_then(serde_json::Value::as_u64)
        else {
            return false;
        };
        let mut split = 0_u64;
        for key in ["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"] {
            if let Some(value) = creation.get(key) {
                let Some(count) = value.as_u64() else {
                    return false;
                };
                let Some(sum) = split.checked_add(count) else {
                    return false;
                };
                split = sum;
            }
        }
        let has_five = creation.get("ephemeral_5m_input_tokens").is_some();
        let has_hour = creation.get("ephemeral_1h_input_tokens").is_some();
        // The host can derive 5m = total - 1h when only 1h is supplied.
        // A partial 5m-only split cannot identify the remaining tariff safely.
        if split > total || ((has_five || !has_hour) && split != total) {
            return false;
        }
    }
    true
}

pub(crate) fn response_usage_observation(
    usage: Usage,
    status: u16,
    input: &str,
    output: &str,
) -> (Usage, ModelAttemptUsageCompleteness) {
    let completeness = if (200..300).contains(&status) && complete_usage_for(&usage, input, output)
    {
        ModelAttemptUsageCompleteness::Complete
    } else {
        ModelAttemptUsageCompleteness::Partial
    };
    (usage, completeness)
}

/// Retain only the fixed Anthropic billing fields when terminal usage is an
/// output-only delta. No arbitrary provider metadata is accumulated.
fn merge_attempt_usage(seed: &Usage, delta: &Usage) -> Usage {
    let mut merged = crate::stream_accumulator::merge_usage(seed, delta);
    // On the registered Anthropic path, field presence distinguishes a true
    // cumulative zero from an omitted field. Preserve absent counters and
    // replace explicitly observed zeros as well as positive observations.
    if delta.provider_metadata.is_object() {
        macro_rules! known_counter {
            ($key:literal, $field:ident) => {
                merged.billable_tokens.$field = if delta
                    .provider_metadata
                    .get($key)
                    .and_then(serde_json::Value::as_u64)
                    .is_some()
                {
                    delta.billable_tokens.$field
                } else {
                    seed.billable_tokens.$field
                };
            };
        }
        known_counter!("input_tokens", input);
        known_counter!("output_tokens", output);
        known_counter!("cache_creation_input_tokens", cache_write);
        known_counter!("cache_read_input_tokens", cache_read);
        known_counter!("reasoning_output_tokens", reasoning_output);
    }
    if !merged.provider_metadata.is_object() {
        merged.provider_metadata = serde_json::json!({});
    }
    let metadata = merged
        .provider_metadata
        .as_object_mut()
        .expect("metadata normalized");
    for key in [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "reasoning_output_tokens",
    ] {
        if !metadata.contains_key(key) {
            if let Some(value) = seed
                .provider_metadata
                .get(key)
                .filter(|value| value.is_u64())
            {
                metadata.insert(key.into(), value.clone());
            }
        }
    }
    let mut creation = serde_json::Map::new();
    for key in ["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"] {
        if let Some(value) = delta
            .provider_metadata
            .get("cache_creation")
            .and_then(|value| value.get(key))
            .or_else(|| {
                seed.provider_metadata
                    .get("cache_creation")
                    .and_then(|value| value.get(key))
            })
        {
            creation.insert(key.into(), value.clone());
        }
    }
    if !creation.is_empty() {
        metadata.insert("cache_creation".into(), creation.into());
    }
    merged
}

pub(crate) fn missing_hooks_error() -> LlmError {
    LlmError::InvalidRequest {
        message: "registered model attempt requires host accounting hooks".into(),
    }
}

pub(crate) fn accounting_error(error: LlmError) -> LlmError {
    LlmError::CostUnavailable {
        message: format!("registered attempt accounting failed: {error}"),
    }
}
