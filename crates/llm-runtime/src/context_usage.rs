//! Host-measured context occupancy for one logical main request.
//!
//! The conversation owner supplies the last provider usage plus newly appended
//! content. Opaque replay data must not turn that measured prefix back into a
//! byte-based estimate. Requests without a matching anchor retain the ordinary
//! full-input fallback.

use lingxi_core::types::ConversationMessage;
use sha2::{Digest, Sha256};
use std::{future::Future, io::Write, sync::Arc};

type Estimate = dyn Fn(&str, Option<&str>, &[ConversationMessage]) -> Option<ContextUsageMeasurement>
    + Send
    + Sync;

/// Durable identity of the content covered by a provider measurement. Cache
/// annotations and output limits are excluded; exact system/schema/source text
/// and the consumed history prefix remain part of the identity.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextUsagePrefix {
    schema: u8,
    prompt: String,
    history_len: usize,
    history: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryPrefix {
    len: usize,
    hash: String,
}

impl HistoryPrefix {
    pub(crate) fn new(history: &[ConversationMessage]) -> Option<Self> {
        Some(Self {
            len: history.len(),
            hash: digest(&history)?,
        })
    }
}

impl ContextUsagePrefix {
    /// Capture the original logical prompt and consumed source history. The
    /// identity grants no request authority and never includes credentials.
    pub fn capture(request: &crate::LlmRequest, history: &[ConversationMessage]) -> Option<Self> {
        Self::new(request, HistoryPrefix::new(history)?)
    }

    pub(crate) fn new(request: &crate::LlmRequest, history: HistoryPrefix) -> Option<Self> {
        Some(Self {
            schema: 1,
            prompt: prompt_fingerprint(request)?,
            history_len: history.len,
            history: history.hash,
        })
    }

    /// Verify the original consumed history before adding newly appended rows.
    pub fn matches_history(&self, history: &[ConversationMessage]) -> bool {
        self.schema == 1
            && history
                .get(..self.history_len)
                .and_then(|prefix| digest(&prefix))
                .as_ref()
                == Some(&self.history)
    }

    pub(crate) fn matches_prompt(&self, request: &crate::LlmRequest) -> bool {
        self.schema == 1 && prompt_fingerprint(request).as_ref() == Some(&self.prompt)
    }

    pub(crate) fn attach(&self, usage: &mut crate::ExecutionUsage) -> Result<(), crate::LlmError> {
        use lingxi_llm_client::protocol::JsonPayload;
        let value =
            JsonPayload::parse(&serde_json::to_string(self).expect("context identity serializes"))
                .expect("generated context identity is valid JSON");
        if !usage.provider_metadata.is_root_object() {
            usage.provider_metadata = JsonPayload::parse("{}")
                .expect("empty object")
                .with_object_field("native", &usage.provider_metadata)
                .map_err(crate::upstream::error)?;
        }
        usage.provider_metadata = usage
            .provider_metadata
            .with_object_field("lingxi_context_usage_prefix", &value)
            .map_err(crate::upstream::error)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextUsageMeasurement {
    tokens: u64,
    prefix: Option<ContextUsagePrefix>,
}

/// Host-owned evidence captured at logical request assembly. Account selection
/// may change the execution profile before this evidence can be checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextUsageBinding {
    history: HistoryPrefix,
    measurement: Option<ContextUsageMeasurement>,
}

impl ContextUsageBinding {
    pub(crate) fn new(
        history: HistoryPrefix,
        measurement: Option<ContextUsageMeasurement>,
    ) -> Self {
        Self {
            history,
            measurement,
        }
    }

