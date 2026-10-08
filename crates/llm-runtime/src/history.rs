//! Host history and presentation DTOs. Provider execution uses SDK protocol types.
//! These serialized shapes remain stable for transcript and UI consumers.

use crate::{CostEstimate, ExecutionUsage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn deserialize_present_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    bool::deserialize(deserializer).map(Some)
}

fn deserialize_present_nullable_value<'de, D>(
    deserializer: D,
) -> Result<Option<Option<Value>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Value>::deserialize(deserializer).map(Some)
}

/// Reserved metadata key for a host-projected Anthropic fallback iteration quote.
pub const SERVER_FALLBACK_COST_QUOTE_KEY: &str = "server_fallback_cost_quote";

/// Read the host-owned fallback quote envelope from response or stream usage
/// metadata. Stream usage nests provider metadata under `stream`.
pub fn server_fallback_cost_quote(metadata: &Value) -> Option<&Value> {
    metadata
        .get("stream")
        .and_then(|stream| stream.get("llm_client"))
        .and_then(|namespace| namespace.get(SERVER_FALLBACK_COST_QUOTE_KEY))
        .or_else(|| {
            metadata
                .get("llm_client")?
                .get(SERVER_FALLBACK_COST_QUOTE_KEY)
        })
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
    /// Redacted provider metadata.
    #[serde(default)]
    pub provider_metadata: Value,
}
impl HistoryResponse {
    /// Host-authored observations only. Wire projection clears this reserved
    /// key before installing events admitted by the execution context.
    pub fn server_fallback_events(&self) -> Vec<HistoryServerFallback> {
        self.provider_metadata["llm_client"]["server_fallback_events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|value| serde_json::from_value(value.clone()).ok())
            .collect()
    }

    /// Native per-iteration quote fact when server fallback billing was active.
    /// Presence distinguishes an incomplete native quote from an ordinary call
    /// with no server-fallback iteration branch.
    pub fn server_fallback_cost_quote(&self) -> Option<&Value> {
        server_fallback_cost_quote(&self.provider_metadata)
    }
}

/// Host history and presentation event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// Incremental host presentation payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        /// Partial JSON text.
        partial_json: String,
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
        citation: Value,
    },
    /// Replace an in-progress opaque native text block with its complete raw
    /// provider snapshot, without creating a second transcript block or
    /// emitting a second client-facing content-block start.
    ProviderContentSnapshot {
        /// Exact final provider-owned block value.
        value: Value,
    },
    /// Final citation-field presence for a completed Text block. Unlike
    /// `CitationsDelta`, this is a block snapshot and can represent an
    /// explicitly null or empty field as well as citations that arrived late.
    TextCitations {
        /// Exact Text citation field presence; the inner option preserves null.
        #[serde(
            default,
            deserialize_with = "deserialize_present_nullable_value",
            skip_serializing_if = "Option::is_none"
        )]
        citations: Option<Option<Value>>,
    },
    /// Append text to a `connector_text` block.
    ///
    /// Wire tag: `connector_text_delta`. Field name `connector_text` mirrors
    /// `api-client::HistoryContentDelta::ConnectorTextDelta` exactly (NOT `text`).
    ConnectorTextDelta {
        /// Connector-text fragment to append.
        #[serde(default)]
        connector_text: String,
    },
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
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
}

/// One system-prompt block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemBlock {
    /// System text.
    pub text: String,
    /// Optional prompt-cache breakpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl SystemBlock {
    /// Create a plain system block without a cache breakpoint.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
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
    /// Message role.
    pub role: String,
    /// Ordered content blocks.
    pub content: Vec<ContentBlock>,
}

/// Durable history content and display payload; never provider model input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Provider-native replay data. Never executed as an application tool.
    ProviderContent {
        /// Communication protocol which owns this payload.
        protocol: String,
        /// Unmodified provider content, including signed/encrypted reasoning.
        value: Value,
    },
    /// Text block.
    Text {
        /// Text payload.
        text: String,
        /// Exact provider field presence; the inner option preserves JSON null.
        #[serde(
            default,
            deserialize_with = "deserialize_present_nullable_value",
            skip_serializing_if = "Option::is_none"
        )]
        citations: Option<Option<Value>>,
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
            deserialize_with = "deserialize_present_nullable_value",
            skip_serializing_if = "Option::is_none"
        )]
        citations: Option<Option<Value>>,
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
        input: Value,
    },
    /// Tool-result block.
    ToolResult {
        /// Tool call id this result answers.
        tool_call_id: String,
        /// Tool result JSON.
        output: Value,
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
        input: Value,
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
        content: Value,
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

impl ContentBlock {
    /// Text a user-facing renderer may display from this block.
    ///
    /// Opaque Anthropic text remains one raw source block for replay, while its
    /// provider-owned `text` field remains available to output surfaces.
    #[must_use]
    pub fn visible_text(&self) -> Option<&str> {
        match self {
            Self::Text { text, .. } | Self::TextJsUtf16 { text, .. } => Some(text),
            Self::ProviderContent { protocol, value }
                if protocol == "anthropic_messages" && value["type"] == "text" =>
            {
                value.get("text").and_then(Value::as_str)
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
    /// Tool name.
    pub name: String,
    /// Tool description.
    pub description: String,
    /// JSON schema for tool input.
    pub input_schema: Value,
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
        crate::history_projection::project_model_response(response, protocol, Value::Null, None)
    }
}

#[cfg(test)]
mod wire_presence_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn persisted_history_preserves_citations_and_optional_error_presence() {
        for (citations, expected) in [
            (None, None),
            (Some(None), Some(Value::Null)),
            (Some(Some(json!([]))), Some(json!([]))),
        ] {
            let text = ContentBlock::Text {
                text: "answer".into(),
                citations,
                cache_control: None,
            };
            let serialized = serde_json::to_value(&text).unwrap();
            assert_eq!(serialized.get("citations"), expected.as_ref());
            let decoded: ContentBlock = serde_json::from_value(serialized).unwrap();
            assert_eq!(decoded, text);
        }

        for is_error in [None, Some(false), Some(true)] {
            let result = ContentBlock::ToolResult {
                tool_call_id: "toolu_1".into(),
                output: json!("ok"),
                is_error,
                cache_control: None,
                cache_reference: None,
            };
            let serialized = serde_json::to_value(&result).unwrap();
            assert_eq!(
                serialized.get("is_error"),
                is_error.map(Value::Bool).as_ref()
            );
            let decoded: ContentBlock = serde_json::from_value(serialized).unwrap();
            assert_eq!(decoded, result);
        }

        let invalid_null = json!({
            "type":"tool_result",
            "tool_call_id":"toolu_1",
            "output":"ok",
            "is_error":null
        });
        assert!(serde_json::from_value::<ContentBlock>(invalid_null).is_err());
    }

    #[test]
    fn text_citation_snapshot_events_preserve_absent_null_and_array() {
        for (citations, expected) in [
            (None, None),
            (Some(None), Some(Value::Null)),
            (Some(Some(json!([]))), Some(json!([]))),
            (
                Some(Some(json!([{"type":"location"}]))),
                Some(json!([{"type":"location"}])),
            ),
        ] {
            let event = HistoryContentDelta::TextCitations { citations };
            let serialized = serde_json::to_value(&event).unwrap();
            assert_eq!(serialized.get("citations"), expected.as_ref());
            assert_eq!(
                serde_json::from_value::<HistoryContentDelta>(serialized).unwrap(),
                event
            );
        }
    }
}
