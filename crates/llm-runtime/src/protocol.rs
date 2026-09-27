//! Canonical protocol types and codec traits.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Capabilities, CostEstimate, LlmError, Usage};

/// Which on-wire streaming framing is used for this provider request.
///
/// Set by codecs before the request is sent; the bridge uses it to choose
/// between SSE splitting and raw binary framing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamFraming {
    /// Server-Sent Events (text, `data: …\n\n` boundaries).  This is the
    /// default for Anthropic, `OpenAI`, Gemini, and Azure.
    #[default]
    Sse,
    /// AWS binary event-stream framing used by Amazon Bedrock streaming
    /// responses.  The bridge passes raw byte chunks directly to the codec's
    /// [`crate::StreamDecoder`] without SSE parsing.
    AwsEventStream,
}

/// Which transport should be used when opening a streaming provider request.
///
/// This is intentionally separate from [`StreamFraming`]: the framing describes
/// how bytes/messages become provider stream frames, while the transport chooses
/// the connection type used to receive those frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStreamTransport {
    /// HTTP streaming response (SSE or raw bytes, according to
    /// [`StreamFraming`]).
    #[default]
    Http,
    /// `OpenAI` Responses API over WebSocket. Only valid for
    /// [`crate::ProtocolFamily::OpenAiResponses`] streaming requests.
    ResponsesWebSocket,
}

/// Canonical request passed to provider protocols.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    /// Host-only registered logical call. JSON cannot create or forward this
    /// authority, and it is never included in a provider request body.
    #[serde(skip)]
    pub model_attempt: Option<platform_api::ModelAttemptContext>,
    /// Requested model id or alias.
    pub model: String,
    /// Optional provider profile that disambiguates `model` when the same id is
    /// offered by multiple providers. `None` = resolve across all providers
    /// (ambiguous ids error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Conversation messages, oldest first.
    pub messages: Vec<Message>,
    /// Historical identities used by signature recovery; never provider wire.
    #[serde(skip)]
    pub thinking_source_message_ids: Vec<::protocol::MessageId>,
    /// Query ownership for retries and lazy streams; never provider wire.
    #[serde(skip)]
    pub thinking_recovery_scope: Option<crate::thinking_scope::ThinkingRecoveryScope>,
    /// System prompt blocks, in order (empty = no system prompt).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<SystemBlock>,
    /// Tool declarations available to the model.
    pub tools: Vec<ToolDeclaration>,
    /// Optional tool-choice policy.
    pub tool_choice: Option<ToolChoice>,
    /// Optional structured-output request.
    pub response_format: Option<ResponseFormat>,
    /// Whether caller requested streaming.
    pub stream: bool,
    /// Optional maximum output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Optional sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Optional nucleus-sampling parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Sequences that end generation early.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    /// Optional reasoning/thinking budget request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    /// Optional per-request thinking-effort hint (claude-code `output_config.effort`):
    /// a level (`"low"`/`"medium"`/`"high"`/`"xhigh"`/`"max"`) or an integer
    /// budget. The Anthropic codec emits it as `output_config: { effort }` and
    /// the caller adds the `effort-2025-11-24` beta header. `None` ⇒ omitted
    /// (zero effect on every existing request).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<serde_json::Value>,
    /// Optional request speed tier (claude-code fast mode). `Some("fast")`
    /// emits the `speed` body key which lights the fast-mode beta (read back by
    /// `service::beta_context`); `None` (the default) keeps every existing
    /// request byte-identical (the field is skipped when serializing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    /// Optional context-hint offer (claude-code `context_hint`): tells the
    /// server how many tokens the client could free if asked. The Anthropic
    /// codec emits it as the top-level `context_hint` body key and
    /// `service::beta_context` reads it back to add the
    /// `context-hint-2026-04-09` beta. `None` (the default) keeps every
    /// existing request byte-identical — the field is skipped when serializing.
    ///
    /// Built by `compaction::context_hint::ContextHintController`, which is
    /// inert unless its gate is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_hint: Option<serde_json::Value>,
    /// Optional request metadata. Emitted by the Anthropic codec as the
    /// `metadata` object (claude-code `claude.ts:1699-1728` always sends it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RequestMetadata>,
    /// Internal response-accounting hint. This is never serialized to a
    /// provider request; side-query adapters use it to capture per-call retries.
    #[serde(skip)]
    pub capture_retry_count: bool,
    /// Internal side-query purpose for telemetry; never sent to providers.
    #[serde(skip)]
    pub query_source: Option<String>,
    /// `OpenAI` Responses API request controls that do not have provider-neutral
    /// equivalents.
    #[serde(
        default,
        skip_serializing_if = "OpenAiResponsesRequestOptions::is_default"
    )]
    pub openai_responses: OpenAiResponsesRequestOptions,
}

