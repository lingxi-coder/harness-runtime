//! Measured current-context occupancy, ported from Claude Code 2.1.319
//! `C7e`/`Ff`/`Nre`. Usage belongs to an assistant identity in the supplied
//! model context; session billing and a removed response cannot become anchors.

use super::ConversationOrchestrator;
use lingxi_core::host::NativeRequestHistoryMetadata;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection as Source};
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use std::collections::HashMap;

fn token_count(value: f64) -> u64 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    if value.is_finite() && value > 0.0 {
        value.min(u64::MAX as f64) as u64
    } else {
        0
    }
}

fn normalized_usage(metadata: &Source) -> Option<Source> {
    let usage = metadata.subprojection("/usage").ok()?;
    (crate::resume::native_usage_input_tokens_from_projection(&usage)? > 0.0).then_some(usage)
}

fn legacy_provider_context_usage(usage: Source) -> Source {
    if [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ]
    .iter()
    .any(|field| usage.contains_property(field))
    {
        return usage;
    }
    // Older JSONL rows retained providers' original names without the SDK's
    // disjoint buckets. For context occupancy only, their full prompt total
    // already contains cache hits; never add that cached subset again.
    let counts = if let Some(input) = source_nonnegative(&usage, "/prompt_tokens") {
        Some((
            input,
            source_nonnegative(&usage, "/completion_tokens").unwrap_or(0.0),
        ))
    } else {
        source_nonnegative(&usage, "/promptTokenCount").map(|input| {
            (
                input,
                source_nonnegative(&usage, "/candidatesTokenCount").unwrap_or(0.0)
                    + source_nonnegative(&usage, "/thoughtsTokenCount").unwrap_or(0.0),
            )
        })
    };
    let Some((input, output)) = counts else {
        return usage;
    };
    let mut usage = usage;
    for (field, value) in [
        ("input_tokens", token_count(input)),
        ("output_tokens", token_count(output)),
    ] {
        usage
            .set_field(
                field,
                Source::from_serde_value(&serde_json::json!(value)).expect("token count"),
            )
            .expect("provider usage is an object");
    }
    usage
}

fn provider_id<'a>(
    message: &ConversationMessage,
    metadata: &'a NativeRequestHistoryMetadata,
) -> Option<&'a str> {
    matches!(message, ConversationMessage::Assistant { .. })
        .then(|| metadata.assistant_usage.get(&message.id()))
        .flatten()
        .and_then(|source| source.string_at("/providerMessageId"))
}

fn usage_anchor(
    history: &[ConversationMessage],
    metadata: &NativeRequestHistoryMetadata,
) -> Option<(usize, u64)> {
    for (index, message) in history.iter().enumerate().rev() {
        if compaction::is_compact_boundary(message) {
            return Some((index, 0));
        }
        if !matches!(message, ConversationMessage::Assistant { .. }) {
            continue;
        }
        let Some(usage) = metadata
            .assistant_usage
            .get(&message.id())
            .and_then(normalized_usage)
        else {
            continue;
        };
        let tokens = token_count(crate::resume::native_usage_context_tokens_from_projection(
            &usage,
        )?);
        let mut anchor = index;
        if let Some(id) = provider_id(message, metadata) {
            for previous in (0..index).rev() {
                if let Some(previous_id) = provider_id(&history[previous], metadata) {
                    if previous_id == id {
                        anchor = previous;
                    } else {
                        break;
                    }
                }
            }
        }
        return Some((anchor, tokens));
    }
    None
}

/// Current retained context (`Ff`), including measured system/tools and output
/// plus a rough estimate of rows appended since that response.
pub(crate) fn has_measured_usage(
    history: &[ConversationMessage],
    metadata: &NativeRequestHistoryMetadata,
) -> bool {
    usage_anchor(history, metadata).is_some_and(|(_, tokens)| tokens > 0)
}

fn measured_route_matches(
    history: &[ConversationMessage],
    metadata: &NativeRequestHistoryMetadata,
    model: &str,
    profile: Option<&str>,
) -> bool {
    // The route must belong to the positive usage anchor. A later zero-usage
    // placeholder on another model must not relabel an older model's count.
    for message in history.iter().rev() {
        if compaction::is_compact_boundary(message) {
            return false;
        }
        if !matches!(message, ConversationMessage::Assistant { .. }) {
            continue;
        }
        let Some(source) = metadata.assistant_usage.get(&message.id()) else {
            continue;
        };
        if normalized_usage(source).is_some() {
            return source.string_at("/requestedModel") == Some(model)
                && source.string_at("/requestedProfile") == profile;
        }
    }
    false
}

