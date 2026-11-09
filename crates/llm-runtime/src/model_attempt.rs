//! App-owned accounting hooks at the physical model transport boundary.
//!
//! This crate does not own prices or ledgers. A registered request requires an
//! installed host hook; a query-source string is never a substitute. Ordinary
//! requests without a capability keep their existing transport behavior.

use async_trait::async_trait;
use lingxi_core::host::ModelAttemptContext;

use crate::{ExecutionUsage, LlmError, LlmRequest, PreparedLlmCall};

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
    /// Return the registered origin before provider preparation. Model payload
    /// fields are never authority for this identity.
    fn request_session_id(
        &self,
        _context: &ModelAttemptContext,
    ) -> Option<lingxi_core::types::SessionId> {
        None
    }

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
    /// Optional originating-session observer for calls outside a query task.
    /// It observes response facts and never affects budget admission.
    fn model_safety_observer(
        &self,
    ) -> Option<lingxi_core::host::model_safety::ModelSafetyObserver> {
        None
    }

    /// Synchronous final live-policy/freeze check and dispatch marker. Invoke
    /// immediately before transport, with no intervening await. A marker is
    /// not proof that the remote service accepted or billed the request.
    fn mark_dispatched(&mut self) -> Result<(), LlmError>;

    /// Retain cumulative normalized facts before yielding them to consumers or
    /// parsing a Fusion/structured-output schema. This method must not await.
    /// Repeated cumulative snapshots replace earlier observations, not add to
    /// them. Content, raw source text and credentials are not part of this seam.
    fn observe_usage(
        &mut self,
        usage: &ExecutionUsage,
        completeness: ModelAttemptUsageCompleteness,
    );

    /// Record that no provider response was ever accepted for this attempt.
    /// This does not prove the provider did not execute the request. Hosts
    /// must retain unknown budget occupancy even when no response was received.
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
    usage: ExecutionUsage,
    usage_observer: Option<crate::RequestUsageObserver>,
    classifier: Option<ClassifierDiagnostic>,
    dispatched: bool,
}

/// Redacted, attempt-local facts only. Drop emits once even on cancellation;
/// cumulative usage observations replace one another rather than being added.
struct ClassifierDiagnostic {
    logical_id: String,
    stage: &'static str,
    attempt_id: String,
    parent_session: Option<String>,
    account_id: Option<String>,
    profile: String,
    provider: String,
    model: String,
    protocol: String,
    continuation: lingxi_llm_client::ConversationRequestSnapshot,
    cache_points: usize,
    cache_mode: String,
}

