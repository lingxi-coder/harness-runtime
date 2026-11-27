//! Canonical request assembly shared by the service entry points.

use crate::convert::{
    ensure_tool_result_pairing_with_sources,
    normalize_messages_for_api_with_tool_search_and_sources, to_llm_messages,
    ConversationMessagesWithSources,
};
use crate::service::anthropic::endpoint::{
    native_anthropic_unix_socket_enabled, native_prompt_cache_bare_mode,
};
use crate::service::anthropic::prompt::{
    apply_anthropic_tool_result_reminder_suffixes, apply_cache_editing, native_prompt_attribution,
};
use crate::service::native_media::strip_native_media_with_sources;
use crate::service::{ApiService, MOD_REQUEST_EFFORT};
use crate::{
    agent_prompt_cache_ttl_override, HistoryEvent, LlmError, LlmRequest, PromptCacheQuerySource,
};
use futures::stream::BoxStream;
use lingxi_core::types::{is_nested_media_value, ContentBlock, ConversationMessage};
use std::sync::Arc;
use std::time::Instant;

pub(super) fn custom_system_prompt(text: Option<&str>) -> Option<crate::SystemPromptInput> {
    text.map(|text| {
        native_custom_system_prompt(
            lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(text),
        )
    })
}

/// and already-typed source vectors must not pass through this splitter.
pub fn native_custom_system_prompt(
    text: lingxi_llm_client::providers::anthropic::system_prompt::PromptText,
) -> crate::SystemPromptInput {
    use lingxi_llm_client::providers::anthropic::system_prompt::{
        PromptText, SystemPromptInput, DYNAMIC_BOUNDARY,
    };

    let units = text.utf16_code_units();
    let lines = units
        .split(|unit| *unit == b'\n' as u16)
        .collect::<Vec<_>>();
    let marker_units = DYNAMIC_BOUNDARY.encode_utf16().collect::<Vec<_>>();
    let Some(boundary) = lines
        .iter()
        .position(|line| js_trim_whitespace(line) == marker_units.as_slice())
    else {
        return SystemPromptInput::native_custom_prompt(text.clone(), vec![text]);
    };

    let mut elements = Vec::with_capacity(3);
    let mut before = join_js_split_lines(&lines[..boundary]);
    before.push(b'\n' as u16);
    if !js_trim_whitespace(&before).is_empty() {
        elements.push(PromptText::from_utf16(before));
    }
    elements.push(PromptText::from_string(DYNAMIC_BOUNDARY));

    let mut after = vec![b'\n' as u16];
    after.extend(join_js_split_lines(&lines[boundary + 1..]));
    if !js_trim_whitespace(&after).is_empty() {
        elements.push(PromptText::from_utf16(after));
    }

    SystemPromptInput::native_custom_prompt(text, elements)
}

pub(super) fn native_custom_system_prompt_source(
    source: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Result<crate::SystemPromptInput, LlmError> {
    let units = source
        .string_units("")
        .ok_or_else(|| LlmError::InvalidRequest {
            message: "side-query system prompt must be a root JSON string".into(),
        })?;
    Ok(native_custom_system_prompt(
        lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(units),
    ))
}

pub(super) fn side_query_system_prompt(
    source: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
    captured: Option<&crate::SystemPromptInput>,
) -> Result<Option<crate::SystemPromptInput>, LlmError> {
    match captured {
        Some(input) => Ok(Some(input.clone())),
        None => source.map(native_custom_system_prompt_source).transpose(),
    }
}

fn join_js_split_lines(lines: &[&[u16]]) -> Vec<u16> {
    let capacity = lines.iter().map(|line| line.len()).sum::<usize>() + lines.len();
    let mut joined = Vec::with_capacity(capacity);
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            joined.push(b'\n' as u16);
        }
        joined.extend_from_slice(line);
    }
    joined
}

fn js_trim_whitespace(mut units: &[u16]) -> &[u16] {
    while units.first().is_some_and(|unit| is_js_whitespace(*unit)) {
        units = &units[1..];
    }
    while units.last().is_some_and(|unit| is_js_whitespace(*unit)) {
        units = &units[..units.len() - 1];
    }
    units
}

fn is_js_whitespace(unit: u16) -> bool {
    matches!(
        unit,
        0x0009
            | 0x000a
            | 0x000b
            | 0x000c
            | 0x000d
            | 0x0020
            | 0x00a0
            | 0x1680
            | 0x2028
            | 0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    ) || (0x2000..=0x200a).contains(&unit)
}

/// Ordered model fallback policy for one logical non-streaming request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FallbackPolicy {
    /// Keep the selected model for this logical request.
    #[default]
    Disabled,
    /// Resolve the selected model's settings, then the global settings.
    Configured,
    /// Use exactly this ordered model chain, including an explicitly empty chain.
    Models(Vec<String>),
}

impl FallbackPolicy {
    /// Parse the current CLI's ordered comma-separated model list.
    #[must_use]
    pub fn from_models_csv(models: &str) -> Self {
        Self::Models(parse_fallback_chain(models))
    }
}

