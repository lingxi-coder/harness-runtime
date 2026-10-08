//! `count_tokens` facade: real endpoint on Anthropic routes, documented
//! character-based approximation elsewhere.

use crate::{client::ModelRuntime, LlmError, LlmRequest, Transport};
use std::sync::Arc;

/// Coarse divisor shared by transcript-size estimates. Request-fit estimation
/// below deliberately uses a more conservative divisor.
pub const APPROX_CHARS_PER_TOKEN: u64 = 4;

/// Conservative divisor for request-fit decisions. Structured JSON/tool
/// schemas and CJK text commonly tokenize more densely than the generic
/// four-bytes heuristic used by transcript-size estimates.
const REQUEST_BYTES_PER_TOKEN: u64 = 3;

/// Conservative token estimate for one image-like input when no provider
/// counter is available.  This matches the order of magnitude used by Codex's
/// model-visible history estimator without charging base64 bytes as text.
const APPROX_MEDIA_TOKENS: u64 = 2_048;

/// Count input tokens for `request`'s resolved route.
pub async fn count_tokens(
    client: &ModelRuntime,
    transport: Arc<dyn Transport>,
    request: &LlmRequest,
) -> Result<u64, LlmError> {
    match try_count_tokens_exact(client, transport, request).await? {
        Some(tokens) => Ok(tokens),
        None => Ok(approximate_tokens(request)),
    }
}

/// Count tokens only when the resolved route exposes Anthropic's exact
/// `count_tokens` endpoint. `None` means callers must use their documented
/// fallback rather than mistaking the generic text approximation for an exact
/// tool-schema count.
pub async fn try_count_tokens_exact(
    client: &ModelRuntime,
    transport: Arc<dyn Transport>,
    request: &LlmRequest,
) -> Result<Option<u64>, LlmError> {
    client.count_tokens_exact(request, transport).await
}

/// Structured provider-visible approximation used when no exact endpoint is
/// available.
///
/// This is deliberately not described as tokenizer-accurate: providers apply
/// model-specific chat templates after receiving the request.  It does cover
/// every canonical message block plus host and provider tool declarations,
/// tool choice and output schemas, then uses ceiling division so partial tokens are never
/// rounded down.
#[must_use]
pub fn approximate_tokens(request: &LlmRequest) -> u64 {
    let input = &request.input;
    let mut byte_len = 0u64;
    for block in &input.system {
        byte_len = byte_len.saturating_add(serialized_len(block));
    }
    for (message_index, message) in input.messages.iter().enumerate() {
        byte_len = byte_len.saturating_add(serialized_len(&message.role));
        for (block_index, block) in message.content.iter().enumerate() {
            byte_len = byte_len.saturating_add(estimated_block_bytes(block));
            use lingxi_llm_client::protocol::ContentBlock;
            let exact_text = match block {
                ContentBlock::Text { text, .. } => Some(("text", text)),
                ContentBlock::ToolResult { content, .. } => Some(("content", content)),
                _ => None,
            };
            if let Some((field, display)) = exact_text {
                if let Some(units) = request
                    .execution
                    .message_json_string_overrides
                    .get(&format!(
                        "/messages/{message_index}/content/{block_index}/{field}"
                    ))
                {
                    byte_len = byte_len
                        .saturating_sub(serialized_len(display))
                        .saturating_add(lingxi_llm_client::exact_json::json_string_len_from_utf16(
                            units,
                        ));
                }
            }
        }
    }
    for tool in &input.tools {
        // Account for the provider's function/tool wrapper in addition to the
        // canonical declaration itself.  The server may add further chat
        // template text; the request-level fit margin covers that uncertainty.
        byte_len = byte_len
            .saturating_add(serialized_len(tool))
            .saturating_add(48);
    }
    for tool in &input.hosted_tools {
        byte_len = byte_len
            .saturating_add(serialized_len(tool))
            .saturating_add(48);
    }
    for options in &input.native_options {
        byte_len = byte_len.saturating_add(serialized_len(options));
    }
    if input.output_format != lingxi_llm_client::protocol::OutputFormat::Text {
        byte_len = byte_len.saturating_add(serialized_len(&input.output_format));
    }
    if input.tool_choice != lingxi_llm_client::protocol::ToolChoice::Auto {
        byte_len = byte_len.saturating_add(serialized_len(&input.tool_choice));
    }

    approximate_tokens_for_bytes(byte_len)
}

