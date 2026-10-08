//! Private unit-test fixture adapter for host request/event projection.
//! Every provider byte is encoded/decoded by the SDK. This module is never
//! compiled into library or test-support feature builds.
use super::*;
use std::sync::Arc;

pub(crate) trait FixtureCodec {
    fn encode_request(&self, request: &dyn FixtureInput) -> Result<ProviderRequest, LlmError>;
    fn response_usage(
        &self,
        response: &ProviderResponse,
    ) -> Option<(ExecutionUsage, ModelAttemptUsageCompleteness)>;
    fn decode_response(&self, response: ProviderResponse) -> Result<HistoryResponse, LlmError>;
    fn stream_decoder(&self) -> Box<dyn FixtureDecoder>;
}
pub(crate) trait FixtureDecoder {
    fn observed_usage(&self) -> Option<(ExecutionUsage, ModelAttemptUsageCompleteness)>;
    fn set_provider_metadata(&mut self, metadata: Value);
    fn decode_frame(&mut self, frame: RawStreamFrame) -> Result<Vec<HistoryEvent>, LlmError>;
    fn finish(&mut self) -> Result<Vec<HistoryEvent>, LlmError>;
}
struct FixtureStream {
    inner: Box<dyn client::StreamDecoder>,
    projection: Decoder,
}
#[derive(Clone)]
pub(crate) struct Codec {
    profile: wire::ProviderProfile,
    inner: Arc<dyn client::WireCodec>,
}
impl std::fmt::Debug for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamCodec")
            .field("family", &self.profile.protocol)
            .finish()
    }
}
impl Codec {
    fn standalone(protocol: wire::ProtocolFamily, base_url: impl Into<String>) -> Self {
        let profile=serde_json::from_value(json!({"provider_id":"configured","profile_name":"configured","base_url":base_url.into(),"protocol":protocol,"auth":"none","models":[],"extra":{"supports_previous_response_id":true}})).expect("static codec profile");
        Self::new(profile)
    }
    pub(crate) fn new(profile: wire::ProviderProfile) -> Self {
        let inner: Arc<dyn client::WireCodec> = match profile.protocol {
            wire::ProtocolFamily::AnthropicMessages => Arc::new(client::AnthropicMessagesCodec),
            wire::ProtocolFamily::OpenAiChat => Arc::new(client::OpenAiChatCodec),
            wire::ProtocolFamily::OpenAiResponses => Arc::new(client::OpenAiResponsesCodec),
            wire::ProtocolFamily::GeminiGenerateContent => Arc::new(client::GeminiCodec),
            wire::ProtocolFamily::GeminiInteractions => Arc::new(client::GeminiInteractionsCodec),
            wire::ProtocolFamily::AzureOpenAi => Arc::new(client::AzureOpenAiCodec),
            wire::ProtocolFamily::BedrockClaude => Arc::new(client::BedrockClaudeCodec),
            wire::ProtocolFamily::VertexClaude => Arc::new(client::VertexClaudeCodec),
            wire::ProtocolFamily::VertexGemini => Arc::new(client::VertexGeminiCodec),
            wire::ProtocolFamily::FoundryClaude => Arc::new(client::FoundryClaudeCodec),
        };
        Self { profile, inner }
    }
    fn context(&self, model: &str, mode: client::RequestMode) -> client::CodecContext {
        if let [selected] = self.profile.models.as_slice() {
            if model.is_empty()
                || model == selected.request_model
                || model == selected.display_model
            {
                return client::CodecContext::for_model(&self.profile, selected, mode);
            }
        }
        client::CodecContext::new(&self.profile, model, mode)
    }
    fn raw_response(response: &ProviderResponse) -> client::HttpResponse {
        client::HttpResponse {
            status: response.status,
            headers: response
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            body: serde_json::to_vec(&response.body_json)
                .expect("response JSON")
                .into(),
        }
    }
    fn encode(
        &self,
        req: &dyn FixtureInput,
        mode: client::RequestMode,
    ) -> Result<ProviderRequest, LlmError> {
        let req = req.canonical(self.profile.protocol)?;
        let context = self
            .context(&req.input.model, mode)
            .with_account_scope(req.execution.account_scope.as_deref())
            .with_file_scope(req.execution.file_account_scope.as_deref());
        let input = request(&req, self.profile.protocol)?;
        let output = self
            .inner
            .encode_request(client::EncodeRequest::new(&input), &context)
            .map_err(error)?;
        let mut result = ProviderRequest::post_json(
            output.url,
            serde_json::from_slice(&output.body).map_err(invalid)?,
        );
        result.method = output.method;
        result.json_encoding =
            lingxi_llm_client::exact_json::JsonEncoding::for_protocol(self.profile.protocol);
        result.body_protocol = Some(self.profile.protocol);
        result.anthropic_request_kind = if self.profile.protocol
            == wire::ProtocolFamily::AnthropicMessages
            && req.execution.query_source.as_deref() == Some("hook_prompt")
        {
            lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::HookPrompt
        } else {
            req.execution.anthropic_request_kind
        };
        result.headers = output.headers.into_iter().collect();
        result.json_string_overrides = lingxi_llm_client::exact_json::map_message_text_overrides(
            &req.input,
            self.profile.protocol,
            &result.body_json,
            &req.execution.message_json_string_overrides,
        )
        .map_err(error)?;
        Ok(result)
    }
}