/// Destination stored in a model fallback chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfiguredFallbackTarget {
    /// An unqualified model name from a legacy fallback chain.
    Model(String),
    /// An exact provider profile selected when routing settings were assembled.
    Profile {
        /// Provider identity of the selected profile.
        provider_id: crate::ProviderId,
        /// Profile name that owns the configured route.
        profile_name: String,
        /// Provider-local model id.
        model: String,
    },
}

impl ConfiguredFallbackTarget {
    pub(super) fn model(&self) -> &str {
        match self {
            Self::Model(model) | Self::Profile { model, .. } => model,
        }
    }
}

/// Controls consumed while assembling and executing a main request.
#[derive(Debug, Clone, Default)]
pub struct MessagesCreateOptions {
    /// Output ceiling, applied before model-aware reasoning and context bounds.
    pub max_output_tokens: Option<u32>,
    /// Explicit request effort, overriding the session's current selection.
    pub effort: Option<serde_json::Value>,
    /// Anthropic context-hint offer; the codec and beta policy consume it.
    pub context_hint: Option<serde_json::Value>,
    /// Activate the hint beta independently of whether an offer meets its floor.
    pub context_hint_beta: bool,
    /// Prior streaming overload count. `Some(0)` identifies a stream fallback.
    pub initial_consecutive_overloaded: Option<u8>,
    /// Ordered fallback policy for this logical request.
    pub fallback: FallbackPolicy,
    /// Exact Native-effective model source for requests whose configured route
    /// cannot reproduce the same wire model. This is intentionally separate
    /// from the route selector and stays empty for ordinary catalog models.
    pub model_source: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Trusted host accounting authority; never enters model input.
    pub model_attempt: Option<lingxi_core::host::ModelAttemptContext>,
    /// Sanitized Native `querySource` used by cache-TTL policy and accounting.
    pub query_source: Option<String>,
    /// Trusted elapsed-time fact from the failed streaming request.
    pub failed_stream_outlasted_timeout: bool,
    /// Native `skipGlobalCacheForSystemPrompt`, computed from registered active
    /// tools before their provider schemas are flattened.
    pub skip_global_cache_for_system_prompt: bool,
    /// Host-owned admission checked immediately before SDK transport dispatch.
    pub request_dispatch_admission: Option<crate::RequestDispatchAdmission>,
    /// Host-owned observation after the first admitted physical dispatch.
    pub request_dispatch_observer: Option<crate::RequestDispatchObserver>,
    /// Request-local Native token reminder target. Applied only to an
    /// Anthropic API-bound clone; it is never written into conversation history.
    pub tool_result_reminder_suffixes: Vec<crate::convert::AnthropicToolResultReminderSuffix>,
    /// Exact transcript identities of typed prompt attachments. These rows
    /// append using Native attachment merge semantics instead of ordinary
    /// adjacent-user text joining.
    pub typed_attachment_ids: std::collections::HashSet<lingxi_core::types::MessageId>,
    /// User source rows whose original content root was a JSON string.
    pub scalar_user_content_ids: std::collections::HashSet<lingxi_core::types::MessageId>,
    /// Captured Native task deliveries indexed by transcript message identity.
    /// These snapshots are request-only and never rewritten into durable history.
    pub task_deliveries: std::collections::HashMap<
        lingxi_core::types::MessageId,
        lingxi_core::host::task_delivery::TaskDeliveryRequest,
    >,
    /// Trusted Native request facts, outside provider input and persisted history.
    pub native_anthropic_request_context: Option<lingxi_llm_client::NativeAnthropicRequestContext>,
}

/// Current owned non-streaming main-message request.
#[derive(Debug, Clone)]
pub struct MessagesCreateRequest {
    pub model: String,
    pub profile: Option<String>,
    pub system: Option<crate::SystemPromptInput>,
    pub messages: Vec<ConversationMessage>,
    pub tools: Vec<crate::ToolDeclaration>,
    pub opts: MessagesCreateOptions,
}

impl MessagesCreateRequest {
    /// Own the assembled conversation and selected route for one logical call.
    #[must_use]
    pub fn new(
        model: &str,
        profile: Option<&str>,
        system: Option<crate::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
    ) -> Self {
        Self {
            model: model.to_owned(),
            profile: profile.map(str::to_owned),
            system,
            messages,
            tools,
            opts: MessagesCreateOptions::default(),
        }
    }
}

/// Header classification independent of a canonical request's body policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonStreamingRequestClass {
    Main,
    Auxiliary,
}

/// Retry controls shared by main and isolated canonical requests.
#[derive(Debug, Clone, Default)]
pub struct NonStreamingRetryOptions {
    pub initial_consecutive_overloaded: Option<u8>,
    pub fallback: FallbackPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QuerySourceCategory {
    Main,
    Subagent,
    Auxiliary,
}

/// Native 2.1.287 `Ss`, src_180615120.js bytes [247071,247246).
pub(super) fn query_source_category(source: Option<&str>) -> Option<QuerySourceCategory> {
    source.map(|source| {
        if source.starts_with("repl_main_thread") || source == "sdk" {
            QuerySourceCategory::Main
        } else if source.starts_with("agent:") || source == "hook_agent" {
            QuerySourceCategory::Subagent
        } else {
            QuerySourceCategory::Auxiliary
        }
    })
}

pub(super) fn request_query_source_category(request: &LlmRequest) -> Option<QuerySourceCategory> {
    let source = request.execution.query_source.as_deref().or_else(|| {
        request
            .execution
            .prompt_cache
            .as_ref()
            .and_then(|context| context.policy.query_source.as_deref())
    });
    query_source_category(source)
}

pub(super) fn prompt_cache_query_source(source: Option<&str>) -> PromptCacheQuerySource<'_> {
    source.map_or(
        PromptCacheQuerySource::Unspecified,
        PromptCacheQuerySource::Named,
    )
}

