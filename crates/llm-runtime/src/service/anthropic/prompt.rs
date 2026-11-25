//! Host inputs for Claude prompt caching and native system layout.

use crate::service::anthropic::endpoint::{
    is_native_main_anthropic_route, native_tbc_first_party_tf,
};
use crate::service::native_media::NativeMediaLimits;
use crate::service::ApiService;
use crate::{agent_prompt_cache_ttl_override, LlmError, LlmRequest, PromptCacheQuerySource};
use lingxi_core::types::ConversationMessage;
use std::sync::{Arc, Mutex, OnceLock};

pub(in crate::service) fn apply_anthropic_tool_result_reminder_suffixes(
    family: lingxi_llm_client::protocol::ProtocolFamily,
    messages: &mut Vec<ConversationMessage>,
    suffixes: &[crate::convert::AnthropicToolResultReminderSuffix],
) {
    if family != lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages {
        return;
    }
    for suffix in suffixes {
        crate::convert::append_anthropic_tool_result_reminder(messages, suffix);
    }
}

/// Convert a Host `customSystemPrompt` string at its Native input boundary.
/// Native `MDo` applies `hwe` only to this string field; `overrideSystemPrompt`

fn native_prompt_cache_1h_allowlist() -> Vec<String> {
    static LATCH: OnceLock<Mutex<Option<Vec<String>>>> = OnceLock::new();
    let latch = LATCH.get_or_init(|| Mutex::new(None));
    let mut guard = latch.lock().unwrap_or_else(|error| error.into_inner());
    guard
        .get_or_insert_with(|| {
            ::telemetry::flag_string_list(
                lingxi_llm_client::providers::anthropic::system_prompt::PROMPT_CACHE_1H_ALLOWLIST_FEATURE,
                lingxi_llm_client::providers::anthropic::system_prompt::PROMPT_CACHE_1H_ALLOWLIST_DEFAULT,
            )
        })
        .clone()
}

pub(in crate::service) fn sdk_prompt_cache_ttl_settings(
    settings: lingxi_core::settings::schema::PromptCacheTtlSettings,
) -> lingxi_llm_client::providers::anthropic::system_prompt::PromptCacheTtlSettings {
    use lingxi_core::settings::schema::PromptCacheTtl as HostTtl;
    use lingxi_llm_client::providers::anthropic::system_prompt::PromptCacheTtl as SdkTtl;

    let convert = |ttl: Option<HostTtl>| {
        ttl.map(|ttl| match ttl {
            HostTtl::FiveMinutes => SdkTtl::FiveMinutes,
            HostTtl::OneHour => SdkTtl::OneHour,
        })
    };
    lingxi_llm_client::providers::anthropic::system_prompt::PromptCacheTtlSettings {
        prompt_cache_ttl: convert(settings.main),
        subagent_prompt_cache_ttl: convert(settings.subagent),
    }
}

pub(in crate::service) fn native_prompt_attribution(
    messages: &[lingxi_core::types::ConversationMessage],
    workload: Option<String>,
) -> lingxi_llm_client::providers::anthropic::system_prompt::NativePromptAttribution {
    use lingxi_llm_client::providers::anthropic::system_prompt::{
        NativePromptAttribution, PromptText,
    };

    let first = messages
        .iter()
        .find_map(|message| match message {
            lingxi_core::types::ConversationMessage::User {
                content,
                is_meta: false,
                ..
            } => Some(content),
            _ => None,
        })
        .and_then(|content| content.iter().find(|block| block.visible_text().is_some()));
    let first_user_text = first
        .map(|block| {
            let units = match block {
                lingxi_core::types::ContentBlock::Text { text, .. } => {
                    Some(text.encode_utf16().collect())
                }
                lingxi_core::types::ContentBlock::TextJsUtf16 {
                    utf16_code_units, ..
                } => Some(utf16_code_units.clone()),
                lingxi_core::types::ContentBlock::ProviderContent { protocol, value }
                    if protocol == "anthropic_messages"
                        && value.string_at("/type") == Some("text") =>
                {
                    value.string_units("/text")
                }
                _ => None,
            };
            match units {
                Some(units) => PromptText::from_utf16(units),
                None => PromptText::from_string(block.visible_text().unwrap_or_default()),
            }
        })
        .unwrap_or_else(|| PromptText::from_string(""));
    NativePromptAttribution {
        first_user_text,
        is_subagent: lingxi_core::host::session_flags::current_request_is_subagent(),
        workload,
        previous_request_id: None,
        prompt_id: None,
        turn_origin: None,
        turn_position: None,
    }
}

