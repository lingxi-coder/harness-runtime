//! Host execution envelope and transport compatibility boundary.
//! Model input and output types are owned by the SDK.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Capabilities, LlmError, ToolChoice};

/// Which transport should be used when opening a streaming provider request.
/// The SDK owns HTTP event framing and WebSocket frame handling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStreamTransport {
    /// HTTP streaming through the SDK.
    #[default]
    Http,
    /// `OpenAI` Responses API over WebSocket. Only valid for
    /// [`crate::ProtocolFamily::OpenAiResponses`] streaming requests.
    ResponsesWebSocket,
}

/// Host execution envelope around the SDK's canonical model input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    pub input: lingxi_llm_client::protocol::ChatRequest,
    /// Trusted host authority and exact-string sidecars never enter model input.
    #[serde(skip)]
    pub execution: crate::ExecutionContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default)]
    pub stream: bool,
}

impl Default for LlmRequest {
    fn default() -> Self {
        Self::new("")
    }
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

impl LlmRequest {
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            input: lingxi_llm_client::protocol::ChatRequest::new(model),
            execution: Default::default(),
            profile: None,
            stream: false,
        }
    }

    #[must_use]
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    #[must_use]
    pub fn with_user_text(mut self, text: impl Into<String>) -> Self {
        use lingxi_llm_client::protocol as wire;
        self.input.messages.push(wire::ConversationMessage {
            role: wire::MessageRole::User,
            content: vec![wire::ContentBlock::Text {
                text: text.into(),
                thought_signature: None,
                citations: None,
            }],
            native_options: Vec::new(),
        });
        self
    }

    #[must_use]
    pub fn with_image(mut self, media_type: impl Into<String>, bytes: Vec<u8>) -> Self {
        use base64::Engine as _;
        use lingxi_llm_client::protocol as wire;
        let block = wire::ContentBlock::Image {
            source: wire::ImageSource::Base64 {
                media_type: media_type.into(),
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        };
        match self.input.messages.last_mut() {
            Some(message) if message.role == wire::MessageRole::User => message.content.push(block),
            _ => self.input.messages.push(wire::ConversationMessage {
                role: wire::MessageRole::User,
                content: vec![block],
                native_options: Vec::new(),
            }),
        }
        self
    }

    /// Resolve host session thinking policy into canonical SDK controls.
    pub fn set_reasoning(&mut self, reasoning: Option<ReasoningConfig>) {
        use lingxi_llm_client::protocol as wire;
        let thinking = self.input.thinking.get_or_insert_with(Default::default);
        thinking.mode = reasoning.map(|r| match r {
            ReasoningConfig::Adaptive => wire::ThinkingMode::Adaptive,
            ReasoningConfig::Enabled { .. } => wire::ThinkingMode::Enabled,
        });
        thinking.budget = reasoning.and_then(|r| match r {
            ReasoningConfig::Enabled { budget_tokens } => {
                Some(wire::ThinkingBudget::Tokens(budget_tokens))
            }
            _ => None,
        });
        if *thinking == wire::ThinkingConfig::default() {
            self.input.thinking = None;
        }
    }

    /// Parse an application effort selection once, at the input boundary.
    pub fn set_effort(&mut self, effort: Option<Value>) -> Result<(), LlmError> {
        use lingxi_llm_client::protocol as wire;
        let Some(effort) = effort else {
            return Ok(());
        };
        let invalid = |message: String| LlmError::InvalidRequest { message };
        let thinking = self.input.thinking.get_or_insert_with(Default::default);
        if let Some(mode) = effort
            .as_str()
            .filter(|s| matches!(*s, "enabled" | "disabled"))
        {
            thinking.mode = Some(
                serde_json::from_value(Value::String(mode.into()))
                    .map_err(|e| invalid(e.to_string()))?,
            );
        } else if effort.is_string() {
            thinking.effort =
                Some(serde_json::from_value(effort).map_err(|e| invalid(e.to_string()))?);
        } else if let Some(tokens) = effort.as_u64() {
            thinking.budget =
                Some(wire::ThinkingBudget::Tokens(tokens.try_into().map_err(
                    |e: std::num::TryFromIntError| invalid(e.to_string()),
                )?));
        } else {
            return Err(invalid("effort must be a level or token budget".into()));
        }
        Ok(())
    }

    pub fn set_speed(&mut self, speed: Option<String>) -> Result<(), LlmError> {
        use lingxi_llm_client::protocol::ServiceTier;
        self.input.service_tier = match speed.as_deref() {
            Some("fast" | "priority") => Some(ServiceTier::Fast),
            Some("standard" | "default") => Some(ServiceTier::Standard),
            None => None,
            Some(other) => {
                return Err(LlmError::InvalidRequest {
                    message: format!("unsupported service tier: {other}"),
                });
            }
        };
        Ok(())
    }

    pub fn set_tool_choice(&mut self, choice: Option<ToolChoice>) {
        use lingxi_llm_client::protocol as wire;
        self.input.tool_choice = match choice {
            Some(ToolChoice::Required) => wire::ToolChoice::Any,
            Some(ToolChoice::None) => wire::ToolChoice::None,
            Some(ToolChoice::Tool { name }) => wire::ToolChoice::Tool { name },
            _ => wire::ToolChoice::Auto,
        };
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

/// Provider-native request envelope with normalized single-value headers.

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
    /// Serialization semantics of the selected wire protocol.
    pub json_encoding: lingxi_llm_client::exact_json::JsonEncoding,
    /// Exact codec selected for this prepared provider body. Runtime-created
    /// model requests set this before SDK serialization; standalone host
    /// utility requests may leave it absent.
    #[serde(skip)]
    pub body_protocol: Option<lingxi_llm_client::protocol::ProtocolFamily>,
    /// Trusted Native request kind passed through the SDK final body serializer.
    #[serde(skip)]
    pub anthropic_request_kind:
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind,
    /// Exact UTF-16 overrides for specific JSON string leaves inside
    /// [`body_json`], keyed by canonical JSON Pointer. The selected JSON encoding
    /// also applies when no exact strings are present.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub json_string_overrides: BTreeMap<String, Vec<u16>>,
    /// Which transport should be used for streaming this request.
    ///
    /// Defaults to [`ProviderStreamTransport::Http`]; route selection may set
    /// this to [`ProviderStreamTransport::ResponsesWebSocket`] for `OpenAI`
    /// Responses providers that explicitly support WebSocket transport.
    #[serde(default)]
    pub stream_transport: ProviderStreamTransport,
    /// Optional raw request body; takes precedence over `body_json` when set.
    ///
    /// Retains an exact authenticated byte image, including UTF-16 overrides.
    /// Independent file operations use the SDK resource API directly.
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
    #[must_use]
    pub fn post_json(url: impl Into<String>, body_json: Value) -> Self {
        Self {
            method: "POST".to_string(),
            url: url.into(),
            headers: BTreeMap::new(),
            body_json,
            json_encoding: Default::default(),
            body_protocol: None,
            anthropic_request_kind: Default::default(),
            json_string_overrides: BTreeMap::new(),
            stream_transport: ProviderStreamTransport::Http,
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
        lingxi_llm_client::exact_json::serialize_for_request(
            &self.body_json,
            &self.json_string_overrides,
            self.json_encoding,
            self.body_protocol,
            self.anthropic_request_kind,
        )
        .map_err(crate::upstream::error)
    }
}

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

/// Extract cross-provider streaming control metadata from normalized headers.
#[must_use]
pub fn stream_provider_metadata_from_headers(headers: &BTreeMap<String, String>) -> Value {
    let headers = headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<Vec<_>>();
    lingxi_llm_client::providers::response_headers::stream_metadata(&headers)
}

/// Validate request capabilities before transport I/O.
pub fn validate_capabilities(
    request: &LlmRequest,
    capabilities: Capabilities,
) -> Result<(), LlmError> {
    use lingxi_llm_client::protocol::ContentBlock as InputBlock;
    if request.stream && !capabilities.streaming {
        return Err(LlmError::UnsupportedCapability {
            capability: "streaming".to_string(),
        });
    }

    if (!request.input.tools.is_empty()
        || request.input.tool_choice != lingxi_llm_client::protocol::ToolChoice::Auto)
        && !capabilities.tools
    {
        return Err(LlmError::UnsupportedCapability {
            capability: "tools".to_string(),
        });
    }

    if request.input.thinking.is_some() && !capabilities.reasoning {
        return Err(LlmError::UnsupportedCapability {
            capability: "reasoning".to_string(),
        });
    }

    if (request.input.output_format != lingxi_llm_client::protocol::OutputFormat::Text)
        && !capabilities.structured_output
    {
        return Err(LlmError::UnsupportedCapability {
            capability: "structured_output".to_string(),
        });
    }

    for message in &request.input.messages {
        for block in &message.content {
            match block {
                InputBlock::Image { .. } if !capabilities.vision => {
                    return Err(LlmError::UnsupportedCapability {
                        capability: "vision".to_string(),
                    });
                }
                InputBlock::Document { .. } if !capabilities.documents => {
                    return Err(LlmError::UnsupportedCapability {
                        capability: "documents".to_string(),
                    });
                }
                InputBlock::ToolUse { .. } | InputBlock::ToolResult { .. }
                    if !capabilities.tools =>
                {
                    return Err(LlmError::UnsupportedCapability {
                        capability: "tools".to_string(),
                    });
                }
                InputBlock::Thinking { .. } | InputBlock::RedactedThinking { .. }
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
    fn request_serialization_has_one_canonical_model_input_and_no_host_authority() {
        let mut request = LlmRequest::new("model").with_user_text("hello");
        request
            .execution
            .message_json_string_overrides
            .insert("/messages/0/content/0/text".into(), vec![0xd800]);
        request.execution.query_source = Some("host-only".into());
        let serialized = serde_json::to_value(&request).unwrap();
        assert_eq!(serialized["input"]["model"], "model");
        assert!(serialized.get("model").is_none());
        assert!(serialized.get("execution").is_none());
        let restored: LlmRequest = serde_json::from_value(serialized).unwrap();
        assert!(restored.execution.message_json_string_overrides.is_empty());
        assert!(restored.execution.query_source.is_none());
        assert_eq!(restored.input, request.input);
    }

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
