//! `SideQueryClient` trait and request/response DTOs.
//!
//! A side query is a stateless, one-shot LLM call: no tool loop, no state
//! machine slot, no shared cache prefix with the parent. The host owns
//! lifetimes and timeouts. Side queries power §6.3 memory selection,
//! §7.4 permission explanations, §6.5 session search, lightweight
//! classifiers, and prompt suggestions.
//!
//! For full-loop spawns that need to share the parent's prompt cache, use
//! [`crate::forked_agent::ForkedAgentRunner`] instead.

use crate::purposes::QuerySource;
use async_trait::async_trait;
use lingxi_core::host::task_delivery::TaskDeliveryRequest;
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ConversationMessage, MessageId};
use lingxi_llm_client::providers::anthropic::system_prompt::{
    NativePromptAttribution, NativeSystemPrefix, SystemPromptInput,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::time::Duration;
use thiserror::Error;

fn deserialize_optional_string_source<'de, D>(
    deserializer: D,
) -> Result<Option<Utf16JsonProjection>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let source = Option::<Box<serde_json::value::RawValue>>::deserialize(deserializer)?
        .map(|raw| Utf16JsonProjection::parse(raw.get()).map_err(serde::de::Error::custom))
        .transpose()?;
    if source
        .as_ref()
        .is_some_and(|value| value.string_units("").is_none())
    {
        return Err(serde::de::Error::custom(
            "system_prompt must be a JSON string",
        ));
    }
    Ok(source)
}
/// Host-only Native system context for a deliberately opted-in side query.
/// This is never serialized into side-query JSON; `ProviderSideQueryClient`
/// projects it into the existing SDK prompt-cache context.
#[derive(Clone)]
pub struct NativeSideQueryPromptContext {
    /// Captured SDK prefix with the role appropriate for the resolved provider.
    pub prefix: NativeSystemPrefix,
    /// Source attribution facts for this particular request.
    pub attribution: NativePromptAttribution,
    /// Only the Insights host call opts into the SDK's full Native prefix.
    pub include_host_identity: bool,
}

impl std::fmt::Debug for NativeSideQueryPromptContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeSideQueryPromptContext")
            .field("prefix", &"<redacted>")
            .field("attribution", &"<redacted>")
            .field("include_host_identity", &self.include_host_identity)
            .finish()
    }
}

/// Parameters for one side-query LLM call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideQueryRequest {
    /// Host-owned registered logical call; request JSON cannot supply it.
    #[serde(skip)]
    pub model_attempt: Option<lingxi_core::host::ModelAttemptContext>,
    /// Model id (e.g. `claude-haiku-4-5`).
    pub model: String,
    /// Host-local exact Native model source used only when the parent route
    /// could not represent its configured model id. It never enters side-query
    /// JSON; the provider adapter applies it to the SDK request body.
    #[serde(skip)]
    pub model_source: Option<Utf16JsonProjection>,
    /// Optional provider profile that owns `model`.
    ///
    /// Compaction inherits this from the live session so a model id shared by
    /// multiple providers is routed through the same provider as the parent
    /// turn instead of falling back to registry-first resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Optional system prompt; when `None` the provider default applies.
    #[serde(
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "deserialize_optional_string_source"
    )]
    pub system_prompt: Option<Utf16JsonProjection>,
    /// Host-local identities of typed attachments whose exact text boundaries
    /// must survive provider normalization. This sidecar is not serialized
    /// into the model-facing request or canonical size estimate.
    #[serde(skip)]
    pub typed_attachment_ids: HashSet<MessageId>,
    /// Host-local Native task-delivery snapshots plus the placement facts for
    /// this request, keyed by their source user-row identity. This sidecar is
    /// not serialized into side-query JSON.
    #[serde(skip)]
    pub task_deliveries: HashMap<MessageId, TaskDeliveryRequest>,
    /// Host-captured Native system prefix and attribution for an explicitly
    /// opted-in side query. This is never part of the serialized payload.
    #[serde(skip)]
    pub native_prompt_context: Option<NativeSideQueryPromptContext>,
    /// Host-captured provider system structure for forked requests; not part of
    /// the serialized model-facing side-query payload.
    #[serde(skip)]
    pub system_prompt_input: Option<SystemPromptInput>,
    /// Whether this side-query call explicitly enables automatic Native
    /// prompt-cache markers. Native's `bze` option defaults to false; cache-safe
    /// fork callers opt in when they intentionally reuse the parent cache.
    #[serde(skip)]
    pub enable_prompt_caching: bool,
    /// Conversation messages to feed the model.
    pub messages: Vec<ConversationMessage>,
    /// Tool schemas exposed to the model (may be empty).
    pub tools: Vec<llm_runtime::ToolDeclaration>,
    /// Tool-choice control (e.g. force a specific tool / json mode).
    pub tool_choice: Option<Value>,
    /// Structured-output schema (e.g. JSON Schema for `response_format`).
    pub output_format: Option<Value>,
    /// Output cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Caller-side retry budget for transient API errors.
    pub max_retries: u32,
    /// Sampling temperature; defaults to provider default when `None`.
    pub temperature: Option<f32>,
    /// Session thinking configuration to INHERIT on this call (cc 2.1.198
    /// "Subagents + compaction inherit extended thinking config"; binary: the
    /// compaction summarizer passes `thinkingConfig: mXt(r)` — the session
    /// `options.thinkingConfig` — @216945141). `None` = legacy: no `thinking`
    /// field on the wire (utility side queries — memory selection, `WebFetch`
    /// summarization — match the binary's explicit `{type:"disabled"}`
    /// callers). Replaces the never-forwarded `thinking_budget` knob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<llm_runtime::model::thinking::ThinkingConfig>,
    /// Output effort inherited by compaction from the live parent request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Value>,
    /// Stop sequences.
    pub stop_sequences: Vec<String>,
    /// COGS tag — see [`QuerySource`].
    pub query_source: QuerySource,
    /// Skip the engine-injected system-prompt prefix (e.g. LINGXI.md preamble).
    pub skip_system_prompt_prefix: bool,
}