pub(super) fn parse_fallback_chain(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for model in raw.split(',').map(str::trim).filter(|m| !m.is_empty()) {
        if !out.iter().any(|existing| existing == model) {
            out.push(model.to_string());
        }
    }
    out
}

/// Maximum media items (images + documents) the API accepts per request.
/// Above this we trim oldest-first. Mirrors TS `API_MAX_MEDIA_PER_REQUEST`
/// (apiLimits.ts:94).
pub(super) const MAX_MEDIA_PER_REQUEST: usize = 100;

/// True when a nested `tool_result.content` block (a raw JSON value, e.g. an MCP
/// image/resource result) is a media item — `type === "image" || "document"`,
/// matching claude-code `isMedia` (`claude.ts:943`).
fn is_media_value(v: lingxi_core::types::utf16_json::JsonValueView<'_>) -> bool {
    is_nested_media_value(v)
}

fn nested_media_count(blocks: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> usize {
    let Some(values) = blocks.at_pointer("") else {
        return 0;
    };
    let Some(length) = values.array_len() else {
        return 0;
    };
    (0..length)
        .filter(|index| values.get_index(*index).is_some_and(is_media_value))
        .count()
}

fn strip_nested_media(
    blocks: &mut lingxi_core::types::utf16_json::Utf16JsonProjection,
    to_remove: &mut usize,
) {
    let Some(length) = blocks.array_len("") else {
        return;
    };
    if *to_remove == 0 {
        return;
    }
    let mut retained = Vec::with_capacity(length);
    for index in 0..length {
        let item = blocks
            .subprojection(&format!("/{index}"))
            .expect("content-block array item is a valid JSON subtree");
        if *to_remove > 0 && item.at_pointer("").is_some_and(is_media_value) {
            *to_remove -= 1;
        } else {
            retained.push(item);
        }
    }
    if retained.len() != length {
        *blocks = lingxi_core::types::utf16_json::Utf16JsonProjection::array(retained)
            .expect("retained content-block items form a valid JSON array");
    }
}

/// Count media (image/document) content blocks across all messages, INCLUDING
/// media NESTED inside `tool_result.content` (the `content_blocks` array MCP
/// image/resource results populate). 1:1 with claude-code `stripExcessMediaItems`
/// counting (`claude.ts:961-971`) — top-level media that ignored the nested
/// channel let an MCP-image-heavy transcript silently exceed the API media cap.
pub(super) fn count_media(msgs: &[ConversationMessage]) -> usize {
    msgs.iter()
        .map(|m| match m {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } => content
                .iter()
                .map(|b| match b {
                    ContentBlock::Image { .. } | ContentBlock::Document { .. } => 1,
                    ContentBlock::ToolResult {
                        content_blocks: Some(blocks),
                        ..
                    } => nested_media_count(blocks),
                    _ => 0,
                })
                .sum::<usize>(),
            ConversationMessage::System { .. } => 0,
        })
        .sum()
}

/// Return `msgs` with the OLDEST media items stripped until at most `limit`
/// remain. 1:1 with claude-code `stripExcessMediaItems` (`claude.ts:975-1014`):
/// for each message (oldest-first), strip media NESTED in `tool_result.content`
/// FIRST (the `.map`, `:982-999`), then TOP-LEVEL media (the `.filter`,
/// `:1000-1006`).
pub(super) fn strip_excess_media(
    msgs: Vec<ConversationMessage>,
    limit: usize,
) -> Vec<ConversationMessage> {
    strip_excess_media_with_sources(ConversationMessagesWithSources::new(msgs), limit).messages
}

fn strip_excess_media_with_sources(
    mut msgs: ConversationMessagesWithSources,
    limit: usize,
) -> ConversationMessagesWithSources {
    let total = count_media(&msgs.messages);
    if total <= limit {
        return msgs;
    }
    let mut to_remove = total - limit;
    for (m, block_sources) in msgs.messages.iter_mut().zip(&mut msgs.block_sources) {
        if to_remove == 0 {
            break;
        }
        let content = match m {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } => content,
            ConversationMessage::System { .. } => continue,
        };
        // (1) Nested-in-tool_result media first (claude-code `.map`).
        for block in content.iter_mut() {
            if to_remove == 0 {
                break;
            }
            if let ContentBlock::ToolResult {
                content_blocks: Some(blocks),
                ..
            } = block
            {
                strip_nested_media(blocks, &mut to_remove);
            }
        }
        // (2) Top-level media (claude-code `.filter`).
        let mut retained_content = Vec::with_capacity(content.len());
        let mut retained_sources = Vec::with_capacity(block_sources.len());
        for (block, sources) in content.drain(..).zip(block_sources.drain(..)) {
            if to_remove > 0
                && (matches!(&block, ContentBlock::Image { .. })
                    || matches!(&block, ContentBlock::Document { .. }))
            {
                to_remove -= 1;
            } else {
                retained_content.push(block);
                retained_sources.push(sources);
            }
        }
        *content = retained_content;
        *block_sources = retained_sources;
    }
    msgs
}

