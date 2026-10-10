//! Host history and presentation DTOs. Provider execution uses SDK protocol types.
//! These serialized shapes remain stable for transcript and UI consumers.

use crate::{CostEstimate, ExecutionUsage};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_llm_client::protocol::{JsonPayload, JsonPayloadFragment};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;

fn deserialize_present_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    bool::deserialize(deserializer).map(Some)
}

fn deserialize_present_nullable_payload<'de, D>(
    deserializer: D,
) -> Result<Option<Option<JsonPayload>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<JsonPayload>::deserialize(deserializer).map(Some)
}

fn deserialize_present_payload<'de, D>(deserializer: D) -> Result<Option<JsonPayload>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    JsonPayload::deserialize(deserializer).map(Some)
}

fn deserialize_system_text<'de, D>(deserializer: D) -> Result<Utf16JsonProjection, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let source = lingxi_core::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
    if source.string_units("").is_none() {
        return Err(serde::de::Error::custom(
            "system block text must be a JSON string value",
        ));
    }
    Ok(source)
}

/// Reserved metadata key for a host-projected Anthropic fallback iteration quote.
pub const SERVER_FALLBACK_COST_QUOTE_KEY: &str = "server_fallback_cost_quote";

/// Read the host-owned fallback quote envelope from response or stream usage
/// metadata. Stream usage nests provider metadata under `stream`.
pub fn server_fallback_cost_quote(metadata: &JsonPayload) -> Option<JsonPayload> {
    metadata
        .payload_at(&format!(
            "/stream/llm_client/{SERVER_FALLBACK_COST_QUOTE_KEY}"
        ))
        .or_else(|| metadata.payload_at(&format!("/llm_client/{SERVER_FALLBACK_COST_QUOTE_KEY}")))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryServerFallback {
    pub event: lingxi_llm_client::providers::anthropic::fallback_response::ServerFallbackEvent,
    pub profile: String,
    pub lane: lingxi_llm_client::providers::anthropic::fallback_request::ServerLane,
}

/// Host history response with accounting and presentation metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryResponse {
    /// Provider response id.
    pub id: String,
    /// Model that produced the response.
    pub model: String,
    /// Output content blocks.
    pub content: Vec<ContentBlock>,
    /// Normalized terminal stop reason (Anthropic vocabulary: `end_turn`,
    /// `tool_use`, `max_tokens`, `stop_sequence`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Optional refusal `stop_details` (`{category, explanation}`) for the
    /// terminal refusal message's cyber/bio variant. `None` for non-refusals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<HistoryStopDetails>,
    /// Normalized usage.
    pub usage: ExecutionUsage,
    /// Optional per-call cost estimate.
    pub cost: Option<CostEstimate>,
    /// Source-backed provider metadata, including opaque Native event payloads.
    #[serde(default)]
    pub provider_metadata: JsonPayload,
    /// Exact provider resumability checkpoint used when a streaming assistant
    /// row is persisted for later recovery. `Some(null)` preserves a present
    /// native checkpoint whose JSON value is null.
    #[serde(
        default,
        deserialize_with = "deserialize_present_payload",
        skip_serializing_if = "Option::is_none"
    )]
    pub resumable: Option<JsonPayload>,
}
impl HistoryResponse {}
impl HistoryResponse {
    /// Admitted host request effort, never decoded from provider output.
    pub fn per_turn_effort(&self) -> Option<Utf16JsonProjection> {
        let source = self
            .provider_metadata
            .payload_at("/llm_client/per_turn_effort")?;
        serde_json::from_str(source.as_json_source()).ok()
    }

    /// Record only the admitted effort that produced this provider response.
    /// Nested source edits preserve unrelated provider metadata byte-for-byte.
    pub(crate) fn set_per_turn_effort(
        &mut self,
        effort: Option<&Utf16JsonProjection>,
    ) -> Result<(), crate::LlmError> {
        let Some(effort) = effort else {
            return Ok(());
        };
        let effort_source =
            effort
                .to_json_string()
                .map_err(|error| crate::LlmError::InvalidRequest {
                    message: error.to_string(),
                })?;
        let effort = JsonPayload::parse(&effort_source).map_err(|error| {
            crate::LlmError::InvalidRequest {
                message: error.to_string(),
            }
        })?;
        let empty = || JsonPayload::parse("{}").expect("static JSON object is valid");
        let root = if self.provider_metadata.is_root_object() {
            self.provider_metadata.clone()
        } else {
            empty()
        };
        let client = root
            .payload_at("/llm_client")
            .filter(|value| value.is_root_object())
            .unwrap_or_else(empty)
            .with_object_field("per_turn_effort", &effort)
            .map_err(|error| crate::LlmError::InvalidRequest {
                message: error.to_string(),
            })?;
        self.provider_metadata =
            root.with_object_field("llm_client", &client)
                .map_err(|error| crate::LlmError::InvalidRequest {
                    message: error.to_string(),
                })?;
        Ok(())
    }