/// Request metadata carried in the Anthropic `metadata` request field.
///
/// Mirrors claude-code's `metadata: { user_id }` (`services/api/claude.ts:503-525`,
/// `1699-1728`). `user_id` is the JSON-stringified identity blob
/// (`{...CLAUDE_CODE_EXTRA_METADATA, device_id, account_uuid, session_id}`); the
/// caller composes the string, the codec emits it verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestMetadata {
    /// Opaque identity string sent as `metadata.user_id`.
    pub user_id: String,
}

/// `OpenAI` Responses API controls that mirror Codex core's request envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiResponsesRequestOptions {
    /// Whether the model may issue parallel tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Extra Responses `include` entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Optional service tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Optional prompt cache key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// Optional client metadata map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub client_metadata: BTreeMap<String, String>,
    /// Explicit Responses `store` override. When absent, the codec derives the
    /// Azure default from the base URL, matching Codex core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    /// Optional previous Responses id used by the WebSocket session path to
    /// send only newly-added input items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    /// Optional generation control. The WebSocket prewarm path sets
    /// `generate=false`; regular HTTP/SSE requests leave it unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate: Option<bool>,
}

impl OpenAiResponsesRequestOptions {
    /// Whether all controls are unset.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl LlmRequest {
    /// Create an empty request for a model.
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Self::default()
        }
    }

    /// Pin the provider profile used to resolve `model`.
    #[must_use]
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    /// Add one user text message.
    #[must_use]
    pub fn with_user_text(mut self, text: impl Into<String>) -> Self {
        self.messages.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: text.into(),
                cache_control: None,
            }],
        });
        self
    }

    /// Add an image block to the most recent user message, or start a new
    /// user message when the conversation does not end with one.
    #[must_use]
    pub fn with_image(mut self, media_type: impl Into<String>, bytes: Vec<u8>) -> Self {
        let block = ContentBlock::Image {
            media_type: media_type.into(),
            bytes,
        };

        match self.messages.last_mut() {
            Some(message) if message.role == "user" => message.content.push(block),
            _ => self.messages.push(Message {
                role: "user".to_string(),
                content: vec![block],
            }),
        }

        self
    }
}

/// Provider-neutral reasoning/thinking request.
///
/// Mirrors claude-code's `thinking` request field
/// (`services/api/claude.ts:1596-1630`): the Anthropic Messages API distinguishes
/// `{"type":"adaptive"}` (the model decides depth dynamically — the default for
/// adaptive-capable models) from `{"type":"enabled","budget_tokens":N}` (a fixed
/// thinking budget). The provider-neutral codecs that only consume a numeric
/// budget (Gemini, `OpenAI` Responses) map [`ReasoningConfig::Adaptive`] to a
/// sensible dynamic default — those providers never receive `Adaptive` in
/// practice (only the Anthropic/firstParty path emits it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum ReasoningConfig {
    /// Adaptive thinking — the model decides when and how much to think.
    /// Anthropic wire: `{"type":"adaptive"}`.
    Adaptive,
    /// Fixed thinking budget. Anthropic wire:
    /// `{"type":"enabled","budget_tokens":N}`.
    Enabled {
        /// Maximum tokens the model may spend on reasoning.
        budget_tokens: u32,
    },
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

/// Canonical chat message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Message role.
    pub role: String,
    /// Ordered content blocks.
    pub content: Vec<ContentBlock>,
}

/// Canonical content block.
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
        /// Whether the result reports a tool failure.
        #[serde(default)]
        is_error: bool,
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

/// Canonical non-streaming response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmResponse {
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
    pub stop_details: Option<StopDetails>,
    /// Normalized usage.
    pub usage: Usage,
    /// Optional per-call cost estimate.
    pub cost: Option<CostEstimate>,
    /// Redacted provider metadata.
    #[serde(default)]
    pub provider_metadata: Value,
}