macro_rules! named_codec {
    ($name:ident, $family:ident) => {
        #[derive(Debug, Clone)]
        pub struct $name(Codec);
        impl $name {
            pub fn new(base_url: impl Into<String>) -> Self {
                Self(Codec::standalone(wire::ProtocolFamily::$family, base_url))
            }
            pub fn with_profile_name(mut self, name: impl Into<String>) -> Self {
                self.0.profile.profile_name = name.into();
                if let Some(source) = client::builtin_providers()
                    .expect("pinned catalog parses")
                    .into_iter()
                    .find(|p| p.profile_name == self.0.profile.profile_name)
                {
                    self.0.profile.extra = source.extra;
                    self.0.profile.inference = source.inference;
                    self.0.profile.info = source.info;
                    self.0.profile.models = source.models;
                }
                self
            }
        }
        impl FixtureCodec for $name {
            fn encode_request(&self, req: &dyn FixtureInput) -> Result<ProviderRequest, LlmError> {
                self.0.encode_request(req)
            }
            fn response_usage(
                &self,
                response: &ProviderResponse,
            ) -> Option<(ExecutionUsage, ModelAttemptUsageCompleteness)> {
                self.0.response_usage(response)
            }
            fn decode_response(
                &self,
                response: ProviderResponse,
            ) -> Result<HistoryResponse, LlmError> {
                self.0.decode_response(response)
            }
            fn stream_decoder(&self) -> Box<dyn FixtureDecoder> {
                self.0.stream_decoder()
            }
        }
    };
}
named_codec!(OpenAiChatCodec, OpenAiChat);
named_codec!(OpenAiResponsesCodec, OpenAiResponses);
named_codec!(GeminiCodec, GeminiGenerateContent);
named_codec!(BedrockClaudeCodec, BedrockClaude);
named_codec!(VertexClaudeCodec, VertexClaude);
named_codec!(VertexGeminiCodec, VertexGemini);
named_codec!(FoundryClaudeCodec, FoundryClaude);

#[derive(Debug, Clone)]
pub struct AnthropicMessagesCodec(Codec);
impl AnthropicMessagesCodec {
    pub fn new(base_url: impl Into<String>, version: impl Into<String>) -> Self {
        let mut codec = Codec::standalone(wire::ProtocolFamily::AnthropicMessages, base_url);
        codec.profile.extra["api_version"] = json!(version.into());
        Self(codec)
    }
    pub fn encode_count_tokens_request(
        &self,
        req: &dyn FixtureInput,
    ) -> Result<ProviderRequest, LlmError> {
        self.0.encode(req, client::RequestMode::CountTokens)
    }
    pub fn decode_count_tokens_response(
        &self,
        response: &ProviderResponse,
    ) -> Result<u64, LlmError> {
        if response.status >= 400 {
            return Err(self
                .0
                .decode_response(response.clone())
                .err()
                .unwrap_or(LlmError::ProviderInternal));
        }
        response.body_json["input_tokens"]
            .as_u64()
            .ok_or_else(|| invalid("token count response has no numeric input_tokens"))
    }
}
impl FixtureCodec for AnthropicMessagesCodec {
    fn encode_request(&self, req: &dyn FixtureInput) -> Result<ProviderRequest, LlmError> {
        self.0.encode_request(req)
    }
    fn response_usage(
        &self,
        response: &ProviderResponse,
    ) -> Option<(ExecutionUsage, ModelAttemptUsageCompleteness)> {
        self.0.response_usage(response)
    }
    fn decode_response(&self, response: ProviderResponse) -> Result<HistoryResponse, LlmError> {
        self.0.decode_response(response)
    }
    fn stream_decoder(&self) -> Box<dyn FixtureDecoder> {
        self.0.stream_decoder()
    }
}
#[derive(Debug, Clone)]
pub struct AzureOpenAiCodec(Codec);
impl AzureOpenAiCodec {
    pub fn new(base_url: impl Into<String>, version: impl Into<String>) -> Self {
        let mut codec = Codec::standalone(wire::ProtocolFamily::AzureOpenAi, base_url);
        codec.profile.azure = Some(wire::AzureConfig {
            api_version: Some(version.into()),
            deployment: None,
        });
        Self(codec)
    }
}
impl FixtureCodec for AzureOpenAiCodec {
    fn encode_request(&self, req: &dyn FixtureInput) -> Result<ProviderRequest, LlmError> {
        self.0.encode_request(req)
    }
    fn response_usage(
        &self,
        response: &ProviderResponse,
    ) -> Option<(ExecutionUsage, ModelAttemptUsageCompleteness)> {
        self.0.response_usage(response)
    }
    fn decode_response(&self, response: ProviderResponse) -> Result<HistoryResponse, LlmError> {
        self.0.decode_response(response)
    }
    fn stream_decoder(&self) -> Box<dyn FixtureDecoder> {
        self.0.stream_decoder()
    }
}
impl FixtureCodec for Codec {
    fn encode_request(&self, req: &dyn FixtureInput) -> Result<ProviderRequest, LlmError> {
        self.encode(
            req,
            if req.streaming() {
                client::RequestMode::Stream
            } else {
                client::RequestMode::Complete
            },
        )
    }
    fn response_usage(
        &self,
        response: &ProviderResponse,
    ) -> Option<(ExecutionUsage, ModelAttemptUsageCompleteness)> {
        let context = self.context("", client::RequestMode::Complete);
        let response = Self::raw_response(response);
        usage(
            &self.inner.response_usage(&response, &context),
            &self.inner.response_inference(&response, &context),
        )
    }
    fn decode_response(&self, response: ProviderResponse) -> Result<HistoryResponse, LlmError> {
        let raw = Self::raw_response(&response);
        let decoded = self
            .inner
            .decode_response(&raw, &self.context("", client::RequestMode::Complete))
            .map_err(|failure| {
                if (200..300).contains(&raw.status) {
                    if let wire::LlmError::ProviderInternal { message } = failure {
                        return invalid(message);
                    }
                }
                error(failure)
            })?;
        project_response(decoded, response, self.profile.protocol)
    }