/// A previously-pinned cache_edits block plus the user-message index it must be
/// re-inserted at. Mirrors claude-code's `CachedMCPinnedEdits`
/// (`services/api/claude.ts:3057-3060`).
#[derive(Debug, Clone, Default)]
pub(in crate::service) struct PinnedCacheEdits {
    /// Index into the request `messages` (the user message to splice into).
    pub(in crate::service) user_message_index: usize,
    /// The cache_edits delete operations to re-insert at that position.
    pub(in crate::service) edits: Vec<crate::CacheEdit>,
}

/// Cache-editing builder inputs — the `newCacheEdits` + `pinnedEdits` args of
/// claude-code's `addCacheBreakpoints`. Default is empty (no producer wired):
/// only the gate-armed `cache_reference`-on-tool_results pass runs by default.
#[derive(Debug, Clone, Default)]
pub(in crate::service) struct CacheEditingInputs {
    /// New cache_edits delete ops to insert into the last user message and pin.
    pub(in crate::service) new_edits: Vec<crate::CacheEdit>,
    /// Previously-pinned cache_edits to re-insert at their original positions.
    pub(in crate::service) pinned: Vec<PinnedCacheEdits>,
}

pub(crate) fn extra_body_object(
) -> Result<Option<lingxi_llm_client::protocol::JsonPayload>, LlmError> {
    extra_body_object_uncached()
        .map(lingxi_llm_client::providers::anthropic::request_policy::sanitize_extra_body)
        .transpose()
        .map_err(crate::upstream::error)
}

fn extra_body_object_uncached() -> Option<lingxi_llm_client::protocol::JsonPayload> {
    let Ok(t) = std::env::var("CLAUDE_CODE_EXTRA_BODY") else {
        return None;
    };
    if t.is_empty() {
        return None;
    }
    match lingxi_llm_client::protocol::JsonPayload::parse(t.strip_prefix('\u{FEFF}').unwrap_or(&t))
    {
        Ok(body) if body.is_root_object() => Some(body),
        Ok(_) => {
            tracing::error!(
                "CLAUDE_CODE_EXTRA_BODY env var must be a JSON object, but was given {t}"
            );
            None
        }
        Err(err) => {
            tracing::error!("Error parsing CLAUDE_CODE_EXTRA_BODY: {err}");
            None
        }
    }
}

fn extra_metadata_object() -> Option<serde_json::Map<String, serde_json::Value>> {
    extra_metadata_object_uncached()
}

fn extra_metadata_object_uncached() -> Option<serde_json::Map<String, serde_json::Value>> {
    let Ok(extra_str) = std::env::var("CLAUDE_CODE_EXTRA_METADATA") else {
        return None;
    };
    if extra_str.is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(&extra_str) {
        Ok(serde_json::Value::Object(extra)) => Some(extra),
        _ => {
            tracing::error!(
                "CLAUDE_CODE_EXTRA_METADATA env var must be a JSON object, but was given {extra_str}"
            );
            None
        }
    }
}

/// Insert `block` into a content array relative to its `tool_result` blocks.
/// 1:1 port of claude-code `insertBlockAfterToolResults`
/// (`utils/contentArray.ts:21-51`):
///   - if any `tool_result` exists, insert after the LAST one; if that lands the
///     inserted block last, append a `{type:'text', text:'.'}` continuation
///     (some APIs reject a prompt ending in non-text content);
///   - otherwise insert before the last block (`max(0, len-1)`).
/// Mutates `content` in place.
fn insert_block_after_tool_results(
    content: &mut Vec<crate::ContentBlock>,
    block: crate::ContentBlock,
) {
    use crate::ContentBlock as Cb;
    let mut last_tool_result_index: isize = -1;
    for (i, item) in content.iter().enumerate() {
        if matches!(item, Cb::ToolResult { .. }) {
            last_tool_result_index = i as isize;
        }
    }
    if last_tool_result_index >= 0 {
        let insert_pos = (last_tool_result_index as usize) + 1;
        content.insert(insert_pos, block);
        // Append a text continuation if the inserted block is now last.
        if insert_pos == content.len() - 1 {
            content.push(Cb::Text {
                text: ".".to_string(),
                cache_control: None,
                citations: None,
            });
        }
    } else {
        // No tool_result blocks — insert before the last block.
        let insert_index = content.len().saturating_sub(1);
        content.insert(insert_index, block);
    }
}