impl ApiService {
    // ── Shared request build ─────────────────────────────────────────────────

    /// Convert typed host inputs into an `LlmRequest`.
    // An internal request-assembler: model + profile + system + msgs + tools +
    // stream + max_tokens are all genuinely distinct inputs (8/7).
    #[allow(clippy::too_many_arguments)]
    /// Build a provider request from the host's typed messages and tool
    /// declarations. Source-backed JSON stays in `ToolDeclaration` until the
    /// SDK codec emits each provider request.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_request(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&crate::SystemPromptInput>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        stream: bool,
        max_tokens: Option<u32>,
        skip_global_cache_for_system_prompt: bool,
        query_source: PromptCacheQuerySource<'_>,
    ) -> Result<LlmRequest, LlmError> {
        self.build_request_with_typed_attachment_ids(
            model,
            profile,
            system,
            msgs,
            tools,
            stream,
            max_tokens,
            skip_global_cache_for_system_prompt,
            query_source,
            &std::collections::HashSet::new(),
            &std::collections::HashMap::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_request_with_typed_attachment_ids(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&crate::SystemPromptInput>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        stream: bool,
        max_tokens: Option<u32>,
        skip_global_cache_for_system_prompt: bool,
        query_source: PromptCacheQuerySource<'_>,
        typed_attachment_ids: &std::collections::HashSet<lingxi_core::types::MessageId>,
        task_deliveries: &std::collections::HashMap<
            lingxi_core::types::MessageId,
            lingxi_core::host::task_delivery::TaskDeliveryRequest,
        >,
    ) -> Result<LlmRequest, LlmError> {
        // Auxiliary builders share normal policy, but the main turn alone
        // owns its computer continuation and durable receipt submission.
        crate::computer::without_computer_request(|| {
            self.build_main_request(
                model,
                profile,
                system,
                msgs,
                tools,
                stream,
                max_tokens,
                skip_global_cache_for_system_prompt,
                query_source,
                None,
                &[],
                typed_attachment_ids,
                task_deliveries,
            )
        })
    }

    pub(super) fn build_main_request(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&crate::SystemPromptInput>,
        msgs: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        stream: bool,
        max_tokens: Option<u32>,
        skip_global_cache_for_system_prompt: bool,
        query_source: PromptCacheQuerySource<'_>,
        main_thinking_config: Option<crate::model::thinking::ThinkingConfig>,
        tool_result_reminder_suffixes: &[crate::convert::AnthropicToolResultReminderSuffix],
        typed_attachment_ids: &std::collections::HashSet<lingxi_core::types::MessageId>,
        task_deliveries: &std::collections::HashMap<
            lingxi_core::types::MessageId,
            lingxi_core::host::task_delivery::TaskDeliveryRequest,
        >,
    ) -> Result<LlmRequest, LlmError> {
        // Native Ovn normalizes and pairs tools before Kvn applies the media
        // count/byte budgets. Other providers retain their existing cap policy.
        // Session-scoped tool-search gate (Claude Code `$U()`), published by the
        // orchestrator. NOT inferred from whether THIS request's toolset carries
        // a `ToolSearch` declaration: `$U()` reads only the session mode +
        // provider, and the branch site `if(!$U())W=j6s(W);else W=xPy(W,a)` runs
        // for main-loop AND side-query requests alike. A side query assembled
        // with an empty toolset (compaction summarizer, recap) in a
        // tool-search-enabled session must therefore still take the ENABLED
        // branch — emitting "[…tools no longer available]" rather than the
        // disabled branch's "[…tool search not enabled]". The request's `tools`
        // remain the availability set (`a`) below.
        // Capture base and nullable max-token overlay at logical request build
        // entry. Retry preparation may be repeated, but a live settings change
        // during the call cannot change this request's session policy.
        let mut request_thinking_state = self.thinking_state();
        if let Some(thinking) = main_thinking_config {
            request_thinking_state.set_base(thinking);
        }

        let mut msgs = msgs;
        let family = self.protocol_for_model(model, profile)?;
        apply_anthropic_tool_result_reminder_suffixes(
            family,
            &mut msgs,
            tool_result_reminder_suffixes,
        );
        let context_measurement = crate::context_usage::measurement(model, profile, &msgs);
        let context_history = crate::context_usage::HistoryPrefix::new(&msgs);
        let native_system_prefix = self
            .anthropic
            .native_system_prefix
            .as_ref()
            .filter(|_| self.native_api_system_route(model, profile))
            .map(|prefix| {
                (
                    prefix.clone(),
                    native_prompt_attribution(&msgs, self.ua.workload.clone()),
                )
            });
        let thinking_source_message_ids: Vec<_> = msgs
            .iter()
            .filter_map(|message| {
                if let ConversationMessage::Assistant { id, content, .. } = message {
                    content
                        .iter()
                        .any(|block| {
                            matches!(
                                block,
                                lingxi_core::types::ContentBlock::Thinking { .. }
                                    | lingxi_core::types::ContentBlock::RedactedThinking { .. }
                            )
                        })
                        .then_some(*id)
                } else {
                    None
                }
            })
            .collect();
        let thinking_recovery_scope = self.thinking_recovery_scope();
        thinking_recovery_scope.capture(&thinking_source_message_ids);
        if !crate::model::thinking_signature::thinking_must_round_trip(model, profile) {
            crate::model::thinking_signature::strip_marked_conversation_thinking(
                &mut msgs,
                &thinking_recovery_scope.messages(),
            );
        }
        let source_thinking_messages =
            crate::prefix_lock_recovery::capture_source_thinking_messages(&msgs);
        if !self.native_api_system_route(model, profile) {
            for message in &mut msgs {
                if let ConversationMessage::User {
                    api_message_override,
                    ..
                } = message
                {
                    *api_message_override = None;
                }
            }
        }
        let mut per_message_effort = false;
        if self.native_per_turn_effort_route(model, profile) {
            let mut admitted = LlmRequest::new(model);
            admitted.profile = profile.map(str::to_owned);
            admitted.execution.resolve_native_effort = true;
            admitted.execution.native_anthropic_base_url_fingerprint =
                self.native_anthropic_endpoint_fingerprint(model, profile);
            self.refresh_effort_settings(&mut admitted)?;
            if let Ok(effort) = MOD_REQUEST_EFFORT.try_with(Clone::clone) {
                admitted.set_effort(Some(effort))?;
            }
            if let Some(effort) = self
                .client()
                .native_per_turn_effort_policy(&admitted)?
                .filter(|policy| policy.supported)
                .and_then(|policy| policy.value)
            {
                let effort = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                    effort.as_json_source(),
                )
                .map_err(|error| LlmError::InvalidRequest {
                    message: format!("resolved per-turn effort is invalid JSON source: {error}"),
                })?;
                if effort.string_units("").is_none() {
                    return Err(LlmError::InvalidRequest {
                        message: "resolved per-turn effort must be a JSON string".into(),
                    });
                }
                per_message_effort = true;
                msgs = lingxi_core::types::project_per_turn_effort(msgs, &effort);
            }
        }
        if !per_message_effort {
            msgs.retain_mut(|message| {
                if let ConversationMessage::System {
                    api_system: Some(payload),
                    ..
                } = message
                {
                    payload.output_config = None;
                    return !payload.content.is_empty();
                }
                if let ConversationMessage::User {
                    api_message_override: Some(payload),
                    ..
                } = message
                {
                    payload.output_config = None;
                }
                true
            });
        }
        let local_tool_search_history = crate::local_tool_search::capture_history(&mut msgs)?;
        let tool_search_enabled = self
            .tool_search_enabled
            .load(std::sync::atomic::Ordering::Relaxed);
        let available_tool_names: std::collections::HashSet<String> =
            tools.iter().map(|tool| tool.name.clone()).collect();
        let native_media_limits = self.native_media_limits_for_request(model, profile, family);
        let messages = ConversationMessagesWithSources::new(msgs);
        let messages = if native_media_limits.is_some() {
            messages
        } else {
            strip_excess_media_with_sources(messages, MAX_MEDIA_PER_REQUEST)
        };
        let messages = ensure_tool_result_pairing_with_sources(
            normalize_messages_for_api_with_tool_search_and_sources(
                messages,
                typed_attachment_ids,
                task_deliveries,
                tool_search_enabled,
                Some(&available_tool_names),
            ),
        );
        let messages = if let Some(limits) = native_media_limits {
            strip_native_media_with_sources(messages, limits)
        } else {
            messages
        };
        let request_message_source_ids = messages.contributing_message_ids();
        let request_message_source_indices = messages
            .block_sources
            .iter()
            .enumerate()
            .flat_map(|(index, blocks)| blocks.iter().flatten().map(move |id| (*id, index)))
            .collect();
        let mut request_content_source_ids = messages.block_sources.clone();
        let mut messages = to_llm_messages(messages.messages)?;
        if request_content_source_ids.len() != messages.len() {
            request_content_source_ids.clear();
        } else {
            for (sources, message) in request_content_source_ids.iter_mut().zip(&messages) {
                if sources.len() != message.content.len() {
                    sources.clear();
                    sources.resize_with(message.content.len(), Default::default);
                }
            }
        }
        let mut req = LlmRequest::new(model);
        req.execution.local_tool_search_history =
            tool_search_enabled.then(|| Arc::new(local_tool_search_history));
        req.execution.isolated_query = crate::computer::uses_auxiliary_history_projection();
        if let Some(thinking) = main_thinking_config {
            req.execution.native_thinking_request_type_disabled =
                crate::model::thinking_binding_controls::native_request_type_disabled(thinking);
            req.execution.native_thinking_mechanically_disabled = Some(false);
        }
        req.execution.native_thinking_state_snapshot = Some(request_thinking_state);
        req.execution.native_anthropic_base_url_fingerprint =
            self.native_anthropic_endpoint_fingerprint(model, profile);
        req.execution.request_message_source_ids = request_message_source_ids;
        req.execution.request_message_source_indices = request_message_source_indices;
        req.execution.refusal_fallback_context =
            lingxi_core::host::refusal_driver::current_fallback_target();
        if let Some(p) = profile {
            req = req.with_profile(p);
        }

        use lingxi_llm_client::providers::anthropic::system_prompt as prompt_cache;
        let cache_policy = prompt_cache::CachePolicy::from_process(
            lingxi_core::host::compliance_taints::is_tainted("hipaa"),
            agent_prompt_cache_ttl_override(),
            skip_global_cache_for_system_prompt,
            query_source,
            Default::default(),
            self.prompt_cache_ttl_inputs(),
        );
        let enable_caching = prompt_cache::prompt_caching_enabled(model, family);
        let prompt_cache_overage = self.anthropic.prompt_cache_overage.clone();
        let prompt_cache_epoch = prompt_cache_overage.clone();
        req.execution.prompt_cache = Some(crate::PromptCacheRequestContext {
            system: system.cloned(),
            native_system_prefix,
            policy: cache_policy,
            current_account_epoch: Arc::new(move || prompt_cache_epoch.account_epoch()),
            native_bare_mode: native_prompt_cache_bare_mode(),
            native_unix_socket: native_anthropic_unix_socket_enabled(),
            overage_for_scope: Arc::new(move |scope, account_epoch| {
                prompt_cache_overage.is_using_overage(scope, account_epoch)
            }),
            ttl_settings: self.request_prompt_cache_ttl_settings(),
        });
        req.execution.thinking_source_message_ids = thinking_source_message_ids;
        req.execution.source_thinking_messages = source_thinking_messages;
        req.execution.thinking_recovery_scope = Some(thinking_recovery_scope);

        // 1P experimental cache-editing pass (claude.ts addCacheBreakpoints,
        // 3108-3208). Gated behind `useCachedMC` (`should_use_cache_editing`):
        // when OFF (the default), this is a no-op and the request is
        // byte-identical to the pre-feature path. When ARMED it (a) re-inserts
        // previously-pinned cache_edits at their original positions, (b) inserts
        // the new cache_edits into the last user message, and (c) stamps
        // `cache_reference` onto every tool_result strictly before the last
        // cache_control marker — all with cross-block delete-ref dedup.
        if self.should_use_cache_editing() {
            apply_cache_editing(
                &mut messages,
                enable_caching,
                &self.anthropic.cache_editing_inputs.new_edits,
                &self.anthropic.cache_editing_inputs.pinned,
            );
        }

        if request_content_source_ids.len() == messages.len() {
            for (sources, message) in request_content_source_ids.iter_mut().zip(&messages) {
                if sources.len() < message.content.len() {
                    sources.resize_with(message.content.len(), Default::default);
                } else if sources.len() > message.content.len() {
                    sources.clear();
                    sources.resize_with(message.content.len(), Default::default);
                }
            }
        }

        let cache_enabled = !crate::computer::uses_auxiliary_history_projection()
            && matches!(query_source, PromptCacheQuerySource::Named(source)
                if query_source_category(Some(source)) == Some(QuerySourceCategory::Main));
        let mut projection_cache = cache_enabled.then(|| {
            self.history_projection_cache
                .lock()
                .expect("history projection cache")
        });
        let projection_started = Instant::now();
        let (input, overrides, request_content_source_ids) =
            crate::convert::input_projection::history_input_with_cached_sources(
                model,
                &messages,
                &request_content_source_ids,
                &[],
                &tools,
                family,
                projection_cache.as_deref_mut(),
            )?;
        if let Some(cache) = &projection_cache {
            tracing::debug!(target: "llm_runtime::history_cache", messages = messages.len(), hits = cache.hits,
                misses = cache.misses, retained_bytes = cache.retained_bytes(),
                projection_us = projection_started.elapsed().as_micros() as u64);
        }
        drop(projection_cache);
        req.input = input;
        req.execution.request_content_source_ids = request_content_source_ids;
        req.execution.input_protocol = Some(family);
        req.execution.prompt_projection_profile =
            self.client().prompt_cache_profile(model, profile);
        req.execution.message_json_string_overrides = overrides;
        crate::computer::apply_request_projection(&mut req)?;
        // No tool-array breakpoint (matches TS baseline).
        // Forced tool choice (e.g. `--json-schema` → `StructuredOutput`). Unset
        // for every normal turn, so the request carries no `tool_choice` and the
        // model chooses freely — byte-identical to the pre-feature request.
        if let Some(choice) = &self.forced_tool_choice {
            req.set_tool_choice(Some(choice.clone()));
        }
        if req.input.model.contains("deepseek") || req.profile.as_deref() == Some("deepseek") {
            tracing::debug!(
                event = "build_request",
                model = %req.input.model,
                profile = req.profile.as_deref().unwrap_or("<none>"),
                messages = messages.len(),
                tools = req.input.tools.len(),
                forced_tool_choice = self.forced_tool_choice.is_some(),
                active_tool_choice = ?req.input.tool_choice,
                stream = req.stream,
            );
        }
        req.stream = stream;

        // Keep the logical request's explicit/automatic intent. The SDK
        // resolves the effective budget on every selected connection before
        // validation/encoding; None uses the provider default where optional.
        let env_budget = std::env::var("LINGXI_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|raw| {
                let value = lingxi_core::host::env::parse_int_env(&raw);
                (!value.is_nan() && value > 0.0).then(|| value.min(u32::MAX as f64) as u32)
            });
        req.input.max_tokens = max_tokens.or(env_budget);

        // thinking (DIV-1) + temperature (DIV-4), mirroring claude.ts:1596-1630
        // and claude.ts:1693. Computed AFTER max_tokens is known (the fixed-
        // budget cap clamps to max_tokens-1).
        {
            use crate::model::thinking::{model_sends_temperature, session_thinking_active};

            let thinking = self.main_request_thinking_config(
                model,
                profile,
                query_source,
                request_thinking_state,
            )?;
            if self.native_api_system_route(model, profile) {
                req.execution.resolve_native_effort = true;
                req.execution.anthropic_context_management = Some(
                    lingxi_llm_client::providers::anthropic::request_policy::AnthropicContextManagement {
                        has_thinking: !matches!(thinking, crate::model::thinking::ThinkingConfig::Disabled)
                            && !crate::model::thinking::is_thinking_env_disabled("LINGXI_DISABLE_THINKING"),
                        tool_clearing: None,
                    },
                );
                req.execution.native_thinking_display =
                    self.anthropic.native_thinking_display.clone();
            }
            let has_thinking = session_thinking_active(thinking);
            req.execution.automatic_thinking_budget =
                matches!(thinking, crate::model::thinking::ThinkingConfig::Adaptive);

            // SDK preparation binds automatic manual thinking to each
            // attempt's selected route and effective output ceiling. Keep the
            // logical request independent of the first connection's default.
            req.set_reasoning(crate::model::thinking::reasoning_for_route(
                thinking,
                model,
                if req.execution.automatic_thinking_budget {
                    None
                } else {
                    req.input.max_tokens
                },
                Some(family),
            ));
            if matches!(thinking, crate::model::thinking::ThinkingConfig::Disabled)
                && !self.native_api_system_route(model, profile)
            {
                // Non-native codecs distinguish explicit Off from an omitted
                // mode, which means the provider's default reasoning policy.
                req.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
                    mode: Some(lingxi_llm_client::protocol::ThinkingMode::Disabled),
                    ..Default::default()
                });
            }

            // temperature:1 ONLY when thinking is disabled AND the model is in the
            // `rhn` temperature-gate set (binary @205866168:
            // `!xs && rhn(u) ? temperatureOverride ?? 1 : void 0`). The default
            // opus-4-8 (and 4-7/fable-5/mythos-5/unknowns) are NOT in `rhn` → the
            // field is omitted. The Anthropic codec emits temperature on Some only.
            req.input.temperature = if !has_thinking
                && !matches!(thinking, crate::model::thinking::ThinkingConfig::Automatic)
                && family.is_anthropic_family()
                && model_sends_temperature(model)
            {
                Some(1.0)
            } else {
                None
            };
        }

        self.capture_anthropic_user_id(&mut req, family)?;

        if let Ok(effort) = MOD_REQUEST_EFFORT.try_with(Clone::clone) {
            req.set_effort(Some(effort))?;
        }
        crate::anthropic_extra_body::capture(&mut req)?;
        req.execution.context_usage_binding = context_history.clone().map(|history| {
            crate::context_usage::ContextUsageBinding::new(history, context_measurement.clone())
        });
        req.execution.context_usage_prefix = context_history
            .and_then(|history| crate::context_usage::ContextUsagePrefix::new(&req, history));
        req.execution.context_usage_estimate =
            context_measurement.and_then(|measurement| measurement.bind(&req));
        Ok(req)
    }

    pub(super) fn build_main_message_request(
        &self,
        request: MessagesCreateRequest,
        stream: bool,
    ) -> Result<(LlmRequest, NonStreamingRetryOptions), LlmError> {
        let MessagesCreateRequest {
            model,
            profile,
            system,
            messages,
            tools,
            opts,
        } = request;
        let mut req = self.build_main_request(
            &model,
            profile.as_deref(),
            system.as_ref(),
            messages,
            tools,
            stream,
            opts.max_output_tokens,
            opts.skip_global_cache_for_system_prompt,
            prompt_cache_query_source(opts.query_source.as_deref()),
            None,
            &opts.tool_result_reminder_suffixes,
            &opts.typed_attachment_ids,
            &opts.task_deliveries,
        )?;
        req.execution.scalar_user_content_ids = opts.scalar_user_content_ids;
        req.input.controls.anthropic.context_hint = opts
            .context_hint
            .as_ref()
            .map(lingxi_llm_client::protocol::JsonPayload::from_generated_value)
            .transpose()
            .map_err(|error| LlmError::InvalidRequest {
                message: format!("context hint could not be encoded as source: {error}"),
            })?;
        req.input.model_source = opts
            .model_source
            .as_ref()
            .map(|source| {
                let units = source
                    .string_units("")
                    .ok_or_else(|| LlmError::InvalidRequest {
                        message: "model source must be a JSON string projection".into(),
                    })?;
                Ok::<_, LlmError>(
                    lingxi_llm_client::protocol::JsonPayload::string_from_utf16_code_units(&units),
                )
            })
            .transpose()?;
        req.execution.context_hint_beta = opts.context_hint_beta;
        req.set_effort(opts.effort)?;
        req.execution.model_attempt = opts.model_attempt;
        req.execution
            .set_request_dispatch_admission(opts.request_dispatch_admission);
        req.execution.request_dispatch_observer = opts.request_dispatch_observer;
        req.execution.native_anthropic_request_context = opts.native_anthropic_request_context;
        req.execution.query_source = opts.query_source;
        req.execution.failed_stream_outlasted_timeout = opts.failed_stream_outlasted_timeout;
        req.execution.stream_fallback = opts.initial_consecutive_overloaded.is_some();
        Ok((
            req,
            NonStreamingRetryOptions {
                initial_consecutive_overloaded: opts.initial_consecutive_overloaded,
                fallback: opts.fallback,
            },
        ))
    }

    /// Build a scheduled main turn with its own reasoning policy. The turn
    /// retains its computer scope even though scheduled wire policy is auxiliary.
    pub fn build_scheduled_request(
        &self,
        request: MessagesCreateRequest,
        thinking: crate::model::thinking::ThinkingConfig,
        effort: Option<serde_json::Value>,
    ) -> Result<LlmRequest, LlmError> {
        let model = request.model.clone();
        let (mut req, _) =
            crate::thinking_scope::isolated(|| self.build_main_message_request(request, false))?;
        // Scheduled turns provide a separate thinking config and use the
        // Auxiliary request path; do not retain the StandardMain-only Native
        // type/mechanical sidecars from the shared message builder.
        req.execution.native_thinking_request_type_disabled = None;
        req.execution.native_thinking_mechanically_disabled = None;
        if MOD_REQUEST_EFFORT.try_with(|_| ()).is_ok() {
            if let Some(thinking) = req.input.thinking.as_mut() {
                thinking.effort = None;
            }
        }
        req.set_tool_choice(None);
        req.execution.capture_retry_count = true;
        req.execution.anthropic_request_kind = lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
        self.apply_side_query_thinking(&mut req, &model, Some(thinking), None);
        req.set_effort(effort)?;
        crate::anthropic_extra_body::capture(&mut req)?;
        Ok(req)
    }

    /// Stream the current host-owned Native source vector without flattening
    /// marker elements or section boundaries in the orchestrator.
    /// The sanitized Native query source selects the SDK's main/subagent TTL
    /// environment branch before provider serialization.
    pub async fn stream_with_system_prompt(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&crate::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        max_tokens: Option<u32>,
        effort: Option<serde_json::Value>,
        speed: Option<String>,
        skip_global_cache_for_system_prompt: bool,
        query_source: Option<&str>,
        request_dispatch_admission: Option<crate::RequestDispatchAdmission>,
        tool_result_reminder_suffixes: Vec<crate::convert::AnthropicToolResultReminderSuffix>,
        request_dispatch_observer: Option<crate::RequestDispatchObserver>,
        native_anthropic_request_context: Option<lingxi_llm_client::NativeAnthropicRequestContext>,
        typed_attachment_ids: std::collections::HashSet<lingxi_core::types::MessageId>,
        task_deliveries: std::collections::HashMap<
            lingxi_core::types::MessageId,
            lingxi_core::host::task_delivery::TaskDeliveryRequest,
        >,
        model_source: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        scalar_user_content_ids: std::collections::HashSet<lingxi_core::types::MessageId>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        self.with_account_call_snapshot(async {
            let mut req = self.build_main_request(
                model,
                profile,
                system,
                messages,
                tools,
                true,
                max_tokens,
                skip_global_cache_for_system_prompt,
                prompt_cache_query_source(query_source),
                None,
                &tool_result_reminder_suffixes,
                &typed_attachment_ids,
                &task_deliveries,
            )?;
            req.execution.scalar_user_content_ids = scalar_user_content_ids;
            req.input.model_source = model_source
                .as_ref()
                .map(|source| {
                    let units =
                        source
                            .string_units("")
                            .ok_or_else(|| LlmError::InvalidRequest {
                                message: "model source must be a JSON string projection".into(),
                            })?;
                    Ok::<_, LlmError>(
                        lingxi_llm_client::protocol::JsonPayload::string_from_utf16_code_units(
                            &units,
                        ),
                    )
                })
                .transpose()?;
            req.set_effort(effort)?;
            req.set_speed(speed)?;
            req.execution.query_source = query_source.map(str::to_owned);
            req.execution
                .set_request_dispatch_admission(request_dispatch_admission);
            req.execution.request_dispatch_observer = request_dispatch_observer;
            req.execution.native_anthropic_request_context = native_anthropic_request_context;
            self.drive_stream(req).await
        })
        .await
    }
}