/// Decoded outcome of one side-query call.
#[derive(Debug, Clone)]
pub struct SideQueryResponse {
    /// Best-effort flattened plain-text response when present, retaining exact
    /// provider UTF-16 units through downstream WebFetch/tool consumers.
    pub text: Option<Utf16JsonProjection>,
    /// Structured-output payload when the model returned JSON.
    pub structured: Option<Utf16JsonProjection>,
    /// Tool calls emitted by the model (typically empty for side queries).
    pub tool_calls: Vec<Utf16JsonProjection>,
    /// Token / cost usage for COGS attribution.
    pub usage: cost::Usage,
    /// Stop-reason as reported by the provider (`end_turn`, `tool_use`, ...).
    pub stop_reason: Option<String>,
    /// Budget-consuming retries performed by this specific provider call.
    pub retry_count: u32,
}

/// A response retained by an isolated fork, including replayable assistant
/// blocks and the HTTP request id. Neither field is a session-global snapshot.
#[derive(Debug, Clone)]
pub struct ForkedSideQueryResponse {
    /// Ordinary side-query result.
    pub response: SideQueryResponse,
    /// Complete assistant blocks for a denied-tool continuation.
    pub assistant_content: Vec<lingxi_core::types::ContentBlock>,
    /// Native assistant text candidates in their emission order.
    pub candidates: Vec<String>,
    /// HTTP request id, distinct from the assistant's message id.
    pub request_id: Option<String>,
}

/// Host-owned fork identity, retained across denied-tool continuations.
#[derive(Debug, Clone, Default)]
pub struct ForkedSideQueryOptions {
    /// Explicit system blocks, including each block's cache boundary.
    /// Bypasses native prompt prefixing and implicit message cache markers.
    pub system_blocks: Option<Vec<llm_runtime::SystemBlock>>,
    /// Parent's last assistant group (or last message for an unfinished turn).
    pub fork_point_message_id: Option<lingxi_core::types::MessageId>,
}

/// Side-query failure surface.
#[derive(Debug, Clone, Error)]
pub enum SideQueryError {
    /// Underlying API call failed (transport, 4xx/5xx, malformed stream).
    #[error(transparent)]
    Api(#[from] llm_runtime::LlmError),
    /// Provider returned a response we could not decode into a
    /// [`SideQueryResponse`].
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    /// Provider / codec cannot constrain the response to a JSON schema.
    #[error("structured output is unsupported")]
    StructuredOutputUnsupported,
    /// One or more batches completed before a later batch failed. The partial
    /// accounting must still be charged, while the incomplete analysis is not
    /// safe to persist.
    #[error("{source}")]
    Partial {
        /// Underlying failure that stopped the delegation.
        #[source]
        source: Box<SideQueryError>,
        /// Usage from completed provider calls.
        usage: cost::Usage,
        /// Wall-clock time spent before the failure was observed.
        elapsed: Duration,
        /// Retries reported by completed provider calls.
        retry_count: u32,
        /// Provider batches that were started.
        api_calls: u32,
    },
}

/// Strict JSON-schema side query (Fusion analyst / synthesizer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrictStructuredQueryRequest {
    /// Host-owned registered logical call; request JSON cannot supply it.
    #[serde(skip)]
    pub model_attempt: Option<lingxi_core::host::ModelAttemptContext>,
    /// Model id.
    pub model: String,
    /// Optional provider profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Optional system prompt.
    pub system_prompt: Option<String>,
    /// Conversation messages.
    pub messages: Vec<ConversationMessage>,
    /// JSON Schema the response must satisfy.
    pub schema: Value,
    /// Output cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Temperature. Analyst should pass `Some(0.0)`.
    pub temperature: Option<f32>,
    /// COGS tag.
    pub query_source: QuerySource,
    /// Skip the engine-injected system-prompt prefix.
    pub skip_system_prompt_prefix: bool,
}