/// 1P experimental cache-editing pass — the `useCachedMC` tail of claude-code
/// `addCacheBreakpoints` (`services/api/claude.ts:3108-3208`). The caller gates
/// this behind `should_use_cache_editing`; here we assume it's armed.
///
/// `enable_caching` mirrors the TS `enablePromptCaching` flag (the
/// `cache_reference`-on-tool_results pass at 3164 is additionally gated on it).
/// `new_edits` = `newCacheEdits.edits`; `pinned` = `pinnedEdits`.
pub(in crate::service) fn apply_cache_editing(
    messages: &mut [crate::Message],
    enable_caching: bool,
    new_edits: &[crate::CacheEdit],
    pinned: &[PinnedCacheEdits],
) {
    use crate::ContentBlock as Cb;

    // Track all cache_references being deleted to prevent duplicates across
    // blocks (claude.ts:3112-3125 seenDeleteRefs + deduplicateEdits).
    let mut seen_delete_refs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let dedup = |edits: &[crate::CacheEdit],
                 seen: &mut std::collections::HashSet<String>|
     -> Vec<crate::CacheEdit> {
        edits
            .iter()
            .filter(|e| {
                let crate::CacheEdit::Delete { cache_reference } = e;
                if seen.contains(cache_reference) {
                    false
                } else {
                    seen.insert(cache_reference.clone());
                    true
                }
            })
            .cloned()
            .collect()
    };

    // Re-insert all previously-pinned cache_edits at their original positions
    // (claude.ts:3127-3139). Only when that message is a `user` message.
    for p in pinned {
        if let Some(msg) = messages.get_mut(p.user_message_index) {
            if msg.role == "user" {
                let deduped = dedup(&p.edits, &mut seen_delete_refs);
                if !deduped.is_empty() {
                    insert_block_after_tool_results(
                        &mut msg.content,
                        Cb::CacheEdits { edits: deduped },
                    );
                }
            }
        }
    }

    // Insert new cache_edits into the LAST user message and (in TS) pin them
    // (claude.ts:3141-3162). LingXi has no cross-call pin store, so the pinning
    // side-effect is a residual — the in-request insertion is faithful.
    if !messages.is_empty() {
        let deduped_new = dedup(new_edits, &mut seen_delete_refs);
        if !deduped_new.is_empty() {
            for i in (0..messages.len()).rev() {
                if messages[i].role == "user" {
                    insert_block_after_tool_results(
                        &mut messages[i].content,
                        Cb::CacheEdits { edits: deduped_new },
                    );
                    break;
                }
            }
        }
    }

    // Add cache_reference to tool_result blocks within the cached prefix
    // (claude.ts:3164-3207). Must run AFTER cache_edits insertion since that
    // modifies content arrays.
    if enable_caching {
        // Find the last message containing a cache_control marker.
        let mut last_cc_msg: isize = -1;
        for (i, msg) in messages.iter().enumerate() {
            for block in &msg.content {
                let has_cc = match block {
                    Cb::Text { cache_control, .. } | Cb::ToolResult { cache_control, .. } => {
                        cache_control.is_some()
                    }
                    _ => false,
                };
                if has_cc {
                    last_cc_msg = i as isize;
                }
            }
        }

        // Stamp `cache_reference = tool_use_id` on tool_results in `user`
        // messages STRICTLY before the last cache_control marker. (TS uses strict
        // "before" to avoid edge cases where cache_edits splicing shifts indices;
        // it also clones rather than mutating in place to avoid contaminating
        // blocks reused by non-cache-editing secondary queries — here each
        // request owns its `messages`, so an in-place set is equivalent.)
        if last_cc_msg >= 0 {
            for i in 0..(last_cc_msg as usize) {
                if messages[i].role != "user" {
                    continue;
                }
                for block in &mut messages[i].content {
                    if let Cb::ToolResult {
                        tool_call_id,
                        cache_reference,
                        ..
                    } = block
                    {
                        *cache_reference = Some(tool_call_id.clone());
                    }
                }
            }
        }
    }
}

impl ApiService {
    pub(in crate::service) fn capture_anthropic_user_id(
        &self,
        request: &mut LlmRequest,
        protocol: crate::ProtocolFamily,
    ) -> Result<(), LlmError> {
        if let Some(metadata) = self.anthropic.request_metadata.as_ref() {
            if protocol.is_anthropic_family() {
                request.input.metadata = crate::protocol::anthropic_user_id_metadata(
                    &request.input.metadata,
                    &metadata.user_id,
                )?;
            }
            request.execution.anthropic_user_id = Some(metadata.user_id.clone());
        }
        Ok(())
    }