/// Canonical streaming event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmEvent {
    /// Response start snapshot.
    MessageStart {
        /// Response metadata snapshot.
        response: Box<LlmResponse>,
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
        delta: ContentDelta,
    },
    /// Content block end marker.
    ContentBlockStop {
        /// Block index in the response content list.
        index: u32,
    },
    /// Terminal response delta.
    MessageDelta {
        /// Terminal response delta payload.
        delta: MessageDeltaPayload,
        /// Normalized usage at the terminal boundary.
        usage: Option<Usage>,
    },
    /// Terminal response stop marker.
    MessageStop,
    /// Final response event.
    Completed {
        /// Completed response.
        response: Box<LlmResponse>,
    },
}

/// Canonical content delta.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentDelta {
    /// Text delta payload.
    TextDelta {
        /// Partial text.
        text: String,
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
    /// `api-client::ContentDelta::CitationsDelta` exactly.
    CitationsDelta {
        /// Provider-specific citation payload (URL, title, range, etc.).
        citation: Value,
    },
    /// Append text to a `connector_text` block.
    ///
    /// Wire tag: `connector_text_delta`. Field name `connector_text` mirrors
    /// `api-client::ContentDelta::ConnectorTextDelta` exactly (NOT `text`).
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
pub struct StopDetails {
    /// `cyber` / `bio` / … — the refusal category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Free-text explanation (may embed a `https://claude.com/form/…` exemption URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

/// Canonical terminal message delta payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDeltaPayload {
    /// Optional terminal stop reason.
    pub stop_reason: Option<String>,
    /// Optional refusal `stop_details` (the streaming `message_delta.delta`
    /// carries it on a refusal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
}

/// Canonical tool declaration.
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
    /// strict form ([`crate::strict_schema`]) and sends `strict: true`. Default
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

/// Provider-native request envelope with normalized single-value headers.
///
/// The `stream_framing` field is additive with `#[serde(default)]`: existing
/// serialised envelopes (and test literals that omit it) default to
/// [`StreamFraming::Sse`] without a compile error.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderRequest {
    /// HTTP method.
    pub method: String,
    /// Request URL.
    pub url: String,
    /// Normalized single-value request headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// JSON request body.
    pub body_json: Value,
    /// Exact UTF-16 overrides for specific JSON string leaves inside
    /// [`body_json`], keyed by JSON Pointer. When empty, body serialization is
    /// identical to `body_json.to_string()`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub json_string_overrides: BTreeMap<String, Vec<u16>>,
    /// Which transport should be used for streaming this request.
    ///
    /// Defaults to [`ProviderStreamTransport::Http`]; route selection may set
    /// this to [`ProviderStreamTransport::ResponsesWebSocket`] for `OpenAI`
    /// Responses providers that explicitly support WebSocket transport.
    #[serde(default)]
    pub stream_transport: ProviderStreamTransport,
    /// Which streaming framing protocol to use for this request.
    ///
    /// Defaults to [`StreamFraming::Sse`]; codecs that target the AWS
    /// event-stream binary protocol (e.g. Bedrock) set this to
    /// [`StreamFraming::AwsEventStream`] during `encode_request`.
    #[serde(default)]
    pub stream_framing: StreamFraming,
    /// Optional raw request body; takes precedence over `body_json` when set.
    ///
    /// Used by binary upload flows (e.g. the Gemini File API resumable
    /// protocol) where the request body is raw media bytes, not JSON. The
    /// transport bridge sends these bytes verbatim and suppresses the JSON
    /// body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_bytes: Option<Vec<u8>>,
    /// Optional WebSocket connection timeout in milliseconds.
    ///
    /// Only used when [`ProviderStreamTransport::ResponsesWebSocket`] is
    /// selected; HTTP transports ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket_connect_timeout_ms: Option<u64>,
}

impl ProviderRequest {
    /// Create a POST request with a JSON body.
    ///
    /// `stream_framing` defaults to [`StreamFraming::Sse`]; codecs that need
    /// AWS binary framing set `request.stream_framing = StreamFraming::AwsEventStream`
    /// after calling this constructor.
    #[must_use]
    pub fn post_json(url: impl Into<String>, body_json: Value) -> Self {
        Self {
            method: "POST".to_string(),
            url: url.into(),
            headers: BTreeMap::new(),
            body_json,
            json_string_overrides: BTreeMap::new(),
            stream_transport: ProviderStreamTransport::Http,
            stream_framing: StreamFraming::Sse,
            body_bytes: None,
            websocket_connect_timeout_ms: None,
        }
    }

