//! Concrete [`SideQueryClient`] backed by the provider-neutral LLM stack.
//!
//! A side query is a stateless one-shot LLM call (see [`crate::side_query`]).
//! [`ProviderSideQueryClient`] wires the [`SideQueryRequest`] DTO to the
//! configured provider and decodes the [`LlmResponse`] back into a
//! [`SideQueryResponse`]. Utility callers can construct an isolated Anthropic
//! client with [`ProviderSideQueryClient::new`]. Session-bound compaction and
//! recap use [`ProviderSideQueryClient::from_service`] so the fork reuses the
//! parent [`llm_runtime::ApiService`] — including its exact provider route,
//! OAuth/keychain credential, message normalization, prompt-cache boundaries,
//! headers, and retry behavior.
//!
//! ## Field forwarding
//!
//! Forwards `model`, `system`, `messages`, `max_tokens`, `tools`, `temperature`,
//! plus `tool_choice`, `stop_sequences` and (cc 2.1.198) `thinking` — a
//! `Some` session [`llm_runtime::model::thinking::ThinkingConfig`] resolves
//! through the SAME `reasoning_for_request` rules as the main loop, so the
//! isolated call inherits the supplied extended-thinking config.
//! `output_format` drives the structured-text decode (not a server-side
//! `response_format`); `max_retries` stays dropped. Existing callers pass
//! `None`/empty for the extras, so their wire stays byte-identical.
//!
//! ## System forwarding fix
//!
//! Unlike the previous `api_client::AnthropicProvider` path, this client NOW
//! correctly forwards the `system` field via [`llm_runtime::SystemBlock`], which
//! the Anthropic codec encodes as `"system": [{"type":"text","text":"..."}]`.
//! This fixes the pre-broken `text_response_decodes_text_usage_and_stop_reason`
//! test.

use crate::side_query::{
    CanonicalSideQueryRequest, SideQueryClient, SideQueryError, SideQueryEstimate,
    SideQueryRequest, SideQueryResponse,
};
use async_trait::async_trait;
use llm_runtime::LlmTransportBridge;
use llm_runtime::{
    AuthStrategy, Capabilities, ClientConfig, Credential, CredentialConfig, DefaultLlmClient,
    LlmError, LlmRequest, ModelProfile, PricingConfig, ProtocolFamily, ProviderId, ProviderProfile,
    StaticCredentialProvider, SystemBlock,
};
use platform_api::http::{RawByteStream, SseStream};
use platform_api::{HttpError, HttpTransport};
use protocol::{HttpRequest, HttpResponse, MediaAnalysis};
use std::sync::Arc;

/// Default Anthropic API base URL used when the caller passes `None`.
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Static credential id used inside the internal config for a static API key.
const SIDEQUERY_CRED_ID: &str = "sidequery_key";

/// Sized newtype adapter around an `Arc<dyn HttpTransport>` for the isolated
/// constructor's [`LlmTransportBridge`].
struct ArcTransport(Arc<dyn HttpTransport>);

#[async_trait]
impl HttpTransport for ArcTransport {
    async fn send_stream(
        &self,
        req: platform_api::http::HttpStreamRequest,
    ) -> Result<platform_api::http::RawByteStreamWithMeta, HttpError> {
        self.0.send_stream(req).await
    }

    async fn stream_raw_bytes_with_meta_no_follow_with_resolved_addrs(
        &self,
        req: HttpRequest,
        resolved: Option<platform_api::ResolvedAddressOverride>,
    ) -> Result<platform_api::http::RawByteStreamWithMeta, HttpError> {
        self.0
            .stream_raw_bytes_with_meta_no_follow_with_resolved_addrs(req, resolved)
            .await
    }

    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.0.request(req).await
    }

    async fn stream_sse(&self, req: HttpRequest) -> Result<SseStream, HttpError> {
        self.0.stream_sse(req).await
    }

    async fn stream_raw_bytes(&self, req: HttpRequest) -> Result<RawByteStream, HttpError> {
        self.0.stream_raw_bytes(req).await
    }
}

enum ProviderSideQueryBackend {
    /// Standalone utility-query client built from a raw Anthropic API key.
    Direct {
        client: DefaultLlmClient,
        transport: Arc<dyn HttpTransport>,
    },
    /// The live session service used by compaction/recap.
    Session(Arc<llm_runtime::ApiService>),
}

/// One-shot [`SideQueryClient`] that can either run as an isolated Anthropic
/// utility client or through a live session's provider-neutral service.
pub struct ProviderSideQueryClient {
    backend: ProviderSideQueryBackend,
}

impl ProviderSideQueryClient {
    /// Build a client from raw credentials.
    ///
    /// `None` for `base_url` uses `https://api.anthropic.com`.
    ///
    /// # Panics
    ///
    /// Panics if the internal config is structurally invalid (unreachable in
    /// normal use — the config is built from known-good constants).
    #[must_use]
    pub fn new(
        api_key: impl Into<String>,
        base_url: Option<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        let api_key = api_key.into();
        let base_url = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let config = ClientConfig {
            providers: vec![ProviderProfile {
                wire_profile: None,
                regions: llm_runtime::Region::all(),
                provider_id: ProviderId::AnthropicFirstParty,
                profile_name: "anthropic".to_string(),
                base_url,
                protocol: ProtocolFamily::AnthropicMessages,
                auth: AuthStrategy::ApiKey,
                credential: CredentialConfig::Static {
                    id: SIDEQUERY_CRED_ID.to_string(),
                },
                pricing: PricingConfig::default(),
                signing: None,
                azure: None,
                supports_websockets: false,
                supports_websocket_compression: false,
                websocket_connect_timeout_ms: None,
                vision_delegate: None,
                // Wildcard model support: sidequery uses any model string the
                // caller passes (e.g. "claude-haiku-4-5" for memory summaries,
                // "claude-opus-4-6" for compaction). We register a catch-all
                // entry keyed on the empty prefix so that ANY model string is
                // accepted, then override at request time with the actual model.
                //
                // Because `DefaultLlmClient::prepare` resolves models by exact
                // `display_model` or `aliases` match, we must register the
                // models that callers actually request. The known set is:
                //   - claude-haiku-4-5 (memory selector)
                //   - claude-opus-4-6  (compaction)
                // We register both here (plus common aliases) so the registry
                // resolves them. Unknown model strings will fail with
                // `LlmError::ModelUnavailable` — the caller should use a known
                // model id.
                models: sidequery_model_table(),
                connection: Default::default(),
            }],
        };

        let cred_provider = Arc::new(StaticCredentialProvider::new(Credential::ApiKey(api_key)));
        let client = DefaultLlmClient::from_config(config)
            .expect("sidequery ClientConfig is structurally valid")
            .with_credential_provider(cred_provider);

        Self {
            backend: ProviderSideQueryBackend::Direct { client, transport },
        }
    }

    /// Bind side queries to the live session API service.
    ///
    /// This is the required constructor for compaction and recap. It prevents
    /// the fork from silently switching to a fresh static-key Anthropic client
    /// when the parent is authenticated with OAuth, a keychain credential, a
    /// gateway, or a non-Anthropic provider profile.
    #[must_use]
    pub fn from_service(service: Arc<llm_runtime::ApiService>) -> Self {
        Self {
            backend: ProviderSideQueryBackend::Session(service),
        }
    }
}

/// Build the minimal model table for side-query callers.
///
/// Side queries use "claude-haiku-4-5" (memory selector) and
/// "claude-opus-4-6" (compaction), plus the current session defaults
/// (claude-sonnet-5 / claude-opus-4-8 / claude-fable-5-1) that compaction forks
/// inherit. All entries get the same capability set: streaming=false (side
/// queries are always non-streaming), tools, vision, documents, reasoning
/// (the cc 2.1.198 thinking-inheritance seam).
fn sidequery_model_table() -> Vec<ModelProfile> {
    fn model(display: &str, billing: &str, aliases: &[&str]) -> ModelProfile {
        ModelProfile {
            display_model: display.to_string(),
            request_model: display.to_string(),
            billing_model: billing.to_string(),
            aliases: aliases.iter().map(|s| (*s).to_string()).collect(),
            description: None,
            metadata: Default::default(),
            capabilities: Capabilities {
                streaming: false,
                tools: true,
                vision: true,
                documents: true,
                // cc 2.1.198: the compaction fork call inherits the session
                // extended-thinking config, so the side-query route must
                // accept a `reasoning` field (requests without one are
                // unaffected by this capability flag).
                reasoning: true,
                structured_output: false,
            },
        }
    }

    vec![
        // Memory-selector model
        model("claude-haiku-4-5", "claude-haiku-4-5", &[]),
        model(
            "claude-haiku-4-20250307",
            "claude-haiku-4",
            &["claude-haiku-4", "claude-haiku"],
        ),
        // Compaction model (AutocompactConfig::default)
        model("claude-opus-4-6", "claude-opus-4-6", &[]),
        model("claude-opus-4-7", "claude-opus-4-7", &[]),
        // Current-generation defaults (2.1.197/198, M1b): a compaction fork on
        // the session default (claude-sonnet-5) — or an opus-4-8/fable-5.1
        // session — must resolve here instead of dying with ModelUnavailable.
        // All three support (adaptive) thinking; `reasoning: true` above.
        model("claude-sonnet-5", "claude-sonnet-5", &[]),
        model("claude-opus-4-8", "claude-opus-4-8", &[]),
        model("claude-fable-5-1", "claude-fable-5-1", &[]),
        // Broad Sonnet/Opus/Haiku coverage for callers using any model string
        model("claude-sonnet-4-6", "claude-sonnet-4-6", &[]),
        model(
            "claude-opus-4-20250514",
            "claude-opus-4",
            &["claude-opus-4", "claude-opus"],
        ),
        model(
            "claude-sonnet-4-20250514",
            "claude-sonnet-4",
            &["claude-sonnet-4", "claude-sonnet", "claude"],
        ),
    ]
}