    /// Host-authored observations only. Wire projection clears this reserved
    /// key before installing events admitted by the execution context.
    pub fn server_fallback_events(&self) -> Vec<HistoryServerFallback> {
        self.provider_metadata
            .payload_at("/llm_client/server_fallback_events")
            .and_then(|payload| serde_json::from_str(payload.as_json_source()).ok())
            .unwrap_or_default()
    }

    /// Native per-iteration quote fact when server fallback billing was active.
    /// Presence distinguishes an incomplete native quote from an ordinary call
    /// with no server-fallback iteration branch.
    pub fn server_fallback_cost_quote(&self) -> Option<JsonPayload> {
        server_fallback_cost_quote(&self.provider_metadata)
    }
}

/// Host history and presentation event.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryEvent {
    /// SDK-decoded server routing event, admitted outside provider input.
    ServerFallback {
        event: Box<lingxi_llm_client::providers::anthropic::fallback_response::ServerFallbackEvent>,
        profile: String,
        lane: lingxi_llm_client::providers::anthropic::fallback_request::ServerLane,
    },
    /// Host-owned fact about the model serving the current provider response.
    /// This is emitted for every admitted native fallback start, including
    /// starts that do not produce a controller-facing `ServerFallback` event.
    ResponseObserved {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
    },
    /// Host-owned quote for a physical provider response. Native server
    /// fallback quotes are independent of aggregate token usage: `estimate`
    /// stays `None` when the SDK iteration quote is incomplete, while the
    /// explicit marker prevents callers from substituting the request-model
    /// aggregate tariff.
    CostQuoteObserved {
        estimate: Option<CostEstimate>,
        native_server_fallback: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_model: Option<String>,
    },
    /// SDK-normalized hosted search attribution and progress metadata.
    WebSearch {
        /// Search observation; never executable model content.
        result: lingxi_llm_client::protocol::WebSearchResult,
    },
    /// Response start snapshot.
    MessageStart {
        /// Response metadata snapshot.
        response: Box<HistoryResponse>,
    },
    /// Content block start snapshot.
    ContentBlockStart {
        /// Block index in the response content list.
        index: u32,
        /// Content block snapshot at start.
        content_block: ContentBlock,
    },
    /// Incremental content block delta.
    ContentBlockDelta {
        /// Block index in the response content list.
        index: u32,
        /// Incremental delta payload.
        delta: HistoryContentDelta,
    },
    /// Content block end marker.
    ContentBlockStop {
        /// Block index in the response content list.
        index: u32,
    },
    /// Terminal response delta.
    MessageDelta {
        /// Terminal response delta payload.
        delta: HistoryMessageDelta,
        /// Normalized usage at the terminal boundary.
        usage: Option<ExecutionUsage>,
    },
    /// Terminal response stop marker.
    MessageStop,
    /// Final response event.
    Completed {
        /// Completed response.
        response: Box<HistoryResponse>,
    },
}

#[derive(Deserialize)]
struct RawHistoryEvent {
    #[serde(rename = "type")]
    kind: String,
    event: Option<lingxi_llm_client::providers::anthropic::fallback_response::ServerFallbackEvent>,
    profile: Option<String>,
    lane: Option<lingxi_llm_client::providers::anthropic::fallback_request::ServerLane>,
    model: Option<String>,
    response_id: Option<String>,
    estimate: Option<CostEstimate>,
    native_server_fallback: Option<bool>,
    summary_model: Option<String>,
    result: Option<lingxi_llm_client::protocol::WebSearchResult>,
    response: Option<Box<HistoryResponse>>,
    index: Option<u32>,
    content_block: Option<ContentBlock>,
    #[serde(default, deserialize_with = "deserialize_present_nullable_payload")]
    delta: Option<Option<JsonPayload>>,
    usage: Option<ExecutionUsage>,
}

impl<'de> Deserialize<'de> for HistoryEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Avoid the internally-tagged enum's intermediate `Content` buffer:
        // nested history payloads use source-backed SDK newtypes.
        let event = RawHistoryEvent::deserialize(deserializer)?;
        match event.kind.as_str() {
            "server_fallback" => Ok(Self::ServerFallback {
                event: Box::new(required_field(event.event, "event")?),
                profile: required_field(event.profile, "profile")?,
                lane: required_field(event.lane, "lane")?,
            }),
            "response_observed" => Ok(Self::ResponseObserved {
                model: required_field(event.model, "model")?,
                response_id: event.response_id,
            }),
            "cost_quote_observed" => Ok(Self::CostQuoteObserved {
                estimate: event.estimate,
                native_server_fallback: required_field(
                    event.native_server_fallback,
                    "native_server_fallback",
                )?,
                summary_model: event.summary_model,
            }),
            "web_search" => Ok(Self::WebSearch {
                result: required_field(event.result, "result")?,
            }),
            "message_start" => Ok(Self::MessageStart {
                response: required_field(event.response, "response")?,
            }),
            "content_block_start" => Ok(Self::ContentBlockStart {
                index: required_field(event.index, "index")?,
                content_block: required_field(event.content_block, "content_block")?,
            }),
            "content_block_delta" => Ok(Self::ContentBlockDelta {
                index: required_field(event.index, "index")?,
                delta: parse_required_source(event.delta, "delta")?,
            }),
            "content_block_stop" => Ok(Self::ContentBlockStop {
                index: required_field(event.index, "index")?,
            }),
            "message_delta" => Ok(Self::MessageDelta {
                delta: parse_required_source(event.delta, "delta")?,
                usage: event.usage,
            }),
            "message_stop" => Ok(Self::MessageStop),
            "completed" => Ok(Self::Completed {
                response: required_field(event.response, "response")?,
            }),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &[
                    "server_fallback",
                    "response_observed",
                    "cost_quote_observed",
                    "web_search",
                    "message_start",
                    "content_block_start",
                    "content_block_delta",
                    "content_block_stop",
                    "message_delta",
                    "message_stop",
                    "completed",
                ],
            )),
        }
    }
}