    pub(in crate::service) fn update_anthropic_session_metadata(&self, request: &mut LlmRequest) {
        let (Some(session_id), Some(request_metadata)) = (
            request.execution.request_session_id.as_deref(),
            self.anthropic.request_metadata.as_ref(),
        ) else {
            return;
        };
        let host_value = request_metadata.user_id.as_str();
        let in_input = request.input.metadata.string_at("/user_id").as_deref() == Some(host_value);
        let in_sidecar = request.execution.anthropic_user_id.as_deref() == Some(host_value);
        if !in_input && !in_sidecar {
            return;
        }
        let Ok(identity) =
            lingxi_llm_client::protocol::JsonPayload::parse(&request_metadata.user_id)
        else {
            return;
        };
        if !identity.is_root_object() {
            return;
        }
        let session_source = lingxi_llm_client::protocol::JsonPayload::string(session_id);
        let Ok(identity) = identity.with_object_field("session_id", &session_source) else {
            return;
        };
        if in_sidecar {
            request.execution.anthropic_user_id = Some(identity.as_json_source().to_owned());
        }
        if !in_input {
            return;
        }
        let user_id = lingxi_llm_client::protocol::JsonPayload::string(identity.as_json_source());
        if let Ok(metadata) = request
            .input
            .metadata
            .with_object_field("user_id", &user_id)
        {
            request.input.metadata = metadata;
        }
    }

    /// Explicit host policy for the system attribution envelope. Neutral
    /// providers and ordinary service callers keep their original source.
    pub fn with_native_system_prefix(
        mut self,
        prefix: lingxi_llm_client::providers::anthropic::system_prompt::NativeSystemPrefix,
    ) -> Self {
        self.anthropic.native_system_prefix = Some(prefix);
        self
    }

    /// Return the immutable host prefix snapshot captured for this session.
    /// Side-query producers may reuse its actual environment and host facts
    /// when they need the Native envelope on an explicitly opted-in request.
    #[must_use]
    pub fn native_system_prefix_snapshot(
        &self,
    ) -> Option<lingxi_llm_client::providers::anthropic::system_prompt::NativeSystemPrefix> {
        self.anthropic.native_system_prefix.clone()
    }

    /// `getAPIMetadata()` (`services/api/claude.ts:503-528`): build the Anthropic
    /// request `metadata.user_id` value, which claude-code packs as a JSON STRING
    /// `JSON.stringify({...extra, device_id, account_uuid, session_id})`.
    ///
    /// * `extra` = the `CLAUDE_CODE_EXTRA_METADATA` env var when it parses to a
    ///   JSON object (any other value is ignored, mirroring the TS
    ///   debug-log-and-skip — we have no debug log).
    /// * Key order is `extra…, device_id, account_uuid, session_id` (workspace
    ///   `serde_json` `preserve_order`), and a colliding `extra` key keeps its
    ///   first position but takes the canonical value — byte-identical to the JS
    ///   object spread.
    ///
    /// The composition root supplies `device_id` ([`migrations::global_config::
    /// get_or_create_user_id`]), `account_uuid` (the OAuth account UUID, or `""`
    /// — the TS `getOauthAccountInfo()?.accountUuid ?? ''`), and `session_id`
    /// (the main session id, claude-code's `getSessionId()`).
    #[must_use]
    pub fn build_api_metadata_user_id(
        device_id: &str,
        account_uuid: &str,
        session_id: &str,
        parent_session_id: Option<&str>,
    ) -> String {
        Self::build_api_metadata_user_id_with_extra(
            device_id,
            account_uuid,
            session_id,
            parent_session_id,
            extra_metadata_object(),
        )
    }