/// Current retained context, falling back to message estimates without usage.
pub(crate) fn live_context_tokens(
    history: &[ConversationMessage],
    metadata: &NativeRequestHistoryMetadata,
) -> u64 {
    match usage_anchor(history, metadata) {
        Some((index, tokens)) => tokens.saturating_add(
            compaction::grouping::estimate_tokens_for_range(&history[index + 1..]),
        ),
        None => compaction::grouping::estimate_tokens_for_range(history),
    }
}

fn source_nonnegative(source: &Source, path: &str) -> Option<f64> {
    source
        .at_pointer(path)
        .and_then(JsonValueView::as_number)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn cleared_input_tokens(context: &Source) -> u64 {
    let Some(count) = context.array_len("/applied_edits") else {
        return 0;
    };
    let mut cleared = 0.0;
    for index in 0..count {
        let path = format!("/applied_edits/{index}");
        if context.string_at(&format!("{path}/type")) == Some("clear_tool_uses_20250919") {
            cleared +=
                source_nonnegative(context, &format!("{path}/cleared_input_tokens")).unwrap_or(0.0);
        }
    }
    let Some(client_estimate) =
        source_nonnegative(context, "/client_estimated_cleared_input_tokens")
    else {
        return token_count(cleared);
    };
    let measured = context
        .at_pointer("/client_estimate_measured_from_prompts")
        .and_then(JsonValueView::as_bool)
        == Some(true)
        && cleared > 0.0;
    token_count(if measured {
        client_estimate
    } else {
        cleared.min(client_estimate)
    })
}

fn followed_by_interruption(
    history: &[ConversationMessage],
    index: usize,
    metadata: &NativeRequestHistoryMetadata,
) -> bool {
    let ConversationMessage::Assistant { stop_reason, .. } = &history[index] else {
        return false;
    };
    if !matches!(stop_reason.as_deref(), Some("tool_use" | "end_turn")) {
        return false;
    }
    let id = provider_id(&history[index], metadata);
    for next in &history[index + 1..] {
        match next {
            ConversationMessage::User {
                is_meta, content, ..
            } => {
                if !is_meta {
                    return false;
                }
                let text = content.iter().find_map(|block| match block {
                    ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => {
                        Some(text.as_str())
                    }
                    _ => None,
                });
                return matches!(text,
                    Some("Your response above was cut off mid-stream. Resume directly from where it stops — no apology, no recap. If none of it survived, answer the request from the start." |
                         "Your response above was cut off mid-stream and only your next message is delivered. Write the complete response again from the start — no apology, no mention of the cut-off.")
                );
            }
            ConversationMessage::Assistant { .. } => {
                if let Some(source) = metadata.assistant_usage.get(&next.id()) {
                    if source
                        .at_pointer("/isApiErrorMessage")
                        .and_then(JsonValueView::as_bool)
                        == Some(true)
                    {
                        return source.string_at("/error") == Some("server_error");
                    }
                }
                if provider_id(next, metadata) != id {
                    return false;
                }
            }
            ConversationMessage::System { .. } => {}
        }
    }
    false
}

fn server_cleared_tokens(
    history: &[ConversationMessage],
    metadata: &NativeRequestHistoryMetadata,
) -> u64 {
    for (index, message) in history.iter().enumerate().rev() {
        if compaction::is_compact_boundary(message) {
            return 0;
        }
        if !matches!(message, ConversationMessage::Assistant { .. }) {
            continue;
        }
        let Some(source) = metadata.assistant_usage.get(&message.id()) else {
            continue;
        };
        if normalized_usage(source).is_none() {
            continue;
        }
        let context = source
            .subprojection("/contextManagement")
            .ok()
            .filter(|context| !context.at_pointer("").is_some_and(JsonValueView::is_null));
        if context.is_none()
            && (source
                .at_pointer("/stopReasonIsNull")
                .and_then(JsonValueView::as_bool)
                == Some(true)
                || followed_by_interruption(history, index, metadata))
        {
            continue;
        }
        return context.as_ref().map_or(0, cleared_input_tokens);
    }
    0
}

/// Native auto-compact admission count: retained context plus server-cleared
/// input tokens (`mxr`'s `Ff + Nre`).
#[cfg(test)]
pub(crate) fn context_tokens_for_compaction(
    history: &[ConversationMessage],
    metadata: &NativeRequestHistoryMetadata,
) -> u64 {
    context_token_counts_for_compaction(history, metadata).1
}

/// Retained and admission counts for one snapshot, sharing the retained-context
/// estimate while keeping server-cleared input exclusive to admission.
pub(crate) fn context_token_counts_for_compaction(
    history: &[ConversationMessage],
    metadata: &NativeRequestHistoryMetadata,
) -> (u64, u64) {
    let live = live_context_tokens(history, metadata);
    (
        live,
        live.saturating_add(server_cleared_tokens(history, metadata)),
    )
}

fn synthetic_first_text(content: &[ContentBlock]) -> bool {
    let Some(ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. }) =
        content.first()
    else {
        return false;
    };
    matches!(text.as_str(),
        "[Request interrupted by user]" | "[Request interrupted by user for tool use]" |
        "No response requested." |
        "The user doesn't want to take this action right now. STOP what you are doing and wait for the user to tell you how to proceed." |
        "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed."
    )
}