fn cache_points(body: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> usize {
    let mut pending = body.at_pointer("").into_iter().collect::<Vec<_>>();
    let mut count = 0;
    while let Some(value) = pending.pop() {
        if let Some(fields) = value.object_entries() {
            count += usize::from(fields.iter().any(|field| {
                field.display_key == "cache_control"
                    || field.display_key == "prompt_cache_breakpoint"
            }));
            pending.extend(fields.into_iter().map(|field| field.value));
        } else if let Some((_, items)) = value.array_entries() {
            pending.extend(items.into_iter().map(|(_, item)| item));
        }
    }
    count
}

impl WireAttempt {
    pub(crate) fn model_safety_observer(
        &self,
    ) -> Option<lingxi_core::host::model_safety::ModelSafetyObserver> {
        self.lease
            .as_ref()
            .and_then(|lease| lease.model_safety_observer())
    }

    pub(crate) fn new(lease: Option<Box<dyn ModelAttemptLease>>) -> Self {
        Self {
            lease,
            usage: ExecutionUsage::default(),
            usage_observer: None,
            classifier: None,
            dispatched: false,
        }
    }

    pub(crate) fn with_classifier(
        mut self,
        request: &LlmRequest,
        prepared: &PreparedLlmCall,
    ) -> Self {
        self.usage_observer = request.execution.request_usage_observer.clone();
        if let Some(layout) = &request.execution.classifier_cache_layout {
            // A normal serde string conversion rejects isolated UTF-16 units.
            // Inspect the flat source tree without materializing or logging text.
            let body = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                prepared.provider_request.body_json.as_json_source(),
            )
            .ok();
            self.classifier = Some(ClassifierDiagnostic {
                logical_id: layout.logical_id.clone(),
                stage: layout.stage,
                attempt_id: uuid::Uuid::new_v4().to_string(),
                parent_session: prepared.request_session_id.clone(),
                account_id: request
                    .execution
                    .selected_account
                    .as_ref()
                    .map(|scope| scope.account_id.clone()),
                profile: prepared.route.resolved_route.profile_name.clone(),
                provider: format!("{:?}", prepared.route.resolved_route.provider_id),
                model: prepared.route.resolved_route.request_model.clone(),
                protocol: format!("{:?}", prepared.route.protocol),
                continuation: prepared.continuation_snapshot.clone(),
                cache_points: body.as_ref().map_or(0, cache_points),
                cache_mode: body
                    .as_ref()
                    .and_then(|body| body.at_pointer("/prompt_cache_options/mode"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("provider_default")
                    .to_owned(),
            });
        }
        self
    }

    fn emit_classifier(&mut self, outcome: &'static str) {
        let Some(diag) = self.classifier.take() else {
            return;
        };
        if !self.dispatched {
            return;
        }
        let usage = self.usage.report.usage;
        tracing::info!(target: "llm_runtime::tool_auth_cache",
            logical_id = %diag.logical_id, stage = diag.stage, attempt_id = %diag.attempt_id,
            session_id = ?diag.parent_session, account_id = ?diag.account_id,
            profile = %diag.profile, provider = %diag.provider, model = %diag.model, protocol = %diag.protocol,
            continuation_mode = ?diag.continuation.mode,
            logical_bytes = diag.continuation.logical_bytes, wire_bytes = diag.continuation.wire_bytes,
            new_items = diag.continuation.new_items, cache_mode = %diag.cache_mode,
            cache_points = diag.cache_points, usage_state = ?self.usage.report.state,
            input_tokens = ?usage.map(|u| u.input_tokens),
            cache_read_tokens = ?usage.map(|u| u.cache_read_tokens),
            cache_write_tokens = ?usage.map(|u| u.cache_write_tokens), outcome,
            "tools authorization cache attempt");
    }

    pub(crate) fn mark_dispatched(&mut self) -> Result<(), LlmError> {
        if let Some(lease) = self.lease.as_mut() {
            lease.mark_dispatched().map_err(accounting_error)?;
        }
        self.dispatched = true;
        Ok(())
    }

    pub(crate) fn observe(
        &mut self,
        usage: &ExecutionUsage,
        completeness: ModelAttemptUsageCompleteness,
    ) {
        self.usage = usage.clone();
        if let Some(observer) = &self.usage_observer {
            observer.observe(usage);
        }
        if let Some(lease) = self.lease.as_mut() {
            lease.observe_usage(&self.usage, completeness);
        }
    }

    pub(crate) fn observe_events(&mut self, events: &[crate::HistoryEvent]) {
        if self.lease.is_none() && self.classifier.is_none() {
            return;
        }
        for event in events {
            match event {
                crate::HistoryEvent::MessageStart { response } => {
                    self.observe(&response.usage, ModelAttemptUsageCompleteness::Partial);
                }
                crate::HistoryEvent::MessageDelta {
                    usage: Some(usage), ..
                } => {
                    let merged = self.usage.merge_snapshot(usage);
                    self.observe(
                        &merged,
                        if merged.report.complete().is_some() {
                            ModelAttemptUsageCompleteness::Complete
                        } else {
                            ModelAttemptUsageCompleteness::Partial
                        },
                    );
                }
                crate::HistoryEvent::Completed { response } => {
                    if has_usage_report(&response.usage) {
                        self.observe(&response.usage, ModelAttemptUsageCompleteness::Complete);
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) async fn finish(&mut self) -> Result<(), LlmError> {
        self.emit_classifier("finished");
        if let Some(lease) = self.lease.take() {
            // Missing usage cannot establish whether a dispatched request ran.
            // Leave the lease unknown unless actual provider usage was observed.
            lease.finish().wait().await.map_err(accounting_error)?;
        }
        Ok(())
    }
}

/// A complete SDK report is the only authority for exact settlement.
/// Provider-specific field validation belongs to the SDK codec.
pub(crate) fn has_usage_report(usage: &ExecutionUsage) -> bool {
    usage.report.complete().is_some()
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

impl Drop for WireAttempt {
    fn drop(&mut self) {
        self.emit_classifier("dropped");
    }
}

#[cfg(test)]
mod classifier_diagnostic_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[test]
    fn unregistered_attempt_keeps_observed_usage_when_dropped_before_finish() {
        let snapshot = Arc::new(Mutex::new(None));
        let capture = snapshot.clone();
        let observer = crate::RequestUsageObserver::new(move |usage| {
            if let Some(counts) = usage.report.usage.filter(|_| {
                matches!(
                    usage.report.state,
                    crate::UsageState::Partial | crate::UsageState::Complete
                )
            }) {
                *capture.lock().unwrap() = Some(counts);
            }
        });
        let mut attempt = WireAttempt::new(None);
        attempt.usage_observer = Some(observer);
        attempt.mark_dispatched().unwrap();
        attempt.observe(
            &ExecutionUsage::default(),
            ModelAttemptUsageCompleteness::Partial,
        );
        assert!(snapshot.lock().unwrap().is_none());
        let mut invalid = ExecutionUsage::from_counts(crate::Usage {
            input_tokens: 999,
            ..Default::default()
        });
        invalid.report.state = crate::UsageState::Invalid;
        attempt.observe(&invalid, ModelAttemptUsageCompleteness::Partial);
        assert!(
            snapshot.lock().unwrap().is_none(),
            "invalid usage is not invented as zero"
        );
        for tokens in [42, 84] {
            attempt.observe(
                &ExecutionUsage::from_counts(crate::Usage {
                    input_tokens: tokens,
                    output_tokens: 7,
                    ..Default::default()
                }),
                ModelAttemptUsageCompleteness::Complete,
            );
        }
        // No registered accounting lease is present. Host facts must already
        // be visible before any finish/collected-finish/telemetry future runs.
        drop(attempt);
        let counts = snapshot.lock().unwrap().unwrap();
        assert_eq!((counts.input_tokens, counts.output_tokens), (84, 7));
    }
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<BTreeMap<String, String>>>>);
    struct Fields(BTreeMap<String, String>);
    impl tracing::field::Visit for Fields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().into(), format!("{value:?}"));
        }
    }
    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().target() == "llm_runtime::tool_auth_cache" {
                let mut fields = Fields(BTreeMap::new());
                event.record(&mut fields);
                self.0.lock().unwrap().push(fields.0);
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    fn attempt() -> WireAttempt {
        let mut attempt = WireAttempt::new(None);
        attempt.classifier = Some(ClassifierDiagnostic {
            logical_id: "judgment".into(),
            stage: "fast",
            attempt_id: "physical-1".into(),
            parent_session: Some("parent".into()),
            account_id: Some("account".into()),
            profile: "fixture".into(),
            provider: "OpenAI".into(),
            model: "gpt-6.1-sol".into(),
            protocol: "OpenAiResponses".into(),
            continuation: Default::default(),
            cache_points: 3,
            cache_mode: "implicit".into(),
        });
        attempt
    }
    #[tokio::test]
    async fn one_event_per_dispatched_attempt_uses_latest_cumulative_usage() {
        let capture = Capture::default();
        let _guard = tracing::subscriber::set_default(capture.clone());
        let mut call = attempt();
        call.mark_dispatched().unwrap();
        for tokens in [10, 80] {
            call.observe(
                &ExecutionUsage::from_counts(lingxi_llm_client::protocol::Usage {
                    input_tokens: 5,
                    cache_read_tokens: tokens,
                    ..Default::default()
                }),
                ModelAttemptUsageCompleteness::Complete,
            );
        }
        call.finish().await.unwrap();
        drop(call);
        let events = capture.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["cache_read_tokens"], "Some(80)");
        assert_eq!(events[0]["usage_state"], "Complete");
        assert!(events[0]
            .keys()
            .all(|key| !matches!(key.as_str(), "prompt" | "body" | "headers" | "credential")));
    }
    #[test]
    fn cancelled_attempt_retains_unknown_usage_and_unsent_attempt_is_not_counted() {
        let capture = Capture::default();
        let _guard = tracing::subscriber::set_default(capture.clone());
        drop(attempt());
        let mut call = attempt();
        call.mark_dispatched().unwrap();
        drop(call);
        let events = capture.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["usage_state"], "Missing");
        assert_eq!(events[0]["cache_read_tokens"], "None");
        assert_eq!(events[0]["outcome"], "\"dropped\"");
    }
    #[test]
    fn diagnostics_count_cache_markers_with_isolated_utf16_without_reading_text() {
        let body = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
            r#"{"input":[{"content":[{"text":"\ud800","prompt_cache_breakpoint":true}]}]}"#,
        )
        .unwrap();
        assert_eq!(cache_points(&body), 1);
    }
}