/// The byte-length-divisor half of [`approximate_tokens`], exposed so a
/// caller estimating from a bare text/prompt string it already has in hand
/// (not a full [`LlmRequest`] it would have to fabricate just to feed this
/// function) still uses the SAME formula — never a second, divergent
/// bytes-per-token constant. `approximate_tokens` itself is defined in terms
/// of this function, so the two can never drift apart.
#[must_use]
pub fn approximate_tokens_for_bytes(byte_len: u64) -> u64 {
    byte_len.div_ceil(REQUEST_BYTES_PER_TOKEN).max(1)
}

fn serialized_len<T: serde::Serialize>(value: &T) -> u64 {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len() as u64)
}

fn estimated_block_bytes(block: &lingxi_llm_client::protocol::ContentBlock) -> u64 {
    use lingxi_llm_client::protocol::{ContentBlock, DocumentSource, ImageSource};
    const TEXT_BLOCK_WRAPPER_BYTES: u64 = 30;
    let media_bytes = APPROX_MEDIA_TOKENS * REQUEST_BYTES_PER_TOKEN;
    match block {
        ContentBlock::Text { text, .. } => {
            serialized_len(text).saturating_add(TEXT_BLOCK_WRAPPER_BYTES)
        }
        ContentBlock::Image {
            source: ImageSource::Url { url },
        } => media_bytes.saturating_add(url.len() as u64),
        ContentBlock::Image { .. } => media_bytes,
        ContentBlock::Document {
            source: DocumentSource::Base64 { data, .. },
            ..
        } => {
            // Count decoded document bytes, never the base64 transport expansion.
            let padding = data.bytes().rev().take_while(|byte| *byte == b'=').count() as u64;
            media_bytes.max(
                (data.len() as u64)
                    .saturating_mul(3)
                    .div_euclid(4)
                    .saturating_sub(padding),
            )
        }
        ContentBlock::Document {
            source: DocumentSource::Text { data, .. },
            ..
        } => media_bytes.max(data.len() as u64),
        ContentBlock::Document { .. } | ContentBlock::Video { .. } | ContentBlock::Audio { .. } => {
            media_bytes
        }
        _ => serialized_len(block),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ModelRuntime;
    use crate::{
        AuthStrategy, BoxFuture, Capabilities, ClientConfig, ContentBlock, CredentialConfig,
        LlmRequest, Message, ModelProfile, PricingConfig, ProtocolFamily, ProviderId,
        ProviderProfile, ProviderRequest, ProviderResponse, StreamingResponse, ToolDeclaration,
        Transport,
    };
    use std::sync::Mutex;

    fn push_history(request: &mut LlmRequest, message: Message) {
        let (input, overrides) = crate::convert::history_input(
            "model",
            &[message],
            &[],
            &[],
            ProtocolFamily::AnthropicMessages,
        )
        .unwrap();
        request.input.messages.extend(input.messages);
        request
            .execution
            .message_json_string_overrides
            .extend(overrides);
    }

    fn push_tool(request: &mut LlmRequest, tool: ToolDeclaration) {
        let (input, _) = crate::convert::history_input(
            "model",
            &[],
            &[],
            &[tool],
            ProtocolFamily::AnthropicMessages,
        )
        .unwrap();
        request.input.tools.extend(input.tools);
    }

    // ----------------------------------------------------------------
    // Byte-length approximation math tests
    // ----------------------------------------------------------------

    #[test]
    fn approximate_tokens_empty_request_returns_one() {
        let req = LlmRequest::new("model");
        assert_eq!(approximate_tokens(&req), 1);
    }

    #[test]
    fn approximate_tokens_for_bytes_shares_the_request_divisor() {
        // The accessor must be the exact same ceiling-division formula
        // `approximate_tokens` folds its own byte_len through — not a
        // separately-hand-rolled divisor that could silently drift from it.
        assert_eq!(
            approximate_tokens_for_bytes(0),
            1,
            "empty input floors to 1"
        );
        assert_eq!(
            approximate_tokens_for_bytes(REQUEST_BYTES_PER_TOKEN),
            1,
            "exactly one token's worth of bytes is 1 token"
        );
        assert_eq!(
            approximate_tokens_for_bytes(REQUEST_BYTES_PER_TOKEN + 1),
            2,
            "one byte past a token boundary must round UP, not down"
        );
        assert_eq!(
            approximate_tokens_for_bytes(9),
            9_u64.div_ceil(REQUEST_BYTES_PER_TOKEN)
        );
    }

    #[test]
    fn approximate_tokens_includes_text_envelope() {
        let req =
            LlmRequest::new("model").with_user_text("1234567890123456789012345678901234567890");
        assert_eq!(req.input.messages[0].content.len(), 1);
        assert!(approximate_tokens(&req) > 10);
    }

    #[test]
    fn approximate_tokens_uses_ceiling_division() {
        let req = LlmRequest::new("model").with_user_text("12345");
        let byte_len = serialized_len(&req.input.messages[0].role)
            + estimated_block_bytes(&req.input.messages[0].content[0]);
        assert_eq!(
            approximate_tokens(&req),
            byte_len.div_ceil(REQUEST_BYTES_PER_TOKEN)
        );
    }

    #[test]
    fn approximate_tokens_system_blocks_are_counted() {
        let mut req = LlmRequest::new("model");
        req.input
            .system
            .push(lingxi_llm_client::protocol::SystemBlock {
                text: "12345678".into(),
            }); // 8 bytes
        assert!(approximate_tokens(&req) >= 2);
    }

    #[test]
    fn approximate_tokens_counts_media_blocks_without_base64_inflation() {
        let mut req = LlmRequest::new("model");
        push_history(
            &mut req,
            Message { api_output_config: None,
                role: "user".to_string(),
                content: vec![ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    bytes: vec![0u8; 100],
                }],
            },
        );
        assert_eq!(approximate_tokens(&req), APPROX_MEDIA_TOKENS + 2);
    }

    #[test]
    fn approximate_tokens_counts_structured_messages_and_tools() {
        let mut req = LlmRequest::new("model");
        push_history(
            &mut req,
            Message { api_output_config: None,
                role: "assistant".to_string(),
                content: vec![
                    ContentBlock::TextJsUtf16 {
                        text: "structured text".to_string(),
                        utf16_code_units: "structured text".encode_utf16().collect(),
                        cache_control: None,
                        citations: None,
                    },
                    ContentBlock::ToolCall { input_projection: None,
                        id: "call-1".to_string(),
                        name: "lookup".to_string(),
                        input: serde_json::json!({"query": "weather in San Francisco"}),
                    },
                    ContentBlock::ToolResult { output_projection: None,
                        tool_call_id: "call-1".to_string(),
                        output: serde_json::json!({"temperature": 18, "unit": "celsius"}),
                        is_error: Some(false),
                        cache_control: None,
                        cache_reference: None,
                    },
                ],
            },
        );
        push_tool(
            &mut req,
            ToolDeclaration {
                name: "lookup".to_string(),
                description: "Look up current information for a location".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search query"}
                    },
                    "required": ["query"]
                }),
                ..Default::default()
            },
        );

        assert!(
            approximate_tokens(&req) > 1,
            "provider-visible structured content and tool declarations must contribute"
        );
    }

    #[test]
    fn approximate_tokens_includes_hosted_tool_configuration() {
        use lingxi_llm_client::protocol::{HostedTool, WebSearchConfig};

        let mut request = LlmRequest::new("model").with_user_text("find sources");
        let without_tools = approximate_tokens(&request);
        request
            .input
            .hosted_tools
            .push(HostedTool::WebSearch(WebSearchConfig {
                allowed_domains: vec!["example.com".into()],
                ..Default::default()
            }));
        let with_tools = approximate_tokens(&request);
        assert!(with_tools > without_tools);

        let HostedTool::WebSearch(config) = &mut request.input.hosted_tools[0] else {
            unreachable!()
        };
        config
            .allowed_domains
            .extend((0..100).map(|n| format!("source-{n}.example.com")));
        assert!(approximate_tokens(&request) > with_tools + 500);
    }

    #[test]
    fn approximate_tokens_includes_typed_output_schema() {
        use lingxi_llm_client::protocol::OutputFormat;

        let mut request = LlmRequest::new("model").with_user_text("produce a result");
        let plain = approximate_tokens(&request);
        request.input.output_format = OutputFormat::JsonSchema {
            name: "result".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "answer": {"type": "string", "description": "Detailed instructions. ".repeat(100)}
                }
            }),
            strict: true,
        };
        assert!(approximate_tokens(&request) > plain + 500);
    }

    #[test]
    fn text_js_utf16_ascii_estimate_matches_plain_text() {
        let text = "restored ASCII skill content".repeat(100);
        let mut plain = LlmRequest::new("model");
        push_history(
            &mut plain,
            Message { api_output_config: None,
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: text.clone(),
                    cache_control: None,
                    citations: None,
                }],
            },
        );
        let mut exact_utf16 = LlmRequest::new("model");
        push_history(
            &mut exact_utf16,
            Message { api_output_config: None,
                role: "user".to_string(),
                content: vec![ContentBlock::TextJsUtf16 {
                    utf16_code_units: text.encode_utf16().collect(),
                    text,
                    cache_control: None,
                    citations: None,
                }],
            },
        );

        assert_eq!(approximate_tokens(&exact_utf16), approximate_tokens(&plain));
    }

    #[test]
    fn exact_utf16_escape_bytes_remain_in_the_fit_estimate() {
        let display = "�".repeat(300);
        let plain = LlmRequest::new("model").with_user_text(display.clone());
        let mut exact = LlmRequest::new("model");
        push_history(
            &mut exact,
            Message { api_output_config: None,
                role: "user".into(),
                content: vec![ContentBlock::TextJsUtf16 {
                    text: display,
                    utf16_code_units: vec![0xd800; 300],
                    cache_control: None,
                    citations: None,
                }],
            },
        );
        // Each lone surrogate is six JSON bytes; its replacement glyph is
        // three UTF-8 bytes. The 900 extra bytes contribute 300 fit tokens.
        assert_eq!(approximate_tokens(&exact), approximate_tokens(&plain) + 300);
    }

    // ----------------------------------------------------------------
    // Fake Transport for integration-style tests
    // ----------------------------------------------------------------

    #[derive(Debug)]
    struct ScriptedTransport {
        response: ProviderResponse,
        seen: Mutex<Option<ProviderRequest>>,
    }

    impl ScriptedTransport {
        fn returning(response: ProviderResponse) -> Self {
            Self {
                response,
                seen: Mutex::new(None),
            }
        }
    }

    impl llm_runtime::test_support::FixtureTransport for ScriptedTransport {
        fn execute<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            *self.seen.lock().expect("lock") = Some(request.clone());
            let response = self.response.clone();
            Box::pin(async move { Ok(response) })
        }

        fn open_stream<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            Box::pin(async move {
                Err(LlmError::Transport {
                    message: "not used in count_tokens tests".to_string(),
                })
            })
        }
    }
    llm_runtime::impl_fixture_transport!(ScriptedTransport);

    struct StalledCountTransport {
        body: bool,
    }
    #[async_trait::async_trait]
    impl Transport for StalledCountTransport {
        async fn send(
            &self,
            _: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            use futures::StreamExt;
            if !self.body {
                return std::future::pending().await;
            }
            Ok(lingxi_llm_client::StreamResponse {
                status: 200,
                headers: vec![],
                body: futures::stream::pending().boxed(),
            })
        }
    }
    #[tokio::test(start_paused = true)]
    async fn exact_count_bounds_headers_and_body_without_transport_timeout() {
        for body in [false, true] {
            let client = anthropic_client();
            let request = LlmRequest::new("Claude").with_user_text("hello");
            let start = tokio::time::Instant::now();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(121),
                try_count_tokens_exact(&client, Arc::new(StalledCountTransport { body }), &request),
            )
            .await;
            assert!(
                matches!(result, Ok(Err(LlmError::TransportTimeout { .. }))),
                "{result:?}"
            );
            assert_eq!(start.elapsed(), std::time::Duration::from_secs(120));
        }
    }

    fn anthropic_client() -> ModelRuntime {
        std::env::set_var("LLM_COUNT_TOKENS_TEST_KEY", "ct-test-key");
        ModelRuntime::from_config(ClientConfig {
            providers: vec![ProviderProfile {
                wire_profile: None,
                regions: lingxi_llm_client::protocol::Region::all(),
                provider_id: ProviderId::AnthropicFirstParty,
                profile_name: "anthropic".to_string(),
                base_url: "https://api.anthropic.com".to_string(),
                protocol: ProtocolFamily::AnthropicMessages,
                auth: AuthStrategy::ApiKey,
                credential: CredentialConfig::Env {
                    var: "LLM_COUNT_TOKENS_TEST_KEY".to_string(),
                },
                models: vec![ModelProfile {
                    display_model: "Claude".to_string(),
                    request_model: "claude-sonnet-4-20250514".to_string(),
                    billing_model: "claude-sonnet-4".to_string(),
                    aliases: vec!["claude".to_string()],
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities {
                        streaming: true,
                        tools: true,
                        ..Default::default()
                    },
                }],
                pricing: PricingConfig::default(),
                signing: None,
                azure: None,
                supports_websockets: false,
                supports_websocket_compression: false,
                websocket_connect_timeout_ms: None,
                vision_delegate: None,
                connection: Default::default(),
            }],
        })
        .expect("client")
    }

    fn openai_client() -> ModelRuntime {
        ModelRuntime::from_config(ClientConfig {
            providers: vec![ProviderProfile {
                wire_profile: None,
                regions: lingxi_llm_client::protocol::Region::all(),
                provider_id: ProviderId::OpenAI,
                profile_name: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                protocol: ProtocolFamily::OpenAiChat,
                auth: AuthStrategy::None,
                credential: CredentialConfig::None,
                models: vec![ModelProfile {
                    display_model: "GPT".to_string(),
                    request_model: "gpt-4".to_string(),
                    billing_model: "gpt-4".to_string(),
                    aliases: vec!["gpt".to_string()],
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities {
                        streaming: true,
                        tools: true,
                        ..Default::default()
                    },
                }],
                pricing: PricingConfig::default(),
                signing: None,
                azure: None,
                supports_websockets: false,
                supports_websocket_compression: false,
                websocket_connect_timeout_ms: None,
                vision_delegate: None,
                connection: Default::default(),
            }],
        })
        .expect("client")
    }

    // ----------------------------------------------------------------
    // Happy path: Anthropic → real endpoint → decode input_tokens
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn anthropic_happy_path_returns_count_tokens_from_response() {
        let transport = Arc::new(ScriptedTransport::returning(ProviderResponse::json(
            200,
            serde_json::json!({ "input_tokens": 2095 }),
        )));
        let client = anthropic_client();
        let req = LlmRequest::new("claude").with_user_text("hello world");

        let count = count_tokens(&client, transport.clone(), &req)
            .await
            .expect("count");

        assert_eq!(count, 2095);
        // Also verify the request was sent to the count_tokens endpoint.
        let seen = transport
            .seen
            .lock()
            .unwrap()
            .clone()
            .expect("request sent");
        assert!(
            seen.url.ends_with("/v1/messages/count_tokens?beta=true"),
            "url={}",
            seen.url
        );
        assert_eq!(
            seen.headers.get("x-api-key").map(String::as_str),
            Some("ct-test-key")
        );
    }

    #[tokio::test]
    async fn anthropic_route_sends_count_tokens_beta_header() {
        let transport = Arc::new(ScriptedTransport::returning(ProviderResponse::json(
            200,
            serde_json::json!({ "input_tokens": 42 }),
        )));
        let client = anthropic_client();
        let req = LlmRequest::new("claude").with_user_text("hello");

        let _ = count_tokens(&client, transport.clone(), &req)
            .await
            .expect("count");

        let seen = transport
            .seen
            .lock()
            .unwrap()
            .clone()
            .expect("request sent");
        let expected = crate::model::betas::assemble_beta_header(
            crate::model::betas::Provider::Anthropic,
            crate::model::betas::Endpoint::CountTokens,
            &crate::model::betas::BetaContext::for_model("claude-sonnet-4-20250514"),
        );
        let expected = format!(
            "{expected},{}",
            lingxi_llm_client::providers::anthropic::request_policy::TOKEN_COUNTING
        );
        assert_eq!(
            seen.headers.get("anthropic-beta").map(String::as_str),
            Some(expected.as_str()),
            "countTokens appends the mandatory SDK beta after model betas; got: {:?}",
            seen.headers.get("anthropic-beta"),
        );
    }

    // ----------------------------------------------------------------
    // Non-Anthropic route → approximation fallback
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn non_anthropic_route_falls_back_to_approximation() {
        // Routes without an exact counter use the local approximation and
        // must not send a provider request.
        let transport = Arc::new(ScriptedTransport::returning(ProviderResponse::json(
            200,
            serde_json::json!({ "input_tokens": 9999 }),
        )));
        let client = openai_client();
        // The fallback includes provider-visible message structure as well as
        // text, so it must be larger than the old raw-text / 4 heuristic.
        let req = LlmRequest::new("gpt").with_user_text("12345678901234567890");
        let expected = approximate_tokens(&req);

        let count = count_tokens(&client, transport.clone(), &req)
            .await
            .expect("count");

        assert_eq!(count, expected);
        assert!(count > 5);
        // Transport must NOT have been called.
        assert!(
            transport.seen.lock().unwrap().is_none(),
            "transport should not be called"
        );
    }

    #[tokio::test]
    async fn exact_count_reports_unavailable_for_non_anthropic_route() {
        let transport = Arc::new(ScriptedTransport::returning(ProviderResponse::json(
            200,
            serde_json::json!({ "input_tokens": 9999 }),
        )));
        let client = openai_client();
        let req = LlmRequest::new("gpt").with_user_text("hello");

        assert_eq!(
            try_count_tokens_exact(&client, transport.clone(), &req)
                .await
                .expect("route resolution"),
            None
        );
        assert!(transport.seen.lock().unwrap().is_none());
    }

    // ----------------------------------------------------------------
    // 401 envelope → Authentication error propagates
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn authentication_error_propagates_from_401_response() {
        let transport = Arc::new(ScriptedTransport::returning(ProviderResponse::json(
            401,
            serde_json::json!({
                "type": "error",
                "error": { "type": "authentication_error", "message": "invalid api key" }
            }),
        )));
        let client = anthropic_client();
        let req = LlmRequest::new("claude").with_user_text("hello");

        let err = count_tokens(&client, transport.clone(), &req)
            .await
            .expect_err("must fail");

        assert!(
            matches!(err, LlmError::Authentication { .. }),
            "expected Authentication, got {err:?}"
        );
    }
}