/// Incremental host presentation payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryContentDelta {
    /// Text delta payload.
    TextDelta {
        /// Partial text.
        text: String,
    },
    /// Text delta with a runtime-only exact JavaScript UTF-16 representation.
    /// The serialized host event keeps the ordinary display text; exact units
    /// flow directly to the stream accumulator and are persisted on the final
    /// `TextJsUtf16` block rather than as a Native JSONL field.
    TextJsUtf16Delta {
        text: String,
        #[serde(skip)]
        utf16_code_units: Vec<u16>,
    },
    /// Partial JSON payload.
    InputJsonDelta {
        /// Exact source units of the partial JSON text.
        #[serde(default)]
        partial_json: JsonPayloadFragment,
    },
    /// Reasoning/thinking delta payload.
    ThinkingDelta {
        /// Thinking text.
        thinking: String,
    },
    /// Reasoning signature delta payload.
    SignatureDelta {
        /// Signature fragment for the open reasoning block.
        signature: String,
    },
    /// Append a citation reference to a `text` block.
    ///
    /// Wire tag: `citations_delta`. Field name `citation` mirrors
    /// `api-client::HistoryContentDelta::CitationsDelta` exactly.
    CitationsDelta {
        /// Provider-specific citation payload (URL, title, range, etc.).
        citation: JsonPayload,
    },
    /// Replace an in-progress opaque native text block with its complete raw
    /// provider snapshot, without creating a second transcript block or
    /// emitting a second client-facing content-block start.
    ProviderContentSnapshot {
        /// Exact final provider-owned block value.
        value: JsonPayload,
    },
    /// Final citation-field presence for a completed Text block. Unlike
    /// `CitationsDelta`, this is a block snapshot and can represent an
    /// explicitly null or empty field as well as citations that arrived late.
    TextCitations {
        /// Exact Text citation field presence; the inner option preserves null.
        #[serde(
            default,
            deserialize_with = "deserialize_present_nullable_payload",
            skip_serializing_if = "Option::is_none"
        )]
        citations: Option<Option<JsonPayload>>,
    },
    /// Append text to a `connector_text` block.
    ///
    /// Wire tag: `connector_text_delta`. Field name `connector_text` mirrors
    /// `api-client::HistoryContentDelta::ConnectorTextDelta` exactly (NOT `text`).
    ConnectorTextDelta {
        /// Connector-text fragment to append.
        #[serde(default)]
        connector_text: JsonPayloadFragment,
    },
}

#[derive(Deserialize)]
struct RawHistoryContentDelta {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    utf16_code_units: Option<Vec<u16>>,
    partial_json: Option<JsonPayloadFragment>,
    thinking: Option<String>,
    signature: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present_nullable_payload")]
    citation: Option<Option<JsonPayload>>,
    #[serde(default, deserialize_with = "deserialize_present_nullable_payload")]
    value: Option<Option<JsonPayload>>,
    #[serde(default, deserialize_with = "deserialize_present_nullable_payload")]
    citations: Option<Option<JsonPayload>>,
    connector_text: Option<JsonPayloadFragment>,
}

impl<'de> Deserialize<'de> for HistoryContentDelta {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let delta = RawHistoryContentDelta::deserialize(deserializer)?;
        match delta.kind.as_str() {
            "text_delta" => Ok(Self::TextDelta {
                text: required_field(delta.text, "text")?,
            }),
            "text_js_utf16_delta" => {
                let text = required_field(delta.text, "text")?;
                Ok(Self::TextJsUtf16Delta {
                    utf16_code_units: delta
                        .utf16_code_units
                        .unwrap_or_else(|| text.encode_utf16().collect()),
                    text,
                })
            }
            "input_json_delta" => Ok(Self::InputJsonDelta {
                partial_json: delta.partial_json.unwrap_or_default(),
            }),
            "thinking_delta" => Ok(Self::ThinkingDelta {
                thinking: required_field(delta.thinking, "thinking")?,
            }),
            "signature_delta" => Ok(Self::SignatureDelta {
                signature: required_field(delta.signature, "signature")?,
            }),
            "citations_delta" => Ok(Self::CitationsDelta {
                citation: nullable_payload_or_null(Some(required_field(
                    delta.citation,
                    "citation",
                )?)),
            }),
            "provider_content_snapshot" => Ok(Self::ProviderContentSnapshot {
                value: nullable_payload_or_null(Some(required_field(delta.value, "value")?)),
            }),
            "text_citations" => Ok(Self::TextCitations {
                citations: delta.citations,
            }),
            "connector_text_delta" => Ok(Self::ConnectorTextDelta {
                connector_text: delta.connector_text.unwrap_or_default(),
            }),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &[
                    "text_delta",
                    "text_js_utf16_delta",
                    "input_json_delta",
                    "thinking_delta",
                    "signature_delta",
                    "citations_delta",
                    "provider_content_snapshot",
                    "text_citations",
                    "connector_text_delta",
                ],
            )),
        }
    }
}

