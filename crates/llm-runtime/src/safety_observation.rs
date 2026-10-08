//! Session-attributed safety facts from canonical SDK observations. There is
//! no protocol parser or permission-denial inference here.

use lingxi_core::host::model_safety::{ModelSafetyObserver, ModelSafetyStop};
use lingxi_llm_client::{
    protocol::StopReason,
    providers::anthropic::fallback_response::{FallbackControl, FallbackHop},
};

#[derive(Clone)]
pub(crate) struct SafetyObservation(Option<ModelSafetyObserver>);

impl SafetyObservation {
    pub(crate) fn inherit_if_missing(&mut self, origin: Option<ModelSafetyObserver>) {
        if self.0.is_none() {
            self.0 = origin;
        }
    }
    pub(crate) fn capture(request: &mut crate::LlmRequest) -> Self {
        if request.execution.model_safety_observer.is_none() {
            request.execution.model_safety_observer =
                lingxi_core::host::model_safety::current_model_safety_observer();
        }
        Self(request.execution.model_safety_observer.clone())
    }

    fn refusal(&self) {
        if let Some(observer) = &self.0 {
            observer.record(ModelSafetyStop::Refusal);
        }
    }

    fn hop(&self, hop: &FallbackHop) {
        if hop.reason == "refusal" {
            self.refusal();
        }
    }

    fn end(&self, stop_reason: &StopReason) {
        if *stop_reason == StopReason::Refusal {
            self.refusal();
        }
    }

    pub(crate) fn response(&self, response: &lingxi_llm_client::protocol::ChatResponse) {
        if let Some(fallback) = response.anthropic_fallback() {
            for hop in &fallback.hops {
                self.hop(hop);
            }
        }
        self.end(&response.stop_reason);
    }

    pub(crate) fn batch(&self, batch: &lingxi_llm_client::StreamBatch) {
        for event in &batch.events {
            match event {
                Ok(lingxi_llm_client::protocol::StreamEvent::End { stop_reason, .. }) => {
                    self.end(stop_reason)
                }
                Ok(lingxi_llm_client::protocol::StreamEvent::NativeControl {
                    protocol: lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
                    control,
                }) => {
                    if let Ok(FallbackControl::Start { start }) =
                        control.decode::<FallbackControl>()
                    {
                        self.hop(&start.hop);
                    }
                }
                _ => {}
            }
        }
    }

    /// Native 2.1.293 uC: the exact content-filter message with no original
    /// HTTP status, or status 400. The public build's m3e classifier is false.
    pub(crate) fn error(&self, error: &crate::LlmError) {
        if error.http_status().is_none_or(|status| status == 400)
            && error
                .to_string()
                .contains("Output blocked by content filtering policy")
        {
            if let Some(observer) = &self.0 {
                observer.record(ModelSafetyStop::ContentFiltering);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    #[tokio::test]
    async fn a_request_retains_origin_attribution_after_its_task_scope_ends() {
        let count = Arc::new(AtomicU64::new(0));
        let value = count.clone();
        let observer = ModelSafetyObserver::new(move |_| {
            value.fetch_add(1, Ordering::Relaxed);
        });
        let observation = lingxi_core::host::model_safety::scope_model_safety(observer, async {
            let mut request = crate::LlmRequest::default();
            SafetyObservation::capture(&mut request)
        })
        .await;
        assert!(lingxi_core::host::model_safety::current_model_safety_observer().is_none());
        observation.batch(&lingxi_llm_client::StreamBatch {
            events: vec![Ok(lingxi_llm_client::protocol::StreamEvent::End {
                stop_reason: StopReason::Refusal,
                usage: Default::default(),
                inference: Default::default(),
            })],
            usage: Default::default(),
            inference: Default::default(),
            finished: true,
        });
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn native_refusal_events_can_count_twice_in_one_call_without_counting_sticky_hops() {
        let count = Arc::new(AtomicU64::new(0));
        let value = count.clone();
        let observation = SafetyObservation(Some(ModelSafetyObserver::new(move |_| {
            value.fetch_add(1, Ordering::Relaxed);
        })));
        let mut hop = FallbackHop {
            from_model: "original".into(),
            model: "fallback".into(),
            reason: "refusal".into(),
            category: None,
        };
        observation.hop(&hop);
        observation.end(&StopReason::Refusal);
        assert_eq!(count.load(Ordering::Relaxed), 2);
        hop.reason = "sticky".into();
        observation.hop(&hop);
        observation.end(&StopReason::EndTurn);
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn content_filter_status_predicate_does_not_count_other_denials_or_private_monitor_labels() {
        let count = Arc::new(AtomicU64::new(0));
        let value = count.clone();
        let observation = SafetyObservation(Some(ModelSafetyObserver::new(move |_| {
            value.fetch_add(1, Ordering::Relaxed);
        })));
        for message in [
            "Output blocked by content filtering policy",
            "400 Output blocked by content filtering policy",
        ] {
            observation.error(&crate::LlmError::InvalidRequest {
                message: message.into(),
            });
        }
        for message in [
            "403 Output blocked by content filtering policy",
            "Tool permission denied",
            "safety_monitor_blocked",
        ] {
            observation.error(&crate::LlmError::InvalidRequest {
                message: message.into(),
            });
        }
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }
}
