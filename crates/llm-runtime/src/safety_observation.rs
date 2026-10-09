//! Session-attributed safety facts from canonical SDK observations.

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

    /// Native 2.1.293 content-filter observations require the exact provider
    /// phrase and either no original HTTP status or status 400.
    pub(crate) fn error(&self, error: &crate::LlmError) {
        let status_matches = error.http_status().is_none_or(|status| status == 400);
        let phrase = "Output blocked by content filtering policy";
        let matches_phrase = if let Some(provider_text) = error.provider_error_text() {
            contains_utf16(provider_text.utf16_code_units(), phrase)
        } else {
            contains_utf16(
                &error.to_string().encode_utf16().collect::<Vec<_>>(),
                phrase,
            )
        };
        if status_matches && matches_phrase {
            if let Some(observer) = &self.0 {
                observer.record(ModelSafetyStop::ContentFiltering);
            }
        }
    }
}

fn contains_utf16(haystack: &[u16], needle: &str) -> bool {
    let needle = needle.encode_utf16().collect::<Vec<_>>();
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
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
    fn native_refusal_events_can_count_twice_without_counting_sticky_hops() {
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
    fn content_filter_uses_exact_provider_text_and_rejects_other_statuses() {
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

        let mut units = "Output blocked by content filtering policy"
            .encode_utf16()
            .collect::<Vec<_>>();
        units.push(0xd800);
        let display = String::from_utf16_lossy(&units);
        observation.error(&crate::LlmError::ProviderErrorText {
            source: Box::new(crate::LlmError::InvalidRequest {
                message: "statusless provider error".into(),
            }),
            text: lingxi_llm_client::protocol::ProviderErrorText::new(display, units)
                .expect("lossy display matches the exact UTF-16 text"),
            native_anthropic_facts: None,
        });

        for message in [
            "403 Output blocked by content filtering policy",
            "Tool permission denied",
            "safety_monitor_blocked",
        ] {
            observation.error(&crate::LlmError::InvalidRequest {
                message: message.into(),
            });
        }
        assert_eq!(count.load(Ordering::Relaxed), 3);
    }
}