/// Refusal `stop_details` — the Anthropic response message's
/// `stop_details: {category, explanation}` (present on `stop_reason: "refusal"`
/// responses). Drives the terminal refusal message's cyber/bio category variant
/// (claude-code `U2e`: `t.category`/`t.explanation`). Both the non-streaming
/// body and the streaming `message_delta.delta` carry it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HistoryStopDetails {
    /// `cyber` / `bio` / … — the refusal category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Free-text explanation (may embed a `https://claude.com/form/…` exemption URL).
    #[serde(
        default,
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub explanation: Option<Utf16JsonProjection>,
}

/// Host terminal presentation payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryMessageDelta {
    /// Optional terminal stop reason.
    pub stop_reason: Option<String>,
    /// Optional refusal `stop_details` (the streaming `message_delta.delta`
    /// carries it on a refusal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<HistoryStopDetails>,
    /// Exact provider resumability checkpoint received on the terminal stream
    /// delta. A present JSON null is retained as `Some(null)`.
    #[serde(
        default,
        deserialize_with = "deserialize_present_payload",
        skip_serializing_if = "Option::is_none"
    )]
    pub resumable: Option<JsonPayload>,
}

/// One system-prompt block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemBlock {
    /// Exact system-text source. It must be a JSON root string.
    #[serde(
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize",
        deserialize_with = "deserialize_system_text"
    )]
    pub text: Utf16JsonProjection,
    /// Optional prompt-cache breakpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl SystemBlock {
    /// Create a plain system block without a cache breakpoint.
    #[must_use]
    pub fn text(text: impl Into<Utf16JsonProjection>) -> Self {
        let text = text.into();
        assert!(
            text.string_units("").is_some(),
            "system block text must be a root-string source projection"
        );
        Self {
            text,
            cache_control: None,
        }
    }
}

/// Cache-control scope, mirroring claude-code's `CacheScope`
/// (`services/api/claude.ts` `getCacheControl`).
///
/// Only `Global` is serialized on the wire — `getCacheControl` emits the
/// `scope` key solely when `scope === 'global'` (the org default carries no
/// `scope` key). `Org` is therefore represented by the absence of a scope on a
/// plain [`CacheControl::Ephemeral`] breakpoint; this enum exists only to carry
/// the 1P `global` boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheScope {
    /// First-party global cache scope — emits `"scope":"global"`.
    Global,
}

/// Prompt-cache control marker.
///
/// `Ephemeral` is the org-default breakpoint (`getCacheControl({})` →
/// `{"type":"ephemeral"}`). `EphemeralScoped` carries the optional `scope` /
/// 1h-`ttl` fields the 1P global-cache path emits
/// (`getCacheControl({scope, querySource})` →
/// `{"type":"ephemeral", ttl?:'1h', scope?:'global'}`). The plain unit form is
/// kept so the common (org) construction/match sites stay a unit variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheControl {
    /// Anthropic ephemeral cache breakpoint, org default (no scope, no ttl).
    Ephemeral,
    /// Ephemeral breakpoint carrying optional 1P `scope` and/or 1h `ttl`.
    EphemeralScoped {
        /// `Some(Global)` emits `"scope":"global"`; `None` omits the key
        /// (org default).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<CacheScope>,
        /// When `true`, emits `"ttl":"1h"` (claude-code `should1hCacheTTL`).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        ttl_1h: bool,
    },
}

/// Application history message converted once at the SDK input boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Host API-system controls; consumed once at the provider input boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_output_config: Option<lingxi_core::types::ApiSystemOutputConfig>,
    /// Message role.
    pub role: String,
    /// Ordered content blocks.
    pub content: Vec<ContentBlock>,
}