/// Decode an [`llm_runtime::LlmResponse`] into the side-query response shape.
///
/// * `Text` blocks are concatenated into the flattened `text`, except compact
///   responses, where cc 2.1.261 N0e selects the first text block only.
/// * `ToolCall` blocks become `{"id", "name", "input"}` JSON in `tool_calls`.
/// * Other block kinds (reasoning, server tool use, connector text, advisor
///   tool result, redacted thinking) are ignored for side queries.
/// * `structured` is populated only when `want_structured` is set (the caller
///   requested an `output_format`): the accumulated text is best-effort parsed
///   as JSON. A non-JSON body leaves `structured` as `None` rather than
///   erroring — `MemorySelector` tolerates `None` (empty selection), so the
///   best-effort path is the safer default.
/// * `usage` maps `llm_runtime::Usage.billable_tokens` → `cost::Usage` with the
///   same cross-naming the provider's own cost path uses: API `cache_write` →
///   cost `cache_write`, API `cache_read` → cost `cache_read`.
fn decode_response(
    resp: llm_runtime::LlmResponse,
    want_structured: bool,
    first_text_only: bool,
    separate_text_blocks: bool,
) -> SideQueryResponse {
    let retry_count = resp
        .provider_metadata
        .get("_lingxi_retry_count")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);
    let mut text_acc = String::new();
    let mut saw_text = false;
    let mut tool_calls: Vec<serde_json::Value> = Vec::new();

    for block in resp.content {
        match block {
            llm_runtime::ContentBlock::Text { text, .. }
            | llm_runtime::ContentBlock::TextJsUtf16 { text, .. } => {
                if first_text_only {
                    if !saw_text {
                        text_acc.push_str(&text);
                    }
                } else {
                    if separate_text_blocks && saw_text {
                        text_acc.push_str("\n\n");
                    }
                    text_acc.push_str(&text);
                }
                saw_text = true;
            }
            llm_runtime::ContentBlock::ToolCall { id, name, input } => {
                tool_calls.push(serde_json::json!({
                    "id": id,
                    "name": name,
                    "input": input,
                }));
            }
            // Side queries ignore reasoning / server-tool / connector / advisor
            // / redacted-thinking blocks.
            llm_runtime::ContentBlock::ProviderContent { .. }
            | llm_runtime::ContentBlock::Reasoning { .. }
            | llm_runtime::ContentBlock::RedactedThinking { .. }
            | llm_runtime::ContentBlock::ServerToolUse { .. }
            | llm_runtime::ContentBlock::ConnectorText { .. }
            | llm_runtime::ContentBlock::AdvisorToolResult { .. }
            | llm_runtime::ContentBlock::Image { .. }
            | llm_runtime::ContentBlock::ImageUrl { .. }
            | llm_runtime::ContentBlock::Document { .. }
            | llm_runtime::ContentBlock::ToolResult { .. }
            // cache_edits is a request-only directive — never in a response.
            | llm_runtime::ContentBlock::CacheEdits { .. } => {}
        }
    }

    let text = (!text_acc.is_empty()).then(|| text_acc.clone());
    let structured = if want_structured {
        serde_json::from_str::<serde_json::Value>(&text_acc).ok()
    } else {
        None
    };

    let bt = resp.usage.billable_tokens;
    let usage = cost::Usage {
        tokens: cost::TokenUsage {
            input: bt.input,
            output: bt.output,
            cache_read: bt.cache_read,
            cache_write: bt.cache_write,
            cache_write_1h: 0,
            reasoning_output: bt.reasoning_output,
        },
        server_tool_use: None,
        speed: None,
    };

    SideQueryResponse {
        text,
        structured,
        tool_calls,
        usage,
        stop_reason: resp.stop_reason,
        retry_count,
    }
}

#[async_trait]
impl SideQueryClient for ProviderSideQueryClient {
    fn estimate_request(
        &self,
        request: CanonicalSideQueryRequest,
    ) -> Result<SideQueryEstimate, SideQueryError> {
        match (&self.backend, request) {
            (
                ProviderSideQueryBackend::Session(service),
                CanonicalSideQueryRequest::Plain(request),
            ) => {
                let query_source = request.query_source.as_str();
                let canonical = service.build_side_query_request_with_thinking(
                    &request.model,
                    request.profile.as_deref(),
                    request.system_prompt.as_deref(),
                    request.messages,
                    request.tools,
                    Some(request.max_tokens),
                    convert_tool_choice(request.tool_choice.as_ref()),
                    request.stop_sequences,
                    request.thinking,
                    request.effort,
                    request.temperature,
                    Some(query_source),
                )?;
                estimate_canonical_request(canonical)
            }
            (
                ProviderSideQueryBackend::Session(service),
                CanonicalSideQueryRequest::Strict(request),
            ) => {
                let query_source = request.query_source.as_str();
                let temperature = temperature_for_capable_model(
                    &service.model_listings(),
                    request.profile.as_deref(),
                    &request.model,
                    request.temperature,
                );
                let canonical = service.build_json_schema_request_with_thinking(
                    &request.model,
                    request.profile.as_deref(),
                    request.system_prompt.as_deref(),
                    request.messages,
                    request.schema,
                    Some(request.max_tokens),
                    None,
                    None,
                    temperature,
                    Some(query_source),
                )?;
                estimate_canonical_request(canonical)
            }
            (_, request) => {
                let serialized_bytes = match request {
                    CanonicalSideQueryRequest::Plain(request) => {
                        crate::side_query::serialized_size(&request)
                    }
                    CanonicalSideQueryRequest::Strict(request) => {
                        crate::side_query::serialized_size(&request)
                    }
                }?;
                Ok(SideQueryEstimate {
                    serialized_bytes,
                    input_tokens: llm_runtime::model::count_tokens::approximate_tokens_for_bytes(
                        serialized_bytes,
                    ),
                })
            }
        }
    }

    async fn query(&self, request: SideQueryRequest) -> Result<SideQueryResponse, SideQueryError> {
        let first_text_only = request.query_source == crate::purposes::QuerySource::Compaction;
        let separate_text_blocks = matches!(
            &request.query_source,
            crate::purposes::QuerySource::Custom(source) if source == "side_question"
        );
        if let ProviderSideQueryBackend::Session(service) = &self.backend {
            let wants_structured = request.output_format.is_some();
            let query_source = request.query_source.as_str();
            let mut canonical = service.build_side_query_request_with_thinking(
                &request.model,
                request.profile.as_deref(),
                request.system_prompt.as_deref(),
                request.messages,
                request.tools,
                // Fork compaction resolves the parent's ordinary output
                // budget; other side queries carry their explicit cap.
                Some(request.max_tokens),
                convert_tool_choice(request.tool_choice.as_ref()),
                request.stop_sequences,
                request.thinking,
                request.effort,
                request.temperature,
                Some(query_source),
            )?;
            canonical.model_attempt = request.model_attempt;
            let resp = service.execute_side_query_request(canonical).await?;
            return Ok(decode_response(
                resp,
                wants_structured,
                first_text_only,
                separate_text_blocks,
            ));
        }

        let ProviderSideQueryBackend::Direct { client, transport } = &self.backend else {
            unreachable!("session backend returned above")
        };
        if request.model_attempt.is_some() {
            return Err(LlmError::InvalidRequest {
                message: "registered side query requires session accounting hooks".into(),
            }
            .into());
        }

        // Build the LlmRequest from the SideQueryRequest DTO.
        let system: Vec<SystemBlock> = request
            .system_prompt
            .as_deref()
            .map(|s| vec![SystemBlock::text(s)])
            .unwrap_or_default();

        // Convert protocol::ConversationMessage → llm_runtime::Message.
        // We inline a minimal conversion here so sidequery does not need to
        // depend on `agent` (which depends back on sidequery — a cycle).
        let messages = convert_messages(request.messages)?;

        // Convert JSON tool declarations → llm_runtime::ToolDeclaration.
        let tools = convert_tool_declarations(request.tools)?;

        // thinking (cc 2.1.198): a `Some` session config resolves through the
        // SAME `reasoning_for_request` rules `ApiService::build_request`
        // applies to every main-loop/subagent request (adaptive vs fixed
        // budget, model predicates, env kill switches, max_tokens-1 clamp) —
        // the compaction fork call therefore inherits the session's
        // extended-thinking config. `None` (all utility callers) keeps the
        // wire byte-identical to before (no `thinking` field).
        let reasoning = request.thinking.and_then(|t| {
            llm_runtime::model::thinking::reasoning_for_request(
                t,
                &request.model,
                Some(request.max_tokens),
            )
        });

        let query_source = request.query_source.as_str().to_string();
        let llm_req = LlmRequest {
            model: request.model,
            profile: request.profile,
            system,
            messages,
            tools,
            // output_format drives the structured text decode (NOT
            // response_format); max_retries is a caller-side budget.
            tool_choice: convert_tool_choice(request.tool_choice.as_ref()),
            stop_sequences: request.stop_sequences,
            max_tokens: Some(request.max_tokens),
            temperature: request.temperature.map(f64::from),
            reasoning,
            effort: request.effort,
            capture_retry_count: true,
            query_source: Some(query_source),
            ..LlmRequest::default()
        };

        // Route through the transport bridge so the existing Arc<dyn
        // HttpTransport> is usable as an llm_runtime::Transport.
        let arc_transport = ArcTransport(Arc::clone(transport));
        let bridge = LlmTransportBridge::new(arc_transport);

        let resp = client.execute(&llm_req, Arc::new(bridge)).await?;

        Ok(decode_response(
            resp,
            request.output_format.is_some(),
            first_text_only,
            separate_text_blocks,
        ))
    }