    /// Serialize the request body exactly as the transport/signing layers will
    /// send it on the wire.
    pub fn wire_body_bytes(&self) -> Result<Vec<u8>, LlmError> {
        if let Some(body_bytes) = &self.body_bytes {
            return Ok(body_bytes.clone());
        }
        lingxi_llm_client::exact_json::serialize(&self.body_json, &self.json_string_overrides)
            .map_err(crate::upstream::error)
    }
}

pub(crate) use lingxi_llm_client::exact_json::json_string_len_from_utf16;

/// Provider-native response envelope with normalized single-value headers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponse {
    /// HTTP status code.
    pub status: u16,
    /// Normalized single-value response headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// JSON response body.
    pub body_json: Value,
    /// Optional provider request id.
    pub request_id: Option<String>,
}

impl ProviderResponse {
    /// Create a JSON response envelope.
    #[must_use]
    pub fn json(status: u16, body_json: Value) -> Self {
        Self {
            status,
            headers: BTreeMap::new(),
            body_json,
            request_id: None,
        }
    }
}

/// Raw streaming frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawStreamFrame {
    /// Raw frame bytes.
    pub bytes: Vec<u8>,
}

impl RawStreamFrame {
    /// Create a raw stream frame.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

/// Provider wire codec.
pub trait WireCodec: std::fmt::Debug + Send + Sync {
    /// Freeze the exact catalog row selected by host routing.
    fn for_route(&self, _route: &crate::ResolvedRoute) -> Box<dyn WireCodec> {
        self.clone_box()
    }
    /// Extract actual normalized usage independently of content/tool decoding.
    /// Malformed answer content must not erase provider billing observations.
    fn response_usage(
        &self,
        _response: &ProviderResponse,
    ) -> Option<(Usage, crate::ModelAttemptUsageCompleteness)> {
        None
    }
    /// Encode a canonical request into a provider envelope.
    fn encode_request(&self, request: &LlmRequest) -> Result<ProviderRequest, LlmError>;
    /// Decode a provider envelope into a canonical response.
    fn decode_response(&self, response: ProviderResponse) -> Result<LlmResponse, LlmError>;
    /// Create a stream decoder for this codec.
    fn stream_decoder(&self) -> Box<dyn StreamDecoder>;
    #[allow(missing_docs)]
    fn clone_box(&self) -> Box<dyn WireCodec>;
}

impl Clone for Box<dyn WireCodec> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Stream decoder that emits no events.
#[derive(Debug, Default)]
pub struct NoopStreamDecoder;

impl StreamDecoder for NoopStreamDecoder {
    fn decode_frame(&mut self, _frame: RawStreamFrame) -> Result<Vec<LlmEvent>, LlmError> {
        Ok(Vec::new())
    }
}

/// Provider stream decoder.
pub trait StreamDecoder: std::fmt::Debug + Send {
    /// Latest actual normalized usage retained while decoding, even when no
    /// public event was emitted. Registered accounting samples this before
    /// yielding content; ordinary streaming event shape stays unchanged.
    fn observed_usage(&self) -> Option<(Usage, crate::ModelAttemptUsageCompleteness)> {
        None
    }
    /// Seed decoder-visible provider metadata captured before frames are read.
    ///
    /// HTTP streaming transports expose control-plane data such as rate-limit
    /// headers, model etags, server model names, and turn-state headers before
    /// the first SSE frame. Most codecs ignore it; Responses uses it to retain
    /// Codex control metadata without adding a new streaming event variant.
    fn set_provider_metadata(&mut self, _metadata: Value) {}

    /// Decode one raw stream frame into zero or more canonical events.
    fn decode_frame(&mut self, frame: RawStreamFrame) -> Result<Vec<LlmEvent>, LlmError>;

    /// Finish the stream and emit any terminal events.
    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        Ok(Vec::new())
    }
}

/// Extract cross-provider streaming control metadata from normalized headers.
#[must_use]
pub fn stream_provider_metadata_from_headers(headers: &BTreeMap<String, String>) -> Value {
    let mut metadata = serde_json::Map::new();
    for (name, value) in headers {
        let key = name.to_ascii_lowercase();
        if is_stream_metadata_header(&key) {
            metadata.insert(key, Value::String(value.clone()));
        }
    }
    if metadata.is_empty() {
        Value::Null
    } else {
        Value::Object(metadata)
    }
}

