//! Host-measured context occupancy for one logical main request.
//!
//! The conversation owner supplies the last provider usage plus newly appended
//! content. Opaque replay data must not turn that measured prefix back into a
//! byte-based estimate. Requests without a matching anchor retain the ordinary
//! full-input fallback.

use lingxi_core::types::{ContentBlock, ConversationMessage};
use serde::ser::SerializeSeq;
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
    /// An explicit physical messages array replaces every logical history row.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    history_replaced: bool,
    /// Provider-measured physical input, excluding output and logical deltas.
    /// Older anchors without this evidence fall back to physical counting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u64>,
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
            hash: history_fingerprint(history)?,
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
        let history_replaced = crate::anthropic_extra_body::replaces(request, "messages");
        Some(Self {
            schema: 2,
            prompt: prompt_fingerprint(request)?,
            history_len: if history_replaced { 0 } else { history.len },
            history: if history_replaced {
                String::new()
            } else {
                history.hash
            },
            history_replaced,
            input_tokens: None,
        })
    }

    /// Verify the original consumed history before adding newly appended rows.
    pub fn matches_history(&self, history: &[ConversationMessage]) -> bool {
        self.schema == 2
            && (self.history_replaced
                || history
                    .get(..self.history_len)
                    .and_then(history_fingerprint)
                    .as_ref()
                    == Some(&self.history))
    }

    pub(crate) fn matches_prompt(&self, request: &crate::LlmRequest) -> bool {
        self.schema == 2 && prompt_fingerprint(request).as_ref() == Some(&self.prompt)
    }

    pub(crate) fn attach(&self, usage: &mut crate::ExecutionUsage) -> Result<(), crate::LlmError> {
        use lingxi_llm_client::protocol::JsonPayload;
        let mut measured = self.clone();
        measured.input_tokens = usage.report.usage.map(|usage| {
            usage
                .input_tokens
                .saturating_add(usage.cache_read_tokens)
                .saturating_add(usage.cache_write_tokens)
        });
        let value = JsonPayload::parse(
            &serde_json::to_string(&measured).expect("context identity serializes"),
        )
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
        let tokens = if crate::anthropic_extra_body::replaces(request, "messages") {
            // The previous response and newly appended logical rows will not
            // enter this wire body. Reuse only its matching measured input.
            self.prefix
                .as_ref()
                .filter(|prefix| prefix.history_replaced)?
                .input_tokens?
        } else {
            self.tokens
        };
        ContextUsageEstimate::bind(request, tokens)
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

// Native JSONL stores TextJsUtf16 as an ordinary text block. Hash its exact
// model-visible units, rather than the live DTO tag or display projection.
// Borrow every other block so large opaque replay data is never cloned just
// to compare a measured prefix after cold resume.
fn history_fingerprint(history: &[ConversationMessage]) -> Option<String> {
    struct History<'a>(&'a [ConversationMessage]);
    struct Blocks<'a>(&'a [ContentBlock]);
    struct Api<'a>(&'a Option<lingxi_core::types::ApiSystemMessage>);
    #[derive(serde::Serialize)]
    struct Source<'a>(
        #[serde(serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize")]
        &'a lingxi_core::types::utf16_json::Utf16JsonProjection,
    );
    fn source(
        value: &Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) -> (bool, Option<Source<'_>>) {
        (value.is_some(), value.as_ref().map(Source))
    }
    impl serde::Serialize for Blocks<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
            for block in self.0 {
                match block {
                    ContentBlock::Text { text, citations } => sequence.serialize_element(&(
                        "text",
                        text.encode_utf16().collect::<Vec<_>>(),
                        citations.is_some(),
                        citations.as_ref().and_then(Option::as_ref).map(Source),
                    ))?,
                    ContentBlock::TextJsUtf16 {
                        utf16_code_units,
                        citations,
                        ..
                    } => {
                        sequence.serialize_element(&(
                            "text",
                            utf16_code_units,
                            citations.is_some(),
                            citations.as_ref().and_then(Option::as_ref).map(Source),
                        ))?;
                    }
                    _ => sequence.serialize_element(block)?,
                }
            }
            sequence.end()
        }
    }
    impl serde::Serialize for Api<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            self.0
                .as_ref()
                .map(|api| (Blocks(&api.content), &api.output_config))
                .serialize(serializer)
        }
    }
    impl serde::Serialize for History<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
            for message in self.0 {
                match message {
                    ConversationMessage::User {
                        id,
                        content,
                        is_meta,
                        is_compact_summary,
                        is_visible_in_transcript_only,
                        turn_companion,
                        usage_limit_note,
                        origin,
                        turn_origin,
                        interrupted_by_shutdown,
                        refused_message_id,
                        api_message_override,
                    } => sequence.serialize_element(&(
                        "user",
                        id,
                        Blocks(content),
                        is_meta,
                        is_compact_summary,
                        is_visible_in_transcript_only,
                        (turn_companion.is_some(), turn_companion),
                        (usage_limit_note.is_some(), usage_limit_note),
                        source(origin),
                        source(turn_origin),
                        interrupted_by_shutdown,
                        source(refused_message_id),
                        Api(api_message_override),
                    ))?,
                    ConversationMessage::Assistant {
                        id,
                        content,
                        stop_reason,
                        resumable,
                        per_turn_effort,
                    } => sequence.serialize_element(&(
                        "assistant",
                        id,
                        Blocks(content),
                        stop_reason,
                        source(resumable),
                        source(per_turn_effort),
                    ))?,
                    ConversationMessage::System {
                        id,
                        content,
                        subtype,
                        api_system,
                        compact_metadata,
                        model_fallback,
                        refusal_fallback,
                    } => sequence.serialize_element(&(
                        "system",
                        id,
                        Source(content),
                        subtype,
                        Api(api_system),
                        compact_metadata,
                        model_fallback,
                        refusal_fallback,
                    ))?,
                }
            }
            sequence.end()
        }
    }
    digest(&History(history))
}