    async fn query_json_schema(
        &self,
        request: crate::side_query::StrictStructuredQueryRequest,
    ) -> Result<crate::side_query::StrictStructuredQueryResponse, SideQueryError> {
        let model = request.model.clone();
        let profile = request.profile.clone();
        let resp = match &self.backend {
            ProviderSideQueryBackend::Session(service) => {
                let query_source = request.query_source.as_str().to_string();
                // Round-3 review finding 4: an explicit `temperature`
                // override (e.g. the fusion analyst's hard `Some(0.0)`) used
                // to be silently dropped by the old 7-arg `stream_json_schema`
                // this backend called; now that it reaches the wire
                // verbatim, it must be withheld from a model whose own
                // vendored capability row says temperature control is
                // unsupported — see `temperature_for_capable_model`.
                let temperature = temperature_for_capable_model(
                    &service.model_listings(),
                    request.profile.as_deref(),
                    &request.model,
                    request.temperature,
                );
                // F003 round 2: route through the `_with_thinking` sibling
                // so an explicit `temperature` override is never sent
                // alongside a session-derived `thinking` block —
                // `StrictStructuredQueryRequest` has no thinking field of
                // its own, so `None` here means the same "no reasoning
                // field at all" that `SideQueryRequest{thinking: None}`
                // already means for the non-strict path above.
                let mut canonical = service
                    .build_json_schema_request_with_thinking(
                        &request.model,
                        request.profile.as_deref(),
                        request.system_prompt.as_deref(),
                        request.messages,
                        request.schema,
                        Some(request.max_tokens),
                        None,
                        None,
                        temperature,
                        Some(query_source.as_str()),
                    )
                    .map_err(map_structured_llm_error)?;
                canonical.model_attempt = request.model_attempt;
                let stream = service
                    .stream_request(canonical)
                    .await
                    .map_err(map_structured_llm_error)?;
                collect_completed_response(stream).await?
            }
            ProviderSideQueryBackend::Direct { client, transport } => {
                if request.model_attempt.is_some() {
                    return Err(map_structured_llm_error(LlmError::InvalidRequest {
                        message: "registered side query requires session accounting hooks".into(),
                    }));
                }
                let system: Vec<SystemBlock> = request
                    .system_prompt
                    .as_deref()
                    .map(|s| vec![SystemBlock::text(s)])
                    .unwrap_or_default();
                let messages = convert_messages(request.messages)?;
                // Same capability gate as the Session arm above, sourced
                // from this client's own registry.
                let temperature = temperature_for_capable_model(
                    &client.available_models(),
                    request.profile.as_deref(),
                    &request.model,
                    request.temperature,
                );
                let llm_req = LlmRequest {
                    model: request.model,
                    profile: request.profile,
                    system,
                    messages,
                    tools: Vec::new(),
                    max_tokens: Some(request.max_tokens),
                    temperature: temperature.map(f64::from),
                    capture_retry_count: true,
                    query_source: Some(request.query_source.as_str().to_string()),
                    response_format: Some(llm_runtime::ResponseFormat::JsonSchema {
                        schema: request.schema,
                    }),
                    ..LlmRequest::default()
                };
                let arc_transport = ArcTransport(Arc::clone(transport));
                let bridge = LlmTransportBridge::new(arc_transport);
                client
                    .execute(&llm_req, Arc::new(bridge))
                    .await
                    .map_err(map_structured_llm_error)?
            }
        };
        let request_id = (!resp.id.is_empty()).then(|| resp.id.clone());
        let decoded = decode_response(resp, true, false, false);
        let Some(value) = decoded.structured else {
            return Err(SideQueryError::InvalidResponse(
                "structured output was not valid JSON".into(),
            ));
        };
        Ok(crate::side_query::StrictStructuredQueryResponse {
            value,
            usage: decoded.usage,
            model,
            profile,
            request_id,
            retry_count: decoded.retry_count,
        })
    }

    fn last_retry_count(&self) -> u32 {
        match &self.backend {
            ProviderSideQueryBackend::Session(service) => service.last_retry_count(),
            ProviderSideQueryBackend::Direct { .. } => 0,
        }
    }

    fn has_canonical_estimator(&self) -> bool {
        matches!(&self.backend, ProviderSideQueryBackend::Session(_))
    }
}

fn estimate_canonical_request(request: LlmRequest) -> Result<SideQueryEstimate, SideQueryError> {
    let serialized_bytes = crate::side_query::serialized_size(&request)?;
    Ok(SideQueryEstimate {
        serialized_bytes,
        input_tokens: llm_runtime::model::count_tokens::approximate_tokens(&request),
    })
}

/// Round-3 review finding 4: whether the vendored catalog data reachable
/// from this client explicitly marks `(profile, model)` as NOT accepting a
/// `temperature` parameter — `models.dev`'s `temperature` bit, already
/// surfaced on every configured model's listing as
/// `ModelListing.metadata.temperature_control == Some(false)` (see
/// `llm_runtime::catalog::map::to_metadata`, consumed identically by the
/// client-facing `/model` picker). `profile: None` matches any listing with
/// the given `request_model`; every `StrictStructuredQueryRequest` built in
/// this codebase carries an explicit profile (the Fusion analyst always
/// does), so that branch only matters for a hypothetical profile-less
/// caller. A model with no listing, or an unset/`true` bit, is unaffected.
fn model_rejects_temperature(
    listings: &[llm_runtime::ModelListing],
    profile: Option<&str>,
    model: &str,
) -> bool {
    listings.iter().any(|listing| {
        let profile_matches = match profile {
            Some(p) => listing.profile_name == p,
            None => true,
        };
        profile_matches
            && listing.request_model == model
            && listing.metadata.temperature_control == Some(false)
    })
}

/// Withhold an explicit `temperature` override from a model whose own
/// vendored capability row says temperature control is unsupported —
/// otherwise the provider rejects the whole call (no retry, see
/// `analyze`'s doc comment in the fusion crate), after every panel has
/// already been billed. A `None` temperature (the common case outside the
/// Fusion analyst) is returned unchanged regardless of capability, since
/// there is nothing to withhold.
fn temperature_for_capable_model(
    listings: &[llm_runtime::ModelListing],
    profile: Option<&str>,
    model: &str,
    temperature: Option<f32>,
) -> Option<f32> {
    if temperature.is_some() && model_rejects_temperature(listings, profile, model) {
        None
    } else {
        temperature
    }
}

// ── Inline message/tool conversion (no dep on `agent` crate) ─────────────────

fn map_structured_llm_error(err: llm_runtime::LlmError) -> SideQueryError {
    let message = err.to_string().to_ascii_lowercase();
    let provider = err.provider_message().unwrap_or("").to_ascii_lowercase();
    if message.contains("json schema")
        || message.contains("structured output")
        || message.contains("response_format")
        || provider.contains("json schema")
        || provider.contains("structured output")
    {
        return SideQueryError::StructuredOutputUnsupported;
    }
    SideQueryError::Api(err)
}

/// Drive a `query_json_schema` event stream to its final [`llm_runtime::LlmResponse`].
///
/// F003: this previously scanned for an [`llm_runtime::LlmEvent::Completed`]
/// event by hand — but the Anthropic codec (the provider every real Fusion
/// analyst call over the Session backend uses) never emits `Completed`; a
/// normal stream ends `MessageStart` → `ContentBlockStart/Delta/Stop` →
/// `MessageDelta` → `MessageStop`, with `Completed` reserved for providers
/// (OpenAI Responses) that deliver one fully-assembled event. The hand-rolled
/// scan therefore ran off the end of every real Anthropic stream and always
/// returned this function's own `InvalidResponse` — the analyst call was
/// unreachable end-to-end regardless of the schema/prompt. Delegate to the
/// shared [`llm_runtime::stream_accumulator::accumulate_stream_salvaging`]
/// instead, which assembles the response from `MessageStop` (or `Completed`,
/// when a provider does send it) the same way every other streaming call in
/// the codebase does.
async fn collect_completed_response(
    stream: impl futures_util::Stream<Item = Result<llm_runtime::LlmEvent, llm_runtime::LlmError>>
        + Send
        + 'static,
) -> Result<llm_runtime::LlmResponse, SideQueryError> {
    llm_runtime::stream_accumulator::accumulate_stream_salvaging(Box::pin(stream))
        .await
        .map_err(|(_partial_content, err)| map_structured_llm_error(err))
}

fn convert_messages(
    messages: Vec<protocol::ConversationMessage>,
) -> Result<Vec<llm_runtime::Message>, llm_runtime::LlmError> {
    messages.into_iter().map(convert_one_message).collect()
}