    pub(crate) fn bind(&self, request: &mut crate::LlmRequest) {
        // Context identity uses the selected physical protocol, while replay
        // data retains its source protocol until prepare_on adapts it.
        request.execution.context_usage_estimate = self
            .measurement
            .clone()
            .and_then(|measurement| measurement.bind(request));
        request.execution.context_usage_prefix =
            ContextUsagePrefix::new(request, self.history.clone());
    }
}

impl ContextUsageMeasurement {
    pub(crate) fn bind(self, request: &crate::LlmRequest) -> Option<ContextUsageEstimate> {
        if self
            .prefix
            .as_ref()
            .is_some_and(|prefix| !prefix.matches_prompt(request))
        {
            return None;
        }
        ContextUsageEstimate::bind(request, self.tokens)
    }
}

fn digest(value: &impl serde::Serialize) -> Option<String> {
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
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(format!("{:x}", writer.0.finalize()))
}

fn prompt_fingerprint(request: &crate::LlmRequest) -> Option<String> {
    let projected = crate::prompt_projection::for_estimate(request);
    let input = &projected.input;
    digest(&(
        &input.model,
        &request.profile,
        &request.execution.account_scope,
        &request
            .execution
            .context_usage_protocol
            .or(request.execution.input_protocol),
        &request.execution.prompt_projection_profile,
        &input.system,
        &input.tools,
        &input.hosted_tools,
        &input.native_options,
        &input.output_format,
        &input.tool_choice,
    ))
}

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
        Self(Arc::new(move |model, profile, history| {
            estimate(model, profile, history).map(|tokens| ContextUsageMeasurement {
                tokens,
                prefix: None,
            })
        }))
    }

    /// Production estimators must return the identity saved with the original
    /// measurement, after validating its consumed history prefix.
    pub fn with_prefix(
        estimate: impl Fn(&str, Option<&str>, &[ConversationMessage]) -> Option<(u64, ContextUsagePrefix)>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self(Arc::new(move |model, profile, history| {
            estimate(model, profile, history).map(|(tokens, prefix)| ContextUsageMeasurement {
                tokens,
                prefix: Some(prefix),
            })
        }))
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
        (self.0)(model, profile, history).map(|usage| usage.tokens)
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

pub(crate) fn measurement(
    model: &str,
    profile: Option<&str>,
    history: &[ConversationMessage],
) -> Option<ContextUsageMeasurement> {
    ESTIMATOR
        .try_with(|estimator| (estimator.0)(model, profile, history))
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
    let projected = crate::prompt_projection::for_estimate(request);
    let input = &projected.input;
    let mut writer = HashWriter(Sha256::new());
    // Output ceilings/effort and cache markers may change during preparation;
    // they do not add prompt content. Bind the prompt, schemas, and exact-string
    // overrides so a payload rewrite cannot reuse stale usage.
    serde_json::to_writer(
        &mut writer,
        &(
            &input.model,
            &request.profile,
            &request.execution.account_scope,
            &request.execution.input_protocol,
            &request.execution.context_usage_protocol,
            &request.execution.prompt_projection_profile,
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
    fn durable_measured_prefix_retains_exact_history_and_rejects_rewrites() {
        use lingxi_core::types::MessageId;
        let history = vec![ConversationMessage::user(
            MessageId::new(),
            "original".into(),
        )];
        let request = crate::LlmRequest::new("m")
            .with_profile("p")
            .with_user_text("original");
        let prefix = ContextUsagePrefix::capture(&request, &history).unwrap();
        let mut usage = crate::ExecutionUsage::from_counts(lingxi_llm_client::protocol::Usage {
            input_tokens: 123,
            output_tokens: 7,
            ..Default::default()
        });
        prefix.attach(&mut usage).unwrap();
        let stored = serde_json::to_string(&usage).unwrap();
        let restored: crate::ExecutionUsage = serde_json::from_str(&stored).unwrap();
        let restored: ContextUsagePrefix = serde_json::from_str(
            restored
                .provider_metadata
                .payload_at("/lingxi_context_usage_prefix")
                .unwrap()
                .as_json_source(),
        )
        .unwrap();
        assert_eq!(restored, prefix);
        assert!(restored.matches_history(&history));
        let mut appended = history.clone();
        appended.push(ConversationMessage::user(MessageId::new(), "new".into()));
        assert!(restored.matches_history(&appended));
        let rewritten = vec![ConversationMessage::user(history[0].id(), "changed".into())];
        assert!(!restored.matches_history(&rewritten));
        assert!(!restored.matches_history(&[]));
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