/// Request shape used by Fusion's preflight estimator. The Session provider
/// adapter replaces the fallback DTO serialization with the same canonical
/// LlmRequest builder used for the actual wire call.
#[derive(Debug, Clone)]
pub enum CanonicalSideQueryRequest {
    /// Ordinary text side query.
    Plain(SideQueryRequest),
    /// Strict JSON-schema side query.
    Strict(StrictStructuredQueryRequest),
}

/// Deterministic preflight request-size/token estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SideQueryEstimate {
    /// Bytes in the canonical request representation.
    pub serialized_bytes: u64,
    /// Conservative input-token estimate.
    pub input_tokens: u64,
}

/// Decoded strict JSON-schema side query.
#[derive(Debug, Clone)]
pub struct StrictStructuredQueryResponse {
    /// Parsed JSON value (already schema-decoded by the provider + serde).
    pub value: Value,
    /// Token / cost usage.
    pub usage: cost::Usage,
    /// Model that produced the value.
    pub model: String,
    /// Profile used.
    pub profile: Option<String>,
    /// Provider request id when present.
    pub request_id: Option<String>,
    /// Retry count for this call.
    pub retry_count: u32,
}

/// One-shot LLM client. Implementations route to `AnthropicProvider` (M1)
/// and to other providers in Plan 04 follow-ups.
#[async_trait]
pub trait SideQueryClient: Send + Sync {
    /// Issue one side query and return the decoded response.
    async fn query(&self, request: SideQueryRequest) -> Result<SideQueryResponse, SideQueryError>;

    /// Retain a call's native assistant blocks for a cache-prefix fork.
    /// The default lets in-memory clients exercise the same denial loop.
    async fn query_fork(
        &self,
        request: SideQueryRequest,
        options: ForkedSideQueryOptions,
    ) -> Result<ForkedSideQueryResponse, SideQueryError> {
        if options.system_blocks.is_some() {
            return Err(SideQueryError::InvalidResponse(
                "explicit system blocks are unsupported by this side-query client".into(),
            ));
        }
        let response = self.query(request).await?;
        Ok(crate::fork_response::synthesize(response))
    }

    /// Retry attempts observed by the most recently completed provider call.
    /// Test doubles and direct clients default to zero.
    fn last_retry_count(&self) -> u32 {
        0
    }

    /// Whether request estimates come from the same canonical Session builder
    /// used for dispatch. Small fakes retain the DTO fallback for deterministic
    /// unit tests; production ProviderSideQueryClient opts into this seam.
    fn has_canonical_estimator(&self) -> bool {
        false
    }

    /// Strict JSON-schema query. Default rejects so existing test doubles stay
    /// object-safe and source-compatible. Fusion's production client overrides
    /// this and must not fall back to best-effort `output_format` parsing.
    async fn query_json_schema(
        &self,
        _request: StrictStructuredQueryRequest,
    ) -> Result<StrictStructuredQueryResponse, SideQueryError> {
        Err(SideQueryError::StructuredOutputUnsupported)
    }

    /// Estimate a request without contacting a provider.
    ///
    /// ProviderSideQueryClient overrides this for the live Session backend so
    /// the estimate uses ApiService's canonical request builder. Fakes and
    /// isolated utility clients retain a deterministic DTO fallback.
    fn estimate_request(
        &self,
        request: CanonicalSideQueryRequest,
    ) -> Result<SideQueryEstimate, SideQueryError> {
        let serialized_bytes = match request {
            CanonicalSideQueryRequest::Plain(request) => serialized_size(&request),
            CanonicalSideQueryRequest::Strict(request) => serialized_size(&request),
        }?;
        Ok(SideQueryEstimate {
            serialized_bytes,
            input_tokens: llm_runtime::model::count_tokens::approximate_tokens_for_bytes(
                serialized_bytes,
            ),
        })
    }
}