fn convert_one_message(
    msg: protocol::ConversationMessage,
) -> Result<llm_runtime::Message, llm_runtime::LlmError> {
    match msg {
        protocol::ConversationMessage::User { content, .. } => Ok(llm_runtime::Message {
            role: "user".to_string(),
            content: content
                .into_iter()
                .map(convert_content_block)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        protocol::ConversationMessage::Assistant { content, .. } => Ok(llm_runtime::Message {
            role: "assistant".to_string(),
            content: content
                .into_iter()
                .map(convert_content_block)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        protocol::ConversationMessage::System { .. } => Err(llm_runtime::LlmError::InvalidRequest {
            message:
                "System messages must not appear in the messages vec; pass them via system_prompt"
                    .to_string(),
        }),
    }
}

fn convert_content_block(
    block: protocol::ContentBlock,
) -> Result<llm_runtime::ContentBlock, llm_runtime::LlmError> {
    match block {
        protocol::ContentBlock::ProviderContent { protocol, value } => {
            Ok(llm_runtime::ContentBlock::ProviderContent { protocol, value })
        }
        protocol::ContentBlock::Text { text } => Ok(llm_runtime::ContentBlock::Text {
            text,
            cache_control: None,
        }),
        protocol::ContentBlock::TextJsUtf16 {
            text,
            utf16_code_units,
        } => Ok(llm_runtime::ContentBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            cache_control: None,
        }),
        protocol::ContentBlock::ToolUse {
            id,
            name,
            input,
            provider_id,
        } => Ok(llm_runtime::ContentBlock::ToolCall {
            // Replay the verbatim provider id when preserved (see agent::convert).
            id: provider_id.unwrap_or_else(|| id.to_string()),
            name,
            input,
        }),
        protocol::ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            provider_tool_use_id,
            content_blocks,
        } => Ok(llm_runtime::ContentBlock::ToolResult {
            tool_call_id: provider_tool_use_id.unwrap_or_else(|| tool_use_id.to_string()),
            // A structured content-block array (e.g. MCP image/resource) rides
            // as the `Value::Array` output and is emitted verbatim; plain text
            // stays a `Value::String`.
            output: content_blocks.map_or_else(
                || serde_json::Value::String(content),
                serde_json::Value::Array,
            ),
            is_error,
            cache_control: None,
            cache_reference: None,
        }),
        protocol::ContentBlock::Thinking {
            thinking,
            signature,
        } => Ok(llm_runtime::ContentBlock::Reasoning {
            text: thinking,
            signature,
        }),
        protocol::ContentBlock::Image { source } => convert_image(source),
        protocol::ContentBlock::Document { source } => convert_document(source),
        protocol::ContentBlock::MediaAnalysis { analysis } => Ok(llm_runtime::ContentBlock::Text {
            text: render_media_analysis(&analysis),
            cache_control: None,
        }),
        // Low-frequency server-side blocks: replayed verbatim into the request
        // so the provider round-trips them (see agent::convert::convert_block).
        protocol::ContentBlock::RedactedThinking { data } => {
            Ok(llm_runtime::ContentBlock::RedactedThinking { data })
        }
        protocol::ContentBlock::ServerToolUse { id, name, input } => {
            Ok(llm_runtime::ContentBlock::ServerToolUse { id, name, input })
        }
        protocol::ContentBlock::ConnectorText {
            connector_text,
            signature,
        } => Ok(llm_runtime::ContentBlock::ConnectorText {
            connector_text,
            signature,
        }),
        protocol::ContentBlock::AdvisorToolResult {
            tool_use_id,
            content,
            is_error,
        } => Ok(llm_runtime::ContentBlock::AdvisorToolResult {
            tool_use_id,
            content,
            is_error,
        }),
    }
}

fn convert_image(
    source: protocol::ImageSource,
) -> Result<llm_runtime::ContentBlock, llm_runtime::LlmError> {
    match source {
        protocol::ImageSource::Base64 { media_type, data } => {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&data)
                .map_err(|e| llm_runtime::LlmError::InvalidRequest {
                    message: format!("Image base64 decode failed: {e}"),
                })?;
            Ok(llm_runtime::ContentBlock::Image { media_type, bytes })
        }
        protocol::ImageSource::Url { url } => Ok(llm_runtime::ContentBlock::ImageUrl { url }),
    }
}

fn convert_document(
    source: protocol::DocumentSource,
) -> Result<llm_runtime::ContentBlock, llm_runtime::LlmError> {
    match source {
        protocol::DocumentSource::Base64 { media_type, data } => {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&data)
                .map_err(|e| llm_runtime::LlmError::InvalidRequest {
                    message: format!("Document base64 decode failed: {e}"),
                })?;
            Ok(llm_runtime::ContentBlock::Document { media_type, bytes })
        }
    }
}

fn render_media_analysis(analysis: &MediaAnalysis) -> String {
    format!(
        "[Media analysis sidecar]\n{}",
        serde_json::to_string(analysis).unwrap_or_else(|_| "{}".to_string())
    )
}

fn convert_tool_declarations(
    tools: Vec<serde_json::Value>,
) -> Result<Vec<llm_runtime::ToolDeclaration>, llm_runtime::LlmError> {
    tools.into_iter().map(convert_one_tool).collect()
}

