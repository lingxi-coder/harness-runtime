//! Thin orchestrator-seam adapter over [`llm_runtime::ApiService`].
//!
//! The provider drive loop (prepare → header injection → `transport.execute()` /
//! `open_stream()` → `codec.decode_response()` + the retry/rate-limit/betas
//! machinery) lives in `llm_runtime::ApiService`. This module holds an
//! `Arc<ApiService>` and implements the orchestrator's seam traits
//! ([`OrchestratorApiClient`], [`StreamingApiClient`], [`agent::SubagentApiClient`])
//! by delegating each method 1:1 — the only orchestrator-domain logic kept here is
//! the catalog→`ModelListing` projection and the `RateLimitSnapshot` mapping.

use crate::conversation::{OrchestratorApiClient, OrchestratorApiRequest, StreamingApiClient};
use crate::model::rate_limit::{RateLimitInfo, RawUtilization};
use async_trait::async_trait;
use futures::stream::BoxStream;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ConversationMessage};
use llm_runtime::{HistoryEvent, HistoryResponse, LlmError, MediaDelegationAccounting};
use std::sync::Arc;

tokio::task_local! {
    static MOD_TURN_STEP_EFFORT: serde_json::Value;
}

/// Scope a Mod's effort rewrite to this model request. A session-wide setter
/// would affect concurrently running side queries and background work.
pub(crate) async fn with_mod_turn_step_effort<T>(
    effort: Option<&str>,
    future: impl std::future::Future<Output = T>,
) -> T {
    if let Some(effort) = effort {
        MOD_TURN_STEP_EFFORT
            .scope(
                serde_json::Value::String(effort.to_owned()),
                llm_runtime::with_mod_request_effort(effort, future),
            )
            .await
    } else {
        future.await
    }
}

/// Current subscription seed used by composition roots when building the
/// provider service; execution subsequently reads its live subscription slot.
pub use llm_runtime::SubscriberState;

/// Host resolver for the current source route's model, catalog, launch and
/// feedback facts. The returned snapshot owns every fact needed after the
/// provider stream has advanced to another serving model.
pub type RefusalApiTextSnapshotSource = Arc<
    dyn Fn(
            &str,
            Option<&str>,
        ) -> Result<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot, LlmError>
        + Send
        + Sync,
>;