fn is_stream_metadata_header(name: &str) -> bool {
    name == "openai-model"
        || name == "x-models-etag"
        || name == "x-reasoning-included"
        || name == "x-request-id"
        || name == "x-codex-turn-state"
        || name == "retry-after"
        || name.starts_with("x-ratelimit-")
}

/// Validate request capabilities before transport I/O.
pub fn validate_capabilities(
    request: &LlmRequest,
    capabilities: Capabilities,
) -> Result<(), LlmError> {
    if request.stream && !capabilities.streaming {
        return Err(LlmError::UnsupportedCapability {
            capability: "streaming".to_string(),
        });
    }

    if (!request.tools.is_empty() || request.tool_choice.is_some()) && !capabilities.tools {
        return Err(LlmError::UnsupportedCapability {
            capability: "tools".to_string(),
        });
    }

    if request.reasoning.is_some() && !capabilities.reasoning {
        return Err(LlmError::UnsupportedCapability {
            capability: "reasoning".to_string(),
        });
    }

    if request.response_format.is_some() && !capabilities.structured_output {
        return Err(LlmError::UnsupportedCapability {
            capability: "structured_output".to_string(),
        });
    }

    for message in &request.messages {
        for block in &message.content {
            match block {
                ContentBlock::Image { .. } | ContentBlock::ImageUrl { .. }
                    if !capabilities.vision =>
                {
                    return Err(LlmError::UnsupportedCapability {
                        capability: "vision".to_string(),
                    });
                }
                ContentBlock::Document { .. } if !capabilities.documents => {
                    return Err(LlmError::UnsupportedCapability {
                        capability: "documents".to_string(),
                    });
                }
                ContentBlock::ToolCall { .. } | ContentBlock::ToolResult { .. }
                    if !capabilities.tools =>
                {
                    return Err(LlmError::UnsupportedCapability {
                        capability: "tools".to_string(),
                    });
                }
                ContentBlock::Reasoning { .. } | ContentBlock::RedactedThinking { .. }
                    if !capabilities.reasoning =>
                {
                    return Err(LlmError::UnsupportedCapability {
                        capability: "reasoning".to_string(),
                    });
                }
                _ => {}
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn with_profile_sets_field_and_new_defaults_none() {
        assert_eq!(LlmRequest::new("m").profile, None);
        assert_eq!(
            LlmRequest::new("m")
                .with_profile("openai")
                .profile
                .as_deref(),
            Some("openai")
        );
    }

    #[test]
    fn wire_body_bytes_preserve_registered_utf16_override() {
        let mut request = ProviderRequest::post_json(
            "https://example.test",
            json!({"messages":[{"content":[{"text":"A[]"}]}]}),
        );
        request.json_string_overrides.insert(
            "/messages/0/content/0/text".into(),
            vec![0x0041, 0xD83D, 0x005B, 0x005D],
        );
        let wire = String::from_utf8(request.wire_body_bytes().unwrap()).unwrap();
        assert_eq!(wire, r#"{"messages":[{"content":[{"text":"A\ud83d[]"}]}]}"#);
    }

    #[test]
    fn wire_body_bytes_prefer_body_bytes_over_json_and_overrides() {
        let mut request = ProviderRequest::post_json("https://example.test", json!({"a":"b"}));
        request
            .json_string_overrides
            .insert("/a".into(), vec![0x0062, 0xD83D]);
        request.body_bytes = Some(br#"{"raw":true}"#.to_vec());
        assert_eq!(request.wire_body_bytes().unwrap(), br#"{"raw":true}"#);
    }

    #[test]
    fn wire_body_bytes_reject_missing_utf16_override_target() {
        let mut request = ProviderRequest::post_json("https://example.test", json!({"a":"b"}));
        request
            .json_string_overrides
            .insert("/missing".into(), vec![0xD83D]);
        let error = request.wire_body_bytes().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not target a string leaf: /missing"),
            "{error}"
        );
    }
}

/// Order host stream blocks by provider output position. The high bit marks a
/// native replay companion to a visible block at the same provider position.
/// This preserves replay order even when native data arrives in the final frame.
pub fn stream_content_order(index: u32) -> (u32, bool) {
    (index & 0x7fff_ffff, index & 0x8000_0000 != 0)
}