fn prompt_fingerprint(request: &crate::LlmRequest) -> Option<String> {
    let system = crate::prompt_projection::system_for_estimate(request);
    let input = &request.input;
    let replaces = |field| crate::anthropic_extra_body::replaces(request, field);
    digest(&(
        &input.model,
        &request.profile,
        &request.execution.account_scope,
        &request
            .execution
            .context_usage_protocol
            .or(request.execution.input_protocol),
        &request.execution.prompt_projection_profile,
        (!replaces("system")).then_some(system.as_ref()),
        (!replaces("tools")).then_some(&input.tools),
        (!replaces("tools")).then_some(&input.hosted_tools),
        &input.native_options,
        (!(replaces("output_format") || replaces("output_config"))).then_some(&input.output_format),
        (!replaces("tool_choice")).then_some(&input.tool_choice),
        crate::anthropic_extra_body::prompt(request),
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
    let system = crate::prompt_projection::system_for_estimate(request);
    let input = &request.input;
    let replaces = |field| crate::anthropic_extra_body::replaces(request, field);
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
            (!replaces("system")).then_some(system.as_ref()),
            (!replaces("messages")).then_some(&input.messages),
            (!replaces("tools")).then_some(&input.tools),
            (!replaces("tools")).then_some(&input.hosted_tools),
            &input.native_options,
            (!(replaces("output_format") || replaces("output_config")))
                .then_some(&input.output_format),
            (!replaces("tool_choice")).then_some(&input.tool_choice),
            (!replaces("messages")).then_some(&request.execution.message_json_string_overrides),
            crate::anthropic_extra_body::prompt(request),
        ),
    )
    .ok()?;
    Some(writer.0.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_identity_normalizes_text_carriers_but_preserves_exact_utf16_and_fences() {
        use lingxi_core::types::MessageId;
        let id = MessageId::new();
        let plain = ConversationMessage::user(id, "hello 😀".into());
        let mut exact = plain.clone();
        if let ConversationMessage::User { content, .. } = &mut exact {
            content[0] = ContentBlock::TextJsUtf16 {
                text: "display is not the source".into(),
                utf16_code_units: "hello 😀".encode_utf16().collect(),
                citations: None,
            };
        }
        let request = crate::LlmRequest::new("m");
        let prefix = ContextUsagePrefix::capture(&request, &[exact.clone()]).unwrap();
        assert!(prefix.matches_history(&[plain.clone()]));
        let mut changed = exact.clone();
        if let ConversationMessage::User { content, .. } = &mut changed {
            let ContentBlock::TextJsUtf16 {
                utf16_code_units, ..
            } = &mut content[0]
            else {
                unreachable!()
            };
            utf16_code_units.push(0xd800);
        }
        assert!(!prefix.matches_history(&[changed.clone()]));
        let lone_prefix = ContextUsagePrefix::capture(&request, &[changed.clone()]).unwrap();
        let mut other_lone = changed;
        if let ConversationMessage::User { content, .. } = &mut other_lone {
            let ContentBlock::TextJsUtf16 {
                utf16_code_units, ..
            } = &mut content[0]
            else {
                unreachable!()
            };
            *utf16_code_units.last_mut().unwrap() = 0xd801;
        }
        assert!(!lone_prefix.matches_history(&[other_lone]));
        assert!(!prefix.matches_history(&[ConversationMessage::user(
            MessageId::new(),
            "hello 😀".into()
        )]));
        let mut metadata = plain.clone();
        if let ConversationMessage::User { origin, .. } = &mut metadata {
            *origin =
                Some(lingxi_core::types::utf16_json::Utf16JsonProjection::parse("null").unwrap());
        }
        assert!(!prefix.matches_history(&[metadata]));
        let second = ConversationMessage::user(MessageId::new(), "second".into());
        let ordered =
            ContextUsagePrefix::capture(&request, &[plain.clone(), second.clone()]).unwrap();
        assert!(!ordered.matches_history(&[second, plain]));
    }

    #[test]
    fn measured_identity_tracks_extra_prompt_but_not_metadata_output_caps_or_cache_markers() {
        use lingxi_llm_client::{
            protocol::JsonPayload, providers::anthropic::request_policy::AnthropicRequestKind,
        };
        fn extra(source: &str) -> crate::LlmRequest {
            let mut request = crate::LlmRequest::new("m").with_user_text("hi");
            request.execution.anthropic_extra_body = Some(
                crate::CapturedAnthropicExtraBody::new(
                    Some(JsonPayload::parse(source).unwrap()),
                    AnthropicRequestKind::Main,
                )
                .unwrap(),
            );
            request
        }
        let original = extra(r#"{"system":[{"type":"text","text":"same"}]}"#);
        let prefix = ContextUsagePrefix::capture(&original, &[]).unwrap();
        let measurement = ContextUsageEstimate::bind(&original, 42).unwrap();
        let policy_only = extra(
            r#"{"system":[{"type":"text","text":"same","cache_control":{"type":"ephemeral"}}],"metadata":{"user_id":"new"},"max_tokens":1234,"output_config":{"effort":"high"}}"#,
        );
        assert!(prefix.matches_prompt(&policy_only));
        assert_eq!(measurement.tokens_for(&policy_only), Some(42));
        for changed in [
            r#"{"system":[{"type":"text","text":"changed"}]}"#,
            r#"{"messages":[{"role":"user","content":"changed"}]}"#,
            r#"{"tools":[{"name":"changed","input_schema":{}}]}"#,
            r#"{"output_config":{"format":{"type":"json_schema","schema":{"type":"string"}}}}"#,
        ] {
            let request = extra(changed);
            assert!(!prefix.matches_prompt(&request), "{changed}");
            assert_eq!(measurement.tokens_for(&request), None, "{changed}");
        }
        // Non-Anthropic routes ignore this native-only environment entirely.
        let mut first = original;
        let mut second = extra(r#"{"system":"different"}"#);
        first.execution.context_usage_protocol = Some(crate::ProtocolFamily::OpenAiResponses);
        second.execution.context_usage_protocol = first.execution.context_usage_protocol;
        assert_eq!(prompt_fingerprint(&first), prompt_fingerprint(&second));
    }

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
        let mut measured_prefix = prefix.clone();
        measured_prefix.input_tokens = Some(123);
        assert_eq!(restored, measured_prefix);
        assert!(restored.matches_history(&history));
        let mut appended = history.clone();
        appended.push(ConversationMessage::user(MessageId::new(), "new".into()));
        assert!(restored.matches_history(&appended));
        let rewritten = vec![ConversationMessage::user(history[0].id(), "changed".into())];
        assert!(!restored.matches_history(&rewritten));
        assert!(!restored.matches_history(&[]));
    }

    #[test]
    fn complete_message_override_reuses_only_measured_physical_input() {
        use lingxi_core::types::MessageId;
        use lingxi_llm_client::{
            protocol::JsonPayload, providers::anthropic::request_policy::AnthropicRequestKind,
        };
        let mut request = crate::LlmRequest::new("m").with_user_text("logical history");
        request
            .input
            .system
            .push(lingxi_llm_client::protocol::SystemBlock::new(
                "logical system",
            ));
        request
            .input
            .tools
            .push(lingxi_llm_client::protocol::ToolSpec {
                name: "logical-tool".into(),
                description: JsonPayload::string("logical description"),
                input_schema: JsonPayload::parse(r#"{"type":"object"}"#).unwrap(),
                strict: false,
                defer_loading: false,
                native_options: vec![],
                tool_type: None,
                extra: JsonPayload::parse("null").unwrap(),
            });
        request.execution.anthropic_extra_body = Some(crate::CapturedAnthropicExtraBody::new(
            Some(JsonPayload::parse(r#"{"system":"physical","messages":[{"role":"user","content":"fixed"}],"tools":[]}"#).unwrap()),
            AnthropicRequestKind::Main,
        ).unwrap());
        let logical = [ConversationMessage::user(
            MessageId::new(),
            "original".into(),
        )];
        let unmeasured = ContextUsagePrefix::capture(&request, &logical).unwrap();
        assert!(unmeasured.history_replaced);
        assert!(
            ContextUsageMeasurement {
                tokens: 9000,
                prefix: Some(unmeasured.clone())
            }
            .bind(&request)
            .is_none(),
            "old anchors without input-only evidence must count the physical input"
        );
        let mut usage = crate::ExecutionUsage::from_counts(lingxi_llm_client::protocol::Usage {
            input_tokens: 10,
            cache_read_tokens: 3,
            cache_write_tokens: 2,
            output_tokens: 4000,
            ..Default::default()
        });
        unmeasured.attach(&mut usage).unwrap();
        let measured: ContextUsagePrefix = serde_json::from_str(
            usage
                .provider_metadata
                .payload_at("/lingxi_context_usage_prefix")
                .unwrap()
                .as_json_source(),
        )
        .unwrap();
        assert_eq!(measured.input_tokens, Some(15));
        let mut ignored_rewrite = request.clone().with_user_text("x".repeat(8000));
        ignored_rewrite.input.system.clear();
        ignored_rewrite.input.tools.clear();
        ignored_rewrite
            .execution
            .message_json_string_overrides
            .insert("/messages/0/content/0/text".into(), vec![0xd800]);
        assert!(measured.matches_history(&[]));
        assert!(measured.matches_prompt(&ignored_rewrite));
        let estimate = ContextUsageMeasurement {
            tokens: 9000,
            prefix: Some(measured.clone()),
        }
        .bind(&ignored_rewrite)
        .unwrap();
        assert_eq!(estimate.tokens_for(&ignored_rewrite), Some(15));
        assert_eq!(estimate.tokens_for(&request), Some(15));
        let mut changed = ignored_rewrite;
        changed.execution.anthropic_extra_body = Some(crate::CapturedAnthropicExtraBody::new(
            Some(JsonPayload::parse(r#"{"system":"changed","messages":[{"role":"user","content":"fixed"}],"tools":[]}"#).unwrap()),
            AnthropicRequestKind::Main,
        ).unwrap());
        assert!(!measured.matches_prompt(&changed));
        assert_eq!(estimate.tokens_for(&changed), None);
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