    /// Compose an identity from an explicit host snapshot without reading env.
    #[must_use]
    pub fn build_api_metadata_user_id_with_extra(
        device_id: &str,
        account_uuid: &str,
        session_id: &str,
        parent_session_id: Option<&str>,
        extra: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> String {
        let mut obj = extra.unwrap_or_default();
        obj.insert(
            "device_id".to_string(),
            serde_json::Value::String(device_id.to_string()),
        );
        obj.insert(
            "account_uuid".to_string(),
            serde_json::Value::String(account_uuid.to_string()),
        );
        obj.insert(
            "session_id".to_string(),
            serde_json::Value::String(session_id.to_string()),
        );
        if let Some(parent_session_id) = parent_session_id.filter(|id| !id.is_empty()) {
            // The conditional spread is deliberately last: metadata.user_id is
            // a JSON string and key order is externally observable.
            obj.insert(
                "parent_session_id".to_string(),
                serde_json::Value::String(parent_session_id.to_string()),
            );
        }
        serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or_default()
    }

    /// Inject 1P cache-editing inputs (`newCacheEdits` / `pinnedEdits`). Test-only
    /// today — no production producer (cached-microcompact scheduler) is wired, so
    /// the default is empty. Builder-style.
    #[cfg(test)]
    #[must_use]
    pub(in crate::service) fn with_cache_editing_inputs(
        mut self,
        inputs: CacheEditingInputs,
    ) -> Self {
        self.anthropic.cache_editing_inputs = inputs;
        self
    }

    pub(in crate::service) fn prompt_cache_ttl_inputs(
        &self,
    ) -> lingxi_llm_client::providers::anthropic::system_prompt::PromptCacheTtlInputs {
        use lingxi_llm_client::providers::anthropic::system_prompt::{
            PromptCacheSubscriberState, PromptCacheTtlInputs,
        };
        PromptCacheTtlInputs {
            subscriber: PromptCacheSubscriberState::Unknown,
            is_using_overage: false,
            subscriber_allowlist: Arc::new(native_prompt_cache_1h_allowlist),
        }
    }

    /// Materialize the Native `systemPrompt: string[]` snapshot shape through
    /// the SDK's selected-route gate. An unresolved route deliberately gets no
    /// marker; the source sections themselves are still grouped and retained.
    pub fn prompt_snapshot_source_vector(
        &self,
        model: &str,
        profile: Option<&str>,
        sections: &[lingxi_llm_client::providers::anthropic::system_prompt::SourceSection],
    ) -> Vec<lingxi_llm_client::providers::anthropic::system_prompt::PromptText> {
        use lingxi_llm_client::providers::anthropic::system_prompt as prompt_cache;

        let route_profile = self.client().prompt_cache_profile(model, profile);
        let policy = prompt_cache::CachePolicy::from_process(
            lingxi_core::host::compliance_taints::is_tainted("hipaa"),
            agent_prompt_cache_ttl_override(),
            false,
            PromptCacheQuerySource::Unspecified,
            prompt_cache::PromptCacheTtlSettings::default(),
            prompt_cache::PromptCacheTtlInputs::default(),
        );
        prompt_cache::snapshot_source_vector(sections, route_profile.as_ref(), policy.gate())
    }

    /// 1P experimental cache-EDITING gate — parity `useCachedMC`
    /// (`services/api/claude.ts:3067`, passed down from the caller at
    /// claude.ts:1531-1709, where it additionally requires
    /// `getAPIProvider()==='firstParty' && querySource==='repl_main_thread'`).
    ///
    /// LingXi resolves the concrete provider downstream of this provider-agnostic
    /// request builder and still lacks the rest of the Claude Code protocol:
    /// the once-per-session `CACHE_EDITING_BETA_HEADER` latch and the cross-call
    /// pinned-edits store. Because this partial path can mutate requests without
    /// the required session/header contract, it is kept FAIL-CLOSED here even
    /// when `LINGXI_CACHE_EDITING=1`. Default: off → no `cache_edits` /
    /// `cache_reference` ever emitted, so 3P traffic is byte-unchanged.
    pub(in crate::service) fn should_use_cache_editing(&self) -> bool {
        false
    }

    pub(in crate::service) fn native_media_limits_for_request(
        &self,
        model: &str,
        profile: Option<&str>,
        protocol: crate::ProtocolFamily,
    ) -> Option<NativeMediaLimits> {
        let route = self.client().resolve_media_route(model, profile).ok()?.main;
        if !is_native_main_anthropic_route(&route.provider_id, protocol) {
            return None;
        }
        let constrained_endpoint = match route.provider_id {
            crate::ProviderId::AnthropicFirstParty => self
                .client()
                .prompt_cache_profile(model, Some(&route.profile_name))
                .is_some_and(|profile| {
                    native_tbc_first_party_tf(Some(std::ffi::OsStr::new(&profile.base_url)))
                }),
            crate::ProviderId::BedrockClaude => {
                std::env::var_os("ANTHROPIC_AWS_BASE_URL").is_none()
            }
            _ => false,
        };
        let context_1m_disabled = std::env::var_os("CLAUDE_CODE_DISABLE_1M_CONTEXT")
            .is_some_and(|value| !value.is_empty());
        let context_1m_beta = self
            .anthropic
            .custom_cli_betas
            .iter()
            .any(|beta| beta == crate::model::betas::CONTEXT_1M);
        let native_model = if model.to_ascii_lowercase().contains("[1m]") {
            model
        } else {
            &route.request_model
        };
        Some(NativeMediaLimits::for_model(
            native_model,
            context_1m_disabled,
            context_1m_beta,
            constrained_endpoint,
        ))
    }
}