#[cfg(test)]
mod source_system_prompt_tests {
    use super::*;

    fn request(system_prompt: Option<Utf16JsonProjection>) -> SideQueryRequest {
        SideQueryRequest {
            enable_prompt_caching: false,
            model_attempt: None,
            native_prompt_context: None,
            model: "model".into(),
            model_source: None,
            profile: None,
            system_prompt,
            typed_attachment_ids: HashSet::new(),
            task_deliveries: HashMap::new(),
            system_prompt_input: None,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            output_format: None,
            max_tokens: Some(16),
            max_retries: 0,
            temperature: None,
            thinking: None,
            effort: None,
            stop_sequences: Vec::new(),
            query_source: QuerySource::Custom("test".into()),
            skip_system_prompt_prefix: false,
        }
    }

    fn task_delivery_request(snapshot: &str) -> TaskDeliveryRequest {
        TaskDeliveryRequest {
            snapshot: Utf16JsonProjection::parse(snapshot).unwrap(),
            placement: lingxi_core::host::task_delivery::TaskDeliveryPlacement {
                starts_turn: true,
                in_drain_position: false,
                behind_earlier_messages: false,
                in_human_turn: false,
            },
        }
    }

    #[test]
    fn system_prompt_serializes_as_a_json_string_and_keeps_lone_units() {
        let units = "before".encode_utf16().chain([0xD800]).collect::<Vec<_>>();
        let source =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units.clone())
                .unwrap();
        let mut side_query = request(Some(source));
        side_query.typed_attachment_ids.insert(MessageId::new());
        side_query.task_deliveries.insert(
            MessageId::new(),
            task_delivery_request(r#"{"taskId":"task-a"}"#),
        );
        let serialized = serde_json::to_string(&side_query).unwrap();
        assert!(serialized.contains(r#""system_prompt":"before\ud800""#));
        assert!(!serialized.contains("\"nodes\""));
        assert!(
            !serialized.contains("typed_attachment_ids"),
            "host-local message identities are not part of the serialized side-query payload"
        );
        assert!(
            !serialized.contains("task_deliveries"),
            "task-delivery snapshots and placement facts stay out of side-query JSON"
        );

        let decoded: SideQueryRequest = serde_json::from_str(&serialized).unwrap();
        assert_eq!(
            decoded
                .system_prompt
                .as_ref()
                .and_then(|source| source.string_units("")),
            Some(units)
        );
        assert!(decoded.typed_attachment_ids.is_empty());
        assert!(decoded.task_deliveries.is_empty());

        let absent_serialized = serde_json::to_string(&request(None)).unwrap();
        assert!(absent_serialized.contains(r#""system_prompt":null"#));
        let absent_decoded: SideQueryRequest = serde_json::from_str(&absent_serialized).unwrap();
        assert!(absent_decoded.system_prompt.is_none());
    }

    #[test]
    fn system_prompt_rejects_non_string_source() {
        let error = serde_json::from_str::<SideQueryRequest>(
            r#"{"model":"model","system_prompt":{"value":"wrong"}}"#,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("system_prompt must be a JSON string"));
    }
}

#[derive(Default)]
struct SerializedByteCounter {
    bytes: u64,
}

impl Write for SerializedByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let bytes = u64::try_from(buffer.len())
            .map_err(|_| std::io::Error::other("serialized request length overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| std::io::Error::other("serialized request length overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Count compact JSON bytes directly from a borrowed value. Request sizing is
/// a preflight operation; it must not allocate a second full request body just
/// to learn the length of the body the canonical builder already produced.
pub(crate) fn serialized_size<T: Serialize>(value: &T) -> Result<u64, SideQueryError> {
    let mut counter = SerializedByteCounter::default();
    serde_json::to_writer(&mut counter, value)
        .map_err(|error| SideQueryError::InvalidResponse(error.to_string()))?;
    Ok(counter.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrowed_counter_matches_compact_json_for_escaping_and_unicode() {
        let value = serde_json::json!({
            "task": "界\\\"\n",
            "dimensions": ["coverage", "safety"],
            "nested": {"ok": true}
        });
        let expected = u64::try_from(serde_json::to_vec(&value).unwrap().len()).unwrap();
        assert_eq!(serialized_size(&value).unwrap(), expected);
    }
}