/// Production adapter: a thin holder of [`llm_runtime::ApiService`] (which owns the
/// provider drive loop) that implements the orchestrator's seam traits.
pub struct ProviderApiAdapter {
    service: Arc<llm_runtime::ApiService>,
    refusal_api_text_snapshot_source: Option<RefusalApiTextSnapshotSource>,
    /// Explicit session level. An absent launch value inherits the admitted
    /// settings table; an explicit automatic choice is held by the service.
    initial_effort: std::sync::RwLock<Option<serde_json::Value>>,
    /// (`/fast`) Session-scoped fast-mode toggle, shared (same `Arc`) with the
    /// [`ConversationOrchestrator`] so the handle's `set_fast_mode` flip is seen
    /// here on the next turn. When set and the resolved route admits the
    /// current native Fast policy, the MAIN-loop stream sends
    /// `speed:"fast"`. The
    /// default flag is always `false`, so bodies stay byte-identical until a
    /// live `/fast` toggle flips it.
    fast_mode: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ProviderApiAdapter {
    /// Wrap a constructed [`llm_runtime::ApiService`]. The composition roots build
    /// the service via `ApiService::new_with_routing(...).with_*(...)` and hand the
    /// `Arc` here; every trait method delegates 1:1 to it.
    #[must_use]
    pub fn new(service: Arc<llm_runtime::ApiService>) -> Self {
        Self {
            service,
            refusal_api_text_snapshot_source: None,
            initial_effort: std::sync::RwLock::new(None),
            fast_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Attach the embedding host's current refusal-fact resolver. Both main
    /// and child API clients use this same source without deriving policy
    /// from the model allowlist or from a provider endpoint.
    #[must_use]
    pub fn with_refusal_api_text_snapshot_source(
        mut self,
        source: RefusalApiTextSnapshotSource,
    ) -> Self {
        self.refusal_api_text_snapshot_source = Some(source);
        self
    }

    fn resolved_refusal_api_text_snapshot(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot>, LlmError> {
        self.refusal_api_text_snapshot_source
            .as_ref()
            .map(|source| source(model, profile))
            .transpose()
    }

    /// (`/fast`) Share the session's fast-mode flag with this adapter (the same
    /// `Arc<AtomicBool>` the [`ConversationOrchestrator`] holds). When the flag
    /// is set and the active model supports fast mode, the MAIN-loop stream
    /// carries `speed:"fast"`. Without this the flag is a private always-`false`
    /// default, so bodies are byte-identical.
    #[must_use]
    pub fn with_fast_mode(mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.fast_mode = flag;
        self
    }

    /// (M4 cc2.1.198) Set the session's initial effort (CLI `--effort`,
    /// already validated/normalized by the CLI to one of
    /// low/medium/high/xhigh/max). The MAIN-loop [`StreamingApiClient::stream`]
    /// impl then carries it as `output_config.effort` (the service adds the
    /// `effort-2025-11-24` beta whenever the body has effort). Subagent calls
    /// keep their own per-spawn effort resolution and are unaffected.
    #[must_use]
    pub fn with_initial_effort(mut self, effort: Option<serde_json::Value>) -> Self {
        if let Some(value) = &effort {
            self.service
                .set_session_effort(lingxi_core::host::effort_table::SessionEffort::Level(
                    value.clone(),
                ));
        }
        self.initial_effort = std::sync::RwLock::new(effort);
        self
    }

    fn current_effort(&self) -> Option<serde_json::Value> {
        if let Ok(effort) = MOD_TURN_STEP_EFFORT.try_with(Clone::clone) {
            return Some(effort);
        }
        self.initial_effort
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn build_scheduled_request(
        &self,
        mut settings: crate::scheduled_turn::ScheduledSettings,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        skip_global_cache_for_system_prompt: bool,
        messages: Vec<ConversationMessage>,
        tools: Vec<llm_runtime::ToolDeclaration>,
        max_tokens: Option<u32>,
        tool_result_reminder_suffixes: Vec<llm_runtime::convert::AnthropicToolResultReminderSuffix>,
        typed_attachment_ids: std::collections::HashSet<lingxi_core::types::MessageId>,
        task_deliveries: std::collections::HashMap<
            lingxi_core::types::MessageId,
            lingxi_core::host::task_delivery::TaskDeliveryRequest,
        >,
        model_source: Option<Utf16JsonProjection>,
    ) -> Result<llm_runtime::LlmRequest, LlmError> {
        let route = crate::query_model::current();
        if let Some(route) = route.as_ref() {
            settings.model.clone_from(&route.model);
        }
        let profile = route
            .as_ref()
            .map_or(Some(settings.provider.as_str()), |route| {
                route.profile.as_deref()
            });
        let mut request = llm_runtime::MessagesCreateRequest::new(
            &settings.model,
            profile,
            system.cloned(),
            messages,
            tools,
        );
        request.opts.max_output_tokens = max_tokens;
        request.opts.tool_result_reminder_suffixes = tool_result_reminder_suffixes;
        request.opts.typed_attachment_ids = typed_attachment_ids;
        request.opts.task_deliveries = task_deliveries;
        request.opts.skip_global_cache_for_system_prompt = skip_global_cache_for_system_prompt;
        request.opts.query_source = Some("scheduled_task".into());
        request.opts.model_source = model_source;
        self.service
            .build_scheduled_request(request, settings.thinking, settings.effort)
    }

    /// Return the most recently observed rate-limit header snapshot (the internal
    /// nine-field [`RateLimitInfo`]). Delegates to the service. Kept as an inherent
    /// method so both the `OrchestratorApiClient::last_rate_limit_info` (three-field
    /// projection) and `last_rate_limit_full` (full snapshot) trait overrides can
    /// reach it.
    #[must_use]
    pub fn last_rate_limit_info(&self) -> Option<RateLimitInfo> {
        self.service.last_rate_limit_info()
    }
}
// ── Trait implementations ─────────────────────────────────────────────────────

fn mcp_content_text_blocks(content: &Utf16JsonProjection) -> Option<Vec<ContentBlock>> {
    fn text_block(units: &[u16]) -> ContentBlock {
        ContentBlock::TextJsUtf16 {
            text: String::from_utf16_lossy(units),
            utf16_code_units: units.to_vec(),
            citations: None,
        }
    }

    let root = content.at_pointer("")?;
    if let Some(units) = root.code_units() {
        return Some(vec![text_block(units)]);
    }

    let length = root.array_len()?;
    let blocks = (0..length)
        .filter_map(|index| {
            let block = root.get_index(index)?;
            if block.get_property("type").and_then(|value| value.as_str()) != Some("text") {
                return None;
            }
            let text = block
                .get_property("text")
                .and_then(|value| value.code_units())
                .unwrap_or_default();
            Some(text_block(text))
        })
        .collect::<Vec<_>>();
    (!blocks.is_empty()).then_some(blocks)
}

fn hosted_search_content_source(
    block: &llm_runtime::ContentBlock,
) -> Option<lingxi_llm_client::protocol::JsonPayload> {
    use lingxi_llm_client::protocol::JsonPayload;

    fn object(fields: &[(&str, JsonPayload)]) -> Option<JsonPayload> {
        let mut payload = JsonPayload::parse("{}").ok()?;
        for (name, value) in fields {
            payload = payload.with_object_field(name, value).ok()?;
        }
        Some(payload)
    }

    match block {
        llm_runtime::ContentBlock::ProviderContent { value, .. } => Some(value.clone()),
        llm_runtime::ContentBlock::ServerToolUse { id, name, .. } => object(&[
            ("type", JsonPayload::string("server_tool_use")),
            ("id", JsonPayload::string(id)),
            ("name", JsonPayload::string(name)),
        ]),
        llm_runtime::ContentBlock::Text { text, .. } => object(&[
            ("type", JsonPayload::string("text")),
            ("text", JsonPayload::string(text)),
        ]),
        llm_runtime::ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => object(&[
            ("type", JsonPayload::string("text")),
            (
                "text",
                JsonPayload::string_from_utf16_code_units(utf16_code_units),
            ),
        ]),
        _ => None,
    }
}

fn hosted_search_domain_payload(
    source: &Utf16JsonProjection,
) -> Option<lingxi_llm_client::protocol::JsonPayload> {
    let source = source.to_json_string().ok()?;
    let payload = lingxi_llm_client::protocol::JsonPayload::parse(&source).ok()?;
    payload.is_root_string().then_some(payload)
}

fn hosted_search_input_error(message: &str) -> tool_api::HostedSearchError {
    tool_api::HostedSearchError {
        message: Utf16JsonProjection::from(message),
        http_status: None,
        timeout: false,
    }
}

#[cfg(test)]
mod mcp_token_projection_tests {
    use super::*;

    #[test]
    fn root_text_keeps_lone_utf16_units_for_token_counting() {
        let content = Utf16JsonProjection::parse(r#""a\ud800b""#).unwrap();
        let blocks = mcp_content_text_blocks(&content).unwrap();
        assert_eq!(
            blocks,
            vec![ContentBlock::TextJsUtf16 {
                text: "a�b".into(),
                utf16_code_units: vec![b'a' as u16, 0xd800, b'b' as u16],
                citations: None,
            }]
        );
    }

    #[test]
    fn content_array_counts_only_text_blocks_without_lowering_units() {
        let content = Utf16JsonProjection::parse(
            r#"[{"type":"text","text":"x\udfff"},{"type":"image","text":"ignored"},{"type":"text"}]"#,
        )
        .unwrap();
        let blocks = mcp_content_text_blocks(&content).unwrap();
        assert_eq!(
            blocks,
            vec![
                ContentBlock::TextJsUtf16 {
                    text: "x�".into(),
                    utf16_code_units: vec![b'x' as u16, 0xdfff],
                    citations: None,
                },
                ContentBlock::TextJsUtf16 {
                    text: String::new(),
                    utf16_code_units: Vec::new(),
                    citations: None,
                },
            ]
        );
    }
}

#[async_trait]
impl tool_api::McpTokenCounter for ProviderApiAdapter {
    async fn count_mcp_content_tokens(
        &self,
        model: &str,
        content: &Utf16JsonProjection,
    ) -> Result<Option<u64>, String> {
        let Some(blocks) = mcp_content_text_blocks(content) else {
            return Ok(None);
        };
        let message = ConversationMessage::User {
            id: lingxi_core::types::MessageId::new(),
            content: blocks,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
        };
        self.service
            .count_tokens_exact(model, None, None, vec![message], Vec::new())
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl OrchestratorApiClient for ProviderApiAdapter {
    fn supports_native_mid_conversation_system(&self, model: &str, profile: Option<&str>) -> bool {
        self.service
            .supports_native_mid_conversation_system(model, profile)
    }

    fn native_mid_conversation_tool_change_supported(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Option<bool> {
        self.service
            .native_mid_conversation_tool_change_supported(model, profile)
    }

    fn native_tool_change_feature_gates(&self) -> (bool, bool) {
        self.service.native_tool_change_feature_gates()
    }

    fn next_output_budget(
        &self,
        model: &str,
        profile: Option<&str>,
        usage: &llm_runtime::ExecutionUsage,
    ) -> Result<Option<u32>, LlmError> {
        self.service.next_output_budget(model, profile, usage)
    }

    fn is_first_party_route(&self, model: &str, profile: Option<&str>) -> bool {
        self.service
            .is_first_party_route(model, profile)
            .unwrap_or(false)
    }

    fn protocol_for_model(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Option<llm_runtime::ProtocolFamily> {
        self.service.protocol_for_model(model, profile).ok()
    }

    fn native_model_option_capability_for_request(
        &self,
        model: &str,
        profile: Option<&str>,
        capability: &str,
    ) -> Option<bool> {
        let mut request = llm_runtime::LlmRequest::new(model);
        request.profile = profile.map(str::to_owned);
        self.service
            .native_model_option_capability_for_request(&request, capability)
            .ok()
            .flatten()
    }

    fn native_thinking_resumption_refused(&self) -> Option<bool> {
        Some(self.service.native_thinking_resumption_refused())
    }

    fn foundry_tool_search_supported(
        &self,
        model: &str,
        profile: Option<&str>,
        model_source: Option<&Utf16JsonProjection>,
    ) -> Option<bool> {
        let source_units = model_source.and_then(|source| source.string_units(""));
        self.service
            .foundry_tool_search_supported(model, profile, source_units.as_deref())
    }

    fn model_validation_custom_model_option(&self) -> Option<Vec<u16>> {
        self.service.model_validation_custom_model_option()
    }

    fn model_validation_account_generation(&self) -> Option<u64> {
        Some(self.service.model_validation_account_generation())
    }

    async fn credential_source(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<llm_runtime::CredentialSource, LlmError> {
        self.service.credential_source(model, profile).await
    }

    async fn classifier_model_route_facts(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<llm_runtime::model::classifier_model_policy::ClassifierModelRouteFacts, LlmError>
    {
        self.service
            .classifier_model_route_facts(model, profile)
            .await
    }

    fn native_computer_provider(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Option<lingxi_llm_client::protocol::computer::NativeComputerProvider> {
        use lingxi_llm_client::protocol::{computer::NativeComputerProvider, ProtocolFamily};
        match self.service.protocol_for_model(model, profile).ok()? {
            ProtocolFamily::OpenAiResponses => Some(NativeComputerProvider::OpenAi),
            ProtocolFamily::AnthropicMessages => Some(NativeComputerProvider::Anthropic),
            ProtocolFamily::GeminiInteractions => Some(NativeComputerProvider::Gemini),
            _ => None,
        }
    }

    fn prompt_snapshot_source_vector(
        &self,
        model: &str,
        profile: Option<&str>,
        sections: &[lingxi_llm_client::providers::anthropic::system_prompt::SourceSection],
    ) -> Vec<lingxi_llm_client::providers::anthropic::system_prompt::PromptText> {
        self.service
            .prompt_snapshot_source_vector(model, profile, sections)
    }

    fn refusal_api_text_snapshot(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot>, LlmError> {
        self.resolved_refusal_api_text_snapshot(model, profile)
    }

    async fn validate_fast_enable(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<(), LlmError> {
        self.service.validate_fast_enable(model, profile).await
    }

    async fn probe_model(
        &self,
        route_model: &str,
        profile: Option<&str>,
        source_model: &Utf16JsonProjection,
    ) -> Result<(), LlmError> {
        let source_units =
            source_model
                .string_units("")
                .ok_or_else(|| LlmError::InvalidRequest {
                    message: "model source must be a JSON string projection".to_owned(),
                })?;
        let mut request = llm_runtime::LlmRequest::new(route_model).with_user_text("Hi");
        request.profile = profile.map(str::to_owned);
        request.input.model_source = Some(
            lingxi_llm_client::protocol::JsonPayload::string_from_utf16_code_units(&source_units),
        );
        request.input.max_tokens = Some(1);
        request
            .input
            .prompt_cache
            .breakpoints
            .push(lingxi_llm_client::protocol::CacheBreakpoint {
                scope: None,
                position: lingxi_llm_client::protocol::CachePosition::Message {
                    index: 0,
                    block: 0,
                },
                ttl: lingxi_llm_client::protocol::CacheTtl::FiveMinutes,
            });

        self.service
            .execute_model_validation_request(request)
            .await
            .map(|_| ())
    }

    fn context_budget(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<Option<llm_runtime::ResolvedContext>, LlmError> {
        self.service.resolve_context(model, profile).map(Some)
    }

    fn context_window_tokens(&self, model: &str, profile: Option<&str>) -> Option<u64> {
        let context = self.service.resolve_context(model, profile).ok()?;
        let native_claude = self.is_first_party_route(model, profile)
            && llm_runtime::model::context_window::is_claude_family(model);
        if !native_claude {
            return context.context_window_tokens;
        }

        let native_window = || {
            llm_runtime::model::context_window::context_window_for_model(
                model,
                self.service.active_custom_betas(),
            )
        };
        match context.context_window_tokens {
            Some(_)
                if matches!(
                    &context.context_window_source,
                    lingxi_llm_client::protocol::ContextWindowSource::Catalog
                        | lingxi_llm_client::protocol::ContextWindowSource::Observation
                ) =>
            {
                // Catalog and provider-observation windows are route facts, but
                // the first-party Claude workflow window follows Native's
                // selected model and beta policy. This intentionally does not
                // narrow ApiService's raw/effective context result.
                Some(native_window())
            }
            Some(window) => Some(window),
            // Preserve the established first-party fallback for profiles with
            // no published context window, independent of provenance.
            None => Some(native_window()),
        }
    }

    fn active_betas(&self) -> Vec<String> {
        self.service.active_custom_betas().to_vec()
    }

    fn set_thinking_config(&self, thinking: llm_runtime::model::thinking::ThinkingConfig) {
        self.service.set_thinking(thinking);
    }

    fn set_max_thinking_tokens_override(
        &self,
        value: Option<llm_runtime::model::thinking_state::NativeIntegerNumber>,
    ) {
        self.service.set_max_thinking_tokens_override(value);
    }

    fn set_effort(&self, effort: lingxi_core::host::effort_table::SessionEffort) {
        self.service.set_session_effort(effort.clone());
        let effort = match effort {
            lingxi_core::host::effort_table::SessionEffort::Level(value) => Some(value),
            _ => None,
        };
        *self
            .initial_effort
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = effort;
    }

    fn effort_command_snapshot(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<Option<lingxi_core::host::effort::EffortCommandSnapshot>, LlmError> {
        self.service.effort_command_snapshot(model, profile)
    }

    async fn messages_create_buffered_stream(
        &self,
        request: llm_runtime::MessagesCreateRequest,
    ) -> Result<HistoryResponse, LlmError> {
        self.service.messages_create_buffered_stream(request).await
    }

    async fn messages_create(
        &self,
        request: OrchestratorApiRequest,
    ) -> Result<HistoryResponse, LlmError> {
        match request {
            OrchestratorApiRequest::Main(request) => {
                if let Some(settings) = crate::scheduled_turn::current() {
                    let llm_runtime::MessagesCreateRequest {
                        model: _,
                        profile: _,
                        system,
                        messages,
                        tools,
                        opts,
                    } = request;
                    let mut canonical = self.build_scheduled_request(
                        settings,
                        system.as_ref(),
                        opts.skip_global_cache_for_system_prompt,
                        messages,
                        tools,
                        opts.max_output_tokens,
                        opts.tool_result_reminder_suffixes,
                        opts.typed_attachment_ids,
                        opts.task_deliveries,
                        opts.model_source,
                    )?;
                    canonical.input.controls.anthropic.context_hint =
                        opts.context_hint.map(|value| {
                            lingxi_llm_client::protocol::JsonPayload::from_generated_value(&value)
                                .expect("scheduled request context hint is valid JSON")
                        });
                    canonical.execution.context_hint_beta = opts.context_hint_beta;
                    canonical.execution.model_attempt = opts.model_attempt;
                    canonical
                        .execution
                        .set_request_dispatch_admission(opts.request_dispatch_admission);
                    canonical.execution.failed_stream_outlasted_timeout =
                        opts.failed_stream_outlasted_timeout;
                    canonical.execution.stream_fallback =
                        opts.initial_consecutive_overloaded.is_some();
                    // Scheduled turns own route, reasoning and query source;
                    // the caller owns this logical call's retry controls.
                    return self
                        .service
                        .execute_non_stream_request(
                            canonical,
                            llm_runtime::NonStreamingRequestClass::Auxiliary,
                            llm_runtime::NonStreamingRetryOptions {
                                initial_consecutive_overloaded: opts.initial_consecutive_overloaded,
                                fallback: opts.fallback,
                            },
                        )
                        .await;
                }
                self.service.messages_create(request).await
            }
            OrchestratorApiRequest::HookPrompt(request) => {
                let system = Utf16JsonProjection::from(request.system.as_str());
                let stream = self.service.stream_json_schema_with_thinking(
                    &request.model, request.profile.as_deref(), Some(&system), None, request.messages,
                    serde_json::json!({"type":"object","properties":{"ok":{"type":"boolean"},"reason":{"type":"string"},"impossible":{"type":"boolean"}},"required":["ok","reason"],"additionalProperties":false}),
                    None, None, Some(llm_runtime::model::thinking::ThinkingConfig::Disabled), None, Some("hook_prompt"),
                ).await?;
                llm_runtime::stream_accumulator::accumulate_stream_salvaging(stream)
                    .await
                    .map_err(|(_, error)| error)
            }
        }
    }

    async fn count_tokens(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::host::SourceToolWireDefinition>,
    ) -> Result<u64, LlmError> {
        let tools = llm_runtime::convert::to_tool_declarations(tools)?;
        self.service
            .count_tokens(model, profile, system, msgs, tools)
            .await
    }

    async fn count_tokens_exact(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&str>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::host::SourceToolWireDefinition>,
    ) -> Result<Option<u64>, LlmError> {
        let tools = llm_runtime::convert::to_tool_declarations(tools)?;
        self.service
            .count_tokens_exact(model, profile, system, msgs, tools)
            .await
    }

    fn available_models(&self) -> Vec<String> {
        self.service.available_models()
    }

    fn resolve_media_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<llm_runtime::MediaRoute, LlmError> {
        self.service.resolve_media_route(model, profile)
    }

    async fn analyze_vision_delegation(
        &self,
        packet: sidequery::VisionPacket,
    ) -> Result<sidequery::VisionDelegationResult, LlmError> {
        let client = std::sync::Arc::new(sidequery::ProviderSideQueryClient::from_service(
            self.service.clone(),
        ));
        let service = sidequery::VisionDelegationService::new(client);
        service.analyze(packet).await.map_err(|error| match error {
            sidequery::SideQueryError::Api(error) => error,
            sidequery::SideQueryError::InvalidResponse(message) => {
                LlmError::MediaDelegationUnavailable { message }
            }
            sidequery::SideQueryError::StructuredOutputUnsupported => {
                LlmError::MediaDelegationUnavailable {
                    message: "structured output is unsupported".into(),
                }
            }
            sidequery::SideQueryError::Partial {
                source,
                usage,
                elapsed,
                retry_count,
                api_calls,
            } => LlmError::MediaDelegationPartial {
                message: source.to_string(),
                accounting: MediaDelegationAccounting::from_counts(
                    usage.tokens.input,
                    usage.tokens.output,
                    usage.tokens.cache_write,
                    usage.tokens.cache_read,
                    usage.tokens.reasoning_output,
                    usage.tokens.cache_write_1h,
                    elapsed,
                    retry_count,
                    api_calls,
                ),
            },
        })
    }

    fn list_model_listings(&self) -> Vec<lingxi_core::host::orchestrator::ModelListing> {
        self.service
            .model_listings()
            .into_iter()
            .filter(|listing| listing.capabilities.tools)
            .map(lower_model_listing)
            .collect()
    }

    /// Return the most recently observed rate-limit header snapshot.
    ///
    /// Delegates to [`Self::last_rate_limit_info`] and maps the internal
    /// `RateLimitInfo` struct into the public [`lingxi_core::host::RateLimitSnapshot`]
    /// (all three fields: `rate_limit_type`, `overage_status`, and
    /// `overage_disabled_reason`).
    fn last_rate_limit_info(&self) -> Option<lingxi_core::host::RateLimitSnapshot> {
        self.last_rate_limit_info()
            .map(|info| lingxi_core::host::RateLimitSnapshot {
                rate_limit_type: info.rate_limit_type,
                overage_status: info.overage_status,
                overage_disabled_reason: info.overage_disabled_reason,
            })
    }

    fn last_request_id(&self) -> Option<String> {
        self.service.last_request_id()
    }

    fn last_retry_count(&self) -> u32 {
        self.service.last_retry_count()
    }

    fn thinking_signature_stripped(&self) -> bool {
        self.service.thinking_signature_stripped()
    }

    fn set_thinking_signature_stripped(&self, stripped: bool) {
        self.service.set_thinking_signature_stripped(stripped);
    }

    fn thinking_stripped_messages(
        &self,
    ) -> std::collections::HashMap<lingxi_core::types::MessageId, usize> {
        self.service.thinking_stripped_messages()
    }

    fn set_thinking_stripped_messages(
        &self,
        messages: std::collections::HashMap<lingxi_core::types::MessageId, usize>,
    ) {
        self.service.set_thinking_stripped_messages(messages);
    }

    /// Task 8 (llm-runtime future-work batch 3): expose the FULL internal
    /// nine-field snapshot for the turn drivers' `emit_rate_limit` seam.
    /// Delegates to the inherent [`Self::last_rate_limit_info`] (which
    /// already returns the internal `RateLimitInfo`); the trait method of
    /// the same name above keeps its three-field projection untouched.
    fn last_rate_limit_full(&self) -> Option<RateLimitInfo> {
        self.last_rate_limit_info()
    }

    /// Task 2 (llm-runtime future-work batch 5): expose the raw per-window
    /// snapshot cached by `record_rate_limit_from_headers` for the turn
    /// drivers' `emit_raw_utilization` seam.
    fn last_raw_utilization(&self) -> Option<RawUtilization> {
        self.service.last_raw_utilization()
    }

    /// Task 6 (llm-runtime future-work batch 5): expose the limits copy
    /// composed by `record_rate_limit_from_429` from the most recent
    /// 429 error response's unified headers, for the orchestrator's
    /// terminal-429 re-map (claude-code `errors.ts:480-524`).
    fn last_rate_limit_error_message(&self) -> Option<String> {
        self.service.last_rate_limit_error_message()
    }

    async fn prewarm_responses_websocket(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::host::SourceToolWireDefinition>,
        skip_global_cache_for_system_prompt: bool,
    ) -> Result<(), LlmError> {
        let tools = llm_runtime::convert::to_tool_declarations(tools)?;
        self.service
            .prewarm_responses_websocket(
                model,
                profile,
                system,
                messages,
                tools,
                skip_global_cache_for_system_prompt,
            )
            .await
    }

    async fn close_responses_websocket_session(&self) -> Result<(), LlmError> {
        self.service.close_responses_websocket_session().await
    }
}

/// Build the grouped-picker listing for EVERY provider.
///
/// First-party Anthropic is NOT part of the models.dev preset catalog
/// (`builtin_presets`); in a live session it enters via
/// `anthropic_provider_profile`. We surface it here too so this seam is
/// provider-COMPLETE on its own — it previously omitted Anthropic, which is why
/// `list_available_models` had to carry a Claude-only hardcoded fallback. With
/// Anthropic in the catalog that special case is gone. Anthropic is listed first
/// to keep the picker's Claude-first ordering for catalog-only (no live config)
/// callers.
fn lower_reasoning_spec(
    raw: llm_runtime::ReasoningControlSpec,
) -> lingxi_core::host::ReasoningControlSpec {
    let mandatory = raw
        .mandatory_selection
        .as_ref()
        .map(|selection| match selection {
            llm_runtime::ReasoningSelection::Automatic => {
                lingxi_core::host::ReasoningSelection::Automatic
            }
            llm_runtime::ReasoningSelection::Disabled => {
                lingxi_core::host::ReasoningSelection::Disabled
            }
            llm_runtime::ReasoningSelection::Enabled => {
                lingxi_core::host::ReasoningSelection::Enabled
            }
            llm_runtime::ReasoningSelection::Level(id) => {
                lingxi_core::host::ReasoningSelection::Level { id: id.clone() }
            }
            llm_runtime::ReasoningSelection::TokenBudget(tokens) => {
                lingxi_core::host::ReasoningSelection::TokenBudget {
                    tokens: u64::from(*tokens),
                }
            }
        });
    let mut available = vec![lingxi_core::host::ReasoningSelection::Automatic];
    if let Some(required) = mandatory.as_ref() {
        available = vec![required.clone()];
    } else {
        if raw.can_disable {
            available.push(lingxi_core::host::ReasoningSelection::Disabled);
        }
        if raw.can_enable {
            available.push(lingxi_core::host::ReasoningSelection::Enabled);
        }
        available.extend(
            raw.levels
                .iter()
                .cloned()
                .map(|id| lingxi_core::host::ReasoningSelection::Level { id }),
        );
    }
    let auto_only = available.len() == 1
        && matches!(
            available.first(),
            Some(lingxi_core::host::ReasoningSelection::Automatic)
        )
        && raw.token_budget.is_none();
    lingxi_core::host::ReasoningControlSpec {
        available,
        selections_persistable: mandatory.is_none(),
        budget_range: raw
            .token_budget
            .map(|range| lingxi_core::host::ReasoningBudgetRange {
                min_tokens: range.min,
                max_tokens: range.max,
                supports_dynamic: false,
                supports_disabled: raw.can_disable,
            }),
        provider_default: mandatory.unwrap_or(lingxi_core::host::ReasoningSelection::Automatic),
        forced: raw.mandatory_selection.is_some(),
        modifiable: raw.mandatory_selection.is_none() && !auto_only,
        disabled_reason: if raw.mandatory_selection.is_some() {
            Some("reasoning_required".to_string())
        } else if auto_only {
            Some("reasoning_unavailable".to_string())
        } else {
            None
        },
    }
}

/// Project an llm-runtime route listing into the provider-neutral picker type.
#[must_use]
pub fn lower_model_listing(listing: llm_runtime::ModelListing) -> lingxi_core::host::ModelListing {
    let capabilities = lingxi_core::host::ModelCapabilities {
        streaming: listing.capabilities.streaming,
        tools: listing.capabilities.tools,
        vision: listing.capabilities.vision,
        documents: listing.capabilities.documents,
        reasoning: listing.capabilities.reasoning,
        structured_output: listing.capabilities.structured_output,
    };
    // Computed before the struct literal moves `profile_name` out.
    // Only report a group when the profile actually is one connection of
    // several; a standalone provider leaves this default so every existing
    // consumer keeps reading `provider_id` and nothing changes for it.
    let connection = if listing.group == listing.profile_name {
        lingxi_core::host::ConnectionRef::default()
    } else {
        lingxi_core::host::ConnectionRef {
            group: Some(listing.group),
            connection_id: Some(listing.connection_id),
        }
    };
    lingxi_core::host::ModelListing {
        display_model: listing.display_model,
        request_model: listing.request_model,
        provider_label: provider_label_owned(&listing.profile_name),
        provider_id: listing.profile_name,
        description: listing.description,
        supports_reasoning: capabilities.reasoning,
        metadata: listing.metadata,
        capabilities,
        reasoning: lower_reasoning_spec(listing.reasoning),
        fusion_analyst_capable: listing.fusion_analyst_capable,
        connection,
    }
}

fn catalog_model_listings() -> Vec<lingxi_core::host::orchestrator::ModelListing> {
    // Build one listing, defaulting an absent description to a known per-model
    // parity blurb for the Claude family (models.dev / the Anthropic profiles
    // carry no such string).
    let row = |display_model: String,
               request_model: String,
               label: String,
               provider: String,
               description: Option<String>,
               supports_reasoning: bool| {
        let description =
            description.or_else(|| model_description(&request_model).map(str::to_string));
        lingxi_core::host::orchestrator::ModelListing {
            connection: Default::default(),
            display_model,
            request_model,
            provider_label: label,
            provider_id: provider,
            description,
            supports_reasoning,
            metadata: lingxi_core::host::ModelMetadata::default(),
            capabilities: lingxi_core::host::ModelCapabilities {
                reasoning: supports_reasoning,
                ..lingxi_core::host::ModelCapabilities::default()
            },
            reasoning: lingxi_core::host::ReasoningControlSpec::default(),
            // This fallback catalog carries no per-route capability or protocol
            // facts (`ModelCapabilities::default()` above), so it cannot claim
            // a route can judge. Fail closed: the `/fusion setup` analyst
            // picker shows nothing here rather than offering a pick that only
            // fails once every panel has spent.
            fusion_analyst_capable: false,
        }
    };

    // 1. First-party Anthropic (not in the preset catalog).
    let mut listings: Vec<lingxi_core::host::orchestrator::ModelListing> =
        llm_runtime::anthropic_model_profiles()
            .into_iter()
            .map(|m| {
                let supports_reasoning = m.capabilities.reasoning;
                row(
                    m.display_model,
                    m.request_model,
                    provider_label("anthropic").to_string(),
                    "anthropic".to_string(),
                    m.description,
                    supports_reasoning,
                )
            })
            .collect();

    // 2. Static models.dev presets (OpenAI, Gemini, DeepSeek, …). EXCLUDE models
    //    that don't support tool calls (`tool_call=false` — image/TTS/audio
    //    models, gpt-3.5-turbo, gpt-5-chat-latest, the aion-labs set, ~85
    //    OpenRouter passthroughs). The agent sends tools on EVERY turn, so such a
    //    model can never complete an agentic turn — it hard-fails "unsupported
    //    capability: tools" (llm-runtime `validate_capabilities`). Offering it in
    //    the `/model` picker is offering a permanently-broken pick; it stays
    //    resolvable by explicit id for any non-agentic caller.
    let catalog = llm_runtime::builtin_presets();
    if let Ok(registry) = llm_runtime::ModelRegistry::from_config(llm_runtime::ClientConfig {
        providers: catalog.providers,
    }) {
        listings.extend(
            registry
                .available_models()
                .into_iter()
                .filter(|m| m.capabilities.tools)
                .map(lower_model_listing),
        );
    }
    listings
}

/// Cached `request_model -> display_model` map over the full static catalog
/// (Anthropic first-party + models.dev presets), built once on first use.
fn catalog_display_names() -> &'static std::collections::HashMap<String, String> {
    static MAP: std::sync::OnceLock<std::collections::HashMap<String, String>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        catalog_model_listings()
            .into_iter()
            .map(|m| (m.request_model, m.display_model))
            .collect()
    })
}

/// The catalog display name for `request_model` (e.g. `deepseek-v4-pro` ->
/// `"DeepSeek V4 Pro"`), or `None` for an id not in the catalog.
///
/// Feeds the `<env>` identity line's marketing-name slot for NON-Claude models
/// — Claude ids resolve their name via [`crate::prompt::env_meta::
/// marketing_name_for_model`] (byte-parity), and only when THAT returns `None`
/// (a non-Claude model) does the builder fall back to this so the line reads
/// the strong "powered by the model named {name}" form instead of the weak
/// id-only fallback. Cached, so it is cheap to call per turn.
#[must_use]
pub(crate) fn display_name_for_model(request_model: &str) -> Option<String> {
    catalog_display_names().get(request_model).cloned()
}

/// Cached `request_model -> UNIQUE provider profile`. The value is `None` when
/// the same wire id is served by MORE than one provider (ambiguous), so callers
/// don't guess. Built once from the full catalog.
fn catalog_provider_profiles() -> &'static std::collections::HashMap<String, Option<String>> {
    static MAP: std::sync::OnceLock<std::collections::HashMap<String, Option<String>>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        let mut map: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        for l in catalog_model_listings() {
            let request_model = l.request_model;
            let provider = l.provider_id;
            match map.get_mut(&request_model) {
                // Already seen under a DIFFERENT provider → ambiguous.
                Some(existing) => {
                    if existing.as_deref() != Some(provider.as_str()) {
                        *existing = None;
                    }
                }
                None => {
                    map.insert(request_model, Some(provider));
                }
            }
        }
        map
    })
}

/// The UNIQUE provider profile that serves `request_model` in the catalog, or
/// `None` when the id is unknown OR served by more than one provider. Lets
/// cost/telemetry attribute a bare wire id whose live session profile is unknown
/// (e.g. after a cross-provider `--resume` clears it) to its REAL provider
/// instead of blindly defaulting to Anthropic.
#[must_use]
pub(crate) fn provider_for_model(request_model: &str) -> Option<String> {
    catalog_provider_profiles()
        .get(request_model)
        .cloned()
        .flatten()
}

/// Whether a catalog model has multiple possible connection profiles.
pub(crate) fn model_has_ambiguous_profile(request_model: &str) -> bool {
    catalog_provider_profiles().get(request_model) == Some(&None)
}

/// Known one-line description for a built-in model wire id, mirroring the
/// claude-code `/model` picker blurbs (`modelOptions.ts`). Matched by a
/// case-insensitive family substring so dated ids (`claude-opus-4-7`, etc.) and
/// short aliases (`opus`, `sonnet`, `haiku`) both resolve. Returns `None` for
/// any id we don't recognize (then the row renders with no sub-line).
#[must_use]
pub(crate) fn model_description(request_model: &str) -> Option<&'static str> {
    let id = request_model.to_ascii_lowercase();
    if id.contains("opus") {
        Some("Best for everyday, complex tasks")
    } else if id.contains("haiku") {
        Some("Fastest for quick answers")
    } else if id.contains("sonnet") {
        Some("Efficient for routine tasks")
    } else {
        None
    }
}

/// Human provider header for a catalog profile name, connection-aware.
///
/// A provider reachable several ways names each connection
/// `<group>:<connection>` (+ `#<n>` per extra key slot). Labelling that raw
/// would put `deepseek:cn#1` in the settings model directory as if it were a
/// vendor, so the label is built from the VENDOR plus the connection using the
/// same parser the TUI picker and the desktop header use.
fn provider_label_owned(profile_name: &str) -> String {
    let (group, connection, slot) = lingxi_core::host::split_connection_profile(profile_name);
    if connection.is_none() && slot.is_none() {
        return provider_label(profile_name).to_string();
    }
    let base = provider_label(group).to_string();
    match (connection, slot) {
        (Some(connection), Some(slot)) => format!("{base} · {connection} · key {}", slot + 1),
        (Some(connection), None) => format!("{base} · {connection}"),
        (None, Some(slot)) => format!("{base} · key {}", slot + 1),
        (None, None) => base,
    }
}

/// Human provider header for a catalog profile name.
fn provider_label(profile_name: &str) -> &str {
    match profile_name {
        "anthropic" => "Anthropic",
        "openrouter" => "OpenRouter",
        "deepseek" => "DeepSeek",
        "kimi" => "Kimi",
        "kimi-code" => "Kimi Code",
        "glm-coding" => "GLM (coding)",
        "zai" => "Z.AI",
        "openai" => "OpenAI",
        "openai-chatgpt" => "OpenAI (ChatGPT login)",
        "github-copilot" => "GitHub Copilot",
        "gemini" => "Google Gemini",
        "zhipuai-coding-plan" => "GLM (coding)",
        other => other,
    }
}

/// Subagent API seam — forwards every owned request field to the provider service.
#[async_trait]
impl agent::SubagentApiClient for ProviderApiAdapter {
    fn refusal_api_text_snapshot(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot>, LlmError> {
        self.resolved_refusal_api_text_snapshot(model, profile)
    }

    fn resolve_mod_media_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Option<llm_runtime::MediaRoute> {
        self.service.resolve_media_route(model, profile).ok()
    }

    fn consume_pending_near_limit_wrap_up_hint(&self) -> bool {
        self.service.consume_pending_near_limit_wrap_up_hint()
    }

    fn dispatch_near_limit_checkpoint(&self, request: agent::NearLimitCheckpointRequest) {
        // The port has one shared `SessionState.todos` list rather than the
        // oracle's per-agent todo buckets. A child-specific bucket therefore
        // has no representable entries here; keep it empty instead of
        // accidentally snapshotting the main thread's plan.
        let _ = session::dispatch_rate_limit_checkpoint(session::OwnedCheckpointRequest {
            session_id: session::checkpoint_session_key(request.session_id),
            trigger: session::CheckpointTrigger::NearLimit,
            todos: Vec::new(),
            cwd: request.cwd,
            gates: session::CheckpointGates {
                non_interactive: request.non_interactive,
                remote_workspace: false,
                policy_allows: session::local_checkpoint_commit_allowed(),
            },
        });
    }

    fn record_usage_limit_near_wrap_up(&self) {
        // Oracle `y("usage_limit_near_wrapup")`: a success/count gate rather
        // than a `tengu_*` analytics-bus event. Match the existing `y(...)`
        // ports by emitting a debug tracing event with the literal gate name.
        tracing::debug!(event = "usage_limit_near_wrapup");
    }

    async fn stream(
        &self,
        request: agent::api::SubagentApiRequest,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.service
            .stream_with_attempt_opts(
                &request.model,
                request.profile.as_deref(),
                request.system.as_ref(),
                request.messages,
                request.tools,
                request.forced_tool.as_deref(),
                request.effort,
                request.opts.max_output_tokens,
                request.opts.query_source_label.as_deref(),
                request.opts.model_attempt,
            )
            .await
    }
}

#[async_trait]
impl StreamingApiClient for ProviderApiAdapter {
    async fn stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<llm_runtime::ToolDeclaration>,
        query_source: &str,
        skip_global_cache_for_system_prompt: bool,
        request_dispatch_admission: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.stream_with_effort_override(
            model,
            profile,
            system,
            messages,
            tools,
            None,
            None,
            Vec::new(),
            query_source,
            skip_global_cache_for_system_prompt,
            request_dispatch_admission,
            None,
            None,
            Default::default(),
            Default::default(),
            None,
        )
        .await
    }

    async fn stream_with_effort_override(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<llm_runtime::ToolDeclaration>,
        max_tokens: Option<u32>,
        effort: Option<&str>,
        tool_result_reminder_suffixes: Vec<llm_runtime::convert::AnthropicToolResultReminderSuffix>,
        query_source: &str,
        skip_global_cache_for_system_prompt: bool,
        request_dispatch_admission: Option<llm_runtime::RequestDispatchAdmission>,
        request_dispatch_observer: Option<llm_runtime::RequestDispatchObserver>,
        native_anthropic_request_context: Option<lingxi_llm_client::NativeAnthropicRequestContext>,
        typed_attachment_ids: std::collections::HashSet<lingxi_core::types::MessageId>,
        task_deliveries: std::collections::HashMap<
            lingxi_core::types::MessageId,
            lingxi_core::host::task_delivery::TaskDeliveryRequest,
        >,
        model_source: Option<Utf16JsonProjection>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        if let Some(settings) = crate::scheduled_turn::current() {
            let request = self.build_scheduled_request(
                settings,
                system,
                skip_global_cache_for_system_prompt,
                messages,
                tools,
                max_tokens,
                tool_result_reminder_suffixes,
                typed_attachment_ids,
                task_deliveries,
                model_source,
            )?;
            let mut request = request;
            request
                .execution
                .set_request_dispatch_admission(request_dispatch_admission);
            request.execution.request_dispatch_observer = request_dispatch_observer;
            request.execution.native_anthropic_request_context = native_anthropic_request_context;
            return self.service.stream_request(request).await;
        }
        // (M4 cc2.1.198) The MAIN loop carries the session's initial effort
        // (CLI `--effort` → `output_config.effort`); `None` (no flag) keeps
        // the pre-M4 body byte-identical.
        // (/fast) When the shared fast-mode flag is set AND the active model
        // passes the resolved route/environment gate, send `speed:"fast"` — the
        // service's `beta_context` reads it back to add the fast-mode beta.
        // `None` (flag off, or an unsupported model) keeps the body unchanged.
        let speed = if self.fast_mode.load(std::sync::atomic::Ordering::SeqCst)
            && self.service.fast_model_allowed(model, profile)?
        {
            Some("fast".to_string())
        } else {
            None
        };
        self.service
            .stream_with_system_prompt(
                model,
                profile,
                system,
                messages,
                tools,
                max_tokens,
                effort
                    .map(|effort| serde_json::Value::String(effort.to_owned()))
                    .or_else(|| self.current_effort()),
                speed,
                skip_global_cache_for_system_prompt,
                Some(query_source),
                request_dispatch_admission,
                tool_result_reminder_suffixes,
                request_dispatch_observer,
                native_anthropic_request_context,
                typed_attachment_ids,
                task_deliveries,
                model_source,
            )
            .await
    }

    fn last_retry_count(&self) -> u32 {
        self.service.last_retry_count()
    }

    fn thinking_signature_stripped(&self) -> bool {
        self.service.thinking_signature_stripped()
    }

    fn set_thinking_signature_stripped(&self, stripped: bool) {
        self.service.set_thinking_signature_stripped(stripped);
    }

    fn thinking_stripped_messages(
        &self,
    ) -> std::collections::HashMap<lingxi_core::types::MessageId, usize> {
        self.service.thinking_stripped_messages()
    }

    fn set_thinking_stripped_messages(
        &self,
        messages: std::collections::HashMap<lingxi_core::types::MessageId, usize>,
    ) {
        self.service.set_thinking_stripped_messages(messages);
    }
}
// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {

    /// The settings model directory must not show a raw connection profile name
    /// as if it were a vendor. This is the third copy of the profile-name→label
    /// mapping (with `engine-desktop` and the TUI picker) and was the one left
    /// behind, so `deepseek:cn#1` reached the desktop and mobile catalog.
    #[test]
    fn provider_label_is_connection_aware() {
        assert_eq!(super::provider_label_owned("deepseek"), "DeepSeek");
        assert_eq!(super::provider_label_owned("deepseek:cn"), "DeepSeek · cn");
        assert_eq!(
            super::provider_label_owned("deepseek:cn#1"),
            "DeepSeek · cn · key 2"
        );
        // `#` is a key slot only when a number follows it: an unknown provider
        // keeps its WHOLE name rather than losing half of it to a phantom slot.
        // (This adapter returns unknown names verbatim; only the desktop header
        // title-cases them.)
        assert_eq!(super::provider_label_owned("weird#name"), "weird#name");
    }
    use super::*;
    use llm_runtime::model::user_agent::UserAgentEnv;
    use llm_runtime::ModelRuntime;
    use llm_runtime::{
        ApiService, AuthStrategy, BoxFuture, Capabilities, ClientConfig, CredentialConfig,
        LlmError, ModelProfile, PricingConfig, ProtocolFamily, ProviderId, ProviderProfile,
        ProviderRequest, ProviderResponse, StreamingResponse, Transport,
    };
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    fn structured_output_tool() -> llm_runtime::ToolDeclaration {
        let source = lingxi_core::host::SourceToolWireDefinition {
            name: "StructuredOutput".into(),
            description: lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                r#""Return structured output""#,
            )
            .unwrap(),
            input_schema: lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                r#"{"type":"object","properties":{}}"#,
            )
            .unwrap(),
            strict: false,
            defer_loading: false,
        };
        llm_runtime::convert::to_tool_declarations(vec![source])
            .unwrap()
            .pop()
            .unwrap()
    }

    fn request_content_block_type_count(
        body: &lingxi_llm_client::protocol::JsonPayload,
        expected: &str,
    ) -> usize {
        let Some(messages) = body.payload_at("/messages") else {
            return 0;
        };
        (0..messages.array_len("").unwrap_or_default())
            .filter_map(|index| messages.array_item(index))
            .filter_map(|message| message.payload_at("/content"))
            .flat_map(|content| {
                (0..content.array_len("").unwrap_or_default())
                    .filter_map(|index| content.array_item(index))
                    .collect::<Vec<_>>()
            })
            .filter(|block| block.string_at("/type").as_deref() == Some(expected))
            .count()
    }

    fn request_has_content_block_type(
        body: &lingxi_llm_client::protocol::JsonPayload,
        expected: &str,
    ) -> bool {
        request_content_block_type_count(body, expected) > 0
    }

    // ── FakeTransport (duplicated into the orchestrator-domain tests) ─────────────

    /// A fake Transport that returns a scripted sequence of responses (or errors).
    struct FakeTransport {
        /// Pre-recorded responses returned in order; cycles to last entry.
        responses: Mutex<Vec<FakeResponse>>,
        /// All requests received, in order.
        seen: Mutex<Vec<ProviderRequest>>,
    }

    #[derive(Clone)]
    #[allow(dead_code)]
    enum FakeResponse {
        Ok(ProviderResponse),
        Err(LlmError),
    }

    impl FakeTransport {
        /// Return the same response on every call.
        fn always(resp: ProviderResponse) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(vec![FakeResponse::Ok(resp)]),
                seen: Mutex::new(vec![]),
            })
        }

        #[allow(dead_code)]
        fn seen_count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    impl llm_runtime::test_support::FixtureTransport for FakeTransport {
        fn execute<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            let mut seen = self.seen.lock().unwrap();
            seen.push(request.clone());
            let idx = (seen.len() - 1).min({
                let resps = self.responses.lock().unwrap();
                resps.len().saturating_sub(1)
            });
            let resp = {
                let resps = self.responses.lock().unwrap();
                resps[idx].clone()
            };
            Box::pin(async move {
                match resp {
                    FakeResponse::Ok(r) => Ok(r),
                    FakeResponse::Err(e) => Err(e),
                }
            })
        }

        fn open_stream<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            // Record the WIRE request (post-codec `body_json`) so tests can
            // inspect exactly what a streaming call sent — e.g. WP2a item 2's
            // `stream_with_opts`/`stream_forced_with_opts` max_tokens
            // assertion — even though this fake never returns a real stream.
            self.seen.lock().unwrap().push(request.clone());
            Box::pin(async move {
                // `InvalidRequest` (not `Transport`): `ApiService::drive_stream`'s
                // retry classifier (`next_step_with_backoff`,
                // `llm-runtime/src/model/retry.rs`) treats `Transport` as
                // retry-worthy and backs off + retries up to the configured cap
                // — for a caller that never scripts a `StreamingResponse` (every
                // test that only inspects `seen` / the wire request, e.g. this
                // one) that turned one assertion into a multi-hundred-second
                // real-time backoff loop for no reason the assertion cares
                // about. `InvalidRequest` hits the "overflow check, else
                // Terminal" arm — a message that doesn't parse as an overflow
                // report (this one doesn't) is `DriveStep::Terminal` — so the
                // call returns after exactly ONE `open_stream` per attempt,
                // with the request still recorded in `seen` first.
                Err(LlmError::InvalidRequest {
                    message: "open_stream not scripted (FakeTransport)".to_string(),
                })
            })
        }
    }
    llm_runtime::impl_fixture_transport!(FakeTransport);

    /// No-network wire fixture that drives the production SDK batch and SSE
    /// decoders through `ApiService` and `ProviderApiAdapter`.
    struct NativeResumeTransport {
        response: ProviderResponse,
        stream_frames: Mutex<Vec<llm_runtime::RawStreamFrame>>,
        seen: Mutex<Vec<ProviderRequest>>,
    }

    impl NativeResumeTransport {
        fn new() -> Arc<Self> {
            let response = ProviderResponse::json(
                200,
                serde_json::json!({
                    "id": "msg_native_resume_batch",
                    "model": "claude-sonnet-4-20250514",
                    "content": [{
                        "type": "thinking",
                        "thinking": "unfinished reasoning",
                        "signature": "native-signature"
                    }],
                    "stop_reason": "max_tokens",
                    "resumable": true,
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0
                    }
                }),
            );
            let stream_frames = [
                serde_json::json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_native_resume_stream",
                        "model": "claude-sonnet-4-20250514",
                        "content": [],
                        "usage": {
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0
                        }
                    }
                }),
                serde_json::json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "thinking", "thinking": ""}
                }),
                serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": "unfinished reasoning"}
                }),
                serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "signature_delta", "signature": "native-signature"}
                }),
                serde_json::json!({"type": "content_block_stop", "index": 0}),
                serde_json::json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "max_tokens", "resumable": true},
                    "usage": {"output_tokens": 0}
                }),
                serde_json::json!({"type": "message_stop"}),
            ]
            .into_iter()
            .map(|frame| {
                llm_runtime::RawStreamFrame::new(
                    serde_json::to_vec(&frame).expect("stream frame is valid JSON"),
                )
            })
            .collect();
            Arc::new(Self {
                response,
                stream_frames: Mutex::new(stream_frames),
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    struct NativeResumeFrameStream(std::collections::VecDeque<llm_runtime::RawStreamFrame>);

    impl llm_runtime::FrameStream for NativeResumeFrameStream {
        fn next_frame(
            &mut self,
        ) -> BoxFuture<'_, Result<Option<llm_runtime::RawStreamFrame>, LlmError>> {
            Box::pin(async move { Ok(self.0.pop_front()) })
        }
    }

    impl llm_runtime::test_support::FixtureTransport for NativeResumeTransport {
        fn execute<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            self.seen.lock().unwrap().push(request.clone());
            let response = self.response.clone();
            Box::pin(async move { Ok(response) })
        }

        fn open_stream<'a>(
            &'a self,
            request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            self.seen.lock().unwrap().push(request.clone());
            let frames =
                std::collections::VecDeque::from(self.stream_frames.lock().unwrap().clone());
            Box::pin(async move {
                Ok(StreamingResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    frames: Box::new(NativeResumeFrameStream(frames)),
                })
            })
        }
    }
    llm_runtime::impl_fixture_transport!(NativeResumeTransport);

    fn native_resume_adapter(transport: Arc<dyn Transport>) -> ProviderApiAdapter {
        make_adapter_with_session_tool_choice_configured(
            transport,
            None,
            "claude-sonnet-4-20250514",
            |service| service.with_native_anthropic_endpoint("https://api.anthropic.com"),
        )
    }

    #[tokio::test]
    async fn native_resume_sdk_context_fact_reaches_batch_and_stream_consumers() {
        use crate::conversation::OrchestratorApiRequest;
        use lingxi_llm_client::NativeAnthropicRequestContext;

        let transport = NativeResumeTransport::new();
        let adapter = native_resume_adapter(transport.clone());
        let make_request = || {
            let mut request = llm_runtime::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                Some("anthropic"),
                None,
                vec![lingxi_core::types::ConversationMessage::user(
                    lingxi_core::types::MessageId::new(),
                    "Continue the incomplete response.".into(),
                )],
                vec![],
            );
            request.opts.native_anthropic_request_context =
                Some(NativeAnthropicRequestContext::default());
            request
        };

        let batch = adapter
            .messages_create(OrchestratorApiRequest::Main(make_request()))
            .await
            .expect("batch response decodes through ApiService");
        assert_eq!(batch.stop_reason.as_deref(), Some("max_tokens"));
        assert_eq!(
            batch.resumable.as_ref().map(|value| value.as_json_source()),
            Some("true")
        );
        assert!(matches!(
            batch.content.as_slice(),
            [llm_runtime::ContentBlock::Reasoning {
                signature: Some(signature),
                ..
            }] if signature == "native-signature"
        ));
        assert_eq!(
            batch.usage.native_resume_context_exhausted,
            Some(false),
            "SDK batch context evidence must survive HistoryProjector and response accounting"
        );

        let stream = StreamingApiClient::stream_with_effort_override(
            &adapter,
            "claude-sonnet-4-20250514",
            Some("anthropic"),
            None,
            vec![lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "Continue the incomplete response.".into(),
            )],
            vec![],
            None,
            None,
            Vec::new(),
            "main",
            false,
            None,
            None,
            Some(NativeAnthropicRequestContext::default()),
            Default::default(),
            Default::default(),
            None,
        )
        .await
        .expect("stream opens through ProviderApiAdapter and ApiService");
        let streamed = llm_runtime::stream_accumulator::accumulate_stream_salvaging(stream)
            .await
            .expect("stream is decoded and accumulated");
        assert_eq!(streamed.stop_reason.as_deref(), Some("max_tokens"));
        assert_eq!(
            streamed
                .resumable
                .as_ref()
                .map(|value| value.as_json_source()),
            Some("true")
        );
        assert!(matches!(
            streamed.content.as_slice(),
            [llm_runtime::ContentBlock::Reasoning {
                signature: Some(signature),
                ..
            }] if signature == "native-signature"
        ));
        assert_eq!(
            streamed.usage.native_resume_context_exhausted,
            Some(false),
            "SDK streaming getter evidence must survive projector and ResponseAccumulator"
        );
        assert_eq!(transport.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn native_resume_later_usage_unknown_clears_initial_zero_usage_false() {
        use lingxi_llm_client::NativeAnthropicRequestContext;

        let transport = NativeResumeTransport::new();
        let raw = |value: serde_json::Value| {
            llm_runtime::RawStreamFrame::new(
                serde_json::to_vec(&value).expect("stream frame is valid JSON"),
            )
        };
        *transport.stream_frames.lock().unwrap() = vec![
            raw(serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": "msg_native_resume_transition",
                    "model": "claude-sonnet-4-20250514",
                    "content": [],
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0
                    }
                }
            })),
            raw(serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": ""}
            })),
            raw(serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "unfinished reasoning"}
            })),
            raw(serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": "native-signature"}
            })),
            raw(serde_json::json!({"type": "content_block_stop", "index": 0})),
            // This stop snapshot sees zero total usage and temporarily yields
            // Some(false), matching Native's zero-total fast path.
            raw(serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "max_tokens", "resumable": true}
            })),
            // Later provider usage makes the SDK context comparison Unknown
            // because this request has no Native context-window observation.
            raw(serde_json::json!({
                "type": "message_delta",
                "delta": {},
                "usage": {"output_tokens": 1}
            })),
            raw(serde_json::json!({"type": "message_stop"})),
        ];

        let adapter = native_resume_adapter(transport);
        let stream = StreamingApiClient::stream_with_effort_override(
            &adapter,
            "claude-sonnet-4-20250514",
            Some("anthropic"),
            None,
            vec![lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "Continue the incomplete response.".into(),
            )],
            vec![],
            None,
            None,
            Vec::new(),
            "main",
            false,
            None,
            None,
            Some(NativeAnthropicRequestContext::default()),
            Default::default(),
            Default::default(),
            None,
        )
        .await
        .expect("stream opens through ProviderApiAdapter and ApiService");
        let streamed = llm_runtime::stream_accumulator::accumulate_stream_salvaging(stream)
            .await
            .expect("stream is decoded and accumulated");

        assert_eq!(streamed.stop_reason.as_deref(), Some("max_tokens"));
        assert!(matches!(
            streamed.content.as_slice(),
            [llm_runtime::ContentBlock::Reasoning {
                signature: Some(signature),
                ..
            }] if signature == "native-signature"
        ));
        assert_eq!(
            streamed.usage.native_resume_context_exhausted, None,
            "later positive usage invalidates the earlier zero-usage Some(false)"
        );
        let host = crate::conversation::NativeThinkingResumptionHostFacts {
            feature_gate_enabled: Some(true),
            request_gate_enabled: Some(true),
            provider_route_allowed: Some(true),
            model_capability_allowed: Some(true),
            disable_experimental_betas: Some(false),
            hipaa_mode: Some(false),
            thinking_resumption_refused: Some(false),
        };
        assert!(
            !crate::output_recovery::native_thinking_resumption_eligible(
                &host,
                streamed.stop_reason.as_deref(),
                1,
                false,
                streamed.content.len(),
                crate::output_recovery::history_has_resumable_thinking_block(&streamed.content),
                streamed
                    .resumable
                    .as_ref()
                    .map(crate::output_recovery::json_payload_is_true),
                streamed.usage.native_resume_context_exhausted,
            )
        );
    }

    // ── Test helpers ──────────────────────────────────────────────────────────

    fn ok_response_json() -> serde_json::Value {
        serde_json::json!({
            "id": "msg_test",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        })
    }

    #[tokio::test]
    async fn model_probe_builds_one_minimal_request_and_preserves_source_units() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport.clone());
        let source = Utf16JsonProjection::parse(r#""unlisted-\ud800""#).unwrap();
        let mut expected_source_units = "unlisted-".encode_utf16().collect::<Vec<_>>();
        expected_source_units.push(0xd800);
        assert_eq!(source.string_units(""), Some(expected_source_units));

        adapter
            .probe_model("claude-sonnet-4-20250514", Some("anthropic"), &source)
            .await
            .unwrap();

        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "the dedicated probe dispatches once");
        let body_source = seen[0].body_json.as_json_source();
        let body_projection = Utf16JsonProjection::parse(body_source).unwrap();
        let mut expected_wire_model_units = "unlisted-".encode_utf16().collect::<Vec<_>>();
        expected_wire_model_units.push(0xfffd);
        assert_eq!(
            body_projection.string_units("/model"),
            Some(expected_wire_model_units),
            "the SDK well-forms the physical JSON body after retaining the exact source input"
        );

        let body: serde_json::Value = serde_json::from_str(body_source).unwrap();
        assert_eq!(body["max_tokens"], 1);
        assert_eq!(body["messages"][0]["content"][0]["text"], "Hi");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
        for inherited in [
            "system",
            "tools",
            "thinking",
            "temperature",
            "output_config",
            "effort",
            "stop_sequences",
        ] {
            assert!(
                body.get(inherited).is_none(),
                "unexpected {inherited}: {body}"
            );
        }
    }

    struct OwnedRequestWireCapture {
        requests: Mutex<Vec<llm_runtime::services::sdk::HttpRequest>>,
    }

    #[async_trait]
    impl Transport for OwnedRequestWireCapture {
        async fn send(
            &self,
            request: llm_runtime::services::sdk::HttpRequest,
        ) -> Result<
            llm_runtime::services::sdk::StreamResponse,
            llm_runtime::services::sdk::protocol::LlmError,
        > {
            self.requests.lock().unwrap().push(request);
            Err(
                llm_runtime::services::sdk::protocol::LlmError::InvalidRequest {
                    message: "captured owned request".into(),
                },
            )
        }
    }

    fn owned_request_capture() -> Arc<OwnedRequestWireCapture> {
        Arc::new(OwnedRequestWireCapture {
            requests: Mutex::new(Vec::new()),
        })
    }

    fn scheduled_disabled_settings() -> crate::scheduled_turn::ScheduledSettings {
        crate::scheduled_turn::ScheduledSettings {
            model: "claude-sonnet-4-20250514".into(),
            provider: "anthropic".into(),
            reasoning: lingxi_core::host::ReasoningSelection::Disabled,
            thinking: llm_runtime::model::thinking::ThinkingConfig::Disabled,
            effort: Some(serde_json::json!("low")),
        }
    }

    fn custom_test_system_prompt(
        text: &str,
    ) -> lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput {
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::custom_prompt(
            lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(text),
        )
    }

    #[tokio::test]
    async fn owned_main_and_scheduled_context_hint_preserve_final_headers_and_single_body_field() {
        let capture = owned_request_capture();
        let adapter = make_adapter(capture.clone());
        adapter
            .service
            .set_thinking(llm_runtime::model::thinking::ThinkingConfig::Enabled {
                budget_tokens: 2048,
            });
        let hint = serde_json::json!({"enabled": true, "target_tokens_saved": 75_000});
        for scheduled in [false, true] {
            for offer in [None, Some(hint.clone())] {
                let mut request = llm_runtime::MessagesCreateRequest::new(
                    "claude-sonnet-4-20250514",
                    Some("anthropic"),
                    Some(custom_test_system_prompt("system")),
                    vec![ConversationMessage::user(
                        lingxi_core::types::MessageId::new(),
                        "hello".into(),
                    )],
                    Vec::new(),
                );
                crate::turn_loop::api_recovery::apply_main_request_options(
                    &mut request,
                    Some(4096),
                    Some(compaction::context_hint::ContextHintRequestParams {
                        beta: compaction::context_hint::CONTEXT_HINT_BETA_HEADER,
                        body: offer
                            .as_ref()
                            .map(|hint| serde_json::json!({"context_hint": hint})),
                    }),
                    Some("claude-legacy-no-tools"),
                );
                assert_eq!(request.opts.max_output_tokens, Some(4096));
                assert!(request.opts.context_hint_beta);
                assert_eq!(request.opts.context_hint, offer);
                assert_eq!(
                    request.opts.fallback,
                    llm_runtime::FallbackPolicy::Models(vec!["claude-legacy-no-tools".into()])
                );
                let call = adapter.messages_create(crate::OrchestratorApiRequest::Main(request));
                let error = if scheduled {
                    crate::scheduled_turn::SETTINGS
                        .scope(scheduled_disabled_settings(), call)
                        .await
                } else {
                    call.await
                }
                .unwrap_err();
                assert!(
                    matches!(error, LlmError::InvalidRequest { message } if message == "captured owned request")
                );
                let requests = capture.requests.lock().unwrap();
                let wire = requests.last().unwrap();
                let body: serde_json::Value = serde_json::from_slice(&wire.body).unwrap();
                assert_eq!(body["max_tokens"], 4096);
                assert_eq!(body.get("context_hint"), offer.as_ref());
                assert!(body.pointer("/context_hint/context_hint").is_none());
                let beta = wire
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                    .unwrap()
                    .1
                    .as_str();
                assert_eq!(
                    beta.split(',')
                        .filter(|part| *part == compaction::context_hint::CONTEXT_HINT_BETA_HEADER)
                        .count(),
                    1
                );
                assert!(
                    wire.headers
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case("x-api-key")),
                    "capture must observe the authenticated, sealed request"
                );
                if scheduled {
                    assert!(body.get("thinking").is_none());
                    // The native 4.0 model rejects effort even when scheduled
                    // settings select a level; context-hint policy still applies.
                    assert!(body.pointer("/output_config/effort").is_none());
                } else {
                    assert_eq!(body["thinking"]["budget_tokens"], 2048);
                }
            }
        }
        let mut ordinary = llm_runtime::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            Some("anthropic"),
            None,
            Vec::new(),
            Vec::new(),
        );
        ordinary.opts.max_output_tokens = Some(4096);
        let _ = adapter
            .messages_create(crate::OrchestratorApiRequest::Main(ordinary))
            .await
            .unwrap_err();
        let requests = capture.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        let wire = requests.last().unwrap();
        let body: serde_json::Value = serde_json::from_slice(&wire.body).unwrap();
        assert_eq!(
            body["thinking"]["budget_tokens"], 2048,
            "scheduled settings must not change the main thinking policy"
        );
        assert!(body.get("context_hint").is_none());
        assert!(
            !wire
                .headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                .any(|(_, value)| value
                    .split(',')
                    .any(|beta| beta == compaction::context_hint::CONTEXT_HINT_BETA_HEADER)),
            "per-request beta activation must not leak into the next request"
        );
    }

    #[tokio::test]
    async fn main_nonstream_preserves_dispatch_admission_for_scheduled_and_ordinary_calls() {
        for scheduled in [false, true] {
            let capture = owned_request_capture();
            let adapter = make_adapter(capture.clone());
            let mut request = llm_runtime::MessagesCreateRequest::new(
                "claude-sonnet-4-20250514",
                Some("anthropic"),
                None,
                vec![ConversationMessage::user(
                    lingxi_core::types::MessageId::new(),
                    "hello".into(),
                )],
                Vec::new(),
            );
            request.opts.request_dispatch_admission =
                Some(llm_runtime::RequestDispatchAdmission::new(|| false));
            let call = adapter.messages_create(crate::OrchestratorApiRequest::Main(request));
            let result = if scheduled {
                crate::scheduled_turn::SETTINGS
                    .scope(scheduled_disabled_settings(), call)
                    .await
            } else {
                call.await
            };

            assert!(matches!(
                result,
                Err(LlmError::RequestDispatchRejected {
                    prior_dispatch: false
                })
            ));
            assert!(
                capture.requests.lock().unwrap().is_empty(),
                "{scheduled} Main nonstream denial must stop before SDK transport"
            );
        }
    }

    #[tokio::test]
    async fn main_stream_preserves_dispatch_admission_for_scheduled_and_ordinary_calls() {
        for scheduled in [false, true] {
            let capture = owned_request_capture();
            let adapter = make_adapter(capture.clone());
            let messages = vec![ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "hello".into(),
            )];
            let stream = crate::conversation::StreamingApiClient::stream(
                &adapter,
                "claude-sonnet-4-20250514",
                Some("anthropic"),
                None,
                messages,
                Vec::new(),
                "sdk",
                false,
                Some(llm_runtime::RequestDispatchAdmission::new(|| false)),
            );
            let result = if scheduled {
                crate::scheduled_turn::SETTINGS
                    .scope(scheduled_disabled_settings(), stream)
                    .await
            } else {
                stream.await
            };

            assert!(matches!(
                result,
                Err(LlmError::RequestDispatchRejected {
                    prior_dispatch: false
                })
            ));
            assert!(
                capture.requests.lock().unwrap().is_empty(),
                "{scheduled} Main stream denial must stop before SDK transport"
            );
        }
    }

    fn dispatch_test_generation_guard(
        generation: lingxi_core::host::CancellationToken,
    ) -> Arc<dyn hooks::attachment::HookPublicationGuard> {
        Arc::new(
            crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(
                generation,
                Arc::new(tokio::sync::Mutex::new(())),
            ),
        )
    }

    #[tokio::test]
    async fn main_dispatch_does_not_reject_a_guarded_row_pruned_by_compact_boundary() {
        let capture = owned_request_capture();
        let adapter = make_adapter(capture.clone());
        let generation = lingxi_core::host::CancellationToken::new();
        let pruned_id = lingxi_core::types::MessageId::new();
        let messages = vec![
            ConversationMessage::user_meta(pruned_id, "stale guarded pre-boundary row".into()),
            ConversationMessage::System {
                api_system: None,
                id: lingxi_core::types::MessageId::new(),
                content: "Conversation compacted".into(),
                subtype: Some("compact_boundary".into()),
                compact_metadata: None,
                model_fallback: None,
                refusal_fallback: None,
            },
            ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "current post-boundary summary".into(),
            ),
        ];
        let normalized = llm_runtime::convert::normalize_messages_for_api(messages.clone());
        assert_eq!(normalized.len(), 1);
        assert!(!normalized.iter().any(|message| message.id() == pruned_id));
        assert!(normalized[0]
            .text_content()
            .contains("current post-boundary summary"));

        let guard = dispatch_test_generation_guard(generation.clone());
        let admission = crate::prompt::async_hook_response::request_dispatch_admission(
            &messages,
            &[(pruned_id, guard)],
        )
        .expect("the pre-boundary row is guarded before host normalization");
        generation.cancel();

        let mut request = llm_runtime::MessagesCreateRequest::new(
            "claude-sonnet-4-20250514",
            Some("anthropic"),
            None,
            messages,
            Vec::new(),
        );
        request.opts.request_dispatch_admission = Some(admission);
        let result = adapter
            .messages_create(crate::OrchestratorApiRequest::Main(request))
            .await;

        assert!(
            !matches!(&result, Err(LlmError::RequestDispatchRejected { .. })),
            "a guard for a source row removed by the final semantic history must not block the remaining request"
        );
        assert!(
            result.is_err(),
            "the in-memory transport records then rejects the request"
        );
        let requests = capture.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "the current Main request reaches SDK transport"
        );
        let wire = String::from_utf8(requests[0].body.to_vec()).expect("JSON request bytes");
        assert!(
            !wire.contains("stale guarded pre-boundary row"),
            "pruned content is absent on wire"
        );
        assert!(
            wire.contains("current post-boundary summary"),
            "surviving content is sent"
        );
    }

    #[tokio::test]
    async fn stream_dispatch_keeps_a_stale_guard_for_content_merged_from_its_source_row() {
        let capture = owned_request_capture();
        let adapter = make_adapter(capture.clone());
        let generation = lingxi_core::host::CancellationToken::new();
        let first_id = lingxi_core::types::MessageId::new();
        let guarded_id = lingxi_core::types::MessageId::new();
        let messages = vec![
            ConversationMessage::user(first_id, "ordinary user source".into()),
            ConversationMessage::user_meta(guarded_id, "stale guarded hook reminder".into()),
        ];
        let normalized = llm_runtime::convert::normalize_messages_for_api(messages.clone());
        assert_eq!(normalized.len(), 1, "adjacent user rows merge");
        assert_eq!(
            normalized[0].id(),
            first_id,
            "normalization retains the first user ID"
        );
        assert!(!normalized.iter().any(|message| message.id() == guarded_id));
        assert!(
            normalized[0]
                .text_content()
                .contains("stale guarded hook reminder"),
            "the guarded source content survives the merge"
        );

        let guard = dispatch_test_generation_guard(generation.clone());
        let admission = crate::prompt::async_hook_response::request_dispatch_admission(
            &messages,
            &[(guarded_id, guard)],
        )
        .expect("the merged source row is guarded before host normalization");
        generation.cancel();

        let result = crate::conversation::StreamingApiClient::stream(
            &adapter,
            "claude-sonnet-4-20250514",
            Some("anthropic"),
            None,
            messages,
            Vec::new(),
            "sdk",
            false,
            Some(admission),
        )
        .await;

        assert!(matches!(
            result,
            Err(LlmError::RequestDispatchRejected {
                prior_dispatch: false
            })
        ));
        assert!(
            capture.requests.lock().unwrap().is_empty(),
            "stale content merged into the final stream prompt must be rejected before SDK transport"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn owned_main_and_scheduled_seed_keep_the_ordered_fallback_threshold() {
        let hint = serde_json::json!({"enabled": true, "target_tokens_saved": 75_000});
        for scheduled in [false, true] {
            for offer in [None, Some(hint.clone())] {
                let overloaded = ProviderResponse::json(
                    529,
                    serde_json::json!({"type":"error", "error":{"type":"overloaded_error", "message":"Overloaded"}}),
                );
                let transport = Arc::new(FakeTransport {
                    responses: Mutex::new(vec![
                        FakeResponse::Ok(overloaded.clone()),
                        FakeResponse::Ok(overloaded),
                        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
                    ]),
                    seen: Mutex::new(Vec::new()),
                });
                let adapter = make_adapter(transport.clone());
                adapter
                    .service
                    .set_thinking(llm_runtime::model::thinking::ThinkingConfig::Disabled);
                let mut request = llm_runtime::MessagesCreateRequest::new(
                    "claude-sonnet-4-20250514",
                    Some("anthropic"),
                    None,
                    Vec::new(),
                    Vec::new(),
                );
                request.opts.initial_consecutive_overloaded = Some(1);
                crate::turn_loop::api_recovery::apply_main_request_options(
                    &mut request,
                    Some(4096),
                    Some(compaction::context_hint::ContextHintRequestParams {
                        beta: compaction::context_hint::CONTEXT_HINT_BETA_HEADER,
                        body: offer
                            .as_ref()
                            .map(|hint| serde_json::json!({"context_hint": hint})),
                    }),
                    Some("claude-legacy-no-tools"),
                );
                assert_eq!(request.opts.max_output_tokens, Some(4096));
                assert_eq!(request.opts.context_hint, offer);
                assert!(request.opts.context_hint_beta);
                assert_eq!(
                    request.opts.fallback,
                    llm_runtime::FallbackPolicy::Models(vec!["claude-legacy-no-tools".into()])
                );
                assert_eq!(request.opts.initial_consecutive_overloaded, Some(1));
                let call = adapter.messages_create(crate::OrchestratorApiRequest::Main(request));
                if scheduled {
                    let mut settings = scheduled_disabled_settings();
                    settings.effort = None;
                    crate::scheduled_turn::SETTINGS.scope(settings, call).await
                } else {
                    call.await
                }
                .unwrap();
                let seen = transport.seen.lock().unwrap();
                assert_eq!(seen.len(), 3);
                let models: Vec<_> = seen
                    .iter()
                    .map(|request| request.body_json.string_at("/model").unwrap())
                    .collect();
                assert_eq!(
                    models,
                    [
                        "claude-sonnet-4-20250514",
                        "claude-sonnet-4-20250514",
                        "claude-legacy-no-tools"
                    ],
                    "the prior streaming 529 must consume the first overload slot"
                );
                for wire in seen.iter() {
                    assert_eq!(
                        wire.body_json.unsigned_integer_at("/max_tokens"),
                        Some(4096)
                    );
                    assert_eq!(
                        wire.body_json
                            .payload_at("/context_hint")
                            .as_ref()
                            .map(|p| p.as_json_source()),
                        offer.as_ref().map(serde_json::Value::to_string).as_deref()
                    );
                    assert!(wire
                        .body_json
                        .payload_at("/context_hint/context_hint")
                        .is_none());
                    let beta = wire
                        .headers
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                        .unwrap()
                        .1
                        .as_str();
                    assert_eq!(
                        beta.split(',')
                            .filter(
                                |part| *part == compaction::context_hint::CONTEXT_HINT_BETA_HEADER
                            )
                            .count(),
                        1
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn owned_main_schema_and_hook_policy_remain_separate_inside_a_schedule() {
        let capture = owned_request_capture();
        let adapter = make_adapter_with_session_tool_choice(
            capture.clone(),
            Some(llm_runtime::ToolChoice::Tool {
                name: "StructuredOutput".into(),
            }),
            "claude-sonnet-4-6",
        );
        adapter
            .service
            .set_thinking(llm_runtime::model::thinking::ThinkingConfig::Disabled);
        let main = llm_runtime::MessagesCreateRequest::new(
            "claude-sonnet-4-6",
            Some("anthropic"),
            None,
            Vec::new(),
            vec![structured_output_tool()],
        );
        let main_error = adapter
            .messages_create(crate::OrchestratorApiRequest::Main(main))
            .await
            .unwrap_err();
        let hook = crate::HookPromptRequest::new(
            "claude-sonnet-4-6",
            Some("anthropic"),
            "evaluate hook",
            Vec::new(),
        );
        let hook_error = crate::scheduled_turn::SETTINGS
            .scope(
                scheduled_disabled_settings(),
                adapter.messages_create(crate::OrchestratorApiRequest::HookPrompt(hook)),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&main_error, LlmError::InvalidRequest { message } if message == "captured owned request"),
            "main request must reach the capture transport: {main_error:?}",
        );
        assert!(
            matches!(&hook_error, LlmError::InvalidRequest { message } if message == "captured owned request"),
            "hook request must reach the capture transport: {hook_error:?}",
        );
        let requests = capture.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "both requests must reach the sealed transport"
        );
        let main: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            main["tool_choice"],
            serde_json::json!({"type":"tool", "name":"StructuredOutput"})
        );
        let hook: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert!(hook.get("tool_choice").is_none());
        assert!(hook.get("thinking").is_none());
        assert_eq!(hook["output_config"]["format"]["type"], "json_schema");
        assert_eq!(
            hook["output_config"]["format"]["schema"]["required"],
            serde_json::json!(["ok", "reason"])
        );
        assert!(
            hook["output_config"].get("effort").is_none(),
            "hook evaluation must not inherit scheduled effort"
        );
    }

    #[tokio::test]
    async fn scheduled_settings_reach_wire_without_changing_adapter_defaults() {
        let transport = FakeTransport::always(ProviderResponse {
            status: 200,
            headers: BTreeMap::new(),
            body_json: lingxi_llm_client::protocol::JsonPayload::from_generated_value(
                &ok_response_json(),
            )
            .expect("generated fixture response is valid JSON"),
            request_id: None,
        });
        let adapter =
            make_adapter(transport.clone()).with_initial_effort(Some(serde_json::json!("low")));
        let settings = crate::scheduled_turn::ScheduledSettings {
            model: "claude-sonnet-4-20250514".into(),
            provider: "anthropic".into(),
            reasoning: lingxi_core::host::ReasoningSelection::Disabled,
            thinking: llm_runtime::model::thinking::ThinkingConfig::Disabled,
            effort: None,
        };
        let model_source = Utf16JsonProjection::parse(r#""claude-\ud800""#).unwrap();
        let scheduled_request = adapter
            .build_scheduled_request(
                settings.clone(),
                None,
                false,
                Vec::new(),
                Vec::new(),
                Some(4096),
                Vec::new(),
                Default::default(),
                Default::default(),
                Some(model_source.clone()),
            )
            .unwrap();
        assert_eq!(
            scheduled_request
                .input
                .model_source
                .as_ref()
                .and_then(|source| source.root_string_code_units()),
            model_source.string_units(""),
            "scheduled API input must keep the exact source until SDK finalization"
        );
        // The malformed source has no catalog row during SDK preflight, so
        // supply the required Messages output budget explicitly.
        let stream_error = crate::scheduled_turn::SETTINGS
            .scope(
                settings,
                StreamingApiClient::stream_with_effort_override(
                    &adapter,
                    "ignored/default",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                    Some(4096),
                    None,
                    Vec::new(),
                    "sdk",
                    false,
                    None,
                    None,
                    None,
                    Default::default(),
                    Default::default(),
                    Some(model_source),
                ),
            )
            .await
            .err()
            .expect("the fake transport records the request before returning its sentinel error");
        assert!(
            matches!(
                &stream_error,
                LlmError::InvalidRequest { message }
                    if message == "open_stream not scripted (FakeTransport)"
            ),
            "expected the fake transport sentinel after capture, got {stream_error:?}"
        );
        let request = transport.seen.lock().unwrap()[0].body_json.clone();
        // Native PGn/t1e sanitizes only the outgoing generation payload.
        // The API-input assertion above protects the distinct original source.
        assert!(
            request.as_json_source().contains(r#""model":"claude-�""#),
            "the sealed Claude body must match Native's toWellFormed boundary: {}",
            request.as_json_source()
        );
        assert!(!request.as_json_source().contains(r"\ud800"));
        assert!(
            request.payload_at("/output_config/effort").is_none(),
            "scheduled None must not inherit the adapter's initial effort"
        );
        assert!(crate::scheduled_turn::current().is_none());
        assert_eq!(adapter.current_effort(), Some(serde_json::json!("low")));
    }

    #[tokio::test]
    async fn query_fallback_route_overrides_scheduled_main_only() {
        let capture = owned_request_capture();
        let adapter = make_adapter(capture.clone());
        let route = crate::query_model::ModelRoute {
            model: "claude-legacy-no-tools".into(),
            profile: Some("anthropic".into()),
        };
        let scheduled = scheduled_disabled_settings();
        crate::scheduled_turn::SETTINGS
            .scope(
                scheduled.clone(),
                crate::query_model::ROUTE.scope(Some(route), async {
                    let _ = StreamingApiClient::stream_with_effort_override(
                        &adapter,
                        "caller-cannot-override-scheduled",
                        None,
                        None,
                        Vec::new(),
                        Vec::new(),
                        Some(4096),
                        None,
                        Vec::new(),
                        "sdk",
                        false,
                        None,
                        None,
                        None,
                        Default::default(),
                        Default::default(),
                        None,
                    )
                    .await;
                    let mut request = llm_runtime::MessagesCreateRequest::new(
                        "caller-cannot-override-scheduled",
                        None,
                        None,
                        Vec::new(),
                        Vec::new(),
                    );
                    request.opts.max_output_tokens = Some(4096);
                    let _ = adapter
                        .messages_create(crate::OrchestratorApiRequest::Main(request))
                        .await;
                    let hook = crate::HookPromptRequest::new(
                        "claude-sonnet-4-20250514",
                        Some("anthropic"),
                        "fixture hook",
                        Vec::new(),
                    );
                    let _ = adapter
                        .messages_create(crate::OrchestratorApiRequest::HookPrompt(hook))
                        .await;
                }),
            )
            .await;
        crate::scheduled_turn::SETTINGS
            .scope(
                scheduled,
                adapter.stream(
                    "untrusted-caller",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                    "sdk",
                    false,
                    None,
                ),
            )
            .await
            .ok();
        let requests = capture.requests.lock().unwrap();
        let models: Vec<String> = requests
            .iter()
            .map(|r| {
                serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["model"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            models,
            [
                "claude-legacy-no-tools",
                "claude-legacy-no-tools",
                "claude-sonnet-4-20250514",
                "claude-sonnet-4-20250514"
            ]
        );
        assert!(crate::query_model::current().is_none());
    }

    /// Build the thin `ProviderApiAdapter` over an `ApiService` constructed exactly
    /// as the original `make_adapter` did (a single anthropic profile, ApiKey auth).
    pub(super) fn make_adapter(transport: Arc<dyn Transport>) -> ProviderApiAdapter {
        make_adapter_with_session_tool_choice(transport, None, "claude-sonnet-4-20250514")
    }

    fn make_codex_search_adapter(transport: Arc<dyn Transport>) -> ProviderApiAdapter {
        use llm_runtime::services::sdk;
        use serde_json::json;

        let mut profile: ProviderProfile = serde_json::from_value(json!({
            "provider_id":"open_ai",
            "profile_name":"codex",
            "base_url":"https://chatgpt.com/backend-api/codex",
            "protocol":"open_ai_responses",
            "auth":"none",
            "credential":{"type":"none"},
            "models":[{
                "display_model":"gpt-search",
                "request_model":"gpt-search",
                "billing_model":"gpt-search",
                "capabilities":{
                    "streaming":true,"tools":true,"vision":false,"documents":false,
                    "reasoning":false,"structured_output":false
                }
            }]
        }))
        .expect("valid Harness provider profile");
        profile.protocol = ProtocolFamily::OpenAiResponses;
        let mut wire_profile: sdk::protocol::ProviderProfile = serde_json::from_value(json!({
            "provider_id":"openai-chatgpt",
            "profile_name":"codex",
            "base_url":"https://chatgpt.com/backend-api/codex",
            "protocol":"open_ai_responses",
            "auth":"none",
            "models":[],
            "extra":{"web_search":"openai_responses"}
        }))
        .expect("valid Codex wire profile");
        wire_profile.regions = sdk::protocol::Region::all();
        profile.wire_profile = Some(wire_profile);
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![profile],
            })
            .expect("valid Codex model runtime"),
        );
        let service = ApiService::new(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv::default(),
            "test",
            None,
            None,
        );
        ProviderApiAdapter::new(Arc::new(service))
    }

    #[test]
    fn main_and_child_refusal_snapshots_share_current_route_facts_without_transport() {
        use lingxi_core::host::refusal_api_text::{
            RefusalApiTextSnapshot, RefusalBrandCopyOwned, RefusalProviderKind,
        };

        let capture = owned_request_capture();
        let routes = Arc::new(Mutex::new(Vec::new()));
        let feedback = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let adapter =
            make_adapter(capture.clone()).with_refusal_api_text_snapshot_source(Arc::new({
                let routes = routes.clone();
                let feedback = feedback.clone();
                move |model, profile| {
                    routes
                        .lock()
                        .unwrap()
                        .push((model.to_owned(), profile.map(str::to_owned)));
                    Ok(RefusalApiTextSnapshot {
                        serving_model: Some(model.to_owned()),
                        model_eligible: true,
                        display_label: Some("Claude Sonnet 5".into()),
                        model_family: Some("sonnet".into()),
                        fable_copy_suppressed: false,
                        opus_5_5_exception: false,
                        help_url: Some("https://support.claude.com/en/articles/8106465".into()),
                        provider: RefusalProviderKind::FirstParty,
                        interactive: true,
                        feedback_eligible: feedback.load(std::sync::atomic::Ordering::SeqCst),
                        brand: RefusalBrandCopyOwned {
                            api_error_prefix: "API Error".into(),
                            product_name: format!("{} Code", branding::PRODUCT_NAME),
                            generic_model_label: branding::PRODUCT_NAME.into(),
                        },
                    })
                }
            }));

        let main =
            OrchestratorApiClient::refusal_api_text_snapshot(&adapter, "claude-sonnet-5", Some(""))
                .unwrap()
                .unwrap();
        feedback.store(false, std::sync::atomic::Ordering::SeqCst);
        let child = agent::SubagentApiClient::refusal_api_text_snapshot(
            &adapter,
            "claude-sonnet-5[1m]",
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(main.serving_model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(child.serving_model.as_deref(), Some("claude-sonnet-5[1m]"));
        assert!(main.feedback_eligible);
        assert!(!child.feedback_eligible);
        let main_text = main.format(Some("cyber"), Some("req_main")).unwrap();
        let child_text = child.format(Some("cyber"), Some("req_child")).unwrap();
        assert!(main_text.contains("Send feedback with /feedback"));
        assert!(!child_text.contains("Send feedback with /feedback"));
        assert!(main_text.ends_with("Request ID: req_main"));
        assert!(child_text.ends_with("Request ID: req_child"));
        assert_eq!(
            *routes.lock().unwrap(),
            [
                ("claude-sonnet-5".to_owned(), Some(String::new())),
                ("claude-sonnet-5[1m]".to_owned(), None),
            ]
        );
        assert!(capture.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn refusal_snapshot_absence_and_resolution_failure_are_not_synthetic_facts() {
        let capture = owned_request_capture();
        let adapter = make_adapter(capture.clone());
        assert!(OrchestratorApiClient::refusal_api_text_snapshot(
            &adapter,
            "claude-sonnet-5",
            Some("anthropic"),
        )
        .unwrap()
        .is_none());
        let adapter = adapter.with_refusal_api_text_snapshot_source(Arc::new(|_, _| {
            Err(LlmError::ModelUnavailable)
        }));
        assert!(matches!(
            OrchestratorApiClient::refusal_api_text_snapshot(&adapter, "unavailable", None),
            Err(LlmError::ModelUnavailable)
        ));
        assert!(matches!(
            agent::SubagentApiClient::refusal_api_text_snapshot(&adapter, "unavailable", None),
            Err(LlmError::ModelUnavailable)
        ));
        assert!(capture.requests.lock().unwrap().is_empty());
    }

    fn make_adapter_with_session_tool_choice(
        transport: Arc<dyn Transport>,
        main_choice: Option<llm_runtime::ToolChoice>,
        request_model: &str,
    ) -> ProviderApiAdapter {
        make_adapter_with_session_tool_choice_configured(
            transport,
            main_choice,
            request_model,
            |service| service,
        )
    }

    fn make_adapter_with_session_tool_choice_configured(
        transport: Arc<dyn Transport>,
        main_choice: Option<llm_runtime::ToolChoice>,
        request_model: &str,
        configure_service: impl FnOnce(ApiService) -> ApiService,
    ) -> ProviderApiAdapter {
        std::env::set_var("ADAPTER_TEST_KEY", "test-key");
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![ProviderProfile {
                    wire_profile: None,
                    regions: llm_runtime::Region::all(),
                    provider_id: ProviderId::AnthropicFirstParty,
                    profile_name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    protocol: ProtocolFamily::AnthropicMessages,
                    auth: AuthStrategy::ApiKey,
                    credential: CredentialConfig::Env {
                        var: "ADAPTER_TEST_KEY".to_string(),
                    },
                    models: vec![
                        ModelProfile {
                            display_model: request_model.to_string(),
                            request_model: request_model.to_string(),
                            billing_model: "claude-sonnet-4".to_string(),
                            aliases: vec!["claude".to_string()],
                            description: None,
                            metadata: Default::default(),
                            capabilities: Capabilities {
                                streaming: true,
                                tools: true,
                                reasoning: true,
                                // This fixture exercises native schema requests as well as tools.
                                structured_output: true,
                                ..Default::default()
                            },
                        },
                        ModelProfile {
                            display_model: "claude-legacy-no-tools".to_string(),
                            request_model: "claude-legacy-no-tools".to_string(),
                            billing_model: "claude-legacy-no-tools".to_string(),
                            aliases: Vec::new(),
                            description: None,
                            metadata: Default::default(),
                            capabilities: Capabilities {
                                streaming: true,
                                tools: false,
                                reasoning: false,
                                ..Default::default()
                            },
                        },
                    ],
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
            .expect("client"),
        );
        let service = configure_service(ApiService::new(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv {
                user_type: Some("external".to_string()),
                entrypoint: Some("cli".to_string()),
                ..Default::default()
            },
            "0.0.0",
            None,
            None,
        ));
        let service = match main_choice {
            Some(choice) => service.with_forced_tool_choice(choice),
            None => service,
        };
        ProviderApiAdapter::new(Arc::new(service))
    }

    #[tokio::test]
    async fn main_stream_propagates_fast_policy_source_error_before_transport() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let projection_from_json = |json: &str| {
            let units = json.encode_utf16().collect::<Vec<_>>();
            Utf16JsonProjection::parse_utf16_json(&units)
                .expect("test JSON keeps its source-preserving UTF-16 projection")
        };
        let expected_error = LlmError::SettingsSource {
            kind: "managed-settings".to_string(),
            message: projection_from_json(r#""fast policy \ud800 failed""#),
            diagnostics: projection_from_json(r#"{"source":"managed","detail":"\udfff"}"#),
            effective: Some(projection_from_json(
                r#"{"model":"claude-opus-4-8","note":"\ud801"}"#,
            )),
        };
        let policy_error = expected_error.clone();
        let adapter = make_adapter_with_session_tool_choice_configured(
            transport.clone(),
            None,
            "claude-opus-4-8",
            move |service| {
                service.with_fast_policy_source(Arc::new(move || Err(policy_error.clone())))
            },
        )
        .with_fast_mode(Arc::new(std::sync::atomic::AtomicBool::new(true)));

        let actual_error = StreamingApiClient::stream(
            &adapter,
            "claude-opus-4-8",
            Some("anthropic"),
            None,
            Vec::new(),
            Vec::new(),
            "main",
            false,
            None,
        )
        .await
        .err()
        .expect("enabled Fast mode must propagate settings-source failures");

        assert_eq!(actual_error, expected_error);
        let LlmError::SettingsSource { message, .. } = &actual_error else {
            panic!("expected the original SettingsSource error")
        };
        assert_eq!(
            message.string_units(""),
            Some(vec![
                b'f' as u16,
                b'a' as u16,
                b's' as u16,
                b't' as u16,
                b' ' as u16,
                b'p' as u16,
                b'o' as u16,
                b'l' as u16,
                b'i' as u16,
                b'c' as u16,
                b'y' as u16,
                b' ' as u16,
                0xd800,
                b' ' as u16,
                b'f' as u16,
                b'a' as u16,
                b'i' as u16,
                b'l' as u16,
                b'e' as u16,
                b'd' as u16,
            ])
        );
        assert_eq!(
            transport.seen_count(),
            0,
            "policy failure must precede dispatch"
        );
    }

    #[tokio::test]
    async fn cancelled_thinking_retry_is_durable_before_cold_resume_without_duplicate_marker() {
        use crate::test_support::{
            noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
        };
        struct RetryTransport(tokio::sync::Notify);
        impl llm_runtime::test_support::FixtureTransport for RetryTransport {
            fn execute<'a>(
                &'a self,
                request: &'a ProviderRequest,
            ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
                Box::pin(async move {
                    let thinking = request_has_content_block_type(&request.body_json, "thinking");
                    if thinking {
                        Ok(ProviderResponse::json(
                            400,
                            serde_json::json!({
                                "type":"error", "error":{"type":"invalid_request_error", "message":"Invalid signature in thinking block"}
                            }),
                        ))
                    } else {
                        self.0.notify_one();
                        std::future::pending().await
                    }
                })
            }
            fn open_stream<'a>(
                &'a self,
                _: &'a ProviderRequest,
            ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
                Box::pin(async {
                    Err(LlmError::Transport {
                        message: "unused".into(),
                    })
                })
            }
        }
        llm_runtime::impl_fixture_transport!(RetryTransport);
        let transport = Arc::new(RetryTransport(tokio::sync::Notify::new()));
        let adapter = Arc::new(make_adapter(transport.clone()));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                dir.path().to_path_buf(),
            )),
        ));
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            adapter.clone(),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_jsonl_writer(writer);
        let old = ConversationMessage::Assistant {
            per_turn_effort: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![
                lingxi_core::types::ContentBlock::Thinking {
                    thinking: "rejected".into(),
                    signature: Some("sig".into()),
                },
                lingxi_core::types::ContentBlock::Text {
                    text: "old answer".into(),
                    citations: None,
                },
            ],
            stop_reason: Some("end_turn".into()),
            resumable: None,
        };
        orch.session.lock().await.history.push(old.clone());
        orch.persist_message_to_jsonl(&old).await;
        {
            let call = orch.scope_api_session(
                false,
                OrchestratorApiClient::messages_create(
                    adapter.as_ref(),
                    crate::OrchestratorApiRequest::Main(llm_runtime::MessagesCreateRequest::new(
                        "claude-sonnet-4-20250514",
                        None,
                        None,
                        vec![old.clone()],
                        vec![],
                    )),
                ),
            );
            tokio::pin!(call);
            tokio::select! {
                _ = transport.0.notified() => {},
                result = &mut call => panic!("retry should remain pending: {result:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => panic!("retry did not start"),
            }
            // Dropping the in-flight retry skips every post-call persistence seam.
        }
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        let rows: Vec<session::JsonlMessage> = body
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            rows.len(),
            2,
            "one assistant and its recovery attachment: {body}"
        );
        assert_eq!(rows[1].parent_uuid.as_deref(), Some(rows[0].uuid.as_str()));
        let resumed = crate::resume::state_from_messages(
            orch.session.lock().await.session_id.as_uuid(),
            &rows,
        );
        assert!(resumed.thinking_signature_stripped);
        assert!(!resumed.thinking_stripped_messages.is_empty());
        let mut resumed_history = resumed.history.clone();
        llm_runtime::model::thinking_signature::strip_marked_conversation_thinking(
            &mut resumed_history,
            &resumed.thinking_stripped_messages,
        );
        assert!(resumed_history.iter().all(|m| {
            match m {
                ConversationMessage::Assistant { content, .. } => !content
                    .iter()
                    .any(|b| matches!(b, lingxi_core::types::ContentBlock::Thinking { .. })),
                _ => true,
            }
        }));
        orch.persist_thinking_signature_strip_latch().await;
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            body,
            "post-call synchronization cannot duplicate a durable recovery marker"
        );
    }

    #[tokio::test]
    async fn shared_adapter_isolates_thinking_recovery_for_identical_parent_ids() {
        use llm_runtime::thinking_scope::{scope_thinking_recovery, ThinkingRecoveryScope};
        struct ScopedTransport {
            seen: Mutex<Vec<ProviderRequest>>,
        }
        impl llm_runtime::test_support::FixtureTransport for ScopedTransport {
            fn execute<'a>(
                &'a self,
                request: &'a ProviderRequest,
            ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
                Box::pin(async move {
                    // Both sessions enter the transport before either resumes.
                    tokio::task::yield_now().await;
                    self.seen.lock().unwrap().push(request.clone());
                    let body = &request.body_json;
                    let rejecting = body.as_json_source().contains("reject-session");
                    let has_thinking = request_has_content_block_type(body, "thinking");
                    Ok(if rejecting && has_thinking {
                        ProviderResponse::json(
                            400,
                            serde_json::json!({
                                "type":"error", "error":{"type":"invalid_request_error",
                                "message":"Invalid signature in thinking block"}
                            }),
                        )
                    } else {
                        ProviderResponse::json(200, ok_response_json())
                    })
                })
            }
            fn open_stream<'a>(
                &'a self,
                _: &'a ProviderRequest,
            ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
                Box::pin(async {
                    Err(LlmError::Transport {
                        message: "unused".into(),
                    })
                })
            }
        }
        llm_runtime::impl_fixture_transport!(ScopedTransport);
        // Use the same production adapter and service for both query owners.
        let transport = Arc::new(ScopedTransport {
            seen: Mutex::new(Vec::new()),
        });
        let adapter = make_adapter(transport.clone());
        let old = ConversationMessage::Assistant {
            per_turn_effort: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![
                lingxi_core::types::ContentBlock::Thinking {
                    thinking: "parent".into(),
                    signature: Some("sig".into()),
                },
                lingxi_core::types::ContentBlock::Text {
                    text: "answer".into(),
                    citations: None,
                },
            ],
            stop_reason: Some("end_turn".into()),
            resumable: None,
        };
        let a = ThinkingRecoveryScope::default();
        let b = ThinkingRecoveryScope::default();
        let (ra, rb) = tokio::join!(
            scope_thinking_recovery(
                a.clone(),
                OrchestratorApiClient::messages_create(
                    &adapter,
                    crate::OrchestratorApiRequest::Main(llm_runtime::MessagesCreateRequest::new(
                        "claude-sonnet-4-20250514",
                        None,
                        Some(custom_test_system_prompt("reject-session")),
                        vec![old.clone()],
                        vec![]
                    ))
                )
            ),
            scope_thinking_recovery(
                b.clone(),
                OrchestratorApiClient::messages_create(
                    &adapter,
                    crate::OrchestratorApiRequest::Main(llm_runtime::MessagesCreateRequest::new(
                        "claude-sonnet-4-20250514",
                        None,
                        Some(custom_test_system_prompt("healthy-session")),
                        vec![old.clone()],
                        vec![]
                    ))
                )
            ),
        );
        ra.unwrap();
        rb.unwrap();
        assert_eq!(a.messages().get(&old.id()), Some(&0));
        assert!(b.messages().is_empty());
        let fresh = ConversationMessage::Assistant {
            per_turn_effort: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![
                lingxi_core::types::ContentBlock::Thinking {
                    thinking: "fresh".into(),
                    signature: Some("fresh-sig".into()),
                },
                lingxi_core::types::ContentBlock::Text {
                    text: "fresh answer".into(),
                    citations: None,
                },
            ],
            stop_reason: Some("end_turn".into()),
            resumable: None,
        };
        let history = vec![
            old.clone(),
            ConversationMessage::user(lingxi_core::types::MessageId::new(), "continue".into()),
            fresh.clone(),
        ];
        let (ra, rb) = tokio::join!(
            scope_thinking_recovery(
                a.clone(),
                OrchestratorApiClient::messages_create(
                    &adapter,
                    crate::OrchestratorApiRequest::Main(llm_runtime::MessagesCreateRequest::new(
                        "claude-sonnet-4-20250514",
                        None,
                        Some(custom_test_system_prompt("next-a")),
                        history.clone(),
                        vec![]
                    ))
                )
            ),
            scope_thinking_recovery(
                b.clone(),
                OrchestratorApiClient::messages_create(
                    &adapter,
                    crate::OrchestratorApiRequest::Main(llm_runtime::MessagesCreateRequest::new(
                        "claude-sonnet-4-20250514",
                        None,
                        Some(custom_test_system_prompt("next-b")),
                        history,
                        vec![]
                    ))
                )
            ),
        );
        ra.unwrap();
        rb.unwrap();
        for (label, expected) in [("next-a", 1), ("next-b", 2)] {
            let seen = transport.seen.lock().unwrap();
            let body = &seen
                .iter()
                .find(|r| r.body_json.as_json_source().contains(label))
                .unwrap()
                .body_json;
            let thinking = request_content_block_type_count(body, "thinking");
            assert_eq!(thinking, expected, "{label}: {}", body.as_json_source());
        }
        assert!(!a.messages().contains_key(&fresh.id()));
        assert!(b.messages().is_empty());
        assert!(OrchestratorApiClient::thinking_stripped_messages(&adapter).is_empty());
    }

    #[test]
    fn list_model_listings_exposes_actual_configured_catalog() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let listings = OrchestratorApiClient::list_model_listings(&adapter);
        assert_eq!(
            listings.len(),
            1,
            "only configured, tool-capable routes are listed"
        );
        let listing = &listings[0];
        assert_eq!(listing.provider_id, "anthropic");
        assert_eq!(listing.provider_label, "Anthropic");
        assert_eq!(listing.request_model, "claude-sonnet-4-20250514");
        assert!(listing.capabilities.tools);
        assert!(listing.capabilities.reasoning);
    }

    #[test]
    fn list_model_listings_excludes_no_tool_models() {
        // Models with `tools=false` cannot complete an agentic turn and are
        // filtered from the actual configured catalog.
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let listings = OrchestratorApiClient::list_model_listings(&adapter);
        let has = |id: &str| listings.iter().any(|l| l.request_model == id);

        assert!(has("claude-sonnet-4-20250514"));
        assert!(!has("claude-legacy-no-tools"));
    }

    #[test]
    fn list_model_listings_surfaces_reasoning_capability() {
        // The picker's thinking indicator reads `supports_reasoning` off each
        // listing (populated from the catalog `capabilities.reasoning`).
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let listings = OrchestratorApiClient::list_model_listings(&adapter);
        let reasoning = |id: &str| {
            listings
                .iter()
                .find(|l| l.request_model == id)
                .map(|l| l.supports_reasoning)
        };
        assert_eq!(reasoning("claude-sonnet-4-20250514"), Some(true));
        let listing = listings
            .iter()
            .find(|listing| listing.request_model == "claude-sonnet-4-20250514")
            .expect("configured route listed");
        assert!(!listing.reasoning.modifiable);
        assert_eq!(
            listing.reasoning.disabled_reason.as_deref(),
            Some("reasoning_unavailable"),
            "an unverified custom/legacy id stays auto-only even when its broad capability flag is true"
        );
    }

    #[test]
    fn model_description_matches_known_families() {
        assert_eq!(
            model_description("claude-opus-4-7"),
            Some("Best for everyday, complex tasks")
        );
        assert_eq!(
            model_description("anthropic/claude-sonnet-4-6"),
            Some("Efficient for routine tasks")
        );
        assert_eq!(
            model_description("claude-3-5-haiku"),
            Some("Fastest for quick answers")
        );
        assert_eq!(model_description("gpt-4o"), None);
    }

    #[tokio::test]
    async fn subagent_api_client_seam_forwards_through_trait_object() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport.clone());
        let seam: Arc<dyn agent::SubagentApiClient> = Arc::new(adapter);
        let result = seam
            .stream(agent::api::SubagentApiRequest {
                model: "claude-sonnet-4-20250514".into(),
                profile: None,
                system: Some("sys".into()),
                messages: Vec::new(),
                tools: Vec::new(),
                forced_tool: None,
                effort: None,
                opts: agent::api::SubagentApiCallOpts::default(),
            })
            .await;
        // May succeed or fail with UnsupportedCapability if stream not configured,
        // but must not panic.
        let _ = result;
    }

    /// The owned request carries the output ceiling for both ordinary and
    /// forced-tool calls. FakeTransport records stream-opening requests before
    /// returning its scripted transport error, so the assertions inspect the
    /// real wire body without requiring a successful model response.
    #[tokio::test]
    async fn subagent_stream_request_threads_output_limit_onto_the_wire() {
        let auto_transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let auto_seam: Arc<dyn agent::SubagentApiClient> =
            Arc::new(make_adapter(auto_transport.clone()));
        let auto_error = auto_seam
            .stream(agent::api::SubagentApiRequest {
                model: "claude-sonnet-4-20250514".into(),
                profile: None,
                system: None,
                messages: Vec::new(),
                tools: Vec::new(),
                forced_tool: None,
                effort: None,
                opts: agent::api::SubagentApiCallOpts {
                    max_output_tokens: Some(777),
                    model_attempt: None,
                    query_source_label: Some("fusion_panel".to_string()),
                },
            })
            .await
            .err();
        let auto_seen = auto_transport.seen.lock().unwrap();
        let auto_request = auto_seen
            .last()
            .unwrap_or_else(|| panic!("auto request did not dispatch: {auto_error:?}"));
        assert_eq!(
            auto_request.body_json.unsigned_integer_at("/max_tokens"),
            Some(777),
            "wire max_tokens must equal the requested ceiling, not the model's auto default; body: {}",
            auto_request.body_json.as_json_source()
        );
        drop(auto_seen);

        let forced_transport =
            FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let forced_seam: Arc<dyn agent::SubagentApiClient> =
            Arc::new(make_adapter(forced_transport.clone()));
        let _ = forced_seam
            .stream(agent::api::SubagentApiRequest {
                model: "claude-sonnet-4-20250514".into(),
                profile: None,
                system: None,
                messages: Vec::new(),
                tools: vec![structured_output_tool()],
                forced_tool: Some("StructuredOutput".into()),
                effort: None,
                opts: agent::api::SubagentApiCallOpts {
                    max_output_tokens: Some(321),
                    model_attempt: None,
                    query_source_label: Some("fusion_panel".to_string()),
                },
            })
            .await;
        let forced_seen = forced_transport.seen.lock().unwrap();
        let forced_request = forced_seen
            .last()
            .expect("the forced opts-aware call reached the transport");
        assert_eq!(
            forced_request.body_json.unsigned_integer_at("/max_tokens"),
            Some(321),
            "forced opts-aware wire max_tokens must equal the requested ceiling; body: {}",
            forced_request.body_json.as_json_source()
        );
    }

    #[tokio::test]
    async fn child_tool_choice_is_request_local_on_shared_main_structured_output_service() {
        struct CaptureWire {
            bodies: Mutex<Vec<Vec<u8>>>,
        }
        #[async_trait]
        impl Transport for CaptureWire {
            async fn send(
                &self,
                request: llm_runtime::services::sdk::HttpRequest,
            ) -> Result<
                llm_runtime::services::sdk::StreamResponse,
                llm_runtime::services::sdk::protocol::LlmError,
            > {
                self.bodies.lock().unwrap().push(request.body.to_vec());
                Err(
                    llm_runtime::services::sdk::protocol::LlmError::InvalidRequest {
                        message: "captured tool-choice request".into(),
                    },
                )
            }
        }

        let transport = Arc::new(CaptureWire {
            bodies: Mutex::new(Vec::new()),
        });
        let adapter = make_adapter_with_session_tool_choice(
            transport.clone(),
            Some(llm_runtime::ToolChoice::Tool {
                name: "StructuredOutput".into(),
            }),
            "claude-sonnet-4-20250514",
        );
        // This fixture model rejects forced tools with manual thinking.
        // Select a valid main structured-output session for all four requests.
        adapter
            .service
            .set_thinking(llm_runtime::model::thinking::ThinkingConfig::Disabled);
        let child_seam: &dyn agent::SubagentApiClient = &adapter;
        let tool = |name: &str| {
            let source = lingxi_core::host::SourceToolWireDefinition {
                name: name.into(),
                description: lingxi_core::types::utf16_json::Utf16JsonProjection::from("Test tool"),
                input_schema: lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                    r#"{"type":"object","properties":{}}"#,
                )
                .unwrap(),
                strict: false,
                defer_loading: false,
            };
            llm_runtime::convert::to_tool_declarations(vec![source])
                .unwrap()
                .pop()
                .unwrap()
        };
        let cases = [
            ("ordinary-child", None, vec!["Read"], 111),
            ("fusion-panel", None, vec!["Read", "PanelResult"], 222),
            (
                "designated-child",
                Some("PanelResult"),
                vec!["Read", "PanelResult"],
                333,
            ),
        ];
        for (index, (label, forced_tool, tool_names, ceiling)) in cases.into_iter().enumerate() {
            let error = child_seam
                .stream(agent::api::SubagentApiRequest {
                    model: "claude-sonnet-4-20250514".into(),
                    profile: Some("anthropic".into()),
                    system: Some("child-system".into()),
                    messages: vec![ConversationMessage::user(
                        lingxi_core::types::MessageId::new(),
                        label.into(),
                    )],
                    tools: tool_names.iter().map(|name| tool(name)).collect(),
                    forced_tool: forced_tool.map(str::to_string),
                    effort: None,
                    opts: agent::api::SubagentApiCallOpts {
                        max_output_tokens: Some(ceiling),
                        model_attempt: None,
                        query_source_label: Some(label.into()),
                    },
                })
                .await
                .err()
                .expect("the capture transport returns a terminal scripted error");
            assert!(matches!(
                error,
                LlmError::InvalidRequest { message }
                    if message == "captured tool-choice request"
            ));
            let bodies = transport.bodies.lock().unwrap();
            assert_eq!(bodies.len(), index + 1, "one actual dispatch per child");
            let body: serde_json::Value = serde_json::from_slice(&bodies[index]).unwrap();
            assert_eq!(body["model"], "claude-sonnet-4-20250514");
            assert_eq!(body["max_tokens"], ceiling);
            assert_eq!(body["messages"][0]["content"][0]["text"], label);
            let actual_tools = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(actual_tools, tool_names, "no hidden main schema tool");
            match forced_tool {
                Some(name) => assert_eq!(
                    body["tool_choice"],
                    serde_json::json!({"type": "tool", "name": name})
                ),
                None => assert!(
                    body.get("tool_choice").is_none(),
                    "{label} must omit tool_choice from the actual wire body: {body}"
                ),
            }
        }

        let main_error = StreamingApiClient::stream(
            &adapter,
            "claude-sonnet-4-20250514",
            Some("anthropic"),
            Some(&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::custom_prompt(
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                    "main-system",
                ),
            )),
            vec![ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "main-turn".into(),
            )],
            vec![tool("Read").into(), tool("StructuredOutput").into()],
            "sdk", false, None,
        )
        .await
        .err()
        .expect("the main request reaches the same capture transport");
        let bodies = transport.bodies.lock().unwrap();
        let captured_main = bodies
            .get(3)
            .map(|body| serde_json::from_slice::<serde_json::Value>(body).unwrap());
        assert!(
            matches!(
                &main_error,
                LlmError::InvalidRequest { message } if message == "captured tool-choice request"
            ),
            "main error: {main_error:?}; captured main request: {captured_main:?}; actual dispatch count: {}",
            bodies.len()
        );
        assert_eq!(bodies.len(), 4);
        let main_body: serde_json::Value = serde_json::from_slice(&bodies[3]).unwrap();
        assert_eq!(
            main_body["tool_choice"],
            serde_json::json!({"type": "tool", "name": "StructuredOutput"})
        );
        assert_eq!(main_body["tools"][1]["name"], "StructuredOutput");
    }

    /// The trait default (used by mocks / non-routing impls) is the byte/4
    /// approximation over the conversation text: `(system + msg text) / 4`,
    /// floored at 1.
    #[tokio::test]
    async fn count_tokens_default_impl_is_byte_over_four_approximation() {
        let mock = crate::test_support::MockApiClient::new(vec![]);
        // system = 8 bytes; one user message of 40 bytes → (8 + 40) / 4 = 12.
        let msgs = vec![lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "1234567890123456789012345678901234567890".to_string(),
        )];
        let count = OrchestratorApiClient::count_tokens(
            &mock,
            "any-model",
            None,
            Some("12345678"),
            msgs,
            Vec::new(),
        )
        .await
        .expect("default count_tokens ok");
        assert_eq!(count, 12, "(8 system + 40 user) / 4 = 12 tokens");
    }

    // ── Fix 2: RepeatedOverloaded → LlmError::Overloaded { repeated: true } → OrchestratorError ──

    /// Fix 2 end-to-end: a scripted transport that returns 529 three times triggers
    /// the external non-sandbox `DriveStep::RepeatedOverloaded` branch, which the
    /// adapter surfaces as `LlmError::Overloaded { repeated: true }`.  The
    /// `From<LlmError>` conversion on `OrchestratorError` then produces
    /// `OrchestratorError::RepeatedOverloaded` whose Display equals the byte-locked
    /// `"Repeated 529 Overloaded errors"` copy (`errors.ts:166`).
    #[tokio::test]
    async fn repeated_529_terminal_maps_to_byte_locked_copy() {
        use crate::error::{OrchestratorError, REPEATED_529_ERROR_MESSAGE};

        // Transport that always returns 529 overloaded.
        let overloaded_body = serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        });
        let transport = FakeTransport::always(ProviderResponse::json(529, overloaded_body));
        // make_adapter wires user_type=Some("external") in the UserAgentEnv but
        // resolve_retry_control reads USER_TYPE from ResolveRetryEnv::from_process_env().
        // Set the env vars temporarily to gate allow_fallback + is_external.
        // std::env::set_var is deprecated (Rust 2024) but not removed; acceptable
        // in test-only code.
        #[allow(deprecated)]
        std::env::set_var("USER_TYPE", "external");
        #[allow(deprecated)]
        std::env::set_var("FALLBACK_FOR_ALL_PRIMARY_MODELS", "1");
        #[allow(deprecated)]
        std::env::remove_var("IS_SANDBOX");

        let adapter = make_adapter(transport);
        let llm_result = adapter
            .messages_create(crate::OrchestratorApiRequest::Main(
                llm_runtime::MessagesCreateRequest::new(
                    "claude-sonnet-4-20250514",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                ),
            ))
            .await;

        // Clean up before any assert that might panic.
        #[allow(deprecated)]
        std::env::remove_var("FALLBACK_FOR_ALL_PRIMARY_MODELS");
        #[allow(deprecated)]
        std::env::remove_var("USER_TYPE");

        // The adapter must return Err(LlmError::Overloaded { repeated: true }).
        match &llm_result {
            Err(LlmError::Overloaded { repeated: true }) => {} // correct
            other => {
                panic!("expected Err(LlmError::Overloaded {{ repeated: true }}), got {other:?}")
            }
        }

        // The OrchestratorError conversion must yield RepeatedOverloaded.
        let orch_err: OrchestratorError = llm_result.unwrap_err().into();
        assert!(
            matches!(orch_err, OrchestratorError::RepeatedOverloaded),
            "OrchestratorError must be RepeatedOverloaded, got {orch_err:?}"
        );
        assert_eq!(
            orch_err.to_string(),
            REPEATED_529_ERROR_MESSAGE,
            "Display must equal the byte-locked copy"
        );
    }

    // ── 3c-T3: HistoryResponse.cost populated from cost estimator ─────────────────

    fn make_adapter_with_estimator(transport: Arc<dyn Transport>) -> ProviderApiAdapter {
        use crate::cost_wiring::llm_catalog_from_cost;
        use cost::pricing::PricingCatalog as CostCatalog;
        use llm_runtime::{CostEstimator, PricingPolicy};
        #[allow(deprecated)]
        std::env::set_var("ADAPTER_TEST_KEY", "test-key");
        let cost_cat = CostCatalog::builtin_reference();
        let llm_cat = llm_catalog_from_cost(&cost_cat);
        let estimator = Arc::new(CostEstimator::new(llm_cat, PricingPolicy::MarkUnestimated));

        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![ProviderProfile {
                    wire_profile: None,
                    regions: llm_runtime::Region::all(),
                    provider_id: ProviderId::AnthropicFirstParty,
                    profile_name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    protocol: ProtocolFamily::AnthropicMessages,
                    auth: AuthStrategy::ApiKey,
                    credential: CredentialConfig::Env {
                        var: "ADAPTER_TEST_KEY".to_string(),
                    },
                    models: vec![ModelProfile {
                        display_model: "claude-sonnet-4-20250514".to_string(),
                        request_model: "claude-sonnet-4-20250514".to_string(),
                        billing_model: "claude-sonnet-4".to_string(),
                        aliases: vec!["claude".to_string()],
                        description: None,
                        metadata: Default::default(),
                        capabilities: Capabilities {
                            streaming: true,
                            tools: true,
                            reasoning: true,
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
            .expect("client"),
        );
        ProviderApiAdapter::new(Arc::new(ApiService::new_with_estimator(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv {
                user_type: Some("external".to_string()),
                entrypoint: Some("cli".to_string()),
                ..Default::default()
            },
            "0.0.0",
            None,
            None,
            Some(estimator),
        )))
    }

    /// 3c-T3: adapter populates response.cost for a priced model.
    ///
    /// claude-sonnet-4 billing_model → catalog hit → cost is Some(estimate with
    /// total_cost_usd present).
    #[tokio::test]
    async fn cost_populated_for_priced_model() {
        let response_json = serde_json::json!({
            "id": "msg_cost_test",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1_000_000, "output_tokens": 1_000_000}
        });
        let transport = FakeTransport::always(ProviderResponse::json(200, response_json));
        let adapter = make_adapter_with_estimator(transport);
        let resp = adapter
            .messages_create(crate::OrchestratorApiRequest::Main(
                llm_runtime::MessagesCreateRequest::new(
                    "claude-sonnet-4-20250514",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                ),
            ))
            .await
            .expect("ok");
        let cost = resp.cost.expect("cost must be Some for a priced model");
        let total = cost.total_cost_usd.expect("total_cost_usd must be Some");
        // claude-sonnet-4: input 3_000 nano → 3.0 usd/M × 1M + output 15_000 → 15.0 × 1M = 18.0
        assert!(
            (total - 18.0).abs() < 1e-9,
            "expected total $18.0 for 1M in + 1M out at $3/$15, got ${total}"
        );
    }

    /// 3c-T3: unknown billing model → cost stays None (no error).
    ///
    /// The adapter uses a model profile whose billing_model ("claude-sonnet-4")
    /// IS in the catalog; to test the None path we use a profile with a
    /// billing_model that has no entry.
    #[tokio::test]
    async fn cost_none_for_unpriced_model() {
        // Build an adapter with an estimator but a billing model not in the catalog.
        use crate::cost_wiring::llm_catalog_from_cost;
        use cost::pricing::PricingCatalog as CostCatalog;
        use llm_runtime::{CostEstimator, PricingPolicy};
        #[allow(deprecated)]
        std::env::set_var("ADAPTER_TEST_KEY2", "test-key");
        let cost_cat = CostCatalog::builtin_reference();
        let llm_cat = llm_catalog_from_cost(&cost_cat);
        let estimator = Arc::new(CostEstimator::new(llm_cat, PricingPolicy::MarkUnestimated));

        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![ProviderProfile {
                    wire_profile: None,
                    regions: llm_runtime::Region::all(),
                    provider_id: ProviderId::AnthropicFirstParty,
                    profile_name: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com".to_string(),
                    protocol: ProtocolFamily::AnthropicMessages,
                    auth: AuthStrategy::ApiKey,
                    credential: CredentialConfig::Env {
                        var: "ADAPTER_TEST_KEY2".to_string(),
                    },
                    models: vec![ModelProfile {
                        display_model: "claude-future-9999".to_string(),
                        request_model: "claude-future-9999".to_string(),
                        // billing_model not in any catalog entry
                        billing_model: "claude-future-9999".to_string(),
                        aliases: vec![],
                        description: None,
                        metadata: Default::default(),
                        capabilities: Capabilities {
                            reasoning: true,
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
            .expect("client"),
        );
        let response_json = serde_json::json!({
            "id": "msg_unpriced",
            "model": "claude-future-9999",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let transport = FakeTransport::always(ProviderResponse::json(200, response_json));
        let adapter = ProviderApiAdapter::new(Arc::new(ApiService::new_with_estimator(
            client,
            transport,
            SubscriberState::default(),
            UserAgentEnv {
                user_type: Some("external".to_string()),
                entrypoint: Some("cli".to_string()),
                ..Default::default()
            },
            "0.0.0",
            None,
            None,
            Some(estimator),
        )));
        let mut request = llm_runtime::MessagesCreateRequest::new(
            "claude-future-9999",
            None,
            None,
            Vec::new(),
            Vec::new(),
        );
        request.opts.max_output_tokens = Some(4096);
        let resp = adapter
            .messages_create(crate::OrchestratorApiRequest::Main(request))
            .await
            .expect("ok");
        assert!(
            resp.cost.is_none(),
            "unpriced billing_model must leave cost = None; got {:?}",
            resp.cost
        );
    }

    /// 3c-T3: cost tracker recording is unchanged (existing CostTracker tests still pass).
    ///
    /// When no estimator is wired, response.cost stays None — backward-compat.
    #[tokio::test]
    async fn no_estimator_leaves_cost_none() {
        let response_json = serde_json::json!({
            "id": "msg_no_est",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let transport = FakeTransport::always(ProviderResponse::json(200, response_json));
        // make_adapter wires None estimator (ApiService::new default path)
        let adapter = make_adapter(transport);
        let resp = adapter
            .messages_create(crate::OrchestratorApiRequest::Main(
                llm_runtime::MessagesCreateRequest::new(
                    "claude-sonnet-4-20250514",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                ),
            ))
            .await
            .expect("ok");
        assert!(resp.cost.is_none(), "no estimator → cost must be None");
    }

    // ── Task 5 Part B: OrchestratorApiClient::last_rate_limit_info ──────────────

    /// `OrchestratorApiClient::last_rate_limit_info` returns the adapter's stored
    /// rate-limit info mapped into a `lingxi_core::host::RateLimitSnapshot`.
    ///
    /// After a 2xx response with unified headers the snapshot must carry all
    /// three fields: `rate_limit_type`, `overage_status`, and
    /// `overage_disabled_reason`.
    #[tokio::test]
    async fn orchestrator_api_client_last_rate_limit_info_returns_stored_info() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-representative-claim".to_string(),
            "five_hour".to_string(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-overage-status".to_string(),
            "allowed_warning".to_string(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-overage-disabled-reason".to_string(),
            "out_of_credits".to_string(),
        );

        let transport = FakeTransport::always(ProviderResponse {
            status: 200,
            headers,
            body_json: lingxi_llm_client::protocol::JsonPayload::from_generated_value(
                &ok_response_json(),
            )
            .expect("generated fixture response is valid JSON"),
            request_id: None,
        });
        let adapter = make_adapter(transport);
        let _ = adapter
            .messages_create(crate::OrchestratorApiRequest::Main(
                llm_runtime::MessagesCreateRequest::new(
                    "claude-sonnet-4-20250514",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                ),
            ))
            .await
            .expect("ok");

        // Via the OrchestratorApiClient trait method (RateLimitSnapshot).
        let snapshot = OrchestratorApiClient::last_rate_limit_info(&adapter)
            .expect("must be Some after 2xx with unified headers");
        assert_eq!(
            snapshot.rate_limit_type.as_deref(),
            Some("five_hour"),
            "rate_limit_type must round-trip through the snapshot"
        );
        assert_eq!(
            snapshot.overage_status.as_deref(),
            Some("allowed_warning"),
            "overage_status must round-trip through the snapshot"
        );
        assert_eq!(
            snapshot.overage_disabled_reason.as_deref(),
            Some("out_of_credits"),
            "overage_disabled_reason must round-trip through the snapshot"
        );
    }

    /// `OrchestratorApiClient::last_rate_limit_info` returns `None` before any
    /// response with unified headers.
    #[tokio::test]
    async fn orchestrator_api_client_last_rate_limit_info_none_initially() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport);
        let _ = adapter
            .messages_create(crate::OrchestratorApiRequest::Main(
                llm_runtime::MessagesCreateRequest::new(
                    "claude-sonnet-4-20250514",
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                ),
            ))
            .await
            .expect("ok");

        // No unified headers → trait method also returns None.
        assert!(
            OrchestratorApiClient::last_rate_limit_info(&adapter).is_none(),
            "trait method must return None when adapter has no unified header snapshot"
        );
    }

    #[test]
    fn live_effort_replaces_and_clears_the_startup_value() {
        let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
        let adapter = make_adapter(transport).with_initial_effort(Some(serde_json::json!("low")));
        assert_eq!(adapter.current_effort(), Some(serde_json::json!("low")));

        OrchestratorApiClient::set_effort(
            &adapter,
            lingxi_core::host::effort_table::SessionEffort::Level(serde_json::json!("high")),
        );
        assert_eq!(adapter.current_effort(), Some(serde_json::json!("high")));

        OrchestratorApiClient::set_effort(
            &adapter,
            lingxi_core::host::effort_table::SessionEffort::Default,
        );
        assert_eq!(adapter.current_effort(), None);
    }
    struct HostedSdkTransport {
        seen: Mutex<Vec<llm_runtime::services::sdk::HttpRequest>>,
        partial: bool,
    }
    #[async_trait::async_trait]
    impl llm_runtime::Transport for HostedSdkTransport {
        async fn send(
            &self,
            request: llm_runtime::services::sdk::HttpRequest,
        ) -> Result<
            llm_runtime::services::sdk::StreamResponse,
            llm_runtime::services::sdk::protocol::LlmError,
        > {
            use futures::StreamExt;
            use serde_json::json;
            self.seen.lock().unwrap().push(request);
            let mut events = vec![
                json!({"type":"message_start","message":{"id":"msg_search","model":"claude-sonnet-4-20250514","role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":0}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv","name":"web_search","input":{"query":"Rust"}}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv","content":[{"type":"web_search_result","title":"Rust","url":"https://rust-lang.org","encrypted_content":"opaque"}]}}),
                json!({"type":"content_block_stop","index":1}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":7,"output_tokens":3,"server_tool_use":{"web_search_requests":1}}}),
                json!({"type":"message_stop"}),
            ];
            if self.partial {
                events.truncate(5);
            }
            let mut chunks = events
                .into_iter()
                .map(|event| Ok(format!("data: {event}\n\n").into()))
                .collect::<Vec<_>>();
            if self.partial {
                chunks.push(Err(
                    llm_runtime::services::sdk::protocol::LlmError::StreamInterrupted {
                        message: "connection lost".into(),
                    },
                ));
            }
            Ok(llm_runtime::services::sdk::StreamResponse {
                status: 200,
                headers: vec![],
                body: futures::stream::iter(chunks).boxed(),
            })
        }
    }
    #[tokio::test]
    async fn hosted_search_executes_through_sdk_and_preserves_partial_results_without_replay() {
        use tool_api::HostedWebSearchClient;
        for partial in [false, true] {
            let transport = Arc::new(HostedSdkTransport {
                seen: Mutex::new(vec![]),
                partial,
            });
            let adapter = make_adapter(transport.clone());
            let request = tool_api::HostedSearchRequest {
                model: "claude-sonnet-4-20250514".into(),
                profile: Some("anthropic".into()),
                query: "Rust".into(),
                allowed_domains: vec!["rust-lang.org".into()],
                blocked_domains: vec![],
            };
            assert!(adapter.supports_request(&request));
            let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel();
            let output = adapter.search(request, progress).await.unwrap();
            assert!(output.results.iter().any(|entry|matches!(entry,tool_api::hosted_search::SearchResultEntry::Hit(value) if value.as_json_source().contains("rust-lang.org"))));
            assert!(updates.try_recv().is_ok());
            if partial {
                assert!(output.results.iter().any(|entry|matches!(entry,tool_api::hosted_search::SearchResultEntry::Text(text) if text.string_at("").is_some_and(|text| text.contains("incomplete")))));
            } else {
                assert_eq!(output.input_tokens, 7);
                assert_eq!(output.output_tokens, 3);
                assert_eq!(output.searches, 1);
            }
            let requests = transport.seen.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0]
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("x-api-key") && v == "test-key"));
            let wire: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert_eq!(wire["tools"][0]["allowed_domains"][0], "rust-lang.org");
            assert_eq!(wire["model"], "claude-sonnet-4-20250514");
        }
    }
    #[tokio::test]
    async fn hosted_search_openai_reports_one_call_not_one_per_citation() {
        use llm_runtime::services::sdk;
        use serde_json::json;
        use tool_api::HostedWebSearchClient;
        struct SearchTransport {
            partial: bool,
            sends: std::sync::atomic::AtomicUsize,
        }
        #[async_trait::async_trait]
        impl llm_runtime::Transport for SearchTransport {
            async fn send(
                &self,
                request: sdk::HttpRequest,
            ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
                use futures::StreamExt;
                self.sends
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["model"], "gpt-search");
                assert_eq!(body["tools"][0]["type"], "web_search");
                let call = json!({"id":"ws_1","type":"web_search_call","status":"completed","action":{"type":"search","query":"Rust"}});
                let citation = json!({"type":"url_citation","url":"https://rust-lang.org","title":"Rust","start_index":0,"end_index":4});
                let mut events = vec![
                    json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-search"}}),
                    json!({"type":"response.output_item.added","output_index":0,"item":{"id":"ws_1","type":"web_search_call","status":"in_progress"}}),
                    json!({"type":"response.output_item.done","output_index":0,"item":call}),
                    json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"Rust"}),
                    json!({"type":"response.output_text.annotation.added","output_index":1,"content_index":0,"annotation":citation}),
                    json!({"type":"response.completed","response":{"id":"resp_1","model":"gpt-search","status":"completed","output":[call,{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"Rust","annotations":[citation]}]}],"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}),
                ];
                if self.partial {
                    events.pop(); // No response.completed snapshot arrives.
                    events.push(json!({"type":"response.output_item.done","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"Rust","annotations":[citation]}]}}));
                }
                let mut chunks = events
                    .into_iter()
                    .map(|event| Ok(format!("data: {event}\n\n").into()))
                    .collect::<Vec<_>>();
                if self.partial {
                    chunks.push(Err(sdk::protocol::LlmError::StreamInterrupted {
                        message: "connection lost after citation".into(),
                    }));
                }
                Ok(sdk::StreamResponse {
                    status: 200,
                    headers: vec![],
                    body: futures::stream::iter(chunks).boxed(),
                })
            }
        }
        for partial in [false, true] {
            let mut profile: ProviderProfile = serde_json::from_value(json!({
            "provider_id":"open_ai","profile_name":"search-openai","base_url":"https://api.openai.com/v1","protocol":"open_ai_responses","auth":"none","credential":{"type":"none"},
            "models":[{"display_model":"gpt-search","request_model":"gpt-search","billing_model":"gpt-search","capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        })).unwrap();
            profile.protocol = ProtocolFamily::OpenAiResponses;
            let mut wire: sdk::protocol::ProviderProfile = serde_json::from_value(json!({
            "provider_id":"openai","profile_name":"search-openai","base_url":"https://api.openai.com/v1","protocol":"open_ai_responses","auth":"none","models":[],"extra":{"web_search":"openai_responses"}
        })).unwrap();
            wire.regions = sdk::protocol::Region::all();
            profile.wire_profile = Some(wire);
            let client = Arc::new(
                ModelRuntime::from_config(ClientConfig {
                    providers: vec![profile],
                })
                .unwrap(),
            );
            let transport = Arc::new(SearchTransport {
                partial,
                sends: Default::default(),
            });
            let adapter = ProviderApiAdapter::new(Arc::new(ApiService::new(
                client,
                transport.clone(),
                SubscriberState::default(),
                UserAgentEnv::default(),
                "test",
                None,
                None,
            )));
            let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel();
            let request = tool_api::HostedSearchRequest {
                model: "gpt-search".into(),
                profile: Some("search-openai".into()),
                query: "Rust".into(),
                allowed_domains: vec![],
                blocked_domains: vec![],
            };
            assert!(adapter.supports_request(&request));
            let result = adapter.search(request, progress).await.unwrap();
            assert_eq!(result.searches, 1);
            assert!(updates.try_recv().is_ok());
            assert!(
                updates.try_recv().is_err(),
                "duplicated final output counted twice"
            );
            assert_eq!(
                transport.sends.load(std::sync::atomic::Ordering::Relaxed),
                1,
                "interrupted search must not be resent"
            );
            if !partial {
                assert_eq!((result.input_tokens, result.output_tokens), (7, 3));
            }
            let hits = result
                .results
                .iter()
                .filter_map(|entry| match entry {
                    tool_api::hosted_search::SearchResultEntry::Hit(value) => {
                        value.array_len("/content").map(|length| {
                            (0..length)
                                .filter_map(|index| value.array_item_at("/content", index))
                                .collect::<Vec<_>>()
                        })
                    }
                    _ => None,
                })
                .flatten()
                .collect::<Vec<_>>();
            assert_eq!(
                hits.len(),
                1,
                "stream and terminal citations must be deduplicated"
            );
            assert_eq!(
                hits[0].string_at("/url").as_deref(),
                Some("https://rust-lang.org")
            );
            assert!(result.results.iter().any(|entry| matches!(entry, tool_api::hosted_search::SearchResultEntry::Text(text) if text.string_at("").as_deref() == Some("Rust"))));
            assert_eq!(result.results.iter().any(|entry| matches!(entry, tool_api::hosted_search::SearchResultEntry::Text(text) if text.string_at("").is_some_and(|text| text.contains("incomplete")))), partial);
        }
    }

    #[tokio::test]
    async fn hosted_search_codex_preserves_query_source_and_consumes_sdk_payloads() {
        use serde_json::json;
        use tool_api::HostedWebSearchClient;

        let transport = FakeTransport::always(ProviderResponse::json(
            200,
            json!({
                "output":"source-backed answer",
                "results":[{
                    "type":"text_result",
                    "title":"A source",
                    "url":"https://example.test/source"
                }]
            }),
        ));
        let adapter = make_codex_search_adapter(transport.clone());
        let request = tool_api::HostedSearchRequest {
            model: "gpt-search".into(),
            profile: Some("codex".into()),
            query: Utf16JsonProjection::parse(r#""A\ud800B""#).unwrap(),
            allowed_domains: vec![],
            blocked_domains: vec![],
        };
        assert!(adapter.supports_request(&request));

        let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let output = adapter.search(request, progress).await.unwrap();
        assert_eq!(output.searches, 1);
        assert!(updates.try_recv().is_ok());
        assert!(updates.try_recv().is_err());
        assert!(output.results.iter().any(|entry| matches!(entry,
            tool_api::hosted_search::SearchResultEntry::Text(text)
                if text.string_at("").as_deref() == Some("source-backed answer"))));
        assert!(output.results.iter().any(|entry| matches!(entry,
            tool_api::hosted_search::SearchResultEntry::Hit(value)
                if value.as_json_source().contains("https://example.test/source"))));

        let requests = transport.seen.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url,
            "https://chatgpt.com/backend-api/codex/alpha/search"
        );
        assert!(requests[0]
            .body_json
            .as_json_source()
            .contains(r#""q":"A\ud800B""#));
    }
    #[tokio::test]
    async fn hosted_search_gemini_reports_queries_once_per_grounding_snapshot() {
        use llm_runtime::services::sdk;
        use serde_json::json;
        use tool_api::HostedWebSearchClient;
        struct SearchTransport {
            interrupted: bool,
            calls: std::sync::atomic::AtomicUsize,
        }
        #[async_trait::async_trait]
        impl llm_runtime::Transport for SearchTransport {
            async fn send(
                &self,
                request: sdk::HttpRequest,
            ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
                use futures::StreamExt;
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert!(request.url.contains("gemini-search"));
                assert!(body["tools"][0].get("googleSearch").is_some());
                let grounding = json!({"webSearchQueries":["Rust","Rust docs"],"groundingChunks":[{"web":{"uri":"https://rust-lang.org","title":"Rust"}}]});
                let mut events = vec![
                    json!({"modelVersion":"gemini-search","candidates":[{"content":{"role":"model","parts":[{"text":"Rust"}]},"groundingMetadata":grounding}]}),
                    json!({"candidates":[{"content":{"role":"model","parts":[]},"groundingMetadata":grounding,"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"totalTokenCount":10}}),
                ];
                if self.interrupted {
                    events.pop();
                }
                let mut chunks = events
                    .into_iter()
                    .map(|event| Ok(format!("data: {event}\n\n").into()))
                    .collect::<Vec<_>>();
                if self.interrupted {
                    chunks.push(Err(sdk::protocol::LlmError::StreamInterrupted {
                        message: "lost after grounding".into(),
                    }));
                }
                Ok(sdk::StreamResponse {
                    status: 200,
                    headers: vec![],
                    body: futures::stream::iter(chunks).boxed(),
                })
            }
        }
        for interrupted in [false, true] {
            let mut profile: ProviderProfile = serde_json::from_value(json!({
            "provider_id":"gemini","profile_name":"search-google","base_url":"https://generativelanguage.googleapis.com/v1beta","protocol":"gemini_generate_content","auth":"none","credential":{"type":"none"},
            "models":[{"display_model":"gemini-search","request_model":"gemini-search","billing_model":"gemini-search","capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        })).unwrap();
            profile.protocol = ProtocolFamily::GeminiGenerateContent;
            let mut wire: sdk::protocol::ProviderProfile = serde_json::from_value(json!({
            "provider_id":"gemini","profile_name":"search-google","base_url":"https://generativelanguage.googleapis.com/v1beta","protocol":"gemini_generate_content","auth":"none","models":[],"extra":{"web_search":"gemini"}
        })).unwrap();
            wire.regions = sdk::protocol::Region::all();
            profile.wire_profile = Some(wire);
            let client = Arc::new(
                ModelRuntime::from_config(ClientConfig {
                    providers: vec![profile],
                })
                .unwrap(),
            );
            let transport = Arc::new(SearchTransport {
                interrupted,
                calls: Default::default(),
            });
            let adapter = ProviderApiAdapter::new(Arc::new(ApiService::new(
                client,
                transport.clone(),
                SubscriberState::default(),
                UserAgentEnv::default(),
                "test",
                None,
                None,
            )));
            let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel();
            let request = tool_api::HostedSearchRequest {
                model: "gemini-search".into(),
                profile: Some("search-google".into()),
                query: "Rust".into(),
                allowed_domains: vec![],
                blocked_domains: vec![],
            };
            assert!(adapter.supports_request(&request));
            let result = adapter.search(request, progress).await.unwrap();
            assert_eq!(result.searches, 2);
            assert!(updates.try_recv().is_ok());
            assert!(updates.try_recv().is_ok());
            assert!(
                updates.try_recv().is_err(),
                "duplicated final output counted twice"
            );
            if !interrupted {
                assert_eq!((result.input_tokens, result.output_tokens), (7, 3));
            }
            assert_eq!(transport.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(result.results.iter().any(|entry| matches!(entry, tool_api::hosted_search::SearchResultEntry::Text(text) if text.string_at("").is_some_and(|text| text.contains("incomplete")))), interrupted);
            assert!(result.results.iter().any(|entry|matches!(entry,tool_api::hosted_search::SearchResultEntry::Hit(value) if value.as_json_source().contains("rust-lang.org"))));
        }
    }

    fn builtin_context_window_model(model: &str) -> ModelProfile {
        llm_runtime::provider_settings::anthropic_model_profiles()
            .into_iter()
            .find(|entry| entry.request_model == model)
            .unwrap_or_else(|| panic!("built-in Anthropic catalog has no {model:?}"))
    }

    fn make_context_window_adapter_with_profile(
        profile_name: &str,
        provider_id: ProviderId,
        protocol: ProtocolFamily,
        base_url: &str,
        model: ModelProfile,
        custom_betas: Vec<String>,
    ) -> ProviderApiAdapter {
        std::env::set_var("ADAPTER_TEST_KEY", "test-key");
        let profile = ProviderProfile {
            wire_profile: None,
            regions: llm_runtime::Region::all(),
            provider_id,
            profile_name: profile_name.to_owned(),
            base_url: base_url.to_owned(),
            protocol,
            auth: AuthStrategy::ApiKey,
            credential: CredentialConfig::Env {
                var: "ADAPTER_TEST_KEY".to_owned(),
            },
            models: vec![model],
            pricing: PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        };
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![profile],
            })
            .expect("valid context-window provider profile"),
        );
        let service = ApiService::new(
            client,
            owned_request_capture(),
            SubscriberState::default(),
            UserAgentEnv::default(),
            "test",
            None,
            None,
        )
        .with_custom_cli_betas(custom_betas);
        ProviderApiAdapter::new(Arc::new(service))
    }

    fn make_first_party_context_window_adapter(
        model: ModelProfile,
        custom_betas: Vec<String>,
    ) -> ProviderApiAdapter {
        make_context_window_adapter_with_profile(
            "anthropic",
            ProviderId::AnthropicFirstParty,
            ProtocolFamily::AnthropicMessages,
            "https://api.anthropic.com",
            model,
            custom_betas,
        )
    }

    fn make_context_window_adapter() -> ProviderApiAdapter {
        let config: ClientConfig = serde_json::from_value(serde_json::json!({
        "providers": [{
            "provider_id":"anthropic_first_party",
            "profile_name":"anthropic",
            "base_url":"https://api.anthropic.com",
            "protocol":"anthropic_messages",
            "auth":"none",
            "credential":{"type":"none"},
            "models":[
                {
                    "display_model":"claude-sonnet-4-6",
                    "request_model":"claude-sonnet-4-6",
                    "billing_model":"claude-sonnet-4-6",
                    "metadata":{
                        "contextWindowTokens":1000000,
                        "maxInputTokens":200000,
                        "maxOutputTokens":32000,
                        "context":{
                            "sharedWindow":true,
                            "application":{"mode":"custom","input_tokens":200000}
                        }
                    },
                    "capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}
                },
                {
                    "display_model":"claude-opus-4-7",
                    "request_model":"claude-opus-4-7",
                    "billing_model":"claude-opus-4-7",
                    "metadata":{
                        "contextWindowTokens":200000,
                        "maxInputTokens":120000,
                        "maxOutputTokens":32000,
                        "context":{
                            "sharedWindow":true,
                            "application":{"mode":"custom","input_tokens":120000}
                        }
                    },
                    "capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}
                }
            ]
        }]
    }))
    .expect("valid raw-window and effective-budget context fixture");
        let client = Arc::new(ModelRuntime::from_config(config).expect("client"));
        let service = ApiService::new(
            client,
            owned_request_capture(),
            SubscriberState::default(),
            UserAgentEnv::default(),
            "test",
            None,
            None,
        );
        ProviderApiAdapter::new(Arc::new(service))
    }

    #[test]
    fn native_window_fallback_is_route_scoped_and_keeps_profile_window() {
        let sonnet = make_adapter_with_session_tool_choice(
            owned_request_capture(),
            None,
            "claude-sonnet-4-6",
        );
        assert_eq!(
            sonnet.context_window_tokens("claude-sonnet-4-6", Some("anthropic")),
            Some(200_000),
            "a route without a published window uses the Native Claude policy"
        );
        let opus =
            make_adapter_with_session_tool_choice(owned_request_capture(), None, "claude-opus-4-7");
        assert_eq!(
            opus.context_window_tokens("claude-opus-4-7", Some("anthropic")),
            Some(1_000_000)
        );
        let codex = make_codex_search_adapter(owned_request_capture());
        assert_eq!(
            codex.context_window_tokens("gpt-search", Some("codex")),
            None,
            "a non-Claude provider with unknown window stays unknown"
        );
    }

    #[test]
    fn native_window_overrides_catalog_and_observation_but_not_explicit_or_unknown_facts() {
        use lingxi_core::host::ModelContextWindowSource as HostSource;
        use lingxi_llm_client::protocol::ContextWindowSource as SdkSource;

        let opus = builtin_context_window_model("claude-opus-4-6");
        assert_eq!(opus.metadata.context_window_tokens, Some(1_000_000));
        assert_eq!(opus.metadata.context_window_source, HostSource::Catalog);
        let catalog = make_first_party_context_window_adapter(opus.clone(), Vec::new());
        let resolved = catalog
            .context_budget("claude-opus-4-6", Some("anthropic"))
            .unwrap()
            .unwrap();
        assert_eq!(resolved.context_window_source, SdkSource::Catalog);
        assert_eq!(resolved.context_window_tokens, Some(1_000_000));
        assert_eq!(
            catalog.context_window_tokens("claude-opus-4-6", Some("anthropic")),
            Some(200_000),
            "first-party Opus 4.6's catalog 1M listing must follow the default Native window"
        );

        let mut observed_opus = opus.clone();
        observed_opus.metadata.context_window_source = HostSource::Observation;
        let observed = make_first_party_context_window_adapter(observed_opus, Vec::new());
        let resolved = observed
            .context_budget("claude-opus-4-6", Some("anthropic"))
            .unwrap()
            .unwrap();
        assert_eq!(resolved.context_window_source, SdkSource::Observation);
        assert_eq!(
            observed.context_window_tokens("claude-opus-4-6", Some("anthropic")),
            Some(200_000),
            "a provider observation is a known fact but still uses the Native Claude policy"
        );

        for (host_source, sdk_source) in [
            (HostSource::Explicit, SdkSource::Explicit),
            // A direct caller-supplied window has no catalog provenance; the
            // SDK resolves that known raw value as explicit input.
            (HostSource::Unknown, SdkSource::Explicit),
        ] {
            let mut override_opus = opus.clone();
            override_opus.metadata.context_window_source = host_source;
            let adapter = make_first_party_context_window_adapter(override_opus, Vec::new());
            let resolved = adapter
                .context_budget("claude-opus-4-6", Some("anthropic"))
                .unwrap()
                .unwrap();
            assert_eq!(resolved.context_window_source, sdk_source);
            assert_eq!(
                adapter.context_window_tokens("claude-opus-4-6", Some("anthropic")),
                Some(1_000_000),
                "explicit and unknown source facts must preserve the provider's raw window"
            );
        }
    }

    #[test]
    fn native_window_keeps_suffix_and_planned_beta_one_m_context_routes() {
        use lingxi_core::host::ModelContextWindowSource as HostSource;
        use llm_runtime::model::context_window::CONTEXT_1M_BETA_HEADER;

        let mut suffixed = builtin_context_window_model("claude-opus-4-6");
        suffixed.display_model = "claude-opus-4-6[1m]".to_owned();
        suffixed.request_model = suffixed.display_model.clone();
        suffixed.metadata.context_window_tokens = Some(200_000);
        suffixed.metadata.context_window_source = HostSource::Catalog;
        let suffix_adapter = make_first_party_context_window_adapter(suffixed, Vec::new());
        assert_eq!(
            suffix_adapter.context_window_tokens("claude-opus-4-6[1m]", Some("anthropic")),
            Some(1_000_000)
        );

        let mut planned_beta = builtin_context_window_model("claude-opus-4-6");
        planned_beta.metadata.context_window_tokens = Some(200_000);
        planned_beta.metadata.context_window_source = HostSource::Catalog;
        let beta_adapter = make_first_party_context_window_adapter(
            planned_beta,
            vec![CONTEXT_1M_BETA_HEADER.to_owned()],
        );
        assert_eq!(
            beta_adapter.context_window_tokens("claude-opus-4-6", Some("anthropic")),
            Some(1_000_000)
        );
    }

    #[test]
    fn catalog_window_on_foundry_route_remains_provider_raw() {
        use lingxi_core::host::ModelContextWindowSource as HostSource;
        use lingxi_llm_client::protocol::ContextWindowSource as SdkSource;

        let mut opus = builtin_context_window_model("claude-opus-4-6");
        opus.metadata.context_window_source = HostSource::Catalog;
        let adapter = make_context_window_adapter_with_profile(
            "foundry",
            ProviderId::FoundryClaude,
            ProtocolFamily::FoundryClaude,
            "https://example.invalid",
            opus,
            Vec::new(),
        );
        let resolved = adapter
            .context_budget("claude-opus-4-6", Some("foundry"))
            .unwrap()
            .unwrap();
        assert_eq!(resolved.context_window_source, SdkSource::Catalog);
        assert_eq!(resolved.context_window_tokens, Some(1_000_000));
        assert_eq!(
            adapter.context_window_tokens("claude-opus-4-6", Some("foundry")),
            Some(1_000_000),
            "Foundry is Claude-shaped but does not use the first-party Native window policy"
        );
    }

    #[test]
    fn server_fallback_notice_uses_raw_window_not_effective_input_budget() {
        let adapter = make_context_window_adapter();
        let from = adapter
            .context_budget("claude-sonnet-4-6", Some("anthropic"))
            .unwrap()
            .unwrap();
        let to = adapter
            .context_budget("claude-opus-4-7", Some("anthropic"))
            .unwrap()
            .unwrap();
        assert_eq!(from.context_window_tokens, Some(1_000_000));
        assert_eq!(from.effective_input_budget, Some(200_000));
        assert_eq!(to.context_window_tokens, Some(200_000));
        assert_eq!(to.effective_input_budget, Some(120_000));
        assert_eq!(
            adapter.context_window_tokens("claude-sonnet-4-6", Some("anthropic")),
            Some(1_000_000)
        );
        assert_eq!(
            adapter.context_window_tokens("claude-opus-4-7", Some("anthropic")),
            Some(200_000)
        );
        assert_eq!(
        crate::query_model::server_error_notice(
            "Sonnet 4.6",
            "Opus 4.7",
            adapter.context_window_tokens("claude-sonnet-4-6", Some("anthropic")),
            adapter.context_window_tokens("claude-opus-4-7", Some("anthropic")),
        ),
        "Switched to Opus 4.7 due to high demand for Sonnet 4.6 · context window 1M → 200K tokens"
    );
    }
}

#[cfg(test)]
#[path = "provider_adapter_cache_test.rs"]
mod prompt_cache_tests;

#[async_trait::async_trait]
impl tool_api::HostedWebSearchClient for ProviderApiAdapter {
    fn supports(&self, model: &str, profile: Option<&str>) -> bool {
        self.service.supports_hosted_search(model, profile)
    }
    fn supports_request(&self, request: &tool_api::HostedSearchRequest) -> bool {
        let Some(allowed_domains) = request
            .allowed_domains
            .iter()
            .map(hosted_search_domain_payload)
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        let Some(blocked_domains) = request
            .blocked_domains
            .iter()
            .map(hosted_search_domain_payload)
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        self.service.supports_hosted_search_config(
            &request.model,
            request.profile.as_deref(),
            &llm_runtime::services::sdk::protocol::WebSearchConfig {
                allowed_domains,
                blocked_domains,
                max_uses: self
                    .service
                    .hosted_search_max_uses(&request.model, request.profile.as_deref()),
            },
        )
    }
    async fn search(
        &self,
        input: tool_api::HostedSearchRequest,
        progress: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Result<tool_api::HostedSearchOutput, tool_api::HostedSearchError> {
        use futures::StreamExt;
        use llm_runtime::services::sdk;
        use llm_runtime::HistoryEvent;
        let route = self
            .service
            .hosted_search_route(&input.model, input.profile.as_deref())
            .map_err(hosted_search_error)?;
        let require_native_results = matches!(
            &route.provider_id,
            llm_runtime::ProviderId::OpenAICompatible { name }
                | llm_runtime::ProviderId::Custom { name }
                if name == "deepseek"
        );
        let query_units = input
            .query
            .string_units("")
            .ok_or_else(|| hosted_search_input_error("web search query must be a JSON string"))?;
        let codex_standalone_search = self
            .service
            .uses_codex_standalone_search(&input.model, input.profile.as_deref());
        let mut request = llm_runtime::LlmRequest::new(route.request_model);
        if !codex_standalone_search {
            let mut prompt_units = "Perform a web search for the query: "
                .encode_utf16()
                .collect::<Vec<_>>();
            prompt_units.extend_from_slice(&query_units);
            let prompt_text = String::from_utf16_lossy(&prompt_units);
            request
                .input
                .messages
                .push(sdk::protocol::ConversationMessage {
                    role: sdk::protocol::MessageRole::User,
                    content: vec![sdk::protocol::ContentBlock::TextJsUtf16 {
                        text: prompt_text,
                        utf16_code_units: prompt_units,
                        thought_signature: None,
                        citations: None,
                    }],
                    native_options: Vec::new(),
                });
        }
        request.profile = Some(route.profile_name);
        request.input.max_tokens = Some(4096);
        request.execution.query_source = Some("web_search_tool".into());
        request.input.system = vec![sdk::protocol::SystemBlock::new(
            "You are an assistant for performing a web search tool use",
        )];
        request.input.hosted_tools = vec![sdk::protocol::HostedTool::WebSearch(
            sdk::protocol::WebSearchConfig {
                allowed_domains: input
                    .allowed_domains
                    .iter()
                    .map(hosted_search_domain_payload)
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| {
                        hosted_search_input_error("allowed domains must be JSON strings")
                    })?,
                blocked_domains: input
                    .blocked_domains
                    .iter()
                    .map(hosted_search_domain_payload)
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| {
                        hosted_search_input_error("blocked domains must be JSON strings")
                    })?,
                max_uses: self
                    .service
                    .hosted_search_max_uses(&request.input.model, request.profile.as_deref()),
            },
        )];
        if codex_standalone_search {
            request.input.native_options.push(
                sdk::protocol::NativeExtension::from_typed(
                    sdk::providers::openai::codex_search::CodexSearchRequest {
                        id: uuid::Uuid::new_v4().to_string(),
                        query: sdk::protocol::JsonPayload::string_from_utf16_code_units(
                            &query_units,
                        ),
                    },
                )
                .expect("Codex search options are serializable"),
            );
            let response = self
                .service
                .execute_non_stream_request(
                    request,
                    llm_runtime::NonStreamingRequestClass::Auxiliary,
                    Default::default(),
                )
                .await
                .map_err(hosted_search_error)?;
            let search_content = response
                .content
                .iter()
                .filter_map(hosted_search_content_source)
                .collect::<Vec<_>>();
            let mut results = sdk::hosted_search::parse_response_content(&search_content);
            let observations = response
                .provider_metadata
                .payload_at("/llm_client/web_search")
                .filter(sdk::protocol::JsonPayload::is_root_object)
                .map(|web_search| {
                    sdk::protocol::JsonPayload::parse(&format!("[{}]", web_search.as_json_source()))
                        .map_err(|error| {
                            hosted_search_error(llm_runtime::LlmError::InvalidRequest {
                                message: error.to_string(),
                            })
                        })
                })
                .transpose()?;
            sdk::hosted_search::append_citations(&mut results, observations.as_ref());
            let _ = progress.send(());
            return Ok(tool_api::HostedSearchOutput {
                results,
                searches: 1,
                input_tokens: response.usage.counts().input_tokens,
                output_tokens: response
                    .usage
                    .counts()
                    .output_tokens
                    .saturating_sub(response.usage.counts().reasoning_tokens),
            });
        }
        let usage = Arc::new(std::sync::Mutex::new(llm_runtime::ExecutionUsage::default()));
        let searches = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // A failed stream may never deliver its terminal metadata snapshot.
        // Retain normalized citations independently of completed text blocks.
        let citations = Arc::new(std::sync::Mutex::new(
            sdk::protocol::WebSearchResult::default(),
        ));
        let observed_citations = citations.clone();
        let u = usage.clone();
        let n = searches.clone();
        let mut search_progress = sdk::hosted_search::SearchProgress::default();
        let stream = self
            .service
            .stream_request(request)
            .await
            .map_err(hosted_search_error)?
            .inspect(move |event| match event {
                Ok(HistoryEvent::WebSearch { result }) => {
                    observed_citations
                        .lock()
                        .expect("search citations")
                        .citations
                        .extend(result.citations.iter().cloned());
                    let added = search_progress.observe(result);
                    n.fetch_add(added, std::sync::atomic::Ordering::Relaxed);
                    for _ in 0..added {
                        let _ = progress.send(());
                    }
                }
                Ok(HistoryEvent::ContentBlockStart { content_block, .. }) => {
                    let started = match content_block {
                        llm_runtime::ContentBlock::ProviderContent { value, .. } => {
                            sdk::hosted_search::search_started(value)
                        }
                        llm_runtime::ContentBlock::ServerToolUse { name, .. } => {
                            name == "web_search"
                        }
                        _ => false,
                    };
                    if started {
                        n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let _ = progress.send(());
                    }
                }
                Ok(HistoryEvent::MessageStart { response })
                | Ok(HistoryEvent::Completed { response }) => {
                    *u.lock().expect("search usage") = response.usage.clone();
                }
                Ok(HistoryEvent::MessageDelta {
                    usage: Some(usage), ..
                }) => {
                    *u.lock().expect("search usage") = usage.clone();
                }
                _ => {}
            })
            .boxed();
        let (blocks, metadata, partial) =
            match llm_runtime::stream_accumulator::accumulate_stream_salvaging(stream).await {
                Ok(response) => (response.content, response.provider_metadata, None),
                Err((partial, error))
                    if !partial.is_empty()
                        || !citations
                            .lock()
                            .expect("search citations")
                            .citations
                            .is_empty() =>
                {
                    (
                        partial,
                        lingxi_llm_client::protocol::JsonPayload::parse("null")
                            .expect("null is valid JSON"),
                        Some(error.to_string()),
                    )
                }
                Err((_, error)) => return Err(hosted_search_error(error)),
            };
        let search_content = blocks
            .iter()
            .filter_map(hosted_search_content_source)
            .collect::<Vec<_>>();
        let mut results = sdk::hosted_search::parse_response_content(&search_content);
        if require_native_results
            && !results
                .iter()
                .any(|entry| matches!(entry, sdk::hosted_search::SearchResultEntry::Hit(_)))
        {
            return Err(hosted_search_input_error(
                "DeepSeek returned no structured web_search_tool_result; native search did not complete",
            ));
        }
        let mut observations = metadata
            .payload_at("/llm_client/web_search")
            .filter(lingxi_llm_client::protocol::JsonPayload::is_root_array)
            .and_then(|observations| {
                (0..observations.array_len("")?)
                    .map(|index| observations.array_item(index))
                    .collect::<Option<Vec<_>>>()
            })
            .unwrap_or_default();
        let captured = std::mem::take(&mut *citations.lock().expect("search citations"));
        if !captured.citations.is_empty() {
            let source = serde_json::to_string(&captured).expect("normalized search citations");
            observations.push(
                lingxi_llm_client::protocol::JsonPayload::parse(&source)
                    .expect("serialized search citations are valid JSON"),
            );
        }
        let observations = lingxi_llm_client::protocol::JsonPayload::array_from_items(
            &observations,
            lingxi_llm_client::exact_json::JsonEncoding::JavaScript,
        )
        .expect("search observations form valid JSON");
        // The SDK projects both sources together and deduplicates their URLs.
        sdk::hosted_search::append_citations(&mut results, Some(&observations));
        if partial.is_some() {
            results.push(sdk::hosted_search::SearchResultEntry::Text(
                lingxi_llm_client::protocol::JsonPayload::string(
                    "Search response was interrupted; these results are incomplete.",
                ),
            ));
        }
        let usage = usage.lock().expect("search usage");
        Ok(tool_api::HostedSearchOutput {
            results,
            searches: usage
                .server_tool_usage()
                .and_then(|u| u.web_search_requests)
                .unwrap_or_else(|| searches.load(std::sync::atomic::Ordering::Relaxed)),
            input_tokens: usage.counts().input_tokens,
            output_tokens: usage
                .counts()
                .output_tokens
                .saturating_sub(usage.counts().reasoning_tokens),
        })
    }
}

fn hosted_search_error(error: llm_runtime::LlmError) -> tool_api::HostedSearchError {
    use llm_runtime::LlmError;
    let classification = error.classification_source();
    let http_status = error.http_status().or(match classification {
        LlmError::RateLimited { .. } => Some(429),
        LlmError::Overloaded { .. } => Some(529),
        LlmError::RequestTooLarge => Some(413),
        _ => None,
    });
    tool_api::HostedSearchError {
        http_status,
        timeout: matches!(classification, LlmError::TransportTimeout { .. }),
        message: match &error {
            LlmError::SettingsSource { message, .. } => message.clone(),
            _ => Utf16JsonProjection::from(error.to_string()),
        },
    }
}

#[cfg(test)]
mod hosted_error_tests {
    use super::*;
    #[test]
    fn hosted_search_errors_keep_status_and_timeout() {
        for (error, status, timeout) in [
            (
                llm_runtime::LlmError::RateLimited {
                    retry_after: None,
                    scope: None,
                },
                Some(429),
                false,
            ),
            (
                llm_runtime::LlmError::Authentication {
                    message: "401 unauthorized".into(),
                },
                Some(401),
                false,
            ),
            (
                llm_runtime::LlmError::TransportTimeout {
                    message: "deadline".into(),
                },
                None,
                true,
            ),
            (
                llm_runtime::LlmError::Transport {
                    message: "disconnected".into(),
                },
                None,
                false,
            ),
        ] {
            let mapped = hosted_search_error(error);
            assert_eq!(mapped.http_status, status);
            assert_eq!(mapped.timeout, timeout);
        }
    }
}