pub(crate) fn metadata_from_message(message: &Source, outer: &Source) -> Option<Source> {
    let synthetic = message.string_at("/model") == Some("<synthetic>");
    let ignored_usage = synthetic
        || outer
            .at_pointer("/isUnmetered")
            .and_then(JsonValueView::as_bool)
            == Some(true)
        || (message.string_at("/content/0/type") == Some("text")
            && message.string_at("/content/0/text").is_some_and(|text| {
                synthetic_first_text(&[ContentBlock::Text {
                    text: text.into(),
                    citations: None,
                }])
            }));
    let mut metadata = Source::parse("{}").expect("empty object");
    if let Some(id) = message.string_at("/id").filter(|_| !synthetic) {
        metadata.set_field("providerMessageId", id.into()).ok()?;
    }
    for (target, source_path) in [
        ("usage", "/usage"),
        ("contextManagement", "/context_management"),
    ] {
        if ignored_usage {
            continue;
        }
        if let Ok(value) = message.subprojection(source_path) {
            metadata
                .set_field(
                    target,
                    if target == "usage" {
                        legacy_provider_context_usage(value)
                    } else {
                        value
                    },
                )
                .ok()?;
        }
    }
    metadata
        .set_field(
            "stopReasonIsNull",
            Source::from_serde_value(&serde_json::json!(message
                .at_pointer("/stop_reason")
                .is_some_and(JsonValueView::is_null)))
            .ok()?,
        )
        .ok()?;
    for field in ["isApiErrorMessage", "error"] {
        if let Ok(value) = outer.subprojection(&format!("/{field}")) {
            metadata.set_field(field, value).ok()?;
        }
    }
    if let Ok(model) = outer.subprojection("/requestedModel") {
        metadata.set_field("requestedModel", model).ok()?;
    }
    if let Ok(profile) = outer
        .subprojection("/requestedModelProfile")
        .or_else(|_| outer.subprojection("/modelProfile"))
    {
        metadata.set_field("requestedProfile", profile).ok()?;
    }
    Some(metadata)
}

/// Latest metered requested route (`hxr`'s outer requestedModel), independent
/// of the model a provider happened to serve. Unknown provenance stays absent.
pub(crate) fn latest_requested_model<'a>(
    history: &[ConversationMessage],
    metadata: &'a NativeRequestHistoryMetadata,
) -> Option<(&'a str, Option<&'a str>)> {
    let (_, tokens) = usage_anchor(history, metadata)?;
    if tokens == 0 {
        return None;
    }
    for message in history.iter().rev() {
        if compaction::is_compact_boundary(message) {
            return None;
        }
        let Some(source) = metadata.assistant_usage.get(&message.id()) else {
            continue;
        };
        if source.subprojection("/usage").is_err() {
            continue;
        }
        return Some((
            source.string_at("/requestedModel")?,
            source.string_at("/requestedProfile"),
        ));
    }
    None
}

pub(crate) fn execution_context_management(usage: &llm_runtime::ExecutionUsage) -> Option<Source> {
    let source = Source::parse(usage.provider_metadata.as_json_source()).ok()?;
    for path in ["/context_management", "/stream/context_management"] {
        if let Ok(value) = source.subprojection(path) {
            return Some(value);
        }
    }
    for base in [
        "",
        "/stream",
        "/provider_metadata",
        "/provider_metadata/stream",
    ] {
        let events = format!("{base}/llm_client/provider_events");
        for index in (0..source.array_len(&events).unwrap_or(0)).rev() {
            for suffix in ["/context_management", "/message/context_management"] {
                if let Ok(value) =
                    source.subprojection(&format!("{events}/{index}/payload{suffix}"))
                {
                    return Some(value);
                }
            }
        }
    }
    None
}