/// Durable history content and display payload; never provider model input.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Provider-native replay data. Never executed as an application tool.
    ProviderContent {
        /// Communication protocol which owns this payload.
        protocol: String,
        /// Unmodified provider content, including signed/encrypted reasoning.
        value: JsonPayload,
    },
    /// Text block.
    Text {
        /// Text payload.
        text: String,
        /// Exact provider field presence; the inner option preserves JSON null.
        #[serde(
            default,
            deserialize_with = "deserialize_present_nullable_payload",
            skip_serializing_if = "Option::is_none"
        )]
        citations: Option<Option<JsonPayload>>,
        /// Optional prompt-cache breakpoint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Display-safe text carrying an exact JS UTF-16 wire image.
    ///
    /// `text` remains valid UTF-8 for display/debug paths. When this block is
    /// serialized for a Claude-family provider request, `utf16_code_units`
    /// drives the exact JSON string bytes so lone surrogates survive as
    /// `\\udxxx` escapes.
    TextJsUtf16 {
        /// Display-safe text payload.
        text: String,
        /// Exact provider-visible UTF-16 code units.
        utf16_code_units: Vec<u16>,
        /// Exact provider field presence; the inner option preserves JSON null.
        #[serde(
            default,
            deserialize_with = "deserialize_present_nullable_payload",
            skip_serializing_if = "Option::is_none"
        )]
        citations: Option<Option<JsonPayload>>,
        /// Optional prompt-cache breakpoint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Image block.
    Image {
        /// Image media type.
        media_type: String,
        /// Raw image bytes.
        bytes: Vec<u8>,
    },
    /// Image referenced by URL (Anthropic url image source).
    ImageUrl {
        /// Image URL.
        url: String,
    },
    /// Document block.
    Document {
        /// Document media type.
        media_type: String,
        /// Raw document bytes.
        bytes: Vec<u8>,
    },
    /// Tool-call block.
    ToolCall {
        /// Tool call id.
        id: String,
        /// Tool name.
        name: String,
        /// Tool input JSON.
        input: JsonPayload,
    },
    /// Tool-result block.
    ToolResult {
        /// Tool call id this result answers.
        tool_call_id: String,
        /// Tool result JSON.
        output: JsonPayload,
        /// Whether the result reports a tool failure; `None` preserves omission.
        #[serde(
            default,
            deserialize_with = "deserialize_present_bool",
            skip_serializing_if = "Option::is_none"
        )]
        is_error: Option<bool>,
        /// Optional prompt-cache breakpoint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        /// 1P experimental cache-editing tag — the `cache_reference` set on a
        /// `tool_result` that falls within the cached prefix when the cache-editing
        /// gate is armed (claude-code `addCacheBreakpoints`, claude.ts:3164-3207).
        /// `None` (the default 3P/Anthropic path) omits the key entirely, so wire
        /// bytes are unchanged. Set to the answered `tool_use_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_reference: Option<String>,
    },
    /// Reasoning block.
    Reasoning {
        /// Reasoning text or provider-supplied summary.
        text: String,
        /// Provider integrity signature required to round-trip the block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Opaque redacted-reasoning block that must round-trip unmodified.
    RedactedThinking {
        /// Provider-opaque payload.
        data: String,
    },
    /// Anthropic server-side tool invocation (e.g. advisor / `web_search`).
    ///
    /// Wire tag: `server_tool_use`. Mirrors `api-client::ContentBlockApi::ServerToolUse`
    /// exactly. Encode: round-trips back to `server_tool_use` (tool-use round-trip).
    ServerToolUse {
        /// Server-issued tool-use identifier.
        id: String,
        /// Name of the server tool being invoked.
        name: String,
        /// Tool input arguments (provider-specific JSON shape).
        #[serde(default)]
        input: JsonPayload,
    },
    /// Anthropic Connector-Text block.
    ///
    /// Wire tag: `connector_text`. Field name `connector_text` mirrors
    /// `api-client::ContentBlockApi::ConnectorText` exactly (NOT `text`).
    /// api-client decodes this unconditionally (no cfg gate) → llm-runtime
    /// also decodes it unconditionally. Encode: rejected with a message (no
    /// upstream use-case yet — mirrors Document handling).
    ConnectorText {
        /// Connector-emitted text payload.
        #[serde(default)]
        connector_text: String,
        /// Optional provider integrity signature.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Advisor tool result mirrored from the server.
    ///
    /// Wire tag: `advisor_tool_result`. Mirrors
    /// `api-client::ContentBlockApi::AdvisorToolResult` exactly.
    /// Encode: rejected with a message (no upstream use-case yet).
    AdvisorToolResult {
        /// Identifier of the originating `server_tool_use` block.
        tool_use_id: String,
        /// Tool result content (provider-specific JSON shape).
        #[serde(default)]
        content: JsonPayload,
        /// Whether the tool reported an error.
        #[serde(default)]
        is_error: bool,
    },
    /// 1P experimental cache-editing directive block.
    ///
    /// Wire tag: `cache_edits`. Mirrors claude-code's `CachedMCEditsBlock`
    /// (`services/api/claude.ts:3052-3055`):
    /// `{"type":"cache_edits","edits":[{"type":"delete","cache_reference":...}]}`.
    /// Inserted into a user message's content (after the last `tool_result`) only
    /// when the Anthropic-1P cache-editing gate is armed; it never appears on the
    /// default 3P path. Anthropic-only — other codecs drop it.
    CacheEdits {
        /// Ordered cache-editing operations (currently `delete` only).
        edits: Vec<CacheEdit>,
    },
}

#[derive(Deserialize)]
struct RawContentBlock {
    #[serde(rename = "type")]
    kind: String,
    protocol: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present_nullable_payload")]
    value: Option<Option<JsonPayload>>,
    text: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present_nullable_payload")]
    citations: Option<Option<JsonPayload>>,
    cache_control: Option<CacheControl>,
    utf16_code_units: Option<Vec<u16>>,
    media_type: Option<String>,
    bytes: Option<Vec<u8>>,
    url: Option<String>,
    id: Option<String>,
    name: Option<String>,
    tool_use_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present_nullable_payload")]
    input: Option<Option<JsonPayload>>,
    tool_call_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present_nullable_payload")]
    output: Option<Option<JsonPayload>>,
    #[serde(default, deserialize_with = "deserialize_present_bool")]
    is_error: Option<bool>,
    cache_reference: Option<String>,
    signature: Option<String>,
    data: Option<String>,
    connector_text: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present_nullable_payload")]
    content: Option<Option<JsonPayload>>,
    edits: Option<Vec<CacheEdit>>,
}

fn required_field<T, E>(value: Option<T>, name: &'static str) -> Result<T, E>
where
    E: serde::de::Error,
{
    value.ok_or_else(|| E::missing_field(name))
}

fn parse_required_source<T, E>(
    value: Option<Option<JsonPayload>>,
    name: &'static str,
) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    let source = required_field(value, name)?
        .ok_or_else(|| E::custom(format!("field `{name}` must not be null")))?;
    serde_json::from_str(source.as_json_source()).map_err(E::custom)
}

fn nullable_payload_or_null(value: Option<Option<JsonPayload>>) -> JsonPayload {
    value
        .flatten()
        .unwrap_or_else(|| JsonPayload::parse("null").expect("null is valid JSON"))
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Internally tagged Serde enums buffer their contents before invoking
        // a variant deserializer. That intermediary cannot represent SDK
        // JsonPayload's source-backed newtype. Decode a flat struct directly so
        // every payload field sees the original deserializer.
        let block = RawContentBlock::deserialize(deserializer)?;

        match block.kind.as_str() {
            "provider_content" => Ok(Self::ProviderContent {
                protocol: required_field(block.protocol, "protocol")?,
                value: nullable_payload_or_null(Some(required_field(block.value, "value")?)),
            }),
            "text" => Ok(Self::Text {
                text: required_field(block.text, "text")?,
                citations: block.citations,
                cache_control: block.cache_control,
            }),
            "text_js_utf16" => {
                let text = required_field(block.text, "text")?;
                let utf16_code_units = block
                    .utf16_code_units
                    .unwrap_or_else(|| text.encode_utf16().collect());
                Ok(Self::TextJsUtf16 {
                    text,
                    utf16_code_units,
                    citations: block.citations,
                    cache_control: block.cache_control,
                })
            }
            "image" => Ok(Self::Image {
                media_type: required_field(block.media_type, "media_type")?,
                bytes: required_field(block.bytes, "bytes")?,
            }),
            "image_url" => Ok(Self::ImageUrl {
                url: required_field(block.url, "url")?,
            }),
            "document" => Ok(Self::Document {
                media_type: required_field(block.media_type, "media_type")?,
                bytes: required_field(block.bytes, "bytes")?,
            }),
            "tool_call" => Ok(Self::ToolCall {
                id: required_field(block.id, "id")?,
                name: required_field(block.name, "name")?,
                input: required_field(block.input, "input")
                    .map(|value| nullable_payload_or_null(Some(value)))?,
            }),
            "tool_result" => Ok(Self::ToolResult {
                tool_call_id: required_field(block.tool_call_id, "tool_call_id")?,
                output: required_field(block.output, "output")
                    .map(|value| nullable_payload_or_null(Some(value)))?,
                is_error: block.is_error,
                cache_control: block.cache_control,
                cache_reference: block.cache_reference,
            }),
            "reasoning" => Ok(Self::Reasoning {
                text: required_field(block.text, "text")?,
                signature: block.signature,
            }),
            "redacted_thinking" => Ok(Self::RedactedThinking {
                data: required_field(block.data, "data")?,
            }),
            "server_tool_use" => Ok(Self::ServerToolUse {
                id: required_field(block.id, "id")?,
                name: required_field(block.name, "name")?,
                input: nullable_payload_or_null(block.input),
            }),
            "connector_text" => Ok(Self::ConnectorText {
                connector_text: block.connector_text.unwrap_or_default(),
                signature: block.signature,
            }),
            "advisor_tool_result" => Ok(Self::AdvisorToolResult {
                tool_use_id: required_field(block.tool_use_id, "tool_use_id")?,
                content: nullable_payload_or_null(block.content),
                is_error: block.is_error.unwrap_or_default(),
            }),
            "cache_edits" => Ok(Self::CacheEdits {
                edits: required_field(block.edits, "edits")?,
            }),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &[
                    "provider_content",
                    "text",
                    "text_js_utf16",
                    "image",
                    "image_url",
                    "document",
                    "tool_call",
                    "tool_result",
                    "reasoning",
                    "redacted_thinking",
                    "server_tool_use",
                    "connector_text",
                    "advisor_tool_result",
                    "cache_edits",
                ],
            )),
        }
    }
}