/// Map the DTO's JSON `tool_choice` to [`llm_runtime::ToolChoice`]. Accepts the
/// Anthropic wire shapes: `{"type":"auto"}`, `{"type":"any"}`,
/// `{"type":"none"}`, `{"type":"tool","name":N}`. Unknown / absent ⇒ `None`
/// (provider default), so the memory selector that passes `None` is unchanged.
fn convert_tool_choice(choice: Option<&serde_json::Value>) -> Option<llm_runtime::ToolChoice> {
    let ty = choice?.get("type").and_then(serde_json::Value::as_str)?;
    match ty {
        "auto" => Some(llm_runtime::ToolChoice::Auto),
        "none" => Some(llm_runtime::ToolChoice::None),
        "any" | "required" => Some(llm_runtime::ToolChoice::Required),
        "tool" => choice?
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(|name| llm_runtime::ToolChoice::Tool { name: name.into() }),
        _ => None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn convert_one_tool(
    value: serde_json::Value,
) -> Result<llm_runtime::ToolDeclaration, llm_runtime::LlmError> {
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| llm_runtime::LlmError::InvalidRequest {
            message: "Tool declaration missing required string field: name".to_string(),
        })?
        .to_string();

    let description = value
        .get("description")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| llm_runtime::LlmError::InvalidRequest {
            message: "Tool declaration missing required string field: description".to_string(),
        })?
        .to_string();

    let input_schema = value
        .get("input_schema")
        .cloned()
        .filter(|v| !v.is_null())
        .ok_or_else(|| llm_runtime::LlmError::InvalidRequest {
            message: "Tool declaration missing required field: input_schema".to_string(),
        })?;

    Ok(llm_runtime::ToolDeclaration {
        name,
        description,
        input_schema,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::purposes::QuerySource;
    use protocol::{ConversationMessage, MessageId};
    use std::sync::Mutex;

    /// Minimal in-process [`HttpTransport`] that returns a single scripted
    /// 200 body. Kept local so the sidequery crate needs no extra
    /// dev-dependency (mirrors the `OneShot` template in `platform-api/src/http.rs`).
    struct StubTransport {
        body: String,
        received: Mutex<Vec<HttpRequest>>,
    }

    impl StubTransport {
        fn new(body: impl Into<String>) -> Self {
            Self {
                body: body.into(),
                received: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl HttpTransport for StubTransport {
        async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
            self.received.lock().unwrap().push(req);
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: self.body.clone(),
                body_bytes: Vec::new(),
            })
        }

        async fn stream_sse(&self, _req: HttpRequest) -> Result<SseStream, HttpError> {
            Err(HttpError::InvalidRequest(
                "sse not used in side query".into(),
            ))
        }
    }

    fn req(output_format: Option<serde_json::Value>) -> SideQueryRequest {
        SideQueryRequest {
            model_attempt: None,
            model: "claude-haiku-4-5".into(),
            profile: None,
            system_prompt: Some("system".into()),
            messages: vec![ConversationMessage::user(MessageId::new(), "hi".into())],
            tools: vec![],
            tool_choice: None,
            output_format,
            max_tokens: 1024,
            max_retries: 2,
            temperature: Some(0.0),
            thinking: None,
            effort: None,
            stop_sequences: vec![],
            query_source: QuerySource::MemorySelector,
            skip_system_prompt_prefix: false,
        }
    }

    #[tokio::test]
    async fn registered_side_queries_reject_direct_backend_and_json_cannot_grant_authority() {
        let transport = Arc::new(StubTransport::new("unused"));
        let client = ProviderSideQueryClient::new("sk-test", None, transport.clone());
        let run = platform_api::ModelAttemptRun::new(Arc::new(()));
        let mut request = req(None);
        let ordinary = serde_json::to_value(&request).unwrap();
        request.model_attempt = Some(
            run.context(platform_api::ModelAttemptStage::Synthesis, None)
                .unwrap(),
        );
        assert_eq!(serde_json::to_value(&request).unwrap(), ordinary);
        let mut forged = ordinary;
        forged["model_attempt"] = serde_json::json!({"logical_call_id": 1});
        assert!(serde_json::from_value::<SideQueryRequest>(forged)
            .unwrap()
            .model_attempt
            .is_none());
        assert!(client.query(request).await.is_err());
        let mut strict = strict_req();
        strict.model_attempt = Some(
            run.context(platform_api::ModelAttemptStage::Analyst, None)
                .unwrap(),
        );
        assert!(client.query_json_schema(strict).await.is_err());
        assert!(transport.received.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn text_response_decodes_text_usage_and_stop_reason() {
        // A plain-text answer with a populated usage block.
        let body = serde_json::json!({
            "id": "msg_01",
            "model": "claude-haiku-4-5",
            "content": [
                { "type": "text", "text": "hello " },
                { "type": "text", "text": "world" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 12,
                "output_tokens": 7,
                "cache_creation_input_tokens": 3,
                "cache_read_input_tokens": 5
            }
        })
        .to_string();

        let transport = Arc::new(StubTransport::new(body));
        let client = ProviderSideQueryClient::new("sk-test", None, transport.clone());

        let resp = client.query(req(None)).await.expect("query ok");

        assert_eq!(resp.text.as_deref(), Some("hello world"));
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        // No output_format requested -> structured stays None.
        assert!(resp.structured.is_none());
        assert!(resp.tool_calls.is_empty());
        // Usage mapping: API cache_creation -> cost cache_write,
        // API cache_read -> cost cache_read.
        assert_eq!(resp.usage.tokens.input, 12);
        assert_eq!(resp.usage.tokens.output, 7);
        assert_eq!(resp.usage.tokens.cache_write, 3);
        assert_eq!(resp.usage.tokens.cache_read, 5);
        assert_eq!(resp.usage.tokens.reasoning_output, 0);

        // Wire-request mapping: inspect the body the provider actually sent.
        // Exactly one request issued through the injected transport.
        let received = transport.received.lock().unwrap();
        assert_eq!(received.len(), 1);
        let raw_body = received[0].body.as_deref().expect("request has a body");
        let body: serde_json::Value = serde_json::from_str(raw_body).expect("request body is JSON");

        // The forwarded fields reach the wire body.
        assert_eq!(body["model"].as_str(), Some("claude-haiku-4-5"));
        // llm-runtime encodes system as an array of blocks:
        // `"system": [{"type":"text","text":"system"}]`
        // (not the legacy string form the old api-client used).
        let system_arr = body["system"].as_array().expect("system is a JSON array");
        assert_eq!(system_arr.len(), 1);
        assert_eq!(system_arr[0]["type"].as_str(), Some("text"));
        assert_eq!(system_arr[0]["text"].as_str(), Some("system"));
        assert_eq!(
            body["messages"].as_array().map(Vec::len),
            Some(1),
            "the single request message is forwarded"
        );

        // `max_tokens` now carries the request's value (1024), and the request's
        // `temperature: Some(0.0)` is forwarded — both via the opts entrypoint.
        assert_eq!(
            body["max_tokens"].as_u64(),
            Some(1024),
            "request max_tokens (1024) forwarded, not the legacy 4096"
        );
        assert_eq!(
            body["temperature"].as_f64(),
            Some(0.0),
            "request temperature (0.0) forwarded"
        );
        // `tools` is empty on this request, so the `is_empty` guard keeps the
        // body key absent. The remaining DTO fields stay unforwardable.
        assert!(
            body.get("tools").is_none(),
            "tools absent when the request list is empty"
        );
        assert!(body.get("tool_choice").is_none(), "tool_choice dropped");
        assert!(
            body.get("stop_sequences").is_none(),
            "stop_sequences dropped"
        );
    }

    #[tokio::test]
    async fn side_question_text_blocks_use_claude_separator() {
        let body = serde_json::json!({
            "id": "msg_side_question",
            "model": "claude-haiku-4-5",
            "content": [
                { "type": "text", "text": "first" },
                { "type": "text", "text": "second" }
            ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })
        .to_string();
        let transport = Arc::new(StubTransport::new(body));
        let client = ProviderSideQueryClient::new("sk-test", None, transport);
        let mut request = req(None);
        request.query_source = QuerySource::Custom("side_question".into());

        let response = client.query(request).await.expect("query ok");

        assert_eq!(response.text.as_deref(), Some("first\n\nsecond"));
    }

    #[tokio::test]
    async fn compaction_selects_first_text_block_even_when_a_later_block_has_summary_tags() {
        for first_text in ["  first text  ", ""] {
            let body = serde_json::json!({
                "id": "msg_compact_first_text",
                "model": "claude-haiku-4-5",
                "content": [
                    { "type": "thinking", "thinking": "reasoning", "signature": "sig" },
                    { "type": "text", "text": first_text },
                    { "type": "text", "text": "<summary>later text</summary>" }
                ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 4, "output_tokens": 2 }
            })
            .to_string();
            let transport = Arc::new(StubTransport::new(body));
            let client = ProviderSideQueryClient::new("sk-test", None, transport);
            let mut request = req(None);
            request.query_source = QuerySource::Compaction;

            let response = client.query(request).await.expect("compaction response");

            assert_eq!(
                response.text.as_deref(),
                (!first_text.is_empty()).then_some(first_text)
            );
        }
    }

    /// `/compact` must not build a second static-key Anthropic client. The
    /// session backend reuses the parent's provider service, which also runs
    /// Claude Code's pre-wire normalization: transcript-only compact markers
    /// are removed and adjacent user turns are merged before encoding.
    #[tokio::test]
    async fn session_backend_reuses_parent_route_auth_and_message_pipeline() {
        let response = serde_json::json!({
            "id": "msg_compact",
            "model": "claude-sonnet-4-6",
            "content": [
                { "type": "text", "text": "<summary>ok</summary>" },
                { "type": "text", "text": "later text must not join compact summary" }
            ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 3, "output_tokens": 2 }
        })
        .to_string();
        let transport = Arc::new(StubTransport::new(response));

        let config = ClientConfig {
            providers: vec![ProviderProfile {
                wire_profile: None,
                regions: llm_runtime::Region::all(),
                provider_id: ProviderId::AnthropicFirstParty,
                profile_name: "parent-profile".to_string(),
                base_url: DEFAULT_BASE_URL.to_string(),
                protocol: ProtocolFamily::AnthropicMessages,
                auth: AuthStrategy::OAuthBearer,
                credential: CredentialConfig::Static {
                    id: "parent-session-key".to_string(),
                },
                models: vec![ModelProfile {
                    display_model: "claude-sonnet-4-6".to_string(),
                    request_model: "claude-sonnet-4-6".to_string(),
                    billing_model: "claude-sonnet-4-6".to_string(),
                    aliases: vec![],
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities {
                        streaming: false,
                        tools: true,
                        reasoning: true,
                        ..Capabilities::default()
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
        };
        let parent_client = DefaultLlmClient::from_config(config)
            .expect("parent client config")
            .with_credential_provider(Arc::new(StaticCredentialProvider::new(
                Credential::BearerToken("parent-oauth-token".to_string()),
            )));
        let parent_transport: Arc<dyn llm_runtime::Transport> = Arc::new(LlmTransportBridge::new(
            ArcTransport(transport.clone() as Arc<dyn HttpTransport>),
        ));
        let parent_service = Arc::new(
            llm_runtime::ApiService::new(
                Arc::new(parent_client),
                parent_transport,
                llm_runtime::SubscriberState::default(),
                llm_runtime::model::user_agent::UserAgentEnv::default(),
                "test",
                None,
                None,
            )
            // Exercise the main-turn temperature default: disabled thinking
            // would inject `temperature: 1`, which the side query's explicit
            // `None` must clear to preserve its independent wire contract.
            .with_thinking(llm_runtime::model::thinking::ThinkingConfig::Disabled)
            // A main `--json-schema` requirement must not leak into compact.
            .with_forced_tool_choice(llm_runtime::ToolChoice::Tool {
                name: "StructuredOutput".to_string(),
            }),
        );
        let client = ProviderSideQueryClient::from_service(parent_service);
        let mut request = req(None);
        request.model = "claude-sonnet-4-6".to_string();
        request.query_source = QuerySource::Compaction;
        request.profile = Some("parent-profile".to_string());
        request.temperature = None;
        request.thinking = Some(llm_runtime::model::thinking::ThinkingConfig::Adaptive);
        request.effort = Some(serde_json::json!("high"));
        let parent_tools = vec![serde_json::json!({
            "name": "Read",
            "description": "Read a file from the parent session.",
            "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}}
        })];
        request.tools = parent_tools.clone();
        request.messages = vec![
            ConversationMessage::System {
                id: MessageId::new(),
                content: "Conversation compacted".to_string(),
                subtype: None,
                compact_metadata: None,
                refusal_fallback: None,
            },
            ConversationMessage::user(MessageId::new(), "existing context".into()),
            ConversationMessage::user(MessageId::new(), "compact prompt".into()),
        ];

        let result = client.query(request).await.expect("session side query");
        assert_eq!(result.text.as_deref(), Some("<summary>ok</summary>"));

        let received = transport.received.lock().unwrap();
        assert_eq!(received.len(), 1);
        let sent = &received[0];
        assert!(sent.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value == "Bearer parent-oauth-token"
        }));
        assert!(sent.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("anthropic-beta") && value.contains("oauth-2025-04-20")
        }));
        let body: serde_json::Value =
            serde_json::from_str(sent.body.as_deref().expect("request body")).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["tools"], serde_json::json!(parent_tools));
        assert_eq!(body["thinking"], serde_json::json!({"type": "adaptive"}));
        assert_eq!(body["output_config"]["effort"], "high");
        assert!(
            body.get("tool_choice").is_none(),
            "main-turn forced tool choice leaked into compact: {body}"
        );
        assert!(
            body.get("temperature").is_none(),
            "main-turn temperature default leaked into compact: {body}"
        );
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1, "boundary dropped + user turns merged");
        let content = messages[0]["content"].as_array().expect("content array");
        assert_eq!(content[0]["text"], "existing context\n");
        assert_eq!(content[1]["text"], "compact prompt");
    }

    #[tokio::test]
    async fn structured_output_parses_json_text() {
        // JSON-shaped text + an output_format request -> structured populated.
        let body = serde_json::json!({
            "id": "msg_02",
            "model": "claude-haiku-4-5",
            "content": [
                { "type": "text", "text": "{\"filenames\":[\"a.md\",\"b.md\"]}" }
            ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })
        .to_string();

        let transport = Arc::new(StubTransport::new(body));
        let client = ProviderSideQueryClient::new("sk-test", None, transport);

        let resp = client
            .query(req(Some(serde_json::json!({"type": "json_schema"}))))
            .await
            .expect("query ok");

        let structured = resp.structured.expect("structured present");
        let names = structured["filenames"].as_array().expect("array");
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].as_str(), Some("a.md"));
    }

    #[tokio::test]
    async fn structured_output_falls_back_to_none_on_non_json() {
        // output_format requested but the model returns plain (non-JSON) text:
        // the best-effort parse leaves `structured` None and the query still
        // succeeds (the MemorySelector-tolerance contract).
        let body = serde_json::json!({
            "id": "msg_02b",
            "model": "claude-haiku-4-5",
            "content": [
                { "type": "text", "text": "sorry, no files" }
            ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })
        .to_string();

        let transport = Arc::new(StubTransport::new(body));
        let client = ProviderSideQueryClient::new("sk-test", None, transport);

        let resp = client
            .query(req(Some(serde_json::json!({"type": "json_schema"}))))
            .await
            .expect("query ok");

        // Non-JSON body + output_format -> structured stays None, no error.
        assert!(resp.structured.is_none());
        assert_eq!(resp.text.as_deref(), Some("sorry, no files"));
    }

    #[tokio::test]
    async fn ignored_content_blocks_are_dropped() {
        // A mixed-content response: thinking / server_tool_use / connector_text
        // / advisor_tool_result are all dropped, while the interleaved text and
        // tool_use survive.
        let tu = "11111111-2222-3333-4444-555555555555";
        let body = serde_json::json!({
            "id": "msg_mixed",
            "model": "claude-haiku-4-5",
            "content": [
                { "type": "thinking", "thinking": "let me think" },
                { "type": "text", "text": "answer" },
                { "type": "server_tool_use", "id": "srv1", "name": "advisor", "input": {} },
                { "type": "tool_use", "id": tu, "name": "search", "input": { "q": "x" } },
                { "type": "connector_text", "connector_text": "from connector" },
                { "type": "advisor_tool_result", "tool_use_id": "srv1", "content": {}, "is_error": false }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })
        .to_string();

        let transport = Arc::new(StubTransport::new(body));
        let client = ProviderSideQueryClient::new("sk-test", None, transport);

        let resp = client.query(req(None)).await.expect("query ok");

        // Only the `text` block reaches `text` (thinking / connector dropped).
        assert_eq!(resp.text.as_deref(), Some("answer"));
        // Only the client-side `tool_use` reaches `tool_calls` (server_tool_use
        // / advisor_tool_result dropped).
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0]["name"].as_str(), Some("search"));
    }

    #[tokio::test]
    async fn tool_use_response_surfaces_in_tool_calls() {
        // ToolUseId is `#[serde(transparent)]` over a UUID, so the wire id is a
        // bare UUID string.
        let tu = "11111111-2222-3333-4444-555555555555";
        let body = serde_json::json!({
            "id": "msg_03",
            "model": "claude-haiku-4-5",
            "content": [
                {
                    "type": "tool_use",
                    "id": tu,
                    "name": "search",
                    "input": { "q": "rust" }
                }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 4, "output_tokens": 2 }
        })
        .to_string();

        let transport = Arc::new(StubTransport::new(body));
        let client = ProviderSideQueryClient::new("sk-test", None, transport);

        let resp = client.query(req(None)).await.expect("query ok");

        // No text blocks -> flattened text is None.
        assert!(resp.text.is_none());
        assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(resp.tool_calls.len(), 1);
        let call = &resp.tool_calls[0];
        assert_eq!(call["name"].as_str(), Some("search"));
        assert_eq!(call["input"]["q"].as_str(), Some("rust"));
        // ToolUseId serialises transparently as the bare UUID string.
        assert_eq!(call["id"].as_str(), Some(tu));
    }

    #[tokio::test]
    async fn forwards_tool_choice_stop_sequences_and_thinking() {
        let body = serde_json::json!({
            "id": "msg_fwd", "model": "claude-haiku-4-5",
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn", "usage": { "input_tokens": 1, "output_tokens": 1 }
        })
        .to_string();
        let transport = Arc::new(StubTransport::new(body));
        let client = ProviderSideQueryClient::new("sk-test", None, transport.clone());
        let mut r = req(None);
        r.tool_choice = Some(serde_json::json!({ "type": "any" }));
        r.stop_sequences = vec!["STOP".into()];
        client.query(r).await.expect("query ok");
        let received = transport.received.lock().unwrap();
        let body: serde_json::Value =
            serde_json::from_str(received[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body["tool_choice"]["type"].as_str(),
            Some("any"),
            "any→required"
        );
        assert_eq!(body["stop_sequences"][0].as_str(), Some("STOP"));
    }

    /// cc 2.1.198 "Subagents + compaction inherit extended thinking config" —
    /// the COMPACTION seam half, wire level: a `Some` session thinking config
    /// resolves through the SAME `reasoning_for_request` rules as a main-loop
    /// request and lands as the request's `thinking` field.
    #[tokio::test]
    async fn session_thinking_config_rides_on_the_wire() {
        let body = serde_json::json!({
            "id": "msg_think", "model": "claude-opus-4-6",
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn", "usage": { "input_tokens": 1, "output_tokens": 1 }
        })
        .to_string();
        let transport = Arc::new(StubTransport::new(body));
        let client = ProviderSideQueryClient::new("sk-test", None, transport.clone());
        let mut r = req(None);
        // opus-4-6 is in the adaptive-thinking set → the session Adaptive
        // intent renders exactly like a main-loop turn: {"type":"adaptive"}.
        r.model = "claude-opus-4-6".into();
        r.query_source = QuerySource::Compaction;
        r.thinking = Some(llm_runtime::model::thinking::ThinkingConfig::default());
        r.effort = Some(serde_json::json!("high"));
        client.query(r).await.expect("query ok");
        let received = transport.received.lock().unwrap();
        let body: serde_json::Value =
            serde_json::from_str(received[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body["thinking"],
            serde_json::json!({ "type": "adaptive" }),
            "the inherited session config renders like a main-loop request"
        );
        assert_eq!(body["output_config"]["effort"], "high");
    }

    /// `thinking: None` (every utility caller) keeps the wire byte-identical
    /// to before the cc 2.1.198 inheritance seam: no `thinking` field at all.
    #[tokio::test]
    async fn no_thinking_config_keeps_legacy_wire() {
        let body = serde_json::json!({
            "id": "msg_legacy", "model": "claude-haiku-4-5",
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn", "usage": { "input_tokens": 1, "output_tokens": 1 }
        })
        .to_string();
        let transport = Arc::new(StubTransport::new(body));
        let client = ProviderSideQueryClient::new("sk-test", None, transport.clone());
        client.query(req(None)).await.expect("query ok");
        let received = transport.received.lock().unwrap();
        let body: serde_json::Value =
            serde_json::from_str(received[0].body.as_deref().unwrap()).unwrap();
        assert!(
            body.get("thinking").is_none(),
            "legacy callers must not grow a thinking field: {body}"
        );
    }

    /// M1b regression (found in M10): the model table MUST cover the current
    /// session defaults — a production compaction fork on `claude-sonnet-5`
    /// (the 2.1.197/198 default), `claude-opus-4-8` or `claude-fable-5-1` used
    /// to die with `LlmError::ModelUnavailable` before any request was sent.
    #[tokio::test]
    async fn current_default_models_resolve_for_compaction_forks() {
        for m in ["claude-sonnet-5", "claude-opus-4-8", "claude-fable-5-1"] {
            let body = serde_json::json!({
                "id": "msg_cur", "model": m,
                "content": [{ "type": "text", "text": "SUMMARY" }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            })
            .to_string();
            let transport = Arc::new(StubTransport::new(body));
            let client = ProviderSideQueryClient::new("sk-test", None, transport.clone());
            let mut r = req(None);
            r.model = m.into();
            r.query_source = QuerySource::Compaction;
            // Compaction forks inherit the session thinking config (cc
            // 2.1.198); all three models support (adaptive) thinking.
            r.thinking = Some(llm_runtime::model::thinking::ThinkingConfig::default());

            let resp = client
                .query(r)
                .await
                .unwrap_or_else(|e| panic!("{m} must resolve, got {e:?}"));
            assert_eq!(resp.text.as_deref(), Some("SUMMARY"), "{m}");

            // The request reached the wire with the right model AND the
            // adaptive thinking shape (all three are adaptive-thinking models).
            let received = transport.received.lock().unwrap();
            assert_eq!(received.len(), 1, "{m}");
            let body: serde_json::Value =
                serde_json::from_str(received[0].body.as_deref().unwrap()).unwrap();
            assert_eq!(body["model"].as_str(), Some(m), "{m}");
            assert_eq!(
                body["thinking"],
                serde_json::json!({ "type": "adaptive" }),
                "{m}: session thinking must ride like a main-loop request"
            );
        }
    }

    #[tokio::test]
    async fn api_error_maps_to_side_query_error_api() {
        // A 400 body that does not parse as a MessageResponse surfaces as
        // SideQueryError::Api (the codec maps the malformed body to LlmError;
        // the StubTransport returns 200 with a non-MessageResponse body to
        // exercise the decode-failure -> LlmError -> Api(..) path).
        let transport = Arc::new(StubTransport::new("not json"));
        let client = ProviderSideQueryClient::new("sk-test", None, transport);

        let err = client.query(req(None)).await.expect_err("should fail");
        assert!(matches!(err, SideQueryError::Api(_)), "got {err:?}");
    }

    fn strict_req() -> crate::side_query::StrictStructuredQueryRequest {
        crate::side_query::StrictStructuredQueryRequest {
            model_attempt: None,
            model: "claude-sonnet-4-20250514".into(),
            profile: Some("anthropic".into()),
            system_prompt: Some("system".into()),
            messages: vec![ConversationMessage::user(MessageId::new(), "hi".into())],
            schema: serde_json::json!({
                "type": "object",
                "properties": { "ok": { "type": "boolean" } },
                "required": ["ok"],
                "additionalProperties": false
            }),
            max_tokens: 256,
            temperature: Some(0.0),
            query_source: QuerySource::FusionAnalyst,
            skip_system_prompt_prefix: false,
        }
    }

    /// `query_json_schema` on the Session backend is a STREAMING call
    /// (`build_request(..., stream=true, ..)` -> `ApiService::drive_stream`),
    /// so it needs a scripted [`llm_runtime::Transport::open_stream`], not
    /// [`StubTransport`]'s non-streaming `request` (whose `stream_sse` always
    /// errors — driving these tests through it hangs behind the streaming
    /// connect-phase retry/backoff loop instead of failing fast). This mirrors
    /// `llm-runtime/tests/transport_stream_test.rs`'s `StreamTransport` /
    /// `ScriptedFrames` pattern, one layer below the SSE-byte-stream bridge,
    /// and captures each request's decoded `body_json` for wire assertions.
    struct ScriptedFrames {
        items:
            std::collections::VecDeque<Result<llm_runtime::RawStreamFrame, llm_runtime::LlmError>>,
    }

    impl llm_runtime::FrameStream for ScriptedFrames {
        fn next_frame(
            &mut self,
        ) -> llm_runtime::BoxFuture<
            '_,
            Result<Option<llm_runtime::RawStreamFrame>, llm_runtime::LlmError>,
        > {
            let next = match self.items.pop_front() {
                Some(Ok(frame)) => Ok(Some(frame)),
                Some(Err(error)) => Err(error),
                None => Ok(None),
            };
            Box::pin(async move { next })
        }
    }

    struct StreamStubTransport {
        frames: Vec<String>,
        received_bodies: Mutex<Vec<serde_json::Value>>,
    }

    impl StreamStubTransport {
        /// One scripted streaming response: a text-content Anthropic SSE
        /// sequence carrying `text` as its single content block, reusable
        /// across every scripted call this transport receives.
        fn text_response(text: &str) -> Self {
            Self {
                frames: vec![
                    serde_json::json!({
                        "type": "message_start",
                        "message": {
                            "id": "msg_stream",
                            "model": "claude-sonnet-4-20250514",
                            "content": [],
                            "usage": { "input_tokens": 1, "output_tokens": 0 }
                        }
                    })
                    .to_string(),
                    serde_json::json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" }
                    })
                    .to_string(),
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": { "type": "text_delta", "text": text }
                    })
                    .to_string(),
                    serde_json::json!({ "type": "content_block_stop", "index": 0 }).to_string(),
                    serde_json::json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": "end_turn" },
                        "usage": { "input_tokens": 1, "output_tokens": 1 }
                    })
                    .to_string(),
                    serde_json::json!({ "type": "message_stop" }).to_string(),
                ],
                received_bodies: Mutex::new(Vec::new()),
            }
        }
    }

    impl llm_runtime::Transport for StreamStubTransport {
        fn execute<'a>(
            &'a self,
            _request: &'a llm_runtime::ProviderRequest,
        ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::ProviderResponse, llm_runtime::LlmError>>
        {
            Box::pin(async move {
                Err(llm_runtime::LlmError::Transport {
                    message: "execute not scripted; this stub only serves streaming calls".into(),
                })
            })
        }

        fn open_stream<'a>(
            &'a self,
            request: &'a llm_runtime::ProviderRequest,
        ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::StreamingResponse, llm_runtime::LlmError>>
        {
            self.received_bodies
                .lock()
                .unwrap()
                .push(request.body_json.clone());
            let items = self
                .frames
                .iter()
                .map(|payload| {
                    Ok(llm_runtime::RawStreamFrame::new(
                        payload.as_bytes().to_vec(),
                    ))
                })
                .collect();
            Box::pin(async move {
                Ok(llm_runtime::StreamingResponse {
                    status: 200,
                    headers: std::collections::BTreeMap::new(),
                    frames: Box::new(ScriptedFrames { items }),
                })
            })
        }
    }

    /// Build a Session-backend client whose one registered model declares
    /// `structured_output: true` — the capability
    /// [`sidequery_model_table`]'s Direct-backend catalog never sets, and the
    /// capability every real Fusion analyst call requires (fusion's own
    /// `model_resolver` only selects `judge_eligible` catalog entries). This
    /// is the same backend production Fusion analyst calls use
    /// ([`ProviderSideQueryClient::from_service`]), so these tests exercise
    /// the real `query_json_schema` decode path, not the isolated-utility one.
    fn structured_session_client(
        transport: Arc<StreamStubTransport>,
        analytics: Option<Arc<telemetry::AnalyticsBus>>,
    ) -> ProviderSideQueryClient {
        structured_session_client_with_parent_forced_tool_choice(transport, analytics, false)
    }

    fn structured_session_client_with_parent_forced_tool_choice(
        transport: Arc<StreamStubTransport>,
        analytics: Option<Arc<telemetry::AnalyticsBus>>,
        parent_forced_tool_choice: bool,
    ) -> ProviderSideQueryClient {
        let config = ClientConfig {
            providers: vec![ProviderProfile {
                wire_profile: None,
                regions: llm_runtime::Region::all(),
                provider_id: ProviderId::AnthropicFirstParty,
                profile_name: "anthropic".to_string(),
                base_url: DEFAULT_BASE_URL.to_string(),
                protocol: ProtocolFamily::AnthropicMessages,
                auth: AuthStrategy::OAuthBearer,
                credential: CredentialConfig::Static {
                    id: "session-key".to_string(),
                },
                models: vec![ModelProfile {
                    display_model: "claude-sonnet-4-20250514".to_string(),
                    request_model: "claude-sonnet-4-20250514".to_string(),
                    billing_model: "claude-sonnet-4".to_string(),
                    aliases: vec![],
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities {
                        streaming: true,
                        tools: true,
                        reasoning: true,
                        structured_output: true,
                        ..Capabilities::default()
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
        };
        let session_client = DefaultLlmClient::from_config(config)
            .expect("session client config")
            .with_credential_provider(Arc::new(StaticCredentialProvider::new(
                Credential::BearerToken("session-oauth-token".to_string()),
            )));
        let session_transport: Arc<dyn llm_runtime::Transport> = transport;
        let service = llm_runtime::ApiService::new(
            Arc::new(session_client),
            session_transport,
            llm_runtime::SubscriberState::default(),
            llm_runtime::model::user_agent::UserAgentEnv::default(),
            "test",
            analytics,
            None,
        );
        let service = if parent_forced_tool_choice {
            service.with_forced_tool_choice(llm_runtime::ToolChoice::Tool {
                name: "StructuredOutput".to_string(),
            })
        } else {
            service
        };
        ProviderSideQueryClient::from_service(Arc::new(service))
    }

    /// Parent structured-output settings must not leak into an independent
    /// empty-tool JSON-schema side query.
    #[tokio::test]
    async fn session_json_schema_does_not_inherit_parent_forced_tool_choice() {
        let transport = Arc::new(StreamStubTransport::text_response("{\"ok\":true}"));
        let client =
            structured_session_client_with_parent_forced_tool_choice(transport.clone(), None, true);
        let request = strict_req();
        let expected_schema = request.schema.clone();

        let response = client
            .query_json_schema(request)
            .await
            .expect("session structured query succeeds");
        assert_eq!(response.value, serde_json::json!({ "ok": true }));

        let received = transport.received_bodies.lock().unwrap();
        assert_eq!(received.len(), 1);
        let body = &received[0];
        assert!(
            body.get("tools").is_none(),
            "JSON-schema side query must not send tools: {body}"
        );
        assert!(
            body.get("tool_choice").is_none(),
            "parent forced StructuredOutput choice leaked into side query: {body}"
        );
        assert_eq!(
            body["output_config"]["format"],
            serde_json::json!({ "type": "json_schema", "schema": expected_schema }),
            "structured response format must remain on the wire"
        );
    }

    /// F003: `query_json_schema` decodes the accumulated text as a strict
    /// `serde_json::from_str` — a complete, standalone JSON value.
    #[tokio::test]
    async fn query_json_schema_decodes_a_complete_valid_json_value() {
        let transport = Arc::new(StreamStubTransport::text_response("{\"ok\":true}"));
        let client = structured_session_client(transport, None);

        let resp = client
            .query_json_schema(strict_req())
            .await
            .expect("a complete JSON value decodes");
        assert_eq!(resp.value, serde_json::json!({ "ok": true }));
    }

    #[tokio::test]
    async fn query_json_schema_rejects_trailing_text_after_the_json_value() {
        // The model appended prose after an otherwise-valid JSON object —
        // `serde_json::from_str` rejects trailing non-whitespace, so this must
        // surface as a decode failure, not a silently-truncated parse.
        let transport = Arc::new(StreamStubTransport::text_response(
            "{\"ok\":true} hope that helps!",
        ));
        let client = structured_session_client(transport, None);

        let err = client
            .query_json_schema(strict_req())
            .await
            .expect_err("trailing prose after the JSON value must not decode");
        assert!(
            matches!(err, SideQueryError::InvalidResponse(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn query_json_schema_rejects_truncated_json() {
        // A response cut off mid-value (e.g. hit max_tokens) is not valid JSON.
        let transport = Arc::new(StreamStubTransport::text_response("{\"ok\":tr"));
        let client = structured_session_client(transport, None);

        let err = client
            .query_json_schema(strict_req())
            .await
            .expect_err("truncated JSON must not decode");
        assert!(
            matches!(err, SideQueryError::InvalidResponse(_)),
            "got {err:?}"
        );
    }

    #[derive(Default)]
    struct SchemaAttemptProbe {
        usages: Mutex<
            Vec<(
                llm_runtime::Usage,
                llm_runtime::ModelAttemptUsageCompleteness,
            )>,
        >,
        settled: std::sync::atomic::AtomicBool,
    }

    struct SchemaAttemptLease(Arc<SchemaAttemptProbe>);
    struct SchemaAttemptHooks(Arc<SchemaAttemptProbe>);

    #[async_trait]
    impl llm_runtime::ModelAttemptHooks for SchemaAttemptHooks {
        async fn begin(
            &self,
            _: &platform_api::ModelAttemptContext,
            _: &LlmRequest,
            _: &llm_runtime::PreparedLlmCall,
        ) -> Result<Box<dyn llm_runtime::ModelAttemptLease>, LlmError> {
            Ok(Box::new(SchemaAttemptLease(self.0.clone())))
        }
    }

    impl llm_runtime::ModelAttemptLease for SchemaAttemptLease {
        fn mark_dispatched(&mut self) -> Result<(), LlmError> {
            Ok(())
        }
        fn observe_usage(
            &mut self,
            usage: &llm_runtime::Usage,
            completeness: llm_runtime::ModelAttemptUsageCompleteness,
        ) {
            self.0
                .usages
                .lock()
                .unwrap()
                .push((usage.clone(), completeness));
        }
        fn finish(self: Box<Self>) -> Box<dyn llm_runtime::ModelAttemptSettlement> {
            self
        }
    }

    #[async_trait]
    impl llm_runtime::ModelAttemptSettlement for SchemaAttemptLease {
        async fn wait(self: Box<Self>) -> Result<(), LlmError> {
            self.0
                .settled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn registered_schema_error_preserves_actual_usage_and_settles_before_parse() {
        let transport = Arc::new(StreamStubTransport::text_response("{\"ok\":tr"));
        let client = structured_session_client(transport.clone(), None);
        let probe = Arc::new(SchemaAttemptProbe::default());
        let ProviderSideQueryBackend::Session(service) = &client.backend else {
            unreachable!()
        };
        service.set_model_attempt_hooks(Arc::new(SchemaAttemptHooks(probe.clone())));
        let mut request = strict_req();
        request.model_attempt = Some(
            platform_api::ModelAttemptRun::new(Arc::new(()))
                .context(platform_api::ModelAttemptStage::Analyst, None)
                .unwrap(),
        );
        assert!(matches!(
            client.query_json_schema(request).await,
            Err(SideQueryError::InvalidResponse(_))
        ));
        assert!(probe.settled.load(std::sync::atomic::Ordering::SeqCst));
        let usages = probe.usages.lock().unwrap();
        let (usage, completeness) = usages.last().unwrap();
        assert_eq!(usage.billable_tokens.input, 1);
        assert_eq!(usage.billable_tokens.output, 1);
        assert_eq!(
            *completeness,
            llm_runtime::ModelAttemptUsageCompleteness::Complete
        );
        assert_eq!(transport.received_bodies.lock().unwrap().len(), 1);
    }

    /// F003: the Session backend previously dropped `temperature` and
    /// `query_source` on `query_json_schema` — `ApiService::stream_json_schema`
    /// took neither parameter, so the analyst's `temperature: Some(0.0)` never
    /// reached the wire and no `tengu_api_query_source` telemetry fired.
    #[tokio::test]
    async fn session_backend_forwards_temperature_and_query_source_for_structured_queries() {
        let transport = Arc::new(StreamStubTransport::text_response("{\"ok\":true}"));

        let sink = Arc::new(telemetry::InMemorySink::new());
        let bus = Arc::new(telemetry::AnalyticsBus::new());
        bus.attach_sink(sink.clone()).await;

        let client = structured_session_client(transport.clone(), Some(bus));

        let result = client
            .query_json_schema(strict_req())
            .await
            .expect("session structured query succeeds");
        assert_eq!(result.value, serde_json::json!({ "ok": true }));

        let received = transport.received_bodies.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(
            received[0]["temperature"].as_f64(),
            Some(0.0),
            "analyst temperature 0.0 must reach the wire: {}",
            received[0]
        );
        // F003 round 2: a strict structured query must never pair a
        // `temperature` override with the session's `thinking` config —
        // Anthropic rejects `temperature != 1` while extended thinking is
        // enabled, and `ThinkingConfig::default()` is `Adaptive` (session
        // thinking ON) so this pairing is live in the default desktop
        // configuration. The analyst contract is temperature=0, no
        // `thinking` field at all — same as `SideQueryRequest{thinking:
        // None}` used elsewhere.
        assert!(
            received[0].get("thinking").is_none(),
            "a temperature override must not be sent alongside a `thinking` \
             block — the analyst call must carry neither or override both: {}",
            received[0]
        );

        let events = sink.events().await;
        let query_source_event = events
            .iter()
            .find(|event| event.name == "tengu_api_query_source")
            .expect("tengu_api_query_source telemetry must fire for the analyst call");
        assert!(
            matches!(
                query_source_event.metadata.get("querySource"),
                Some(telemetry::AnalyticsValue::String(v)) if v == "fusion_analyst"
            ),
            "got {:?}",
            query_source_event.metadata.get("querySource")
        );
    }

    /// Round-3 review finding 4: a model whose vendored `models.dev` row says
    /// temperature control is unsupported
    /// (`ModelListing.metadata.temperature_control == Some(false)`, the value
    /// every listed gpt-5.6-*/kimi-* judge row in the real catalog carries)
    /// must never receive an explicit `temperature` override on the wire —
    /// otherwise the provider rejects the whole call outright and `analyze`
    /// deliberately does not retry a `SideQueryError::Api`, degrading the
    /// entire run to `AnalysisFailed` after every panel is already billed.
    #[tokio::test]
    async fn session_backend_withholds_temperature_from_a_model_that_declares_it_unsupported() {
        let transport = Arc::new(StreamStubTransport::text_response("{\"ok\":true}"));
        let config = ClientConfig {
            providers: vec![ProviderProfile {
                wire_profile: None,
                regions: llm_runtime::Region::all(),
                provider_id: ProviderId::AnthropicFirstParty,
                profile_name: "anthropic".to_string(),
                base_url: DEFAULT_BASE_URL.to_string(),
                protocol: ProtocolFamily::AnthropicMessages,
                auth: AuthStrategy::OAuthBearer,
                credential: CredentialConfig::Static {
                    id: "session-key".to_string(),
                },
                models: vec![ModelProfile {
                    display_model: "claude-sonnet-4-20250514".to_string(),
                    request_model: "claude-sonnet-4-20250514".to_string(),
                    billing_model: "claude-sonnet-4".to_string(),
                    aliases: vec![],
                    description: None,
                    // The vendored capability bit this fix must consult —
                    // real gpt-5.6-*/kimi-* judge rows carry this exact
                    // value (`llm-runtime/data/models-dev/openai.json` etc.,
                    // mapped by `catalog::map::to_metadata`).
                    metadata: platform_api::ModelMetadata {
                        temperature_control: Some(false),
                        ..Default::default()
                    },
                    capabilities: Capabilities {
                        streaming: true,
                        tools: true,
                        reasoning: true,
                        structured_output: true,
                        ..Capabilities::default()
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
        };
        let session_client = DefaultLlmClient::from_config(config)
            .expect("session client config")
            .with_credential_provider(Arc::new(StaticCredentialProvider::new(
                Credential::BearerToken("session-oauth-token".to_string()),
            )));
        let session_transport: Arc<dyn llm_runtime::Transport> = transport.clone();
        let service = Arc::new(llm_runtime::ApiService::new(
            Arc::new(session_client),
            session_transport,
            llm_runtime::SubscriberState::default(),
            llm_runtime::model::user_agent::UserAgentEnv::default(),
            "test",
            None,
            None,
        ));
        let client = ProviderSideQueryClient::from_service(service);

        let result = client
            .query_json_schema(strict_req())
            .await
            .expect("session structured query succeeds even though temperature is withheld");
        assert_eq!(result.value, serde_json::json!({ "ok": true }));

        let received = transport.received_bodies.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert!(
            received[0].get("temperature").is_none(),
            "a model whose catalog row declares temperature control unsupported \
             must never receive an explicit temperature override: {}",
            received[0]
        );
    }
}