pub(crate) fn metadata_from_execution(
    assistant: &ConversationMessage,
    model: &str,
    usage: &llm_runtime::ExecutionUsage,
) -> Option<Source> {
    let ConversationMessage::Assistant {
        id,
        content,
        stop_reason,
        ..
    } = assistant
    else {
        return None;
    };
    if model == "<synthetic>" || synthetic_first_text(content) {
        return None;
    }
    let mut message = Source::parse("{}").expect("empty object");
    message
        .set_field("id", id.as_uuid().to_string().into())
        .ok()?;
    message
        .set_field(
            "usage",
            Source::parse(
                super::transcript_impl::persisted_assistant_usage(usage).as_json_source(),
            )
            .ok()?,
        )
        .ok()?;
    if stop_reason.is_none() {
        message
            .set_field("stop_reason", Source::parse("null").ok()?)
            .ok()?;
    }
    if let Some(context) = execution_context_management(usage) {
        message.set_field("context_management", context).ok()?;
    }
    metadata_from_message(&message, &Source::parse("{}").expect("empty object"))
}

pub(crate) fn usage_metadata_from_rows(
    rows: &[session::JsonlMessage],
) -> HashMap<MessageId, Source> {
    let mut metadata = HashMap::new();
    let mut previous_group: Option<(String, MessageId)> = None;
    for row in rows {
        if row.message_type != "assistant" || row.is_sidechain {
            previous_group = None;
            continue;
        }
        let key = row.message.string_at("/id").unwrap_or(&row.uuid).to_owned();
        let id = previous_group
            .as_ref()
            .filter(|(previous, _)| *previous == key)
            .map(|(_, id)| *id)
            .or_else(|| {
                uuid::Uuid::parse_str(&key)
                    .ok()
                    .or_else(|| uuid::Uuid::parse_str(&row.uuid).ok())
                    .map(MessageId::from_uuid)
            });
        let Some(id) = id else {
            continue;
        };
        previous_group = Some((key, id));
        let Some(source) = metadata_from_message(&row.message, &row.extra) else {
            continue;
        };
        // Consecutive per-block rows merge into one live assistant on cold
        // replay. A terminal zero snapshot must not erase earlier real usage.
        if normalized_usage(&source).is_some() || !metadata.contains_key(&id) {
            metadata.insert(id, source);
        }
    }
    metadata
}

impl ConversationOrchestrator {
    pub(crate) async fn main_context_usage_estimator(
        &self,
    ) -> llm_runtime::context_usage::ContextUsageEstimator {
        let metadata = NativeRequestHistoryMetadata {
            assistant_usage: self
                .session
                .lock()
                .await
                .native_request_history
                .assistant_usage
                .clone(),
            ..Default::default()
        };
        llm_runtime::context_usage::ContextUsageEstimator::new(move |model, profile, history| {
            measured_route_matches(history, &metadata, model, profile)
                .then(|| live_context_tokens(history, &metadata))
        })
    }

    /// Each API round captures fresh usage. Concurrent chats and side queries
    /// cannot borrow another conversation's measured prefix.
    pub(crate) async fn scope_main_request<F: std::future::Future>(&self, future: F) -> F::Output {
        self.main_context_usage_estimator()
            .await
            .scope(self.model_runtime.prompt_cache_capture.scope(future))
            .await
    }

    #[cfg(test)]
    pub(crate) async fn current_context_usage_for(&self, history: &[ConversationMessage]) -> u64 {
        context_tokens_for_compaction(history, &self.session.lock().await.native_request_history)
    }