    fn stream_decoder(&self) -> Box<dyn FixtureDecoder> {
        Box::new(FixtureStream {
            inner: self
                .inner
                .stream_decoder(&self.context("", client::RequestMode::Stream)),
            projection: Decoder::projection(self.profile.protocol, Value::Null),
        })
    }
}

impl FixtureDecoder for FixtureStream {
    fn observed_usage(&self) -> Option<(ExecutionUsage, ModelAttemptUsageCompleteness)> {
        usage(
            &self.projection.observation.0,
            &self.projection.observation.1,
        )
    }
    fn set_provider_metadata(&mut self, metadata: Value) {
        self.projection.metadata = metadata;
    }
    fn decode_frame(&mut self, frame: RawStreamFrame) -> Result<Vec<HistoryEvent>, LlmError> {
        let bytes = if self.projection.family == wire::ProtocolFamily::BedrockClaude {
            frame.bytes
        } else {
            let mut bytes = b"data: ".to_vec();
            bytes.extend(frame.bytes);
            bytes.extend(b"\n\n");
            bytes
        };
        let inner = self.inner.as_mut();
        let events = inner.push_bytes(&bytes);
        self.projection.observation = (inner.usage_report(), inner.inference_report());
        self.projection.events(events)
    }
    fn finish(&mut self) -> Result<Vec<HistoryEvent>, LlmError> {
        let inner = self.inner.as_mut();
        let events = inner.finish();
        self.projection.observation = (inner.usage_report(), inner.inference_report());
        self.projection.events(events)
    }
}

/// An explicit history boundary fixture, rather than a second model request.
/// Tests can mutate stored history before converting it using the production
/// history converter; wire controls live only on the canonical SDK input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HistoryFixture {
    pub request: LlmRequest,
    pub messages: Vec<Message>,
    pub system: Vec<SystemBlock>,
    pub tools: Vec<ToolDeclaration>,
}
impl HistoryFixture {
    pub fn with_user_text(mut self, text: impl Into<String>) -> Self {
        self.messages.push(Message { api_output_config: None,
            role: "user".into(),
            content: vec![ContentBlock::Text {
                text: text.into(),
                cache_control: None,
                citations: None,
            }],
        });
        self
    }
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            request: LlmRequest::new(model),
            messages: vec![],
            system: vec![],
            tools: vec![],
        }
    }
}
pub(crate) trait FixtureInput {
    fn canonical(&self, family: wire::ProtocolFamily) -> Result<LlmRequest, LlmError>;
    fn streaming(&self) -> bool;
}
impl FixtureInput for LlmRequest {
    fn canonical(&self, _: wire::ProtocolFamily) -> Result<LlmRequest, LlmError> {
        Ok(self.clone())
    }
    fn streaming(&self) -> bool {
        self.stream
    }
}
impl FixtureInput for HistoryFixture {
    fn canonical(&self, family: wire::ProtocolFamily) -> Result<LlmRequest, LlmError> {
        let (history, exact_strings) = crate::convert::history_input(
            &self.request.input.model,
            &self.messages,
            &self.system,
            &self.tools,
            family,
        )?;
        let mut result = self.request.clone();
        result.input.messages = history.messages;
        result.input.system = history.system;
        result.input.tools = history.tools;
        result.input.prompt_cache = history.prompt_cache;
        result.execution.message_json_string_overrides = exact_strings;
        Ok(result)
    }
    fn streaming(&self) -> bool {
        self.request.stream
    }
}