impl ContentBlock {
    /// Text a user-facing renderer may display from this block.
    ///
    /// Opaque Anthropic text remains one raw source block for replay, while its
    /// provider-owned `text` field remains available to output surfaces.
    #[must_use]
    pub fn visible_text(&self) -> Option<Cow<'_, str>> {
        match self {
            Self::Text { text, .. } | Self::TextJsUtf16 { text, .. } => Some(Cow::Borrowed(text)),
            Self::ProviderContent { protocol, value }
                if protocol == "anthropic_messages"
                    && value.string_at("/type").as_deref() == Some("text") =>
            {
                value.string_at("/text").map(Cow::Owned)
            }
            _ => None,
        }
    }
}

/// A single cache-editing operation inside a [`ContentBlock::CacheEdits`] block.
///
/// Mirrors claude-code's `{type:'delete', cache_reference: string}` edit
/// (`services/api/claude.ts:3054`). Only the `delete` op exists today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CacheEdit {
    /// Delete a previously-cached `tool_result` by its `cache_reference`.
    Delete {
        /// The `cache_reference` (the answered `tool_use_id`) to evict.
        cache_reference: String,
    },
}

/// Application tool registration converted to SDK ToolSpec at the input edge.
///
/// Most tools are caller-defined (`tool_type == None`): the provider receives
/// `name`/`description`/`input_schema`. A hosted tool (Anthropic computer use,
/// web search, code execution) sets `tool_type` to the provider wire type
/// (e.g. `computer_use_20250124`); the provider then passes it through as a
/// typed hosted tool and attaches the matching beta header. `extra` carries
/// hosted-tool-specific wire fields (e.g. `display_width_px`) merged verbatim
/// into the encoded tool object. Ported 1:1 from codex `liter-llm`
/// hosted-tool passthrough (`provider/anthropic.rs`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolDeclaration {
    /// Exact schema at the host tool-admission boundary, never a wire field.
    #[serde(skip)]
    pub input_schema_projection: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Tool name.
    pub name: String,
    /// Source-backed JSON string, retaining exact JavaScript UTF-16 units.
    pub description: lingxi_llm_client::protocol::JsonPayload,
    /// Source-backed JSON schema, retaining ordering and exact string units.
    pub input_schema: lingxi_llm_client::protocol::JsonPayload,
    /// Hosted-tool wire type, when this is a provider-hosted tool (e.g.
    /// `computer_use_20250124`). `None` ⇒ caller-defined tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_type: Option<String>,
    /// Extra hosted-tool wire fields merged verbatim into the encoded tool
    /// object (e.g. `display_width_px`, `display_height_px`). Empty for
    /// caller-defined tools.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
    /// Structured-output strict mode (claude `tool.strict`): when `true` and the
    /// model supports it, the Anthropic codec converts `input_schema` to its
    /// strict form ([`lingxi_llm_client::providers::anthropic::strict_schema`]) and sends `strict: true`. Default
    /// `false` for every caller-defined tool, so the encoded wire bytes are
    /// unchanged until a tool opts in.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strict: bool,
    /// Anthropic dynamic-tool-loading marker. Only discovered deferred tools
    /// carry this; unsupported codecs ignore it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub defer_loading: bool,
}