    pub(crate) async fn current_live_context_usage_for(
        &self,
        history: &[ConversationMessage],
    ) -> u64 {
        live_context_tokens(history, &self.session.lock().await.native_request_history)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn assistant(id: MessageId, text: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            id,
            content: vec![ContentBlock::Text {
                text: text.into(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            per_turn_effort: None,
            resumable: None,
        }
    }

    fn test_orchestrator() -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(crate::test_support::MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            crate::test_support::noop_hook_executor(),
            Arc::new(crate::test_support::NoOpPermissionGate),
            Arc::new(crate::test_support::MockOutputStream::new()),
            Arc::new(crate::test_support::StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
    }

    #[tokio::test]
    async fn main_request_uses_provider_usage_plus_only_new_content_on_the_same_route() {
        let orch = test_orchestrator();
        let mut opaque = assistant(MessageId::new(), "old");
        if let ConversationMessage::Assistant { content, .. } = &mut opaque {
            *content = vec![ContentBlock::ProviderContent {
                protocol: "open_ai_responses".into(),
                value: Source::from_serde_value(&serde_json::json!({
                    "type":"reasoning", "encrypted_content":"x".repeat(900_000),
                }))
                .unwrap(),
            }];
        }
        let measured = assistant(MessageId::new(), "last answer");
        let history = vec![
            opaque,
            measured.clone(),
            ConversationMessage::user(MessageId::new(), "tailtail".into()),
        ];
        {
            let mut state = orch.session.lock().await;
            state.history = history.clone();
            state.native_request_history.assistant_usage.insert(measured.id(), Source::parse(
                r#"{"providerMessageId":"last-response","requestedModel":"requested","requestedProfile":"openai","usage":{"input_tokens":3,"cache_creation_input_tokens":106617,"cache_read_input_tokens":20709,"output_tokens":638}}"#,
            ).unwrap());
        }
        let estimator = orch.main_context_usage_estimator().await;
        assert_eq!(
            estimator.estimate("requested", Some("openai"), &history),
            Some(127_969)
        );
        assert_eq!(orch.current_live_context_usage_for(&history).await, 127_969);
        assert_eq!(estimator.estimate("other", Some("openai"), &history), None);
        assert_eq!(
            estimator.estimate("requested", Some("other"), &history),
            None
        );
        assert_eq!(estimator.estimate("requested", None, &history), None);
        assert_eq!(
            estimator.estimate("requested", Some("openai"), &history[2..]),
            None,
            "compacted or rewritten history cannot retain an absent usage anchor"
        );

        let mut grown = history;
        grown.push(ConversationMessage::user(
            MessageId::new(),
            "x".repeat(600_000),
        ));
        assert_eq!(
            estimator.estimate("requested", Some("openai"), &grown),
            Some(277_969)
        );
        let placeholder = assistant(MessageId::new(), "pending");
        orch.session.lock().await.native_request_history.assistant_usage.insert(
            placeholder.id(),
            Source::parse(r#"{"requestedModel":"other","requestedProfile":"other","usage":{"input_tokens":0,"output_tokens":0}}"#).unwrap(),
        );
        grown.push(placeholder);
        assert_eq!(
            orch.main_context_usage_estimator().await.estimate("other", Some("other"), &grown),
            None,
            "a zero-usage model switch cannot borrow the preceding model's count"
        );
        orch.session
            .lock()
            .await
            .native_request_history
            .assistant_usage
            .clear();
        assert_eq!(
            orch.main_context_usage_estimator()
                .await
                .estimate("requested", Some("openai"), &grown),
            None,
            "the next API round captures fresh usage"
        );
    }

    #[test]
    fn usage_anchors_and_server_clears_match_official_319_oracle() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../compaction/tests/fixtures/claude_2_1_319_usage.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let mut history = Vec::new();
            let mut metadata = NativeRequestHistoryMetadata::default();
            for row in case["rows"].as_array().unwrap() {
                let id = MessageId::new();
                let source = Source::from_serde_value(&row["message"]).unwrap();
                let text = source
                    .string_at("/content")
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        (0..source.array_len("/content").unwrap_or(0))
                            .filter_map(|index| source.string_at(&format!("/content/{index}/text")))
                            .collect::<Vec<_>>()
                            .join("")
                    });
                let message = match row["type"].as_str().unwrap() {
                    "user" => ConversationMessage::user(id, text),
                    "assistant" => {
                        if let Some(value) =
                            metadata_from_message(&source, &Source::from_serde_value(row).unwrap())
                        {
                            metadata.assistant_usage.insert(id, value);
                        }
                        assistant(id, &text)
                    }
                    "system" => {
                        compaction::create_compact_boundary(
                            compaction::CompactTrigger::Auto,
                            0,
                            None,
                            None,
                            None,
                            &[],
                        )
                        .0
                    }
                    other => panic!("unsupported oracle row: {other}"),
                };
                history.push(message);
            }
            let expected = &case["expected"];
            let anchor = usage_anchor(&history, &metadata);
            let expected_anchor = expected["anchor"].is_object().then(|| {
                (
                    expected["anchor"]["anchorIndex"].as_u64().unwrap() as usize,
                    expected["anchor"]["tokens"].as_u64().unwrap(),
                )
            });
            assert_eq!(anchor, expected_anchor, "{} anchor", case["name"]);
            let (live, admission) = context_token_counts_for_compaction(&history, &metadata);
            assert_eq!(
                live,
                expected["liveTokens"].as_u64().unwrap(),
                "{} retained pair",
                case["name"]
            );
            assert_eq!(
                admission,
                expected["triggerTokens"].as_u64().unwrap(),
                "{} admission pair",
                case["name"]
            );
            assert_eq!(
                live_context_tokens(&history, &metadata),
                expected["liveTokens"].as_u64().unwrap(),
                "{} retained",
                case["name"]
            );
            assert_eq!(
                server_cleared_tokens(&history, &metadata),
                expected["serverClearedTokens"].as_u64().unwrap(),
                "{} cleared",
                case["name"]
            );
            assert_eq!(
                context_tokens_for_compaction(&history, &metadata),
                expected["triggerTokens"].as_u64().unwrap(),
                "{} trigger",
                case["name"]
            );
        }
    }

    #[test]
    fn shared_compaction_counts_saturate_server_cleared_input() {
        let id = MessageId::new();
        let history = vec![assistant(id, "tiny")];
        let mut metadata = NativeRequestHistoryMetadata::default();
        metadata.assistant_usage.insert(
            id,
            Source::parse(
                r#"{"usage":{"input_tokens":18446744073709551615,"output_tokens":0},"contextManagement":{"applied_edits":[{"type":"clear_tool_uses_20250919","cleared_input_tokens":500}]}}"#,
            )
            .unwrap(),
        );
        assert_eq!(server_cleared_tokens(&history, &metadata), 500);
        assert_eq!(
            context_token_counts_for_compaction(&history, &metadata),
            (u64::MAX, u64::MAX)
        );
    }

    #[tokio::test]
    async fn accepted_provider_usage_includes_prefix_reasoning_and_tail_and_expires_with_identity()
    {
        let orch = test_orchestrator();
        let measured = assistant(MessageId::new(), "tiny");
        orch.session.lock().await.history.push(measured.clone());
        let mut usage =
            llm_runtime::ExecutionUsage::from_counts(lingxi_llm_client::protocol::Usage {
                input_tokens: 160_000,
                cache_read_tokens: 40_000,
                output_tokens: 30_000,
                reasoning_tokens: 10_000,
                ..Default::default()
            });
        usage.provider_metadata = llm_runtime::services::sdk::protocol::JsonPayload::parse(
            r#"{"prompt_tokens":200000,"completion_tokens":30000}"#,
        )
        .unwrap();
        orch.record_main_loop_assistant_usage(&measured, "provider-model", Some(&usage))
            .await;
        orch.session
            .lock()
            .await
            .history
            .push(ConversationMessage::user(
                MessageId::new(),
                "tailtail".into(),
            ));
        let history = orch.session.lock().await.history.clone();
        assert_eq!(orch.current_context_usage_for(&history).await, 230_002);
        let empty = assistant(MessageId::new(), "zero");
        orch.session.lock().await.history.push(empty.clone());
        let output_only =
            llm_runtime::ExecutionUsage::from_counts(lingxi_llm_client::protocol::Usage {
                output_tokens: 999,
                ..Default::default()
            });
        orch.record_main_loop_assistant_usage(&empty, "provider-model", Some(&output_only))
            .await;
        let history = orch.session.lock().await.history.clone();
        assert_eq!(
            orch.current_context_usage_for(&history).await,
            230_003,
            "zero-input usage is not an anchor"
        );
        let retained = history[1..].to_vec();
        assert_eq!(
            orch.current_context_usage_for(&retained).await,
            3,
            "removed assistant usage cannot anchor another history"
        );
    }

    #[test]
    fn cold_resume_reconstructs_usage_anchor_and_hot_snapshot_carries_it() {
        let session = uuid::Uuid::new_v4();
        let id = uuid::Uuid::new_v4();
        let next = uuid::Uuid::new_v4();
        let source = format!(
            r#"{{"type":"assistant","uuid":"{id}","parentUuid":null,"isSidechain":false,"timestamp":"2026-11-09T00:00:00Z","cwd":"/fixture","version":"test","sessionId":"{session}","message":{{"id":"{id}","model":"provider-model","role":"assistant","content":[{{"type":"text","text":"tiny"}}],"stop_reason":"end_turn","usage":{{"input_tokens":170000,"cache_read_input_tokens":10000,"output_tokens":2000}},"context_management":{{"applied_edits":[{{"type":"clear_tool_uses_20250919","cleared_input_tokens":15000}}]}}}}}}
{{"type":"user","uuid":"{next}","parentUuid":"{id}","isSidechain":false,"timestamp":"2026-11-09T00:00:01Z","cwd":"/fixture","version":"test","sessionId":"{session}","message":{{"role":"user","content":"tailtail"}}}}"#
        );
        let loaded = session::jsonl::reader::route_lines(&source);
        let state = crate::resume::state_from_messages(session, &loaded.messages_in_order);
        assert_eq!(
            live_context_tokens(&state.history, &state.native_request_history),
            182_002
        );
        assert_eq!(
            context_tokens_for_compaction(&state.history, &state.native_request_history),
            197_002
        );
        let snapshot = state.clone();
        assert_eq!(
            live_context_tokens(&snapshot.history, &snapshot.native_request_history),
            182_002
        );
    }

    #[tokio::test]
    async fn requested_compact_model_uses_host_request_instead_of_served_model() {
        let orch = test_orchestrator();
        let message = assistant(MessageId::new(), "answer");
        orch.session.lock().await.history.push(message.clone());
        orch.record_main_loop_assistant_usage_for_request(
            &message,
            "served-small",
            "requested-large",
            Some("same-provider"),
            Some(&llm_runtime::ExecutionUsage::from_counts(
                lingxi_llm_client::protocol::Usage {
                    input_tokens: 200_000,
                    output_tokens: 100,
                    ..Default::default()
                },
            )),
        )
        .await;
        let state = orch.session.lock().await;
        assert_eq!(
            latest_requested_model(&state.history, &state.native_request_history),
            Some(("requested-large", Some("same-provider")))
        );
    }

    #[test]
    fn latest_zero_usage_shadows_the_older_requested_model() {
        let first = assistant(MessageId::new(), "valid");
        let last = assistant(MessageId::new(), "zero");
        let mut metadata = NativeRequestHistoryMetadata::default();
        metadata.assistant_usage.insert(
            first.id(),
            Source::parse(
                r#"{"usage":{"input_tokens":200000,"output_tokens":1},"requestedModel":"large"}"#,
            )
            .unwrap(),
        );
        metadata.assistant_usage.insert(
            last.id(),
            Source::parse(
                r#"{"usage":{"input_tokens":0,"output_tokens":0},"requestedModel":"small"}"#,
            )
            .unwrap(),
        );
        let history = [first, last.clone()];
        assert_eq!(
            latest_requested_model(&history, &metadata),
            Some(("small", None))
        );
        metadata
            .assistant_usage
            .insert(last.id(), Source::parse(r#"{"usage":null}"#).unwrap());
        assert_eq!(
            latest_requested_model(&history, &metadata),
            None,
            "hxr does not skip a latest metered row with missing requestedModel"
        );
        assert_eq!(
            latest_requested_model(&history[1..], &metadata),
            None,
            "a real positive input anchor is independently required"
        );
    }

    #[tokio::test]
    async fn cold_resume_restores_outer_requested_model_and_profile() {
        let session = uuid::Uuid::new_v4();
        let id = uuid::Uuid::new_v4();
        let raw = serde_json::to_string(&serde_json::json!({
            "type":"assistant", "uuid":id.to_string(), "parentUuid":null,
            "isSidechain":false, "timestamp":"2026-11-09T00:00:00Z", "cwd":"/fixture",
            "version":"test", "sessionId":session.to_string(),
            "requestedModel":"requested-large", "requestedModelProfile":"same-provider",
            "message":{"id":id.to_string(), "role":"assistant", "model":"served-small",
                "usage":{"input_tokens":200000,"output_tokens":100}, "stop_reason":"end_turn",
                "content":[{"type":"text","text":"answer"}]}
        }))
        .unwrap();
        let loaded = session::jsonl::reader::route_lines(&raw);
        let state = crate::resume::state_from_messages(session, &loaded.messages_in_order);
        assert_eq!(
            latest_requested_model(&state.history, &state.native_request_history),
            Some(("requested-large", Some("same-provider")))
        );
        let orch = test_orchestrator();
        orch.session.lock().await.native_request_history = state.native_request_history;
        assert_eq!(
            orch.main_context_usage_estimator().await.estimate(
                "requested-large",
                Some("same-provider"),
                &state.history,
            ),
            Some(200_100)
        );
    }

    #[test]
    fn cold_preserved_tail_does_not_resurrect_original_usage() {
        let session = uuid::Uuid::new_v4();
        let ids = (0..5)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect::<Vec<_>>();
        let row = |kind: &str, index: usize, parent: Option<&str>, message: serde_json::Value| {
            serde_json::json!({
                "type":kind, "uuid":ids[index], "parentUuid":parent,
                "isSidechain":false, "timestamp":format!("2026-11-09T00:00:0{index}Z"),
                "cwd":"/fixture", "version":"test", "sessionId":session.to_string(), "message":message,
            })
        };
        let old = row(
            "user",
            0,
            None,
            serde_json::json!({"role":"user","content":"old request"}),
        );
        let kept = row(
            "assistant",
            1,
            Some(&ids[0]),
            serde_json::json!({
                "role":"assistant","id":ids[1],"model":"provider-model", "content":[{"type":"text","text":"kept"}],
            "stop_reason":"end_turn", "usage":{"input_tokens":190000,"output_tokens":100,
                "iterations":[{"type":"message","input_tokens":190000,"output_tokens":100,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}]}
            }),
        );
        let mut boundary = row("system", 2, None, serde_json::Value::Null);
        boundary["subtype"] = "compact_boundary".into();
        boundary["compactMetadata"] = serde_json::json!({
            "trigger":"manual", "preTokens":190100,
            "preservedSegment":{"headUuid":ids[1],"tailUuid":ids[1],"anchorUuid":ids[3]},
            "preservedMessages":{"anchorUuid":ids[3],"uuids":[ids[1]],"allUuids":[ids[1]]}
        });
        let mut summary = row(
            "user",
            3,
            Some(&ids[2]),
            serde_json::json!({"role":"user","content":"summary"}),
        );
        summary["isCompactSummary"] = true.into();
        let after = row(
            "user",
            4,
            Some(&ids[1]),
            serde_json::json!({"role":"user","content":"tail"}),
        );
        let source = [old, kept, boundary, summary, after]
            .iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let loaded = session::jsonl::reader::route_lines(&source);
        let (chain, _) = session::jsonl::build_conversation_chain(&loaded, &session.to_string());
        let preserved = chain
            .iter()
            .find(|row| row.uuid == ids[1])
            .expect("preserved assistant");
        assert_eq!(
            preserved
                .message
                .at_pointer("/usage/input_tokens")
                .and_then(JsonValueView::as_number),
            Some(0.0)
        );
        let state = crate::resume::state_from_messages(session, &chain);
        assert!(state
            .history
            .iter()
            .any(|row| compaction::is_compact_boundary(row)));
        assert!(context_tokens_for_compaction(&state.history, &state.native_request_history) < 100);
    }

    #[test]
    fn legacy_provider_cold_resume_counts_cache_and_reasoning_once() {
        for (usage, expected) in [
            (
                serde_json::json!({"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":30}}),
                150,
            ),
            (
                serde_json::json!({"promptTokenCount":100,"candidatesTokenCount":10,"thoughtsTokenCount":5,"cachedContentTokenCount":40}),
                115,
            ),
        ] {
            let session = uuid::Uuid::new_v4();
            let raw = serde_json::to_string(&serde_json::json!({
                "type":"assistant", "uuid":uuid::Uuid::new_v4().to_string(), "parentUuid":null,
                "isSidechain":false, "timestamp":"2026-11-09T00:00:00Z", "cwd":"/fixture",
                "version":"test", "sessionId":session.to_string(),
                "message":{
                    "id":"provider-response", "role":"assistant", "model":"provider-model", "usage":usage,
                    "stop_reason":"end_turn", "content":[{"type":"text","text":"tiny"}]
                }
            })).unwrap();
            let loaded = session::jsonl::reader::route_lines(&raw);
            let state = crate::resume::state_from_messages(session, &loaded.messages_in_order);
            assert_eq!(
                live_context_tokens(&state.history, &state.native_request_history),
                expected
            );
        }
    }

    #[tokio::test]
    async fn successful_compact_retires_usage_on_preserved_assistants() {
        let orch =
            crate::test_support::with_scripted_compactor(test_orchestrator(), "retained summary");
        let first = assistant(MessageId::new(), "first answer");
        let last = assistant(MessageId::new(), "last answer");
        orch.session.lock().await.history = vec![
            ConversationMessage::user(MessageId::new(), "first request".into()),
            first,
            ConversationMessage::user(MessageId::new(), "last request".into()),
            last.clone(),
        ];
        orch.record_main_loop_assistant_usage(
            &last,
            "provider-model",
            Some(&llm_runtime::ExecutionUsage::from_counts(
                lingxi_llm_client::protocol::Usage {
                    input_tokens: 190_000,
                    output_tokens: 100,
                    ..Default::default()
                },
            )),
        )
        .await;
        orch.force_compact_with_cancel(tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        let session = orch.session.lock().await;
        assert!(
            session.history.iter().any(|row| row.id() == last.id()),
            "manual compaction preserves the recent round"
        );
        assert!(
            session.native_request_history.assistant_usage.is_empty(),
            "preserved rows carry zero usage after compact"
        );
        assert!(
            context_tokens_for_compaction(&session.history, &session.native_request_history)
                < 190_000
        );
    }
}
