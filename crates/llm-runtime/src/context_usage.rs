//! Host-measured context occupancy for one logical main request.
//!
//! The conversation owner supplies the last provider usage plus newly appended
//! content. Opaque replay data must not turn that measured prefix back into a
//! byte-based estimate. Requests without a matching anchor retain the ordinary
//! full-input fallback.

use lingxi_core::types::ConversationMessage;
use sha2::{Digest, Sha256};
use std::{future::Future, io::Write, sync::Arc};

type Estimate = dyn Fn(&str, Option<&str>, &[ConversationMessage]) -> Option<u64> + Send + Sync;

/// A conversation snapshot, isolated from concurrent and nested API calls.
#[derive(Clone)]
pub struct ContextUsageEstimator(Arc<Estimate>);

tokio::task_local! {
    static ESTIMATOR: ContextUsageEstimator;
}

impl ContextUsageEstimator {
    pub fn new(
        estimate: impl Fn(&str, Option<&str>, &[ConversationMessage]) -> Option<u64>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self(Arc::new(estimate))
    }

    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        ESTIMATOR.scope(self.clone(), future).await
    }

    pub fn estimate(
        &self,
        model: &str,
        profile: Option<&str>,
        history: &[ConversationMessage],
    ) -> Option<u64> {
        (self.0)(model, profile, history)
    }
}

pub(crate) fn estimate(
    model: &str,
    profile: Option<&str>,
    history: &[ConversationMessage],
) -> Option<u64> {
    ESTIMATOR
        .try_with(|estimator| estimator.estimate(model, profile, history))
        .ok()
        .flatten()
}

/// Host-only measured-prefix estimate bound to the input it was built for.
/// A changed route or rewritten retry must be counted independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    tokens: u64,
    input_fingerprint: [u8; 32],
}

impl ContextUsageEstimate {
    pub(crate) fn bind(request: &crate::LlmRequest, tokens: u64) -> Option<Self> {
        Some(Self {
            tokens,
            input_fingerprint: input_fingerprint(request)?,
        })
    }

    pub(crate) fn tokens_for(&self, request: &crate::LlmRequest) -> Option<u64> {
        (input_fingerprint(request)? == self.input_fingerprint).then_some(self.tokens)
    }
}

fn input_fingerprint(request: &crate::LlmRequest) -> Option<[u8; 32]> {
    // Hash without retaining another copy of potentially large replay blocks.
    struct HashWriter(Sha256);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let input = &request.input;
    let mut writer = HashWriter(Sha256::new());
    // Output ceilings/effort and cache markers may change during preparation;
    // they do not add prompt content. Bind the prompt, schemas, and exact-string
    // overrides so a payload rewrite cannot reuse stale usage.
    serde_json::to_writer(
        &mut writer,
        &(
            &input.model,
            &request.profile,
            &input.system,
            &input.messages,
            &input.tools,
            &input.hosted_tools,
            &input.native_options,
            &input.output_format,
            &input.tool_choice,
            &request.execution.message_json_string_overrides,
        ),
    )
    .ok()?;
    Some(writer.0.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn measured_context_scopes_are_nested_and_task_isolated() {
        assert_eq!(estimate("m", None, &[]), None);
        let first = ContextUsageEstimator::new(|_, _, _| Some(127_967));
        let second = ContextUsageEstimator::new(|_, _, _| Some(42));
        let (a, b) = tokio::join!(
            first.scope(async {
                tokio::task::yield_now().await;
                assert_eq!(estimate("m", None, &[]), Some(127_967));
                assert_eq!(
                    tokio::spawn(async { estimate("m", None, &[]) })
                        .await
                        .unwrap(),
                    None
                );
                second
                    .scope(async {
                        assert_eq!(estimate("m", None, &[]), Some(42));
                    })
                    .await;
                estimate("m", None, &[])
            }),
            second.scope(async {
                tokio::task::yield_now().await;
                estimate("m", None, &[])
            }),
        );
        assert_eq!((a, b), (Some(127_967), Some(42)));
        assert_eq!(estimate("m", None, &[]), None);
    }

    #[test]
    fn measured_context_expires_for_changed_input_but_survives_output_retry() {
        let request = crate::LlmRequest::new("m")
            .with_profile("p")
            .with_user_text("hello");
        let measured = ContextUsageEstimate::bind(&request, 100).unwrap();
        assert_eq!(measured.tokens_for(&request), Some(100));
        let mut output_retry = request.clone();
        output_retry.input.max_tokens = Some(4096);
        output_retry
            .set_effort(Some(serde_json::json!("high")))
            .unwrap();
        assert_eq!(measured.tokens_for(&output_retry), Some(100));
        let mut rerouted = request.clone();
        rerouted.profile = Some("other".into());
        assert_eq!(measured.tokens_for(&rerouted), None);
        rerouted = request.clone();
        rerouted.input.model = "other".into();
        assert_eq!(measured.tokens_for(&rerouted), None);
        let extended = request.clone().with_user_text("new tool result");
        assert_eq!(measured.tokens_for(&extended), None);
        let mut rewritten = request;
        rewritten
            .execution
            .message_json_string_overrides
            .insert("/messages/0/content/0/text".into(), vec![0xd800]);
        assert_eq!(measured.tokens_for(&rewritten), None);
    }
}