/// Tool-choice policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Provider chooses whether to call tools.
    Auto,
    /// No tool calls are allowed.
    None,
    /// A tool call is required.
    Required,
    /// A specific tool must be called.
    Tool {
        /// Required tool name.
        name: String,
    },
}

/// Structured-output request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Provider-native JSON mode.
    JsonObject,
    /// Provider-native JSON schema mode.
    JsonSchema {
        /// JSON schema value.
        schema: Value,
    },
}

impl HistoryResponse {
    /// Project a canonical SDK response into the application's durable history
    /// and presentation shape. Native replay metadata is encoded only here.
    pub fn from_model(
        response: lingxi_llm_client::protocol::ChatResponse,
        protocol: lingxi_llm_client::protocol::ProtocolFamily,
    ) -> Result<Self, crate::LlmError> {
        crate::history_projection::project_model_response(
            response,
            protocol,
            JsonPayload::default(),
            None,
        )
    }
}

#[cfg(test)]
mod wire_presence_tests {
    use super::*;
    use serde_json::json;

    fn payload(value: Value) -> JsonPayload {
        JsonPayload::from_generated_value(&value).expect("test payload is valid JSON")
    }

    #[test]
    fn system_block_round_trips_exact_utf16_text_as_a_json_string() {
        let source = Utf16JsonProjection::parse(r#""side\ud800Ω""#).unwrap();
        let block = SystemBlock::text(source.clone());
        let encoded = serde_json::to_string(&block).unwrap();
        let decoded: SystemBlock = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.text.string_units(""), source.string_units(""));
        assert!(serde_json::from_str::<SystemBlock>(r#"{"text":{}}"#).is_err());
    }

    #[test]
    fn provider_json_and_stream_fragments_keep_exact_source_units() {
        let provider_source = r#"{"type":"future_block","payload":"\ud800"}"#;
        let provider_content = ContentBlock::ProviderContent {
            protocol: "anthropic_messages".into(),
            value: JsonPayload::parse(provider_source).unwrap(),
        };
        let encoded = serde_json::to_string(&provider_content).unwrap();
        let decoded: ContentBlock = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            decoded,
            ContentBlock::ProviderContent { value, .. }
                if value.as_json_source() == provider_source
        ));

        let fragment_units = vec![
            u16::from(b'{'),
            u16::from(b'"'),
            u16::from(b'x'),
            u16::from(b'"'),
            u16::from(b':'),
            u16::from(b'"'),
            0xd800,
            u16::from(b'"'),
            u16::from(b'}'),
        ];
        let event = HistoryContentDelta::InputJsonDelta {
            partial_json: JsonPayloadFragment::from_utf16_code_units(fragment_units.clone()),
        };
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: HistoryContentDelta = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            decoded,
            HistoryContentDelta::InputJsonDelta { partial_json }
                if partial_json.utf16_code_units() == fragment_units.as_slice()
        ));
    }

    #[test]
    fn source_payloads_deserialize_through_tagged_history_events() {
        let provider_source = r#"{"type":"future_block","payload":"\ud800"}"#;
        let block_source = format!(
            r#"{{"type":"provider_content","protocol":"anthropic_messages","value":{provider_source}}}"#
        );
        let block: ContentBlock = serde_json::from_str(&block_source).unwrap();
        assert!(matches!(
            block,
            ContentBlock::ProviderContent { value, .. }
                if value.as_json_source() == provider_source
        ));

        let event_source =
            format!(r#"{{"type":"content_block_start","index":0,"content_block":{block_source}}}"#);
        let event: HistoryEvent = serde_json::from_str(&event_source).unwrap();
        assert!(matches!(
            event,
            HistoryEvent::ContentBlockStart {
                content_block: ContentBlock::ProviderContent { value, .. },
                ..
            } if value.as_json_source() == provider_source
        ));

        let delta: HistoryContentDelta =
            serde_json::from_str(r#"{"type":"citations_delta","citation":{"opaque":"\ud800"}}"#)
                .unwrap();
        assert!(matches!(
            delta,
            HistoryContentDelta::CitationsDelta { citation }
                if citation.as_json_source() == r#"{"opaque":"\ud800"}"#
        ));
    }

    #[test]
    fn resumable_source_distinguishes_missing_from_present_null() {
        let missing: HistoryMessageDelta = serde_json::from_str(r#"{"stop_reason":null}"#).unwrap();
        assert!(missing.resumable.is_none());

        let explicit_null: HistoryMessageDelta =
            serde_json::from_str(r#"{"stop_reason":null,"resumable":null}"#).unwrap();
        assert_eq!(
            explicit_null
                .resumable
                .as_ref()
                .map(JsonPayload::as_json_source),
            Some("null")
        );
        assert_eq!(
            serde_json::to_string(&explicit_null).unwrap(),
            r#"{"stop_reason":null,"resumable":null}"#
        );
    }

    #[test]
    fn persisted_history_preserves_citations_and_optional_error_presence() {
        for (citations, expected) in [
            (None, None),
            (Some(None), Some(Value::Null)),
            (Some(Some(payload(json!([])))), Some(json!([]))),
        ] {
            let text = ContentBlock::Text {
                text: "answer".into(),
                citations,
                cache_control: None,
            };
            let serialized = serde_json::to_value(&text).unwrap();
            assert_eq!(serialized.get("citations"), expected.as_ref());
            let encoded = serde_json::to_string(&text).unwrap();
            let decoded: ContentBlock = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, text);
        }

        for is_error in [None, Some(false), Some(true)] {
            let result = ContentBlock::ToolResult {
                tool_call_id: "toolu_1".into(),
                output: JsonPayload::string("ok"),
                is_error,
                cache_control: None,
                cache_reference: None,
            };
            let serialized = serde_json::to_value(&result).unwrap();
            assert_eq!(
                serialized.get("is_error"),
                is_error.map(Value::Bool).as_ref()
            );
            let encoded = serde_json::to_string(&result).unwrap();
            let decoded: ContentBlock = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, result);
        }

        let invalid_null = json!({
            "type":"tool_result",
            "tool_call_id":"toolu_1",
            "output":"ok",
            "is_error":null
        });
        assert!(serde_json::from_str::<ContentBlock>(&invalid_null.to_string()).is_err());
    }

    #[test]
    fn text_citation_snapshot_events_preserve_absent_null_and_array() {
        for (citations, expected) in [
            (None, None),
            (Some(None), Some(Value::Null)),
            (Some(Some(payload(json!([])))), Some(json!([]))),
            (
                Some(Some(payload(json!([{"type":"location"}])))),
                Some(json!([{"type":"location"}])),
            ),
        ] {
            let event = HistoryContentDelta::TextCitations { citations };
            let serialized = serde_json::to_value(&event).unwrap();
            assert_eq!(serialized.get("citations"), expected.as_ref());
            assert_eq!(
                serde_json::from_str::<HistoryContentDelta>(
                    &serde_json::to_string(&event).unwrap(),
                )
                .unwrap(),
                event
            );
        }
    }
}
