//! Transcript serialization and tool/hook persistence side channels.

use super::*;
use hooks::attachment::HookPublicationGuard;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use lingxi_core::types::ContentBlock;
use llm_runtime::services::sdk::protocol::JsonPayload;

fn erase_tool_dispatch_fence(
    fence: crate::autonomous_tool_scheduler::ToolDispatchPublicationFence,
) -> Arc<dyn HookPublicationGuard> {
    Arc::new(fence)
}

// JSONL decodes valid UTF-16 strings as ordinary Rust text. App Chat's live
// canonical rows must use that same representation; unpaired units still need
// the exact JS carrier. Ordinary Native Chat/Code retains its source carrier.
fn canonicalize_app_chat_assistant_text(message: &mut ConversationMessage) {
    let ConversationMessage::Assistant { content, .. } = message else {
        return;
    };
    for block in content {
        if let ContentBlock::TextJsUtf16 {
            utf16_code_units,
            citations,
            ..
        } = block
        {
            if let Ok(text) = String::from_utf16(utf16_code_units) {
                *block = ContentBlock::Text {
                    text,
                    citations: citations.take(),
                };
            }
        }
    }
}

#[derive(Clone, Copy)]
struct PreappendedRowMetadata<'a> {
    timestamp: &'a str,
    api_error: Option<&'a lingxi_core::host::ServerFallbackApiErrorRow>,
}

pub(crate) fn persisted_assistant_usage(usage: &llm_runtime::ExecutionUsage) -> JsonPayload {
    if usage.provider_metadata.is_root_object() {
        let provider_usage = with_native_usage_iterations(&usage.provider_metadata);
        let provider_usage = without_host_usage_metadata(&provider_usage);
        if provider_usage.object_len("").is_some_and(|len| len > 0) {
            return provider_usage;
        }
    }

    let buckets = usage.counts();
    JsonPayload::from_generated_value(&serde_json::json!({
        "input_tokens": buckets.input_tokens,
        "cache_creation_input_tokens": buckets.cache_write_tokens,
        "cache_read_input_tokens": buckets.cache_read_tokens,
        "output_tokens": buckets.output_tokens,
    }))
    .expect("generated usage counters serialize")
}

fn with_native_usage_iterations(usage: &JsonPayload) -> JsonPayload {
    if usage.array_len("/iterations").is_some() {
        return usage.clone();
    }
    let Some(iterations) = crate::resume::native_usage_iterations_payload(usage) else {
        return usage.clone();
    };
    usage
        .with_object_field("iterations", &iterations)
        .unwrap_or_else(|_| usage.clone())
}

fn without_host_usage_metadata(usage: &JsonPayload) -> JsonPayload {
    let mut usage = usage.clone();
    if usage.is_root_object() {
        // Host annotations are excluded from the provider usage envelope.
        // Each remaining value retains its original JSON source.
        for field in ["stream", "llm_client", "upstreamUsageState"] {
            usage = usage
                .without_object_field(field)
                .expect("usage is an object");
        }
    }
    usage
}

#[test]
fn persisted_usage_removes_host_annotations_without_projecting_provider_source() {
    let usage = llm_runtime::ExecutionUsage {
        provider_metadata: JsonPayload::parse(
            r#"{"input_tokens":7,"future":{"n":1e400,"text":"\ud800"},"stream":{"local":true},"llm_client":{},"upstreamUsageState":"complete"}"#,
        ).unwrap(),
        ..Default::default()
    };
    assert_eq!(
        persisted_assistant_usage(&usage).as_json_source(),
        r#"{"input_tokens":7,"future":{"n":1e400,"text":"\ud800"}}"#,
    );
}

#[test]
fn assistant_row_identity_hash_keeps_source_units_and_ignores_object_key_order() {
    let id = lingxi_core::types::ToolUseId::from("toolu_identity");
    let block = |source: &str| lingxi_core::types::ContentBlock::ToolUse {
        id: id.clone(),
        name: "fixture".into(),
        input: Source::parse(source).unwrap(),
        provider_id: Some("toolu_identity".into()),
    };
    let original = block(r#"{"text":"\ud800","n":1}"#);
    let reordered = block(r#"{"n":1,"text":"\ud800"}"#);
    let different = block(r#"{"text":"\ud801","n":1}"#);
    let signature = ConversationOrchestrator::assistant_block_signature(&original);
    assert!(signature.contains(r#""text":"\ud800""#), "{signature}");
    assert_eq!(
        signature,
        ConversationOrchestrator::assistant_block_signature(&reordered)
    );
    assert_ne!(
        ConversationOrchestrator::assistant_block_derived_uuid("turn", 0, &original),
        ConversationOrchestrator::assistant_block_derived_uuid("turn", 0, &different),
    );
}

tokio::task_local! {
    static MOD_RESULT_STAGE: Arc<Mutex<ModResultStage>>;
}

/// Tool-result side channels are committed only after the `tool.call` Mod
/// middleware has settled. A hook may replace the result returned by `next()`;
/// selected-run metadata is preserved only when the final answer still names
/// that run by `ref`.
#[derive(Default)]
pub(crate) struct ModResultStage {
    tool_use_id: String,
    /// `$.tool.call()` consumes its executor frames locally. It still uses the
    /// permissioned dispatcher, but must not publish the virtual tool row or
    /// persist frame-only side channels into the outer conversation.
    virtual_tool_call: bool,
    pub(crate) tool_use_result: Option<Source>,
    frame: Option<PendingToolFrame>,
    mcp_meta: Option<Source>,
    turn_end: Option<tool_api::tool_trait::ToolResultTurnEnd>,
    denial_kind: Option<String>,
    permission_denial: Option<(String, Source)>,
}

impl ModResultStage {
    pub(crate) fn denial_kind(&self) -> Option<&str> {
        self.denial_kind.as_deref()
    }

    pub(crate) fn into_tool_result_publication(
        self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        replacement: Option<(Source, Source)>,
    ) -> crate::turn_loop::ToolResultPublication {
        if let Some((raw, model_text)) = replacement {
            return crate::turn_loop::ToolResultPublication {
                tool_use_id: id.clone(),
                tool_use_result: Some(raw.clone()),
                mcp_meta: self.mcp_meta,
                turn_end: self.turn_end,
                denial_kind: self.denial_kind,
                permission_denial: self.permission_denial,
                frame: Some(crate::turn_loop::ToolResultFramePublication {
                    tool: tool.to_owned(),
                    model_text,
                    result: raw,
                    denial_kind: None,
                }),
            };
        }
        crate::turn_loop::ToolResultPublication {
            tool_use_id: id.clone(),
            tool_use_result: self.tool_use_result,
            mcp_meta: self.mcp_meta,
            turn_end: self.turn_end,
            denial_kind: self.denial_kind,
            permission_denial: self.permission_denial,
            frame: self
                .frame
                .map(|frame| crate::turn_loop::ToolResultFramePublication {
                    tool: frame.tool,
                    model_text: frame.model_text,
                    result: frame.result,
                    denial_kind: frame.denial_kind,
                }),
        }
    }
}

pub(crate) async fn with_mod_result_stage<F: std::future::Future>(
    tool_use_id: &lingxi_core::types::ToolUseId,
    future: F,
) -> (F::Output, ModResultStage) {
    with_mod_result_stage_kind(tool_use_id, false, future).await
}

pub(crate) async fn with_virtual_mod_result_stage<F: std::future::Future>(
    tool_use_id: &lingxi_core::types::ToolUseId,
    future: F,
) -> (F::Output, ModResultStage) {
    with_mod_result_stage_kind(tool_use_id, true, future).await
}

async fn with_mod_result_stage_kind<F: std::future::Future>(
    tool_use_id: &lingxi_core::types::ToolUseId,
    virtual_tool_call: bool,
    future: F,
) -> (F::Output, ModResultStage) {
    let stage = Arc::new(Mutex::new(ModResultStage {
        tool_use_id: tool_use_id.to_string(),
        virtual_tool_call,
        ..Default::default()
    }));
    let result = MOD_RESULT_STAGE.scope(stage.clone(), future).await;
    let stage = Arc::try_unwrap(stage)
        .ok()
        .expect("mod result stage retained after dispatch")
        .into_inner();
    (result, stage)
}

pub(crate) async fn active_mod_result_stage_is_virtual() -> bool {
    let Some(stage) = active_mod_result_stage() else {
        return false;
    };
    let is_virtual = stage.lock().await.virtual_tool_call;
    is_virtual
}

fn active_mod_result_stage() -> Option<Arc<Mutex<ModResultStage>>> {
    MOD_RESULT_STAGE.try_with(Arc::clone).ok()
}

/// Metadata of the exact task whose scheduled fire is being recorded.
pub struct ScheduledLoopFire {
    pub fire_id: lingxi_core::types::MessageId,
    /// Identity assigned when the task was scheduled.
    pub task_id: String,
    /// Exact cron expression of the fired task.
    pub cron: String,
    /// Display prompt, with default loop sentinels already resolved.
    pub prompt: String,
    /// Whether the task has the upstream `kind: "loop"` discriminator.
    pub task_kind_loop: bool,
}

fn source_object<const N: usize>(fields: [(&str, Source); N]) -> Source {
    Source::object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
    .expect("generated field names form an object")
}

fn source_tool_error_payload(result: &Source, model_text: &Source) -> Option<Source> {
    let error_prefix = "Error: ".encode_utf16().collect::<Vec<_>>();
    let wrapper_prefix = "<tool_use_error>".encode_utf16().collect::<Vec<_>>();
    let wrapper_suffix = "</tool_use_error>".encode_utf16().collect::<Vec<_>>();
    let units = if let Some(units) = result.string_units("") {
        units
            .strip_prefix(error_prefix.as_slice())
            .unwrap_or(&units)
            .to_vec()
    } else {
        let units = model_text.string_units("")?;
        units
            .strip_prefix(wrapper_prefix.as_slice())
            .and_then(|inner| inner.strip_suffix(wrapper_suffix.as_slice()))
            .unwrap_or(&units)
            .to_vec()
    };
    let message = Source::root_string(String::from_utf16_lossy(&units), units).ok()?;
    Some(source_object([("error", message)]))
}

#[cfg(test)]
#[test]
fn source_tool_error_payload_unwraps_side_channel_without_losing_utf16() {
    let error_units = vec![
        b'i' as u16,
        b's' as u16,
        b's' as u16,
        b'u' as u16,
        b'e' as u16,
        0xd800,
    ];
    let mut result_units = "Error: ".encode_utf16().collect::<Vec<_>>();
    result_units.extend_from_slice(&error_units);
    let result = Source::root_string(String::from_utf16_lossy(&result_units), result_units)
        .expect("toolUseResult source is a string");

    let mut model_units = "<tool_use_error>".encode_utf16().collect::<Vec<_>>();
    model_units.extend_from_slice(&error_units);
    model_units.extend("</tool_use_error>".encode_utf16());
    let model_text = Source::root_string(String::from_utf16_lossy(&model_units), model_units)
        .expect("model-facing source is a string");

    let payload = source_tool_error_payload(&result, &model_text)
        .expect("the side-channel error is a string");
    assert_eq!(payload.string_units("/error"), Some(error_units));
}

fn source_array_items(source: &Source) -> Option<Vec<Source>> {
    (0..source.array_len("")?)
        .map(|index| source.subprojection(&format!("/{index}")).ok())
        .collect()
}

fn source_null() -> Source {
    Source::parse("null").expect("null is valid JSON")
}

fn mod_append_content_block(block: &lingxi_core::types::ContentBlock) -> Source {
    use lingxi_core::types::ContentBlock;
    match block {
        ContentBlock::Text { text, citations }
        | ContentBlock::TextJsUtf16 {
            text, citations, ..
        } => {
            let text = match block {
                ContentBlock::TextJsUtf16 {
                    utf16_code_units, ..
                } => Source::root_string(
                    String::from_utf16_lossy(utf16_code_units),
                    utf16_code_units.clone(),
                )
                .expect("text display derives from source units"),
                _ => text.into(),
            };
            let mut value = source_object([("type", "text".into()), ("text", text)]);
            if let Some(citations) = citations {
                value
                    .set_field("citations", citations.clone().unwrap_or_else(source_null))
                    .unwrap();
            }
            value
        }
        ContentBlock::ToolUse {
            id,
            name,
            input,
            provider_id,
        } => source_object([
            ("type", "tool_use".into()),
            (
                "id",
                provider_id.as_deref().unwrap_or_else(|| id.as_str()).into(),
            ),
            ("name", name.into()),
            ("input", input.clone()),
        ]),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            provider_tool_use_id,
            content_blocks,
        } => {
            let mut value = source_object([
                ("type", "tool_result".into()),
                (
                    "tool_use_id",
                    provider_tool_use_id
                        .as_deref()
                        .unwrap_or_else(|| tool_use_id.as_str())
                        .into(),
                ),
                (
                    "content",
                    content_blocks.as_ref().unwrap_or(content).clone(),
                ),
            ]);
            if let Some(is_error) = is_error {
                value
                    .set_field(
                        "is_error",
                        Source::parse(if *is_error { "true" } else { "false" }).unwrap(),
                    )
                    .unwrap();
            }
            value
        }
        ContentBlock::ProviderContent { value, .. } => value.clone(),
        other => Source::parse(&serde_json::to_string(other).expect("content block serializes"))
            .expect("serialized block is valid JSON"),
    }
}

fn mod_append_message(message: &ConversationMessage) -> Source {
    let (kind, name, role, is_meta, content) = match message {
        ConversationMessage::User {
            content, is_meta, ..
        } => (
            "user",
            None,
            Some("user"),
            *is_meta,
            content.iter().map(mod_append_content_block).collect(),
        ),
        ConversationMessage::Assistant { content, .. } => (
            "assistant",
            None,
            Some("assistant"),
            false,
            content.iter().map(mod_append_content_block).collect(),
        ),
        ConversationMessage::System {
            content, subtype, ..
        } => (
            "system",
            subtype.as_deref(),
            None,
            false,
            if content
                .string_units("")
                .is_some_and(|units| units.is_empty())
            {
                Vec::new()
            } else {
                vec![source_object([
                    ("type", "text".into()),
                    ("text", content.clone()),
                ])]
            },
        ),
    };
    let mut result = source_object([
        ("type", kind.into()),
        ("content", Source::array(content).unwrap()),
    ]);
    if let Some(name) = name {
        result.set_field("name", name.into()).unwrap();
    }
    if let Some(role) = role {
        result.set_field("role", role.into()).unwrap();
    }
    if is_meta {
        result
            .set_field("isMeta", Source::parse("true").unwrap())
            .unwrap();
    }
    result
}

fn compact_summary_content_source_if_unmodified(
    original: &ConversationMessage,
    rewritten: &ConversationMessage,
    compact_summary: bool,
) -> Option<Source> {
    if !compact_summary || original != rewritten || !original.is_compact_summary() {
        return None;
    }
    let ConversationMessage::User { content, .. } = original else {
        return None;
    };
    let [block] = content.as_slice() else {
        return None;
    };
    let units = match block {
        lingxi_core::types::ContentBlock::Text {
            text,
            citations: None,
        } => text.encode_utf16().collect(),
        lingxi_core::types::ContentBlock::TextJsUtf16 {
            utf16_code_units,
            citations: None,
            ..
        } => utf16_code_units.clone(),
        _ => return None,
    };
    Source::root_string(String::from_utf16_lossy(&units), units).ok()
}

/// Headless SDK plain-text input is persisted as a JSON string by Native,
/// while later injected rows such as `[Request interrupted by user]` use a
/// text-block array. The ingress must explicitly identify the original SDK
/// content root as a string; a normalized one-text-block projection may have
/// come from an input array and is not sufficient evidence.
fn sdk_plain_prompt_content_source(
    message: &ConversationMessage,
    prompt_source: Option<&Source>,
    sdk_content_is_json_string: Option<bool>,
) -> Option<Source> {
    if prompt_source?.string_at("") != Some("sdk") {
        return None;
    }
    if sdk_content_is_json_string != Some(true) {
        return None;
    }
    let ConversationMessage::User {
        content,
        is_meta: false,
        ..
    } = message
    else {
        return None;
    };
    let [block] = content.as_slice() else {
        return None;
    };
    match block {
        lingxi_core::types::ContentBlock::Text {
            text,
            citations: None,
        } => Source::root_string(text.clone(), text.encode_utf16().collect()).ok(),
        lingxi_core::types::ContentBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            citations: None,
        } => Source::root_string(text.clone(), utf16_code_units.clone()).ok(),
        _ => None,
    }
}

fn mod_append_block_matches(
    original: &lingxi_core::types::ContentBlock,
    incoming: &Source,
) -> bool {
    mod_append_content_block(original).semantically_eq(incoming)
}

fn mod_append_new_text_block(incoming: &Source) -> Option<lingxi_core::types::ContentBlock> {
    let units = incoming.string_units("/text")?;
    Some(match String::from_utf16(&units) {
        Ok(text) => lingxi_core::types::ContentBlock::Text {
            text,
            citations: Some(None),
        },
        Err(_) => lingxi_core::types::ContentBlock::TextJsUtf16 {
            text: String::from_utf16_lossy(&units),
            utf16_code_units: units,
            citations: Some(None),
        },
    })
}

fn mod_append_projection_matches(expected: &ConversationMessage, actual: &Source) -> bool {
    mod_append_message(expected).semantically_eq(actual)
}

fn rewrite_mod_append_tool_result_blocks(
    original: Option<&Source>,
    incoming: &[Source],
) -> Vec<Source> {
    let original = original.and_then(source_array_items).unwrap_or_default();
    let mut used = vec![false; original.len()];
    incoming
        .iter()
        .filter_map(|block| {
            if let Some((index, old)) = original
                .iter()
                .enumerate()
                .find(|(index, old)| !used[*index] && old.semantically_eq(block))
            {
                used[index] = true;
                return Some(old.clone());
            }
            if block.string_at("/type") == Some("text")
                && block
                    .string_at("/text")
                    .is_some_and(|text| !text.is_empty())
            {
                return Some(source_object([
                    ("type", "text".into()),
                    ("text", block.subprojection("/text").unwrap()),
                    ("citations", source_null()),
                ]));
            }
            None
        })
        .collect()
}

fn rewrite_mod_append_tool_result(
    original: &lingxi_core::types::ContentBlock,
    incoming: &Source,
) -> lingxi_core::types::ContentBlock {
    use lingxi_core::types::ContentBlock;
    let ContentBlock::ToolResult {
        tool_use_id,
        content,
        provider_tool_use_id,
        content_blocks,
        ..
    } = original
    else {
        return original.clone();
    };
    let valid_content = incoming.get_property("content").is_none_or(|value| {
        value.as_str().is_some()
            || value.array_len().is_some_and(|len| {
                (0..len).all(|index| {
                    value
                        .get_index(index)
                        .and_then(|block| block.get_property("type"))
                        .and_then(|kind| kind.as_str())
                        .is_some()
                })
            })
    });
    if !valid_content
        || incoming
            .get_property("is_error")
            .is_some_and(|value| value.as_bool().is_none())
    {
        return original.clone();
    }
    let mut next_content = content.clone();
    let mut next_content_blocks = content_blocks.clone();
    if let Ok(value) = incoming.subprojection("/content") {
        if value.string_at("").is_some() {
            next_content = value;
            next_content_blocks = None;
        } else if let Some(blocks) = source_array_items(&value) {
            if content_blocks
                .as_ref()
                .is_some_and(|old| old.semantically_eq(&value))
            {
                next_content_blocks = Some(value);
            } else {
                let blocks =
                    rewrite_mod_append_tool_result_blocks(content_blocks.as_ref(), &blocks);
                let mut units = Vec::new();
                let mut first = true;
                for block in &blocks {
                    if let Some(text) = block.string_units("/text") {
                        if !first {
                            units.push(10);
                        }
                        first = false;
                        units.extend(text);
                    }
                }
                next_content =
                    Source::root_string(String::from_utf16_lossy(&units), units).unwrap();
                next_content_blocks = Some(Source::array(blocks).unwrap());
            }
        }
    }
    ContentBlock::ToolResult {
        tool_use_id: tool_use_id.clone(),
        content: next_content,
        is_error: incoming
            .get_property("is_error")
            .and_then(|value| value.as_bool()),
        provider_tool_use_id: provider_tool_use_id.clone(),
        content_blocks: next_content_blocks,
    }
}

fn mod_append_identity_key(
    kind: &str,
    tool_identity: Option<&str>,
    counts: &mut std::collections::HashMap<String, usize>,
) -> Option<String> {
    if matches!(kind, "text" | "image" | "document") {
        return None;
    }
    if kind == "tool_use" {
        if let Some(id) = tool_identity {
            return Some(format!("tool_use:{id}"));
        }
    } else if kind == "tool_result" {
        if let Some(id) = tool_identity {
            return Some(format!("tool_result:{id}"));
        }
    }
    let index = counts.entry(kind.to_owned()).or_default();
    let key = format!("{kind}#{}", *index);
    *index += 1;
    Some(key)
}

fn rewrite_mod_append_blocks(
    original: &[lingxi_core::types::ContentBlock],
    incoming: &[Source],
) -> Vec<lingxi_core::types::ContentBlock> {
    use lingxi_core::types::ContentBlock;

    let mut source_type_counts = std::collections::HashMap::new();
    let source_keys = original
        .iter()
        .map(|block| {
            let projection = mod_append_content_block(block);
            let kind = projection.string_at("/type")?;
            let tool_identity = match block {
                ContentBlock::ToolUse {
                    id, provider_id, ..
                } => provider_id.as_deref().or(Some(id.as_str())),
                ContentBlock::ToolResult {
                    tool_use_id,
                    provider_tool_use_id,
                    ..
                } => provider_tool_use_id
                    .as_deref()
                    .or(Some(tool_use_id.as_str())),
                _ => None,
            };
            mod_append_identity_key(kind, tool_identity, &mut source_type_counts)
        })
        .collect::<Vec<_>>();
    let source_key_set = source_keys
        .iter()
        .filter_map(Clone::clone)
        .collect::<std::collections::HashSet<_>>();
    let source_tool_result_keys = original
        .iter()
        .zip(&source_keys)
        .filter_map(|(block, key)| {
            matches!(block, ContentBlock::ToolResult { .. })
                .then(|| key.clone())
                .flatten()
        })
        .collect::<Vec<_>>();
    let unbound_tool_result_count = incoming
        .iter()
        .filter(|block| {
            block.string_at("/type") == Some("tool_result")
                && block.string_at("/tool_use_id").is_none()
        })
        .count();
    let can_bind_unidentified_tool_results =
        unbound_tool_result_count == source_tool_result_keys.len();
    let mut unbound_tool_result_index = 0;
    let mut incoming_type_counts = std::collections::HashMap::new();
    let mut raised_source_blocks = vec![false; original.len()];
    let mut active_anchor = None::<String>;
    let mut overlays = std::collections::HashMap::<Option<String>, Vec<ContentBlock>>::new();
    let mut tool_result_overlays = std::collections::HashMap::<String, Source>::new();

    for block in incoming {
        let Some(kind) = block.string_at("/type") else {
            continue;
        };
        if matches!(kind, "text" | "image" | "document") {
            if let Some((index, old)) = original.iter().enumerate().find(|(index, old)| {
                !raised_source_blocks[*index]
                    && source_keys[*index].is_none()
                    && mod_append_block_matches(old, block)
            }) {
                raised_source_blocks[index] = true;
                overlays
                    .entry(active_anchor.clone())
                    .or_default()
                    .push(old.clone());
            } else if kind == "text" {
                if let Some(text) = mod_append_new_text_block(block).filter(|text| match text {
                    ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => {
                        !text.is_empty()
                    }
                    _ => false,
                }) {
                    overlays
                        .entry(active_anchor.clone())
                        .or_default()
                        .push(text);
                }
            }
            continue;
        }

        let tool_identity = match kind {
            "tool_use" => block.string_at("/id"),
            "tool_result" => block.string_at("/tool_use_id"),
            _ => None,
        };
        let generated_key = mod_append_identity_key(kind, tool_identity, &mut incoming_type_counts);
        let identity_key = if kind == "tool_result" && tool_identity.is_none() {
            let key = can_bind_unidentified_tool_results
                .then(|| {
                    source_tool_result_keys
                        .get(unbound_tool_result_index)
                        .cloned()
                })
                .flatten();
            unbound_tool_result_index += 1;
            key
        } else {
            generated_key
        };
        let Some(identity_key) = identity_key.filter(|key| source_key_set.contains(key)) else {
            continue;
        };
        active_anchor = Some(identity_key.clone());
        if kind == "tool_result" {
            tool_result_overlays.insert(identity_key, block.clone());
        }
    }

    let sentinel_index = original
        .iter()
        .position(|block| {
            !matches!(
                block,
                ContentBlock::Thinking { .. }
                    | ContentBlock::RedactedThinking { .. }
                    | ContentBlock::ToolResult { .. }
            )
        })
        .unwrap_or(original.len());
    let sentinel = overlays.remove(&None).unwrap_or_default();
    let mut rewritten = Vec::new();
    for index in 0..=original.len() {
        if index == sentinel_index {
            rewritten.extend(sentinel.iter().cloned());
        }
        if index == original.len() {
            break;
        }
        if let Some(identity_key) = &source_keys[index] {
            let block = match (&original[index], tool_result_overlays.get(identity_key)) {
                (ContentBlock::ToolResult { .. }, Some(incoming)) => {
                    rewrite_mod_append_tool_result(&original[index], incoming)
                }
                _ => original[index].clone(),
            };
            rewritten.push(block);
            rewritten.extend(
                overlays
                    // Native q reads `a.get(key)` at every source occurrence.
                    // Duplicate ToolUse ids therefore replay the same anchored
                    // text after each matching source block; only the unkeyed
                    // sentinel is consumed once above.
                    .get(&Some(identity_key.clone()))
                    .cloned()
                    .unwrap_or_default(),
            );
        }
    }
    rewritten
}

fn rewrite_mod_append_message(
    original: &ConversationMessage,
    incoming: &Source,
) -> Result<ConversationMessage, String> {
    let Some(content) = incoming
        .subprojection("/content")
        .ok()
        .and_then(|content| source_array_items(&content))
    else {
        return Err("session.append message.content must be an array".into());
    };
    let mut rewritten = original.clone();
    match &mut rewritten {
        ConversationMessage::User {
            content: old_content,
            ..
        }
        | ConversationMessage::Assistant {
            content: old_content,
            ..
        } => {
            *old_content = rewrite_mod_append_blocks(old_content, &content);
        }
        ConversationMessage::System {
            content: old_content,
            ..
        } => {
            let mut units = Vec::new();
            let mut first = true;
            for text in content
                .iter()
                .filter(|block| block.string_at("/type") == Some("text"))
                .filter_map(|block| block.string_units("/text"))
            {
                if !first {
                    units.push(u16::from(b'\n'));
                }
                first = false;
                units.extend(text);
            }
            *old_content = Source::root_string(String::from_utf16_lossy(&units), units)
                .expect("system text carries exact UTF-16 units");
        }
    }
    Ok(rewritten)
}

#[test]
fn mod_append_rewrite_preserves_message_metadata_and_system_source() {
    let original: ConversationMessage = serde_json::from_str(
        r#"{
        "role":"user","id":"00000000-0000-0000-0000-000000000001",
        "content":[{"type":"text","text":"before"}],
        "turnCompanion":false,"usageLimitNote":null,
        "origin":{"name":"\ud800"},"turnOrigin":"\ud801"
    }"#,
    )
    .unwrap();
    let incoming = Source::parse(r#"{"content":[{"type":"text","text":"after"}]}"#).unwrap();
    let rewritten = rewrite_mod_append_message(&original, &incoming).unwrap();
    let source = Source::parse(&serde_json::to_string(&rewritten).unwrap()).unwrap();
    assert_eq!(
        source
            .get_property("turnCompanion")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert!(source.get_property("usageLimitNote").unwrap().is_null());
    assert_eq!(source.string_units("/origin/name"), Some(vec![0xd800]));
    assert_eq!(source.string_units("/turnOrigin"), Some(vec![0xd801]));
    let system = ConversationMessage::System {
        id: MessageId::new(),
        content: "before".into(),
        subtype: Some("fixture".into()),
        api_system: None,
        compact_metadata: None,
        model_fallback: None,
        refusal_fallback: None,
    };
    let incoming = Source::parse(
        r#"{"content":[{"type":"text","text":"\ud800"},{"type":"text","text":"\ud801"}]}"#,
    )
    .unwrap();
    let ConversationMessage::System {
        content, subtype, ..
    } = rewrite_mod_append_message(&system, &incoming).unwrap()
    else {
        panic!("system rewrite changed role")
    };
    assert_eq!(content.string_units(""), Some(vec![0xd800, 10, 0xd801]));
    assert_eq!(subtype.as_deref(), Some("fixture"));
}

#[cfg(test)]
#[tokio::test]
async fn compact_summary_source_override_preserves_session_append_rewrites() {
    use hooks::mods::ModHost;
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
    use platform_posix::fs::PosixFileSystem;
    use std::sync::Arc;

    let root = tempfile::tempdir().expect("temporary compact-summary root");
    let transcript_path = root.path().join("session.jsonl");
    let writer = Arc::new(session::JsonlWriter::new(
        transcript_path.clone(),
        Arc::new(PosixFileSystem::new(root.path().to_path_buf())),
    ));
    let module_path = root.path().join("compact-summary-append.js");
    std::fs::write(
        &module_path,
        r#"
        export function register(on) {
          on('session.append', ($, event, next) => {
            if (event.door !== 'compaction') return next(event);
            const content = event.message.content.map(block =>
              block.type === 'text' && block.text === 'source to rewrite'
                ? { ...block, text: 'rewritten by Mod' }
                : block
            );
            return next({ ...event, message: { ...event.message, content } });
          });
        }
        "#,
    )
    .expect("write compact-summary Mod fixture");
    let host = ModHost::start(None)
        .await
        .expect("start session.append Mod host");
    host.load(
        "compact-summary-rewrite",
        root.path(),
        &module_path,
        Source::parse("{}").unwrap(),
    )
    .await
    .expect("load compact-summary Mod");
    let mut hook_registry = hooks::HookRegistry::new();
    hook_registry.set_mod_host(host);
    let orchestrator = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(crate::test_support::MockApiClient::new(Vec::new())),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        crate::test_support::noop_hook_executor(),
        Arc::new(crate::test_support::NoOpPermissionGate),
        Arc::new(crate::test_support::MockOutputStream::new()),
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hook_registry)))
    .with_jsonl_writer(writer);

    let exact_units = vec![65, 0xd800, 66];
    let summary = |id, content| ConversationMessage::User {
        id,
        content: vec![content],
        is_meta: false,
        is_compact_summary: true,
        is_visible_in_transcript_only: true,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
    };
    let source_summary = summary(
        MessageId::new(),
        ContentBlock::TextJsUtf16 {
            text: String::from_utf16_lossy(&exact_units),
            utf16_code_units: exact_units.clone(),
            citations: None,
        },
    );
    let rewritten_summary = summary(
        MessageId::new(),
        ContentBlock::Text {
            text: "source to rewrite".into(),
            citations: None,
        },
    );
    orchestrator
        .persist_message_to_jsonl_inner(&source_summary, None, None, true)
        .await;
    orchestrator
        .persist_message_to_jsonl_inner(&rewritten_summary, None, None, true)
        .await;

    let raw = std::fs::read_to_string(&transcript_path).expect("session transcript");
    let rows = raw
        .lines()
        .map(|line| Source::parse(line).expect("valid JSONL row"))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].string_units("/message/content"), Some(exact_units));
    assert_eq!(
        rows[1].at_pointer("/message/content").unwrap().array_len(),
        Some(1)
    );
    assert_eq!(
        rows[1].string_at("/message/content/0/text"),
        Some("rewritten by Mod")
    );
}

#[cfg(test)]
#[test]
fn shutdown_interruption_is_persisted_in_native_optional_slot() {
    use lingxi_core::types::{ConversationMessage, MessageId};
    use std::sync::Arc;

    let root = tempfile::tempdir().expect("temporary transcript root");
    let orchestrator = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(crate::test_support::MockApiClient::new(Vec::new())),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        crate::test_support::noop_hook_executor(),
        Arc::new(crate::test_support::NoOpPermissionGate),
        Arc::new(crate::test_support::MockOutputStream::new()),
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    );
    let message = ConversationMessage::User {
        api_message_override: None,
        id: MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: "[Request interrupted by user]".into(),
            citations: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
        interrupted_by_shutdown: Some(true),
        refused_message_id: None,
    };
    let row = orchestrator.to_jsonl_message(&message, "s", None, None, None, None);
    let encoded = serde_json::to_string(&row).unwrap();
    let timestamp = encoded.find("\"timestamp\"").unwrap();
    let shutdown = encoded.find("\"interruptedByShutdown\":true").unwrap();
    let user_type = encoded.find("\"userType\"").unwrap();
    assert!(
        timestamp < shutdown && shutdown < user_type,
        "Native head slot: {encoded}"
    );
    assert_eq!(
        row.extra
            .at_pointer("/interruptedByShutdown")
            .and_then(lingxi_core::types::utf16_json::JsonValueView::as_bool),
        Some(true)
    );
}

#[cfg(test)]
#[test]
fn refused_message_id_is_persisted_after_timestamp_with_exact_utf16_units() {
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use lingxi_core::types::{ConversationMessage, MessageId};
    use std::sync::Arc;

    let root = tempfile::tempdir().expect("temporary transcript root");
    let orchestrator = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(crate::test_support::MockApiClient::new(Vec::new())),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        crate::test_support::noop_hook_executor(),
        Arc::new(crate::test_support::NoOpPermissionGate),
        Arc::new(crate::test_support::MockOutputStream::new()),
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    );
    let refused_message_id = Utf16JsonProjection::parse(r#""A\ud800B""#).unwrap();
    let message = ConversationMessage::User {
        api_message_override: None,
        id: MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: "retry".into(),
            citations: None,
        }],
        is_meta: true,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: Some(Some(true)),
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
        interrupted_by_shutdown: None,
        refused_message_id: Some(refused_message_id),
    };
    let row = orchestrator.to_jsonl_message(&message, "s", None, None, None, None);
    let encoded = serde_json::to_string(&row).unwrap();
    let timestamp = encoded.find("\"timestamp\"").unwrap();
    let refused = encoded.find("\"refusedMessageId\":\"A\\ud800B\"").unwrap();
    let user_type = encoded.find("\"userType\"").unwrap();
    assert!(
        timestamp < refused && refused < user_type,
        "Native head slot: {encoded}"
    );
    assert_eq!(
        row.extra.string_units("/refusedMessageId"),
        Some(vec![0x0041, 0xd800, 0x0042])
    );
}

impl ConversationOrchestrator {
    async fn mod_append_tool_name(&self, id: &lingxi_core::types::ToolUseId) -> String {
        self.session
            .lock()
            .await
            .history
            .iter()
            .rev()
            .find_map(|message| {
                let ConversationMessage::Assistant { content, .. } = message else {
                    return None;
                };
                content.iter().find_map(|block| match block {
                    lingxi_core::types::ContentBlock::ToolUse {
                        id: candidate,
                        name,
                        ..
                    } if candidate == id => Some(name.clone()),
                    _ => None,
                })
            })
            .unwrap_or_else(|| "unknown".into())
    }

    async fn mod_append_door_origin(
        &self,
        message: &ConversationMessage,
    ) -> (String, serde_json::Value) {
        use lingxi_core::types::ContentBlock;

        match message {
            ConversationMessage::Assistant { .. } => {
                let model = match crate::scheduled_turn::current() {
                    Some(settings) => settings.model,
                    None => self.session.lock().await.model.clone(),
                };
                (
                    "response".into(),
                    serde_json::json!({"kind":"model","model":model}),
                )
            }
            ConversationMessage::System { subtype, .. } => {
                let door = subtype.as_deref().map_or("notice", |subtype| {
                    if subtype.ends_with("_boundary") {
                        "compaction"
                    } else if subtype == "local_command" {
                        "command"
                    } else {
                        "notice"
                    }
                });
                (door.into(), serde_json::json!({"kind":"engine"}))
            }
            ConversationMessage::User {
                content,
                is_meta,
                is_compact_summary,
                ..
            } => {
                if *is_compact_summary {
                    return ("compaction".into(), serde_json::json!({"kind":"engine"}));
                }
                if let Some(tool_use_id) = content.iter().find_map(|block| match block {
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id),
                    _ => None,
                }) {
                    let tool = self.mod_append_tool_name(tool_use_id).await;
                    return (
                        "tool-result".into(),
                        serde_json::json!({"kind":"tool","tool":tool}),
                    );
                }
                if *is_meta {
                    return ("note".into(), serde_json::json!({"kind":"engine"}));
                }
                ("prompt".into(), crate::mod_prompt_origin::current())
            }
        }
    }

    async fn replace_session_history_row(
        &self,
        original_id: lingxi_core::types::MessageId,
        rewritten: ConversationMessage,
    ) {
        let mut session = self.session.lock().await;
        if let Some(stored) = session
            .history
            .iter_mut()
            .rev()
            .find(|stored| stored.id() == original_id)
        {
            *stored = rewritten;
        }
    }

    /// Dispatch the main-session row before JSONL serialization and reflect
    /// accepted content changes into the already-appended in-memory history.
    pub(crate) async fn mod_session_append_row(
        &self,
        message: &ConversationMessage,
        row_uuid: Option<&str>,
        update_history: bool,
        publication_fence: Option<Arc<dyn HookPublicationGuard>>,
    ) -> ConversationMessage {
        let mut original = message.clone();
        let persistent_app_chat = self.is_persistent_app_chat();
        if persistent_app_chat {
            canonicalize_app_chat_assistant_text(&mut original);
        }
        if publication_fence.as_ref().is_some_and(|fence| {
            !hooks::attachment::HookPublicationGuard::is_current(fence.as_ref())
        }) {
            return original;
        }
        let Some(registry) = &self.lifecycle_runtime.hook_registry else {
            return original;
        };
        let Some(host) = registry.read().await.mod_host() else {
            return original;
        };
        if !host.has_event("session.append") {
            return original;
        }

        let uuid = row_uuid
            .map(str::to_owned)
            .unwrap_or_else(|| original.id().as_uuid().to_string());
        let (door, origin) = self.mod_append_door_origin(&original).await;
        let input = source_object([
            ("message", mod_append_message(&original)),
            (
                "door",
                Source::from_serde_value(&serde_json::json!(door)).unwrap(),
            ),
            (
                "origin",
                Source::from_serde_value(&serde_json::json!(origin)).unwrap(),
            ),
            ("uuid", uuid.clone().into()),
        ]);
        let core_input = input.clone();
        let core_uuid = uuid.clone();
        let core_original = original.clone();
        let applied = Arc::new(std::sync::Mutex::new(None::<(ConversationMessage, Source)>));
        let applied_by_core = applied.clone();
        let log_output = self.output.clone();
        let toast_output = self.output.clone();
        let status_output = self.output.clone();
        let cwd = self.current_cwd();
        let guarded_session = if let Some(fence) = publication_fence.clone() {
            let Some(session) = crate::turn_loop::generation_bound_mod_session_context(self, fence)
            else {
                return original;
            };
            Some(session)
        } else {
            None
        };
        let session: &dyn hooks::mods::ModSessionContext =
            guarded_session.as_deref().map_or(self, |session| session);
        let log_fence = publication_fence.clone();
        let toast_fence = publication_fence.clone();
        let status_fence = publication_fence.clone();
        let dispatched = host
            .dispatch_with_utf16_at_context(
                "session.append",
                input,
                &cwd,
                Some(session),
                None,
                None,
                None,
                lingxi_core::host::task_registry::FieldPresence::Missing,
                move |forwarded_projection| {
                    let core_input = core_input.clone();
                    let core_uuid = core_uuid.clone();
                    let core_original = core_original.clone();
                    let applied = applied_by_core.clone();
                    async move {
                        let forwarded = forwarded_projection.ok_or_else(|| {
                            hooks::mods::ModError::Hook("session.append needs an event".into())
                        })?;
                        for key in ["door", "origin", "uuid", "agentId"] {
                            if forwarded
                                .subprojection(&format!("/{key}"))
                                .ok()
                                .is_some_and(|value| {
                                    core_input
                                        .subprojection(&format!("/{key}"))
                                        .ok()
                                        .is_none_or(|original| !original.semantically_eq(&value))
                                })
                            {
                                return Err(hooks::mods::ModError::Hook(format!(
                                    "session.append {key} is pinned"
                                )));
                            }
                        }
                        let Ok(incoming) = forwarded.subprojection("/message") else {
                            return Err(hooks::mods::ModError::Hook(
                                "session.append needs message.content".into(),
                            ));
                        };
                        let original_projection = mod_append_message(&core_original);
                        for key in ["type", "name", "role", "isMeta"] {
                            if incoming.subprojection(&format!("/{key}")).ok().is_some_and(
                                |value| {
                                    original_projection
                                        .subprojection(&format!("/{key}"))
                                        .ok()
                                        .is_none_or(|original| !original.semantically_eq(&value))
                                },
                            ) {
                                return Err(hooks::mods::ModError::Hook(format!(
                                    "session.append message.{key} is pinned"
                                )));
                            }
                        }
                        let mut rewritten = rewrite_mod_append_message(&core_original, &incoming)
                            .map_err(hooks::mods::ModError::Hook)?;
                        if persistent_app_chat {
                            canonicalize_app_chat_assistant_text(&mut rewritten);
                        }
                        let projected = mod_append_message(&rewritten);
                        *applied
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some((rewritten, projected.clone()));
                        Ok(Some(source_object([
                            ("message", projected),
                            ("uuid", core_uuid.into()),
                        ])))
                    }
                },
                move |plugin, text| {
                    let output = log_output.clone();
                    let fence = log_fence.clone();
                    async move {
                        if let Some(fence) = fence {
                            fence
                                .publish_if_current(Box::pin(output.emit_mod_log(&plugin, &text)))
                                .await;
                        } else {
                            output.emit_mod_log(&plugin, &text).await;
                        }
                    }
                },
                move |plugin, text, timeout_ms| {
                    let output = toast_output.clone();
                    let fence = toast_fence.clone();
                    async move {
                        if let Some(fence) = fence {
                            fence
                                .publish_if_current(Box::pin(
                                    output.emit_mod_toast(&plugin, &text, timeout_ms),
                                ))
                                .await;
                        } else {
                            output.emit_mod_toast(&plugin, &text, timeout_ms).await;
                        }
                    }
                },
                move |plugin, text| {
                    let output = status_output.clone();
                    let fence = status_fence.clone();
                    async move {
                        if let Some(fence) = fence {
                            fence
                                .publish_if_current(Box::pin(
                                    output.emit_mod_status(&plugin, text.as_ref()),
                                ))
                                .await;
                        } else {
                            output.emit_mod_status(&plugin, text.as_ref()).await;
                        }
                    }
                },
            )
            .await;
        if publication_fence.as_ref().is_some_and(|fence| {
            !hooks::attachment::HookPublicationGuard::is_current(fence.as_ref())
        }) {
            return original;
        }
        let rewritten = match dispatched {
            Ok(outcome) => {
                let result = outcome.result_projection;
                let accepted = applied
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                match accepted {
                    Some((rewritten, _projected))
                        if result.string_at("/uuid") == Some(uuid.as_str())
                            && result.subprojection("/message").is_ok_and(|message| {
                                mod_append_projection_matches(&rewritten, &message)
                            }) =>
                    {
                        rewritten
                    }
                    _ => original.clone(),
                }
            }
            Err(error) => {
                tracing::warn!(%uuid, %error, "session.append Mod dispatch failed; preserving original row");
                original.clone()
            }
        };

        if update_history && rewritten != original {
            let replace = self.replace_session_history_row(original.id(), rewritten.clone());
            if let Some(fence) = publication_fence.as_ref() {
                fence.commit_if_current(Box::pin(replace)).await;
            } else {
                replace.await;
            }
        }
        rewritten
    }

    /// Publish a core tool outcome after every `tool.call` wrapper has returned.
    /// A replacement owns the model text and transcript/SDK payload, while a
    /// ref-backed rewrite still carries the selected run's MCP/end-turn facts.
    pub(crate) async fn commit_mod_result_stage(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        stage: ModResultStage,
        replacement: Option<(Source, Source)>,
    ) {
        if let Some((raw, text)) = replacement {
            // Native's ref-backed result rewrite reconstructs the selected
            // result row while retaining the selected run's MCP metadata and
            // turn-end marker. The direct `$.tool.call()` virtual-frame path
            // never commits this stage, so this only applies to the ordinary
            // event middleware path.
            if let Some((tool_name, tool_input)) = stage.permission_denial {
                self.record_permission_denial(&tool_name, id, &tool_input)
                    .await;
            }
            if let Some(meta) = stage.mcp_meta {
                self.record_tool_use_mcp_meta(id, meta).await;
            }
            if let Some(turn_end) = stage.turn_end {
                self.record_pending_tool_result_turn_end(id, turn_end).await;
            }
            self.record_tool_use_result(id, raw.clone()).await;
            self.emit_tool_result_frame(id, tool, &text, &raw, None)
                .await;
            return;
        }
        if let Some((tool_name, tool_input)) = stage.permission_denial {
            self.record_permission_denial(&tool_name, id, &tool_input)
                .await;
        }
        if let Some(raw) = stage.tool_use_result {
            self.record_tool_use_result(id, raw).await;
        }
        if let Some(meta) = stage.mcp_meta {
            self.record_tool_use_mcp_meta(id, meta).await;
        }
        if let Some(turn_end) = stage.turn_end {
            self.record_pending_tool_result_turn_end(id, turn_end).await;
        }
        if let Some(denial_kind) = stage.denial_kind {
            self.record_tool_denial_kind(id, &denial_kind).await;
        }
        if let Some(frame) = stage.frame {
            self.emit_tool_result_frame(
                id,
                &frame.tool,
                &frame.model_text,
                &frame.result,
                frame.denial_kind.as_deref(),
            )
            .await;
        }
    }

    /// Persist Claude's fire envelope and separate meta user companion while
    /// holding the same ordering gate as foreground transcript writes.
    pub async fn append_scheduled_loop_wakeup(
        &self,
        message: String,
        companion: Option<String>,
        streak: u32,
        since_ms: u64,
        fire: ScheduledLoopFire,
    ) -> Result<(), lingxi_core::host::HandleError> {
        let _turn = self.turn_gate.lock().await;
        let session_id = self.session.lock().await.session_id;
        let mut payload = serde_json::json!({"message": message, "companion": companion,
            "streak": streak, "since_ms": since_ms, "taskId": fire.task_id,
            "cron": fire.cron, "prompt": scheduled_fire_prompt(&fire.prompt),
            "taskKindLoop": fire.task_kind_loop});
        if streak > 0 {
            let mut uuids = Vec::<String>::new();
            if let Some(writer) = &self.transcript.jsonl_writer {
                if let Ok(file) = writer
                    .filesystem_handle()
                    .read_file(&writer.active_path().to_string_lossy(), None, None)
                    .await
                {
                    let rows: Vec<Source> = file
                        .content
                        .lines()
                        .filter_map(|line| Source::parse(line).ok())
                        .collect();
                    if let Some(start) = rows.iter().rposition(|row| {
                        row.string_at("/type") == Some("system")
                            && row.string_at("/subtype") == Some("scheduled_task_fire")
                    }) {
                        uuids = rows[start..]
                            .iter()
                            .filter_map(|row| row.string_at("/uuid").map(str::to_owned))
                            .collect();
                    }
                }
            }
            payload["foldedUuids"] = serde_json::json!(uuids);
        }
        let record = ConversationMessage::System {
            api_system: None,
            id: fire.fire_id,
            content: payload.to_string().into(),
            subtype: Some("scheduled_task_fire".into()),
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        };
        let record = self
            .mod_session_append_row(&record, None, false, None)
            .await;
        let result = self
            .persist_scheduled_record(&record, session_id, false)
            .await;
        {
            let mut session = self.session.lock().await;
            session.model_context_excluded_messages.insert(record.id());
            session.history.push(record);
        }
        if let Some(text) = companion {
            let row = ConversationMessage::user_meta(lingxi_core::types::MessageId::new(), text);
            let row = self.mod_session_append_row(&row, None, false, None).await;
            let companion_result = if result.is_ok() {
                self.persist_scheduled_record(&row, session_id, true).await
            } else {
                Ok(())
            };
            self.session.lock().await.history.push(row);
            result?;
            companion_result?;
        } else {
            result?;
        }
        Ok(())
    }

    async fn persist_scheduled_record(
        &self,
        record: &ConversationMessage,
        session_id: lingxi_core::types::SessionId,
        companion: bool,
    ) -> Result<(), lingxi_core::host::HandleError> {
        let Some(writer) = &self.transcript.jsonl_writer else {
            return Ok(());
        };
        let mut row = self.to_jsonl_message_with_inner_id(
            record,
            &session_id.as_uuid().to_string(),
            self.transcript.last_jsonl_uuid.lock().await.clone(),
            self.resolve_git_branch().await,
            Some(entrypoint_value()),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        if companion {
            row.turn_companion = Some(Some(true));
            if let ConversationMessage::User { content, .. } = record {
                let units: Vec<u16> = content
                    .iter()
                    .filter_map(|block| {
                        let projected = mod_append_content_block(block);
                        (projected.string_at("/type") == Some("text"))
                            .then(|| projected.string_units("/text"))
                            .flatten()
                    })
                    .flatten()
                    .collect();
                let text = Source::root_string(String::from_utf16_lossy(&units), units)
                    .expect("scheduled companion carries exact text");
                row.message = source_object([("role", "user".into()), ("content", text)]);
            }
        }
        self.append_transcript_row(&writer, &row)
            .await
            .map_err(|error| {
                lingxi_core::host::HandleError::ActionFailed(format!(
                    "could not persist scheduled fire: {error}"
                ))
            })?;
        *self.transcript.last_jsonl_uuid.lock().await = Some(row.uuid);
        Ok(())
    }

    /// Build a deterministic UUID for the non-streaming per-block fallback.
    ///
    /// Native streamed rows have a preassigned row UUID that does not depend on
    /// transcript parentage. The fallback has no stop-time row identity, so it
    /// derives one from the turn id, block position, and source block. Keeping
    /// parent selection out of this identity lets the durable append choose the
    /// current transcript parent after append-through callbacks complete.
    fn assistant_block_derived_uuid(
        turn_id: &str,
        block_index: usize,
        block: &lingxi_core::types::ContentBlock,
    ) -> String {
        let block_index = u64::try_from(block_index).unwrap_or(u64::MAX);
        let block_signature = Self::assistant_block_signature(block);
        let mut hasher = Sha256::new();

        hasher.update(b"lingxi-assistant-block-v2");
        hasher.update(turn_id.as_bytes());
        hasher.update(b"|");
        hasher.update(block_index.to_le_bytes());
        hasher.update(b"|");
        hasher.update(block_signature.as_bytes());
        let digest = hasher.finalize();

        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        uuid::Uuid::from_bytes(bytes).to_string()
    }

    fn assistant_block_signature(block: &lingxi_core::types::ContentBlock) -> String {
        use lingxi_core::types::utf16_json::JsonValueView;
        enum Part<'a> {
            Value(JsonValueView<'a>),
            Key(&'a [u16]),
            Punctuation(char),
        }
        let source =
            Source::parse(&serde_json::to_string(block).expect("content block serializes"))
                .expect("serialized block is valid source JSON");
        let mut pending = vec![Part::Value(source.at_pointer("").unwrap())];
        let mut signature = String::new();
        while let Some(part) = pending.pop() {
            match part {
                Part::Punctuation(character) => signature.push(character),
                Part::Key(units) => signature.push_str(
                    &Source::root_string(String::from_utf16_lossy(units), units.to_vec())
                        .unwrap()
                        .to_json_string()
                        .unwrap(),
                ),
                Part::Value(value) => {
                    if let Some(units) = value.code_units() {
                        pending.push(Part::Key(units));
                    } else if let Some(mut entries) = value.object_entries() {
                        entries.sort_by(|a, b| a.code_units.cmp(b.code_units));
                        signature.push('{');
                        pending.push(Part::Punctuation('}'));
                        for (index, entry) in entries.into_iter().enumerate().rev() {
                            pending.push(Part::Value(entry.value));
                            pending.push(Part::Punctuation(':'));
                            pending.push(Part::Key(entry.code_units));
                            if index != 0 {
                                pending.push(Part::Punctuation(','));
                            }
                        }
                    } else if let Some(length) = value.array_len() {
                        signature.push('[');
                        pending.push(Part::Punctuation(']'));
                        for index in (0..length).rev() {
                            pending.push(Part::Value(value.get_index(index).unwrap()));
                            if index != 0 {
                                pending.push(Part::Punctuation(','));
                            }
                        }
                    } else if let Some(number) = value.as_number() {
                        if number.is_finite() {
                            signature.push_str(
                                &Source::parse(&number.to_string())
                                    .unwrap()
                                    .to_json_string()
                                    .unwrap(),
                            );
                        } else {
                            signature.push_str("null");
                        }
                    } else if let Some(boolean) = value.as_bool() {
                        signature.push_str(if boolean { "true" } else { "false" });
                    } else {
                        signature.push_str("null");
                    }
                }
            }
        }
        signature
    }

    /// Seed the JSONL parent-uuid chain pointer so the FIRST append after a
    /// resume chains via `parent_uuid` off the resumed transcript's tail
    /// (matching the M5-07 writer's chain semantics). Used by the CLI's
    /// resume-into-TUI seed alongside adopting the resumed history + id; without
    /// it the first appended message would be a chain orphan (recoverable, but
    /// this keeps the on-disk chain linear).
    /// Emit a `tool_result` SDK frame, or buffer it when the streaming driver
    /// has ordering active. Every dispatch-side emission goes through here.
    pub(crate) async fn emit_tool_result_frame(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        model_text: &Source,
        result: &Source,
        denial_kind: Option<&str>,
    ) {
        if let Some(stage) = active_mod_result_stage() {
            let mut stage = stage.lock().await;
            if stage.tool_use_id == id.as_str() {
                stage.frame = Some(PendingToolFrame {
                    tool: tool.to_string(),
                    model_text: model_text.clone(),
                    result: result.clone(),
                    denial_kind: denial_kind.map(str::to_string),
                });
                return;
            }
        }
        if let Some(buf) = self.transcript.tool_frames.lock().await.as_mut() {
            buf.insert(
                id.to_string(),
                PendingToolFrame {
                    tool: tool.to_string(),
                    model_text: model_text.clone(),
                    result: result.clone(),
                    denial_kind: denial_kind.map(str::to_string),
                },
            );
            return;
        }
        let mcp_meta = self
            .transcript
            .tool_use_mcp_meta
            .lock()
            .await
            .get(id.as_str())
            .cloned();
        match denial_kind {
            Some(kind) => {
                self.output
                    .emit_tool_result_denied(id, tool, model_text, result, kind, None)
                    .await;
            }
            None => {
                self.output
                    .emit_tool_result(id, tool, model_text, result, mcp_meta.as_ref())
                    .await
            }
        }
    }

    /// Turn frame buffering on for the streaming driver, and off again.
    pub(crate) async fn set_tool_frame_buffering(&self, on: bool) {
        let abandoned = {
            let mut slot = self.transcript.tool_frames.lock().await;
            let old = std::mem::replace(&mut *slot, on.then(std::collections::HashMap::new));
            old.into_iter()
                .flat_map(|frames| frames.into_keys())
                .collect::<Vec<_>>()
        };
        // A non-empty old buffer means the stream terminated before those
        // results reached the received-order release/persist point. Discard
        // their parallel metadata too so a failed iteration cannot leak it
        // for the lifetime of the session.
        if !abandoned.is_empty() {
            let mut results = self.transcript.tool_use_results.lock().await;
            let mut denials = self.transcript.tool_denial_kinds.lock().await;
            let mut mcp_meta = self.transcript.tool_use_mcp_meta.lock().await;
            let mut turn_end = self.transcript.pending_tool_result_turn_end.lock().await;
            let mut sources = self.transcript.tool_source_assistant_uuids.lock().await;
            for id in abandoned {
                results.remove(&id);
                denials.remove(&id);
                mcp_meta.remove(&id);
                turn_end.remove(&id);
                sources.remove(&id);
            }
        }
    }

    /// Release one buffered frame, in the CALLER's order.
    ///
    /// `content` is the block's FINAL model-facing text, so a synthetic that
    /// replaced a cancelled tool's real outcome wins over whatever the dispatch
    /// buffered. A tool that never dispatched (queued, then cancelled) has no
    /// buffered frame and still gets one, which is the case that previously
    /// emitted nothing at all.
    #[cfg(test)]
    pub(crate) async fn release_tool_frame(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        content: &Source,
        is_error: bool,
    ) {
        self.release_tool_frame_with_publication(id, tool, content, is_error, None)
            .await;
    }

    pub(crate) async fn release_tool_frame_with_publication(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        content: &Source,
        is_error: bool,
        publication: Option<crate::turn_loop::ToolResultFramePublication>,
    ) {
        let id_key = id.to_string();
        let pending = self
            .transcript
            .tool_frames
            .lock()
            .await
            .as_mut()
            .and_then(|b| b.remove(&id_key));
        let result_from_side_table = self
            .transcript
            .tool_use_results
            .lock()
            .await
            .get(&id_key)
            .cloned();
        let denial_kind_from_side_table = self
            .transcript
            .tool_denial_kinds
            .lock()
            .await
            .get(&id_key)
            .cloned();
        let substituted = pending
            .as_ref()
            .is_some_and(|p| !p.model_text.semantically_eq(content));
        let (tool, pending_result, pending_denial_kind) = match pending {
            // A substitution replaced the model-facing text, so the buffered
            // payload describes an outcome that was DISCARDED. claude-code's
            // synthetic carries a synthetic `toolUseResult` too, so the real
            // one must not reach the SDK.
            Some(p) => (p.tool, Some(p.result), p.denial_kind),
            // Actor-backed dispatches carry their frame only when Tn accepts
            // the completed row; it was deliberately not emitted from W1.
            None => match publication {
                Some(frame) => (frame.tool, Some(frame.result), frame.denial_kind),
                // Never dispatched: synthesize the payload the dispatch would
                // have carried, matching every other error result.
                None => (tool.to_string(), None, None),
            },
        };
        // `tool_use_results` stores the transcript's `toolUseResult` side
        // channel (which is a scalar `Error: ...` string on failures), while
        // a pending frame stores the structured host OutputEvent payload.
        // When the model-facing content is unchanged, the explicit frame is
        // authoritative; letting the transcript side channel shadow it
        // would turn `{"error": bare_message}` into an error containing the
        // `<tool_use_error>` presentation wrapper. A substituted row is the
        // exception: its side channel belongs to the accepted synthetic, so
        // it must continue to override the discarded frame.
        let result = if substituted {
            result_from_side_table
        } else {
            pending_result.or(result_from_side_table)
        }
        .unwrap_or_else(|| source_object([("error", content.clone())]));
        let result = if is_error && result.object_entries().is_none() {
            // `toolUseResult` is a scalar `Error: ...` side channel, while the
            // accepted model text may wrap the same message in
            // `<tool_use_error>`. When a substitution makes that side channel
            // authoritative, derive the host error payload from its bare
            // source text instead of copying the presentation wrapper.
            source_tool_error_payload(&result, content)
                .unwrap_or_else(|| source_object([("error", content.clone())]))
        } else {
            result
        };
        let mcp_meta = if substituted {
            None
        } else {
            self.transcript
                .tool_use_mcp_meta
                .lock()
                .await
                .get(id.as_str())
                .cloned()
        };
        let denial_kind = denial_kind_from_side_table.or(pending_denial_kind);
        match denial_kind {
            Some(kind) => {
                self.output
                    .emit_tool_result_denied(id, &tool, content, &result, &kind, None)
                    .await;
            }
            None => {
                self.output
                    .emit_tool_result(id, &tool, content, &result, mcp_meta.as_ref())
                    .await
            }
        }
    }

    /// Record the `toolDenialKind` for a tool that was denied rather than run,
    /// so its `tool_result` user line carries the provenance when persisted.
    ///
    /// Values are claude's: `user-rejected`, `permission-rule`,
    /// `automode-blocked`, `automode-unavailable`, `automode-parsing-error`,
    /// plus the abort kinds `cancelled` / `interrupted`.
    pub(crate) async fn record_tool_denial_kind(
        &self,
        id: &lingxi_core::types::ToolUseId,
        kind: &str,
    ) {
        if let Some(stage) = active_mod_result_stage() {
            let mut stage = stage.lock().await;
            if stage.tool_use_id == id.as_str() {
                stage.denial_kind = Some(kind.to_string());
                return;
            }
        }
        // The `/loop` fold's `tool_denial` / `tool_abort` vetoes. This is the
        // single funnel every denial passes through, so counting here cannot
        // miss one the way a per-call-site count could.
        self.turn_span.note_denial(kind);
        self.transcript
            .tool_denial_kinds
            .lock()
            .await
            .insert(id.to_string(), kind.to_string());
    }

    /// Record a refused tool call for the stream-json `result` frame's
    /// `permission_denials` (oracle schema `LF`).
    ///
    /// Recorded at the SAME funnel as [`Self::record_tool_denial_kind`], and for
    /// the same reason its doc gives: every denial — rule, mode, plan,
    /// classifier, hook override, prompt-transport reject — passes through here,
    /// so a count taken here cannot miss one. Deriving the list from the
    /// `permission_denied` system event instead would miss the three cases
    /// claude-code's own schema doc says that event does not cover.
    ///
    /// Session-scoped and append-only: the result frame reports the whole run.
    pub(crate) async fn record_permission_denial(
        &self,
        tool_name: &str,
        id: &lingxi_core::types::ToolUseId,
        tool_input: &Source,
    ) {
        if let Some(stage) = active_mod_result_stage() {
            let mut stage = stage.lock().await;
            if stage.tool_use_id == id.as_str() {
                if !stage.virtual_tool_call {
                    stage.permission_denial = Some((tool_name.to_string(), tool_input.clone()));
                }
                return;
            }
        }
        self.transcript
            .permission_denials
            .lock()
            .await
            .push(lingxi_core::host::PermissionDenial {
                tool_name: tool_name.to_string(),
                tool_use_id: id.to_string(),
                tool_input: tool_input.clone(),
            });
    }

    /// Every tool call refused this session, in order — read by the stream-json
    /// result builders.
    pub async fn permission_denials(&self) -> Vec<lingxi_core::host::PermissionDenial> {
        self.transcript.permission_denials.lock().await.clone()
    }

    /// Share the denial cell itself, so a transport can read the live list when
    /// it builds its terminal frame instead of being handed a snapshot it might
    /// take at the wrong moment (or forget to take at one emit site out of six).
    #[must_use]
    pub fn permission_denials_handle(
        &self,
    ) -> std::sync::Arc<tokio::sync::Mutex<Vec<lingxi_core::host::PermissionDenial>>> {
        std::sync::Arc::clone(&self.transcript.permission_denials)
    }

    /// Take the recorded kind for a message carrying EXACTLY ONE `tool_result`.
    ///
    /// The single-block guard is claude's own (`Tpr`): a user message with zero
    /// or several tool_results cannot attribute one message-level kind, so it
    /// gets none. Taking (rather than reading) keeps a denial from stamping a
    /// second line if the same result were ever persisted twice.
    pub(crate) async fn take_tool_denial_kind(&self, msg: &ConversationMessage) -> Option<String> {
        let only = Self::sole_tool_result_id(msg)?;
        self.transcript.tool_denial_kinds.lock().await.remove(&only)
    }

    /// claude's `Tpr` guard, factored out so every tool-result head key shares
    /// ONE definition: the id of the message's `tool_result` block when it
    /// carries EXACTLY ONE, else `None`.
    pub(super) fn sole_tool_result_id(msg: &ConversationMessage) -> Option<String> {
        let ConversationMessage::User { content, .. } = msg else {
            return None;
        };
        let mut results = content.iter().filter_map(|b| match b {
            lingxi_core::types::ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id),
            _ => None,
        });
        match (results.next(), results.next()) {
            (Some(only), None) => Some(only.to_string()),
            _ => None,
        }
    }

    /// Record a tool's `toolUseResult` payload for its `tool_result` user line.
    ///
    /// `data` is claude's `se.data` on success (2.1.220 BIN off 235420375) —
    /// the RAW structured result, not the model-facing string — or the plain
    /// string `` `Error: ${message}` `` on the error/denial arms
    /// (BIN off 235424595 / 235400200 / 232972524 / …).
    pub(crate) async fn record_tool_use_result(
        &self,
        id: &lingxi_core::types::ToolUseId,
        data: Source,
    ) {
        if let Some(stage) = active_mod_result_stage() {
            let mut stage = stage.lock().await;
            if stage.tool_use_id == id.as_str() {
                stage.tool_use_result = Some(data);
                return;
            }
        }
        self.transcript
            .tool_use_results
            .lock()
            .await
            .insert(id.to_string(), data);
    }

    /// Record an MCP tool's `mcpMeta` for its `tool_result` user line
    /// (2.1.220 BIN off 232969604 — verbatim on the main chain).
    pub(crate) async fn record_tool_use_mcp_meta(
        &self,
        id: &lingxi_core::types::ToolUseId,
        meta: Source,
    ) {
        if let Some(stage) = active_mod_result_stage() {
            let mut stage = stage.lock().await;
            if stage.tool_use_id == id.as_str() {
                stage.mcp_meta = Some(meta);
                return;
            }
        }
        self.transcript
            .tool_use_mcp_meta
            .lock()
            .await
            .insert(id.to_string(), meta);
    }

    /// Record that a successful tool result should end the current turn once
    /// its persisted/tool-hook boundary has completed.
    pub(crate) async fn record_pending_tool_result_turn_end(
        &self,
        id: &lingxi_core::types::ToolUseId,
        turn_end: tool_api::tool_trait::ToolResultTurnEnd,
    ) {
        if let Some(stage) = active_mod_result_stage() {
            let mut stage = stage.lock().await;
            if stage.tool_use_id == id.as_str() {
                stage.turn_end = Some(turn_end);
                return;
            }
        }
        self.transcript
            .pending_tool_result_turn_end
            .lock()
            .await
            .insert(id.to_string(), turn_end);
    }

    /// Drop result metadata whose real tool outcome was replaced by a
    /// streaming synthetic. The synthetic is an error result and therefore
    /// carries neither the real MCP metadata nor its turn-end request.
    pub(crate) async fn clear_discarded_tool_result_metadata(
        &self,
        id: &lingxi_core::types::ToolUseId,
    ) {
        let key = id.to_string();
        // A fallback may have tombstoned a completed-but-not-yet-persisted
        // result after its PostToolUse hooks queued attachments. Those payloads
        // belong only to the removed result and must not remain available for a
        // later tool with a reused identity or accumulate until session reset.
        self.transcript
            .pending_hook_attachments
            .lock()
            .await
            .remove(&key);
        self.transcript.tool_use_mcp_meta.lock().await.remove(&key);
        self.transcript
            .pending_tool_result_turn_end
            .lock()
            .await
            .remove(&key);
    }

    /// Peek at the requesting assistant line for a result without consuming
    /// the value that transcript serialization must still write as
    /// `sourceToolAssistantUUID`.
    pub(crate) async fn source_tool_assistant_uuid(
        &self,
        id: &lingxi_core::types::ToolUseId,
    ) -> Option<String> {
        self.transcript
            .tool_source_assistant_uuids
            .lock()
            .await
            .get(id.as_str())
            .cloned()
    }

    /// Queue one hook `attachment` payload produced while dispatching `id`.
    ///
    /// Flushed by [`Self::flush_hook_attachments`] right after that tool's
    /// `tool_result` line is written, which is where claude's own stream order
    /// puts it.
    pub(crate) async fn queue_hook_attachment(
        &self,
        id: &lingxi_core::types::ToolUseId,
        projection: lingxi_core::types::utf16_json::Utf16JsonProjection,
        publication_fence: Option<Arc<dyn HookPublicationGuard>>,
    ) {
        if active_mod_result_stage_is_virtual().await {
            return;
        }
        if projection.validate().is_err() {
            tracing::warn!(tool_use_id = %id, "discarding invalid exact hook attachment projection");
            return;
        }
        self.transcript
            .pending_hook_attachments
            .lock()
            .await
            .entry(id.to_string())
            .or_default()
            .push((projection, publication_fence));
    }

    /// Persist (and drain) every attachment queued for `id`.
    pub(crate) async fn flush_hook_attachments(&self, id: &lingxi_core::types::ToolUseId) {
        for (projection, publication_fence) in self.take_queued_hook_attachments(id).await {
            let persist = self.persist_hook_attachment_to_jsonl(projection);
            if let Some(fence) = publication_fence {
                fence.commit_if_current(Box::pin(persist)).await;
            } else {
                persist.await;
            }
        }
    }

    /// Take the recorded `toolUseResult` under the same single-block guard.
    pub(crate) async fn take_tool_use_result(&self, msg: &ConversationMessage) -> Option<Source> {
        let only = Self::sole_tool_result_id(msg)?;
        self.transcript.tool_use_results.lock().await.remove(&only)
    }

    /// Take the recorded `mcpMeta` under the same single-block guard.
    pub(crate) async fn take_tool_use_mcp_meta(&self, msg: &ConversationMessage) -> Option<Source> {
        let only = Self::sole_tool_result_id(msg)?;
        self.transcript.tool_use_mcp_meta.lock().await.remove(&only)
    }

    /// Whether this `tool_result` user message should persist `toolEndsTurn`.
    /// MCP `_meta` termination is represented solely by `mcpMeta`; the oracle
    /// writes this sibling only for a native `ToolResult.endsTurn`.
    async fn tool_result_message_ends_turn(&self, msg: &ConversationMessage) -> bool {
        let Some(only) = Self::sole_tool_result_id(msg) else {
            return false;
        };
        self.transcript
            .pending_tool_result_turn_end
            .lock()
            .await
            .get(&only)
            .is_some_and(|turn_end| {
                turn_end.source == tool_api::tool_trait::ToolResultTurnEndSource::Tool
            })
    }

    /// Drain pending tool-result turn-end requests for the supplied ids.
    ///
    /// Claude Code stores one `toolRequestedEndTurn` scalar and overwrites it
    /// whenever a later result also requests termination. Returning the last
    /// matching id therefore preserves both its source and the one-event
    /// telemetry cardinality for concurrent batches.
    pub(crate) async fn take_pending_tool_result_turn_ends(
        &self,
        ids: &[lingxi_core::types::ToolUseId],
    ) -> Option<tool_api::tool_trait::ToolResultTurnEnd> {
        let mut pending = self.transcript.pending_tool_result_turn_end.lock().await;
        let mut selected = None;
        for id in ids {
            if let Some(turn_end) = pending.remove(&id.to_string()) {
                selected = Some(turn_end);
            }
        }
        selected
    }

    pub async fn seed_last_jsonl_uuid(&self, last_uuid: Option<String>) {
        *self.transcript.last_jsonl_uuid.lock().await = last_uuid;
    }

    /// Convert an in-memory `ConversationMessage` into a `JsonlMessage`.
    ///
    /// `parent_uuid` is the UUID of the prior persisted entry (None for the
    /// first turn). `cwd` is read from the LIVE `current_cwd()` cell (the
    /// post-`cd` shell cwd; falls back to `self.cwd` when no firer is wired).
    /// The `message` payload
    /// is the Anthropic-shaped inner object: for user/assistant we splat
    /// the content blocks through their raw source serializers so provider text
    /// and metadata retain their exact JSON representation.
    ///
    /// Writer-field fidelity (§G gap 4) — mirrors `insertMessageChain`
    /// (`sessionStorage.ts:1039-1064`):
    /// - `git_branch`: the once-per-chain `getBranch()` value (`None` on a
    ///   non-repo), resolved by the caller and threaded in.
    /// - `entrypoint`: `getEntrypoint()` — `"cli"` for this engine (caller-supplied).
    /// - `prompt_id`: `getPromptId()` on `user` lines ONLY; `None` elsewhere. The
    ///   caller passes the in-flight turn's id and we apply it only to `user`.
    /// - `logical_parent_uuid`: compact-boundary back-link. The orchestrator's
    ///   append path is NOT a compaction boundary (compaction replays through a
    ///   separate engine), so this is always `None` here. See the
    ///   `persist_message_to_jsonl` note.
    #[cfg(test)]
    pub(crate) fn to_jsonl_message(
        &self,
        msg: &ConversationMessage,
        session_id: &str,
        parent_uuid: Option<String>,
        git_branch: Option<String>,
        entrypoint: Option<String>,
        prompt_id: Option<String>,
    ) -> session::JsonlMessage {
        self.to_jsonl_message_with_inner_id(
            msg,
            session_id,
            parent_uuid,
            git_branch,
            entrypoint,
            prompt_id,
            None,
            None,
            None,
            None,
            None,
        )
    }

    /// As [`Self::to_jsonl_message`], but allows stamping a shared inner
    /// `message.id` on the persisted line.
    ///
    /// claude-code's streaming writer emits one JSONL line per
    /// `content_block_stop`, each carrying a DISTINCT top-level `uuid` but the
    /// SAME inner Anthropic `message.id` (the `message_start` message id shared
    /// across all blocks of the turn — `claude.ts:1981, 2192-2203`). That shared
    /// inner id is what the loader's parallel-tool-result recovery groups
    /// siblings by (`loader::message_id` → `loader::recover_orphaned_parallel_tool_results`).
    /// When `inner_message_id` is `Some`, it is injected into the assistant
    /// line's inner `message` object as `"id"`. `None` reproduces the prior
    /// (no inner id) shape exactly.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn to_jsonl_message_with_inner_id(
        &self,
        msg: &ConversationMessage,
        session_id: &str,
        parent_uuid: Option<String>,
        git_branch: Option<String>,
        entrypoint: Option<String>,
        prompt_id: Option<String>,
        inner_message_id: Option<&str>,
        // The real-response persist path supplies the response `model` and the
        // raw Anthropic `usage` object, which makes the assistant line carry the
        // full BetaMessage envelope (`{id,type,role,content,model,stop_reason,
        // stop_sequence,usage}`, matching claude-code + the golden fixtures).
        // Both `None` (the synthetic / user / system path) keeps the prior
        // `{role,content}` inner shape.
        assistant_model: Option<&str>,
        assistant_usage: Option<&JsonPayload>,
        // The Anthropic `request-id` response header for a REAL assistant line
        // → the top-level `requestId` field (via `extra`). `None` (synthetic /
        // user / system) omits it, matching claude-code's `requestId: undefined`.
        request_id: Option<&str>,
        // When `Some`, this is a synthetic api-error assistant line: stamp the
        // top-level `isApiErrorMessage`/`error`/`apiErrorStatus` envelope fields
        // (via `extra`) and apply any inner `stop_reason` override. `None`
        // (every non-api-error line) leaves the shape exactly as before.
        api_error: Option<&ApiErrorEnvelope>,
    ) -> session::JsonlMessage {
        let (mut kind, mut inner_message) = match msg {
            ConversationMessage::User { .. } => {
                ("user", serde_json::json!({ "role": "user", "content": [] }))
            }
            ConversationMessage::Assistant { stop_reason, .. } => {
                let inner = if let Some(model) = assistant_model {
                    // Build in the BetaMessage key order (`id` first); the block
                    // below re-stamps the shared `id` idempotently.
                    let mut m = serde_json::Map::new();
                    if let Some(id) = inner_message_id {
                        m.insert("id".to_string(), serde_json::Value::String(id.to_string()));
                    }
                    m.insert(
                        "type".to_string(),
                        serde_json::Value::String("message".to_string()),
                    );
                    m.insert(
                        "role".to_string(),
                        serde_json::Value::String("assistant".to_string()),
                    );
                    m.insert("content".to_string(), serde_json::json!([]));
                    m.insert(
                        "model".to_string(),
                        serde_json::Value::String(model.to_string()),
                    );
                    m.insert(
                        "stop_reason".to_string(),
                        stop_reason
                            .clone()
                            .map_or(serde_json::Value::Null, serde_json::Value::String),
                    );
                    m.insert("stop_sequence".to_string(), serde_json::Value::Null);
                    m.insert("usage".to_string(), serde_json::Value::Null);
                    serde_json::Value::Object(m)
                } else {
                    // SYNTHETIC assistant line. LingXi's only synthetic assistant
                    // persist is the terminal API-error line (conversation.rs
                    // ~4399). claude-code 2.1.238 builds it in `Mqm`
                    // (cc-238 @296633254), whose `message` literal is:
                    //   {diagnostics:null, id, container:null, model:yD,
                    //    role:"assistant", stop_details:null,
                    //    stop_reason:"stop_sequence", stop_sequence:"",
                    //    type:"message", usage:l, content, context_management:null}
                    //
                    // SC-05 — the previous note here was read off a **2.1.185**
                    // binary and was wrong for the current oracle on two counts:
                    //   * `diagnostics:null` exists and is the FIRST key.
                    //   * `usage` is NOT omitted. `Mqm`'s `usage` parameter has a
                    //     DEFAULT — a fully zeroed usage object — so the key is
                    //     always serialized; it never reaches `JSON.stringify` as
                    //     `undefined`. The old "usage is dropped" claim came from
                    //     `tc` passing no argument, which selects that default
                    //     rather than omitting the field.
                    // Key ORDER is load-bearing: these envelopes are compared
                    // byte-for-byte against recorded JSONL.
                    //
                    // `stop_reason` stays hardcoded `"stop_sequence"` (the refusal
                    // path overrides it); the real terminal reason lives in the
                    // OUTER apiError/error fields, which claude-code does not
                    // write into the persisted inner message. `model` is the
                    // `<synthetic>` sentinel (`WR`/`yD`).
                    let mut m = serde_json::Map::new();
                    m.insert("diagnostics".to_string(), serde_json::Value::Null);
                    m.insert(
                        "id".to_string(),
                        serde_json::Value::String(
                            inner_message_id
                                .map_or_else(|| msg.id().as_uuid().to_string(), str::to_string),
                        ),
                    );
                    m.insert("container".to_string(), serde_json::Value::Null);
                    m.insert(
                        "model".to_string(),
                        serde_json::Value::String("<synthetic>".to_string()),
                    );
                    m.insert(
                        "role".to_string(),
                        serde_json::Value::String("assistant".to_string()),
                    );
                    m.insert("stop_details".to_string(), serde_json::Value::Null);
                    m.insert(
                        "stop_reason".to_string(),
                        serde_json::Value::String(
                            // `ql`/`tc` leave the synthetic inner `stop_reason` as
                            // `"stop_sequence"`; the refusal `fje` path overrides
                            // it to `"refusal"` (verified on disk).
                            api_error
                                .and_then(|e| e.inner_stop_reason)
                                .unwrap_or("stop_sequence")
                                .to_string(),
                        ),
                    );
                    m.insert(
                        "stop_sequence".to_string(),
                        serde_json::Value::String(String::new()),
                    );
                    m.insert(
                        "type".to_string(),
                        serde_json::Value::String("message".to_string()),
                    );
                    // SC-05: `Mqm`'s default `usage` literal, key order included.
                    // `output_tokens_details` leads and is null — the same field
                    // 2.1.238 grew a `thinking_tokens` member on (see SC-01).
                    m.insert(
                        "usage".to_string(),
                        serde_json::json!({
                            "output_tokens_details": serde_json::Value::Null,
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0,
                            "server_tool_use": {
                                "web_search_requests": 0,
                                "web_fetch_requests": 0
                            },
                            "service_tier": serde_json::Value::Null,
                            "cache_creation": {
                                "ephemeral_1h_input_tokens": 0,
                                "ephemeral_5m_input_tokens": 0
                            },
                            "inference_geo": serde_json::Value::Null,
                            "iterations": serde_json::Value::Null,
                            "speed": serde_json::Value::Null
                        }),
                    );
                    m.insert("content".to_string(), serde_json::json!([]));
                    m.insert("context_management".to_string(), serde_json::Value::Null);
                    // `stop_reason` from the ConversationMessage is intentionally
                    // not used here (the synthetic envelope hardcodes it).
                    let _ = stop_reason;
                    serde_json::Value::Object(m)
                };
                ("assistant", inner)
            }
            ConversationMessage::System {
                api_system: Some(_),
                ..
            } => (
                "system",
                serde_json::json!({"role":"system","content":null}),
            ),
            ConversationMessage::System { .. } => (
                "system",
                serde_json::json!({ "role": "system", "content": null }),
            ),
        };
        let api_output_config = if let ConversationMessage::System {
            api_system: Some(message),
            ..
        } = msg
        {
            message.output_config.as_ref()
        } else {
            None
        };
        // Stamp the shared inner Anthropic `message.id` on assistant lines so the
        // loader's sibling-grouping (by inner `message.id`) reconstructs the DAG.
        if let (Some(id), Some(obj)) = (inner_message_id, inner_message.as_object_mut()) {
            obj.insert("id".to_string(), serde_json::Value::String(id.to_string()));
        }
        // `promptId` is a USER-line-only field (TS: `type === 'user' ?
        // getPromptId() : undefined`). Drop it on assistant/system lines even
        // when the caller passes one.
        let prompt_id = if kind == "user" { prompt_id } else { None };
        // Use the raw UUID (8-4-4-4-12 lowercase), NOT the `msg.id().to_string()`
        // form which carries the `"msg:"` prefix — that prefix would break the
        // byte-equivalent JSONL schema (see `JsonlMessage::uuid` doc) and the
        // `validate_uuid` regex.
        //
        // `isMeta` is a TOP-LEVEL envelope field in claude-code (a sibling of
        // `message`/`uuid`, emitted at `utils/messages.ts:765,810` and read as an
        // outer field at `session/src/jsonl/title.rs:101`). Emit it ONLY for a
        // meta user message (default-`false` is omitted), so normal lines — and
        // every existing golden fixture — keep their exact byte shape.
        let mut extra = serde_json::Map::new();
        let mut source_extra_fields = Vec::new();
        if matches!(
            msg,
            ConversationMessage::System {
                api_system: Some(_),
                ..
            }
        ) {
            extra.insert("subtype".into(), "api_system".into());
        }
        if let Some(config) = api_output_config {
            source_extra_fields.push((
                "outputConfig",
                Source::parse(&serde_json::to_string(config).expect("typed API output config"))
                    .expect("valid source API output config"),
            ));
        }
        if let ConversationMessage::Assistant {
            per_turn_effort: Some(effort),
            ..
        } = msg
        {
            source_extra_fields.push(("effort", effort.clone()));
            source_extra_fields.push(("perTurnEffort", effort.clone()));
        }
        let invoked_skill_contents = self
            .transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&msg.id())
            .cloned();
        if msg.is_meta() {
            extra.insert("isMeta".to_string(), serde_json::Value::Bool(true));
        }
        if let ConversationMessage::User {
            interrupted_by_shutdown: Some(interrupted_by_shutdown),
            ..
        } = msg
        {
            extra.insert(
                "interruptedByShutdown".to_string(),
                serde_json::Value::Bool(*interrupted_by_shutdown),
            );
        }
        if let ConversationMessage::User {
            refused_message_id: Some(refused_message_id),
            ..
        } = msg
        {
            source_extra_fields.push(("refusedMessageId", refused_message_id.clone()));
        }
        // Top-level `requestId` (the Anthropic `request-id` response header) —
        // claude-code persists it on REAL assistant lines only. The caller
        // passes `Some` from the per-block real-response path; `None` (synthetic
        // / user / system) omits it, matching `requestId: undefined`.
        if let Some(rid) = request_id {
            extra.insert(
                "requestId".to_string(),
                serde_json::Value::String(rid.to_string()),
            );
        }
        // Top-level `effort` (2.1.212): the session's resolved reasoning-effort
        // LEVEL string. claude-code spreads `...effort!==void 0&&{effort}` (the
        // `Y4n(effort).level`) as the last field of the in-memory assistant
        // message object, which persists verbatim into the transcript record —
        // so it lands on REAL assistant lines only, right after `timestamp` and
        // before the `userType`/`cwd` trailer (the serializer places it there).
        // Gated on a REAL response (`assistant_model.is_some()`) so synthetic
        // api-error assistant lines — which claude builds via a different builder
        // with no effort — stay byte-identical. `None` effort omits the field,
        // matching claude's `!==void 0` guard.
        if kind == "assistant" && assistant_model.is_some() {
            if let Some(effort) = self
                .model_runtime
                .current_effort
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                extra.insert(
                    "effort".to_string(),
                    serde_json::Value::String(effort.clone()),
                );
            }
            let selection = self
                .model_runtime
                .current_reasoning_selection
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if !matches!(selection, lingxi_core::host::ReasoningSelection::Automatic) {
                if let Ok(value) = serde_json::to_value(selection) {
                    extra.insert("reasoningSelection".to_string(), value);
                }
            }
        }
        if kind == "assistant" && assistant_model.is_some() {
            if let Some(settings) = crate::scheduled_turn::current() {
                // Actual response settings belong to this turn, not the next
                // human turn's persisted defaults. Replay retains the message
                // and its real attribution while ignoring these defaults.
                extra.insert("perTurnSettings".into(), serde_json::Value::Bool(true));
                extra.remove("effort");
                if let Some(effort) = settings.effort {
                    extra.insert("effort".into(), effort);
                }
                if let Ok(selection) = serde_json::to_value(settings.reasoning) {
                    extra.insert("reasoningSelection".into(), selection);
                }
            }
        }
        // Top-level api-error envelope (`createAssistantAPIErrorMessage`/`fje`):
        // `error` (omitted when the builder took no `error:` arg), the always-on
        // `isApiErrorMessage: true`, and `apiErrorStatus` (set only for an
        // `APIError` with a numeric status). On disk these sit between
        // `requestId` and `userType`. These flow through the `extra` channel;
        // `JsonlMessage`'s hand-written `Serialize` (session/jsonl/schema.rs)
        // now places them in claude's EXACT per-kind outer-key order
        // (api-error head: type, uuid, timestamp, message, requestId?, error?,
        // errorDetails?, truncatedAfterOutput?, isApiErrorMessage,
        // apiErrorStatus?) — presence + values + ORDER are 1:1. See
        // [`ApiErrorEnvelope`].
        if let Some(ae) = api_error {
            if let Some(cat) = ae.error {
                extra.insert(
                    "error".to_string(),
                    serde_json::Value::String(cat.to_string()),
                );
            }
            extra.insert(
                "isApiErrorMessage".to_string(),
                serde_json::Value::Bool(true),
            );
            if ae.truncated_after_output {
                extra.insert(
                    "truncatedAfterOutput".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
            if let Some(status) = ae.api_error_status {
                extra.insert(
                    "apiErrorStatus".to_string(),
                    serde_json::Value::Number(status.into()),
                );
            }
            if let Some(message_id) = ae.refused_message_id.as_ref() {
                extra.insert(
                    "refusedMessageId".to_string(),
                    serde_json::Value::String(message_id.clone()),
                );
            }
        }
        if let ConversationMessage::System {
            content,
            subtype,
            model_fallback,
            refusal_fallback,
            ..
        } = msg
        {
            if let Some(metadata) = refusal_fallback
                .as_ref()
                .filter(|_| subtype.as_deref() == Some("model_refusal_fallback"))
            {
                extra.insert(
                    "subtype".into(),
                    serde_json::json!("model_refusal_fallback"),
                );
                extra.insert("content".into(), serde_json::Value::Null);
                source_extra_fields.push(("content", content.clone()));
                extra.insert("level".into(), serde_json::json!("warning"));
                if let serde_json::Value::Object(fields) =
                    serde_json::to_value(metadata).expect("refusal metadata is JSON")
                {
                    extra.extend(fields);
                }
                // Native `sHo` emits this field explicitly as JSON null when
                // the event has no refusal explanation.
                extra
                    .entry("apiRefusalExplanation")
                    .or_insert(serde_json::Value::Null);
                extra.insert("isMeta".into(), serde_json::json!(false));
                inner_message = serde_json::Value::Null;
            }
            if let Some(metadata) = model_fallback
                .as_ref()
                .filter(|_| subtype.as_deref() == Some("model_fallback"))
            {
                extra.insert("subtype".into(), serde_json::json!("model_fallback"));
                extra.insert("content".into(), serde_json::Value::Null);
                source_extra_fields.push(("content", content.clone()));
                extra.insert("level".into(), serde_json::json!("warning"));
                extra.insert("trigger".into(), serde_json::json!(metadata.trigger));
                extra.insert(
                    "originalModel".into(),
                    serde_json::json!(metadata.original_model),
                );
                extra.insert(
                    "fallbackModel".into(),
                    serde_json::json!(metadata.fallback_model),
                );
                extra.insert("isMeta".into(), serde_json::json!(false));
                inner_message = serde_json::Value::Null;
            }
            if subtype.as_deref() == Some("scheduled_task_fire") {
                if let Some(payload) = content
                    .string_units("")
                    .and_then(|units| Source::parse_utf16_json(&units).ok())
                {
                    extra.insert("subtype".into(), serde_json::json!("scheduled_task_fire"));
                    extra.insert("content".into(), serde_json::Value::Null);
                    source_extra_fields.push((
                        "content",
                        payload
                            .subprojection("/message")
                            .unwrap_or_else(|_| source_null()),
                    ));
                    extra.insert("isMeta".into(), serde_json::json!(false));
                    for key in ["taskId", "cron", "prompt"] {
                        extra.insert(key.into(), serde_json::Value::Null);
                        source_extra_fields.push((
                            key,
                            payload
                                .subprojection(&format!("/{key}"))
                                .unwrap_or_else(|_| source_null()),
                        ));
                    }
                    if payload
                        .get_property("taskKindLoop")
                        .and_then(|value| value.as_bool())
                        == Some(true)
                    {
                        extra.insert("taskKind".into(), serde_json::json!("loop"));
                        extra.insert("cronKind".into(), serde_json::json!("loop"));
                    }
                    if payload
                        .get_property("streak")
                        .and_then(|value| value.as_number())
                        .unwrap_or(0.0)
                        > 0.0
                    {
                        extra.insert("noOpStreak".into(), serde_json::Value::Null);
                        source_extra_fields
                            .push(("noOpStreak", payload.subprojection("/streak").unwrap()));
                        let since = payload
                            .get_property("since_ms")
                            .and_then(|value| value.as_number())
                            .unwrap_or(0.0) as i64;
                        if let Some(time) =
                            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(since)
                        {
                            extra.insert(
                                "streakStartedAt".into(),
                                serde_json::json!(
                                    time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                                ),
                            );
                        }
                        if payload
                            .get_property("foldedUuids")
                            .and_then(|value| value.array_len())
                            .is_some_and(|len| len > 0)
                        {
                            extra.insert("foldedUuids".into(), serde_json::Value::Null);
                            source_extra_fields.push((
                                "foldedUuids",
                                payload.subprojection("/foldedUuids").unwrap(),
                            ));
                        }
                    }
                    inner_message = serde_json::Value::Null;
                }
            }
        }
        let mut source_attachment = None;
        if let Some(attachment) = self
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&msg.id())
            .cloned()
        {
            kind = "attachment";
            inner_message = serde_json::Value::Null;
            extra.clear();
            source_extra_fields.clear();
            source_attachment = Some(attachment);
        }
        let source_content = match msg {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. }
                if matches!(kind, "user" | "assistant") =>
            {
                Some(
                    Source::array(
                        content
                            .iter()
                            .map(|block| {
                                if matches!(
                                    block,
                                    lingxi_core::types::ContentBlock::TextJsUtf16 { .. }
                                ) {
                                    mod_append_content_block(block)
                                } else {
                                    Source::parse(
                                        &serde_json::to_string(block)
                                            .expect("content block serializes"),
                                    )
                                    .expect("serialized content block is valid JSON")
                                }
                            })
                            .collect(),
                    )
                    .expect("content blocks form an array"),
                )
            }
            ConversationMessage::System {
                api_system: Some(message),
                ..
            } if !inner_message.is_null() => Some(
                Source::array(
                    message
                        .content
                        .iter()
                        .map(mod_append_content_block)
                        .collect(),
                )
                .expect("API system content"),
            ),
            ConversationMessage::System { content, .. } if !inner_message.is_null() => {
                Some(content.clone())
            }
            _ => None,
        };
        let mut extra_projection =
            lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &serde_json::Value::Object(extra),
            )
            .expect("generated JSONL extra metadata is valid JSON");
        for (key, value) in source_extra_fields {
            extra_projection
                .set_field(key, value)
                .expect("extra is an object");
        }
        if let Some(contents) = invoked_skill_contents.filter(|_| source_attachment.is_none()) {
            extra_projection
                .set_field(
                    "invokedSkillContents",
                    lingxi_core::types::utf16_json::Utf16JsonProjection::array(contents)
                        .expect("invoked-skill contents form an array"),
                )
                .expect("JSONL extra metadata is an object");
        }
        let mut jsonl_message = session::JsonlMessage {
            message_type: kind.to_string(),
            uuid: msg.id().as_uuid().to_string(),
            parent_uuid,
            session_id: session_id.to_string(),
            timestamp: match msg {
                ConversationMessage::System {
                    refusal_fallback: Some(metadata),
                    ..
                } => metadata.notice_timestamp.clone(),
                _ => None,
            }
            .unwrap_or_else(|| {
                chrono::Utc::now()
                    .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                    .to_string()
            }),
            // Per-line cwd readback — the LIVE session cwd (advanced by a Bash
            // `cd` via the shared `current_cwd` cell), NOT the static init cwd.
            // 1:1 with claude-code, which stamps `getCwd()` on every persisted
            // line and where `cd` mutates that single global cwd. Falls back to
            // the static `cwd` when no firer is wired (the cell never moves).
            cwd: self.current_cwd().to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            message: lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &inner_message,
            )
            .expect("generated JSONL inner message is valid JSON"),
            attachment_projection: source_attachment,
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch,
            entrypoint,
            // Plan-slug cache is not wired in this engine — TS reads
            // `getPlanSlugCache().get(sessionId)`, which is `undefined` for any
            // session without a stored plan slug. We have no such cache, so this
            // is always omitted (matches the common TS path).
            slug: None,
            prompt_id: prompt_id.filter(|_| kind == "user"),
            // Always `None` from this append path — see the doc comment above and
            // the `persist_message_to_jsonl` note.
            logical_parent_uuid: None,
            extra: extra_projection,
        };
        if let ConversationMessage::User {
            turn_companion,
            usage_limit_note,
            origin,
            turn_origin,
            ..
        } = msg
        {
            jsonl_message.turn_companion = *turn_companion;
            jsonl_message.usage_limit_note = *usage_limit_note;
            jsonl_message.origin_projection = origin.clone();
            jsonl_message.turn_origin_projection = turn_origin.clone();
        }
        if let ConversationMessage::Assistant {
            resumable: Some(resumable),
            ..
        } = msg
        {
            jsonl_message
                .message
                .set_field("resumable", resumable.clone())
                .expect("assistant message is an object");
        }
        if kind == "assistant" && assistant_model.is_some() {
            if let Some(usage) = assistant_usage {
                jsonl_message
                    .message
                    .set_field(
                        "usage",
                        lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                            usage.as_json_source(),
                        )
                        .expect("SDK usage carries valid JSON"),
                    )
                    .expect("assistant message is an object");
            }
        }
        if let Some(content) = source_content {
            jsonl_message
                .message
                .set_field("content", content)
                .expect("message is an object");
        }
        jsonl_message
    }

    /// Resolve the cwd's git branch ONCE and cache it — the parity analog of TS
    /// `getBranch()` (`sessionStorage.ts:1012-1019`), which is called per
    /// `insertMessageChain` and stamped on every line. We resolve lazily on the
    /// first append and memoize, so subsequent appends pay nothing.
    ///
    /// Reuses the loader's shell-git pattern (`std::process::Command`, no new
    /// dependency): `git rev-parse --abbrev-ref HEAD` in `self.cwd`. Returns
    /// `None` on ANY failure (git missing, not a repo, non-zero exit, detached
    /// HEAD reporting `"HEAD"`), matching TS's `try { getBranch() } catch {
    /// undefined }` — a `None` is then omitted from the JSONL line.
    pub(super) async fn resolve_git_branch(&self) -> Option<String> {
        if self.prompt_runtime.host_environment_probes_disabled {
            return None;
        }
        {
            let cache = self.transcript.git_branch_cache.lock().await;
            if let Some(resolved) = cache.as_ref() {
                return resolved.clone();
            }
        }
        let resolved = git_branch_for_cwd(&self.cwd);
        *self.transcript.git_branch_cache.lock().await = Some(resolved.clone());
        resolved
    }

    /// Reserve the identity shared by inherited fork rows and the next real
    /// user prompt. Repeated calls before that prompt reuse the same value.
    pub async fn reserve_next_prompt_id(&self) -> String {
        self.prompt_runtime
            .pending_prompt_id
            .lock()
            .await
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone()
    }

    /// The stable per-turn `promptId` for `msg` — the parity analog of
    /// `getPromptId()` (`sessionStorage.ts:1045`).
    ///
    /// TS stamps the SAME prompt id on the user prompt line AND every
    /// `tool_result` `user` line of the turn. We reproduce that through the
    /// single append chokepoint: a genuine new user prompt (a `user` message
    /// that is NOT a `tool_result` carrier) MINTS a fresh UUID into
    /// `current_prompt_id`; a `tool_result` `user` line REUSES the cached id; any
    /// non-`user` message returns `None` (the caller / `to_jsonl_message` also
    /// guards this, so the field never lands on assistant/system lines).
    async fn prompt_id_for_message(&self, msg: &ConversationMessage) -> Option<String> {
        if self
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&msg.id())
        {
            return None;
        }
        let ConversationMessage::User { content, .. } = msg else {
            // Non-user line — no promptId (mirrors `type === 'user' ? … :
            // undefined`). Leave the cached turn id untouched.
            return None;
        };
        let is_tool_result_carrier = content
            .iter()
            .any(|b| matches!(b, lingxi_core::types::ContentBlock::ToolResult { .. }));
        let mut slot = self.prompt_runtime.current_prompt_id.lock().await;
        if is_tool_result_carrier {
            // Continuation of the in-flight turn — reuse the current id. If none
            // exists yet (defensive: a tool_result persisted before any prompt),
            // mint one so the field is still populated.
            if slot.is_none() {
                *slot = Some(uuid::Uuid::new_v4().to_string());
            }
        } else {
            // Genuine new user prompt — start a fresh prompt id for this turn.
            *slot = Some(
                self.prompt_runtime
                    .pending_prompt_id
                    .lock()
                    .await
                    .take()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            );
        }
        slot.clone()
    }

    /// Persist a single message to the optional JSONL writer.
    ///
    /// Best-effort: write failures are logged via the telemetry
    /// `tengu_session_corrupted` event and emit one sanitized user-visible
    /// warning per session, but never fail the turn. On success, emits
    /// `tengu_session_appended` and updates the `last_jsonl_uuid` cache.
    pub(crate) async fn persist_message_to_jsonl(&self, msg: &ConversationMessage) {
        self.persist_message_to_jsonl_with_parent(msg, None).await;
    }

    /// Append an SDK/stream-json supplied assistant or system history entry to
    /// the live session and its transcript. This is intentionally not routed
    /// through a model turn: the next user message observes the seeded history
    /// exactly once and input ordering remains owned by the caller.
    pub async fn append_external_history_message(&self, msg: ConversationMessage) {
        // SDK history/Bash inputs arrive between model turns, but background
        // Fusion publication and session switches still run concurrently.
        // Own the same gate before choosing a session or transcript parent.
        // Do not acquire it inside persistence helpers: model turns already
        // hold it when calling those helpers.
        let _turn_guard = self.turn_gate.lock().await;
        let compact_metadata = match &msg {
            ConversationMessage::System {
                subtype: Some(subtype),
                compact_metadata: Some(metadata),
                ..
            } if subtype == "compact_boundary" => Some(metadata.clone()),
            _ => None,
        };
        {
            let mut session = self.session.lock().await;
            session.history.push(msg.clone());
        }
        if let Some(metadata) = compact_metadata {
            let rewritten = self.mod_session_append_row(&msg, None, true, None).await;
            self.persist_compact_boundary_to_jsonl(&rewritten, &metadata)
                .await;
        } else {
            self.persist_message_to_jsonl(&msg).await;
        }
    }

    /// Persist with an optional explicit `parentUuid` override.
    ///
    /// The streaming executor passes the originating assistant message's UUID so
    /// each tool result parents to the assistant that requested it (TS
    /// `sourceToolAssistantUUID`), rather than the linear `last_jsonl_uuid` chain.
    ///
    /// When `parent_override` is `None`, behaves exactly as before (chain off
    /// `last_jsonl_uuid`). In BOTH cases the `last_jsonl_uuid` cache is advanced
    /// to this line's UUID so any subsequent non-overridden line chains correctly.
    pub(crate) async fn persist_message_to_jsonl_with_parent(
        &self,
        msg: &ConversationMessage,
        parent_override: Option<String>,
    ) {
        // Android Computer Use screenshots must reach the current model but
        // must not be written to the durable JSONL transcript. The tool marks
        // only those results with `_lingxi_ephemeral`; every existing desktop
        // and mobile result remains byte-identical.
        let sanitized = redact_ephemeral_tool_result_images(msg);
        self.persist_message_to_jsonl_inner(&sanitized, parent_override, None, false)
            .await;
    }

    /// Append an accepted tool-result row through `session.append`, then hold
    /// the generation commit lease only for the durable transcript/state
    /// mutation. This keeps reset able to cancel the hook dispatch while making
    /// an admitted JSONL write finish before reset returns.
    pub(crate) async fn persist_guarded_message_to_jsonl_with_parent(
        &self,
        msg: &ConversationMessage,
        parent_override: Option<String>,
        publication_fence: Arc<dyn HookPublicationGuard>,
    ) {
        let sanitized = redact_ephemeral_tool_result_images(msg);
        let rewritten = self
            .mod_session_append_row(&sanitized, None, true, Some(publication_fence.clone()))
            .await;
        if !publication_fence.is_current() {
            return;
        }
        let persist = self.persist_message_to_jsonl_inner_with_queue_metadata(
            &rewritten,
            parent_override,
            None,
            false,
            None,
            None,
            true,
        );
        publication_fence.commit_if_current(Box::pin(persist)).await;
    }

    /// Append a host-injected prompt row through the current `session.append`
    /// middleware and publish its accepted history/transcript projection under
    /// the same originating generation fence. The row is visible to the
    /// callback first, matching the existing append order; reset can cancel the
    /// callback and prevents its accepted rewrite or JSONL write from escaping.
    pub(crate) async fn append_guarded_injected_message(
        &self,
        message: &ConversationMessage,
        source_id: lingxi_core::types::ToolUseId,
        publication_guard: Arc<dyn HookPublicationGuard>,
    ) -> bool {
        if !publication_guard.is_current() {
            return false;
        }
        let original = message.clone();
        let guard_for_row = Arc::clone(&publication_guard);
        let append_history = async {
            {
                let mut session = self.session.lock().await;
                session.history.push(original.clone());
                session
                    .injected_message_sources
                    .insert(original.id(), source_id);
            }
            self.prompt_runtime
                .remember_guarded_prompt_message(original.id(), guard_for_row)
                .await;
        };
        if !publication_guard
            .commit_if_current(Box::pin(append_history))
            .await
        {
            return false;
        }

        // `user_meta` is an ephemeral view of the hook attachment already
        // persisted by its owner. Native does not pass that view back through
        // the ordinary session.append JSONL path.
        if original.is_meta() {
            return true;
        }

        let accepted = self
            .mod_session_append_row(&original, None, false, Some(Arc::clone(&publication_guard)))
            .await;
        if !publication_guard.is_current() {
            return false;
        }
        let accepted_for_commit = accepted.clone();
        let commit = async {
            self.replace_session_history_row(original.id(), accepted_for_commit.clone())
                .await;
            self.persist_message_to_jsonl_inner_with_queue_metadata(
                &accepted_for_commit,
                None,
                None,
                false,
                None,
                None,
                true,
            )
            .await;
        };
        publication_guard.commit_if_current(Box::pin(commit)).await
    }

    /// Run the stream append bridge and return the accepted row that Native's
    /// through-wrapper yields to the query. The caller uses this same row for
    /// W1/K/JE and its later preappended storage journal; session history is
    /// updated only by the query-row owner.
    pub(crate) async fn append_streamed_query_row(
        &self,
        raw: &ConversationMessage,
        publication_guard: Option<Arc<dyn HookPublicationGuard>>,
    ) -> ConversationMessage {
        // Native increments its query-local num_turns counter when this
        // through-wrapper yields a user row. Count here instead of diffing
        // session history: compaction can rebuild IDs, and fallback cleanup
        // can later remove already-yielded attempt rows without undoing that
        // event.
        let current_before = publication_guard
            .as_ref()
            .is_none_or(|guard| guard.is_current());
        let stored = self
            .mod_session_append_row(raw, None, false, publication_guard.clone())
            .await;
        let current_after = publication_guard
            .as_ref()
            .is_none_or(|guard| guard.is_current());
        if current_before && current_after && matches!(&stored, ConversationMessage::User { .. }) {
            self.note_query_user_event();
        }
        stored
    }

    /// Commit the canonical merged assistant turn to session history while
    /// preserving the same generation fence used by its accepted stream rows.
    /// The message ID is retained in the existing private request-admission
    /// carrier so a reset between history append and the next Main request
    /// cannot send this stale assistant row.
    pub(crate) async fn append_streamed_assistant_to_history(
        &self,
        assistant: &ConversationMessage,
        publication_guard: Option<Arc<dyn HookPublicationGuard>>,
        note_assistant_commit: bool,
    ) -> bool {
        let mut message = assistant.clone();
        if self.is_persistent_app_chat() {
            canonicalize_app_chat_assistant_text(&mut message);
        }
        let guard_for_commit = publication_guard.clone();
        let commit = async {
            self.session.lock().await.history.push(message.clone());
            if let Some(guard) = guard_for_commit {
                self.prompt_runtime
                    .remember_guarded_prompt_message(message.id(), guard)
                    .await;
            }
            if note_assistant_commit {
                self.note_assistant_stream_commit(&message).await;
            }
        };
        if let Some(guard) = publication_guard {
            guard.commit_if_current(Box::pin(commit)).await
        } else {
            commit.await;
            true
        }
    }

    /// Serialize an accepted stream row once the recorder can advance past it.
    /// Its append hook already ran before the stream yielded the accepted row.
    pub(crate) async fn persist_preappended_stream_row(
        &self,
        stored: &ConversationMessage,
        parent_override: Option<String>,
        timestamp: &str,
        publication_guard: Option<std::sync::Arc<dyn HookPublicationGuard>>,
    ) {
        let sanitized = redact_ephemeral_tool_result_images(stored);
        let persist = self.persist_message_to_jsonl_inner_with_queue_metadata(
            &sanitized,
            parent_override,
            None,
            false,
            None,
            Some(PreappendedRowMetadata {
                timestamp,
                api_error: None,
            }),
            false,
        );
        if let Some(guard) = publication_guard {
            guard.commit_if_current(Box::pin(persist)).await;
        } else {
            persist.await;
        }
    }

    /// Record the accepted append projection with the host-created error row's
    /// original identity, creation time and complete synthetic envelope.
    pub(crate) async fn persist_preappended_server_fallback_api_error_row(
        &self,
        stored: &ConversationMessage,
        row: &lingxi_core::host::ServerFallbackApiErrorRow,
    ) {
        self.persist_message_to_jsonl_inner_with_queue_metadata(
            stored,
            None,
            None,
            false,
            None,
            Some(PreappendedRowMetadata {
                timestamp: &row.timestamp,
                api_error: Some(row),
            }),
            false,
        )
        .await;
    }

    /// Persist one queued user message's host-only envelope metadata. Keeping
    /// queue metadata out of ConversationMessage keeps it out of model input.
    pub(super) async fn persist_queued_message_to_jsonl(
        &self,
        msg: &ConversationMessage,
        input: &QueuedPromptInput,
    ) {
        let sanitized = redact_ephemeral_tool_result_images(msg);
        self.persist_message_to_jsonl_inner_with_queue_metadata(
            &sanitized,
            None,
            None,
            false,
            Some(input),
            None,
            false,
        )
        .await;
    }

    /// Append a checkpointed user row by its stable pending-prompt identity.
    /// Unlike ordinary history writes, any persistence error aborts the turn
    /// before session history or a model request can advance.
    pub(super) async fn persist_pending_prompt_queued_message_once(
        &self,
        msg: &ConversationMessage,
        input: &QueuedPromptInput,
        prompt_id: &str,
    ) -> Result<ConversationMessage, OrchestratorError> {
        let sanitized = redact_ephemeral_tool_result_images(msg);
        self.persist_message_to_jsonl_inner_with_pending_prompt_identity(
            &sanitized,
            None,
            None,
            false,
            Some(input),
            None,
            false,
            Some(prompt_id),
        )
        .await?
        .ok_or_else(|| {
            OrchestratorError::Internal(
                "pending prompt append completed without a source message".into(),
            )
        })
    }

    fn pending_text_display_matches_source(text: &str, units: &[u16]) -> bool {
        String::from_utf16_lossy(units) == text
    }

    fn pending_prompt_content_block_matches_source(
        persisted: &lingxi_core::types::ContentBlock,
        retry: &lingxi_core::types::ContentBlock,
    ) -> bool {
        use lingxi_core::types::ContentBlock;

        match (persisted, retry) {
            (
                ContentBlock::Text {
                    text: persisted_text,
                    citations: persisted_citations,
                },
                ContentBlock::Text {
                    text: retry_text,
                    citations: retry_citations,
                },
            ) => persisted_text == retry_text && persisted_citations == retry_citations,
            (
                ContentBlock::Text {
                    text,
                    citations: text_citations,
                },
                ContentBlock::TextJsUtf16 {
                    text: display,
                    utf16_code_units,
                    citations: source_citations,
                },
            )
            | (
                ContentBlock::TextJsUtf16 {
                    text: display,
                    utf16_code_units,
                    citations: source_citations,
                },
                ContentBlock::Text {
                    text,
                    citations: text_citations,
                },
            ) => {
                Self::pending_text_display_matches_source(display, utf16_code_units)
                    && text.encode_utf16().eq(utf16_code_units.iter().copied())
                    && text_citations == source_citations
            }
            (
                ContentBlock::TextJsUtf16 {
                    text: persisted_display,
                    utf16_code_units: persisted_units,
                    citations: persisted_citations,
                },
                ContentBlock::TextJsUtf16 {
                    text: retry_display,
                    utf16_code_units: retry_units,
                    citations: retry_citations,
                },
            ) => {
                Self::pending_text_display_matches_source(persisted_display, persisted_units)
                    && Self::pending_text_display_matches_source(retry_display, retry_units)
                    && persisted_units == retry_units
                    && persisted_citations == retry_citations
            }
            _ => persisted == retry,
        }
    }

    fn pending_prompt_message_matches_source(
        persisted: &lingxi_core::types::ConversationMessage,
        retry: &lingxi_core::types::ConversationMessage,
    ) -> bool {
        use lingxi_core::types::ConversationMessage;

        match (persisted, retry) {
            (
                ConversationMessage::User {
                    id: persisted_id,
                    content: persisted_content,
                    is_meta: persisted_meta,
                    is_compact_summary: persisted_compact,
                    is_visible_in_transcript_only: persisted_transcript_only,
                    turn_companion: persisted_companion,
                    usage_limit_note: persisted_usage_note,
                    origin: persisted_origin,
                    turn_origin: persisted_turn_origin,
                    interrupted_by_shutdown: persisted_shutdown,
                    refused_message_id: persisted_refused_message_id,
                    api_message_override: persisted_override,
                },
                ConversationMessage::User {
                    id: retry_id,
                    content: retry_content,
                    is_meta: retry_meta,
                    is_compact_summary: retry_compact,
                    is_visible_in_transcript_only: retry_transcript_only,
                    turn_companion: retry_companion,
                    usage_limit_note: retry_usage_note,
                    origin: retry_origin,
                    turn_origin: retry_turn_origin,
                    interrupted_by_shutdown: retry_shutdown,
                    refused_message_id: retry_refused_message_id,
                    api_message_override: retry_override,
                },
            ) => {
                persisted_id == retry_id
                    && persisted_meta == retry_meta
                    && persisted_compact == retry_compact
                    && persisted_transcript_only == retry_transcript_only
                    && persisted_companion == retry_companion
                    && persisted_usage_note == retry_usage_note
                    && persisted_origin == retry_origin
                    && persisted_turn_origin == retry_turn_origin
                    && persisted_shutdown == retry_shutdown
                    && persisted_refused_message_id == retry_refused_message_id
                    && persisted_override == retry_override
                    && persisted_content.len() == retry_content.len()
                    && persisted_content
                        .iter()
                        .zip(retry_content)
                        .all(|(left, right)| {
                            Self::pending_prompt_content_block_matches_source(left, right)
                        })
            }
            _ => false,
        }
    }

    /// Publish a pending user row to live history only after its durable
    /// append succeeded. Cold retry is idempotent by message UUID and rejects
    /// a different source projection for that UUID.
    pub(super) async fn append_pending_prompt_message_to_history_once(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), OrchestratorError> {
        let mut session = self.session.lock().await;
        if let Some(existing) = session
            .history
            .iter()
            .find(|existing| existing.id() == msg.id())
        {
            let matches_source = match (existing, msg) {
                (ConversationMessage::User { .. }, ConversationMessage::User { .. }) => {
                    Self::pending_prompt_message_matches_source(existing, msg)
                }
                _ => existing == msg,
            };
            return if matches_source {
                Ok(())
            } else {
                Err(OrchestratorError::Internal(format!(
                    "pending prompt history UUID {} has different source content",
                    msg.id()
                )))
            };
        }
        session.history.push(msg.clone());
        Ok(())
    }

    /// Persist a synthetic api-error assistant line, stamping the top-level
    /// `isApiErrorMessage`/`error`/`apiErrorStatus` envelope (and any inner
    /// `stop_reason` override) from `env`. 1:1 with claude-code's
    /// `createAssistantAPIErrorMessage` (`ql`/`tc`) and refusal (`fje`) lines.
    pub(crate) async fn persist_api_error_message_to_jsonl(
        &self,
        msg: &ConversationMessage,
        env: ApiErrorEnvelope,
    ) {
        self.persist_message_to_jsonl_inner(msg, None, Some(env), false)
            .await;
    }

    /// Record a best-effort transcript write failure. The raw error remains in
    /// the local diagnostic log; the upstream telemetry event has an empty
    /// payload and must not receive paths, session ids, or backend details.
    pub(super) async fn record_transcript_append_failure(
        &self,
        session_id: &str,
        operation: &'static str,
        error: &(impl std::fmt::Display + ?Sized),
    ) {
        // Ordinary append failures remain diagnostic. A full disk also pauses
        // this session so pending history can be saved before work continues.
        tracing::error!(error = %error, operation, "jsonl writer append failed");
        telemetry::emit_session_persistence_failed();
        if cost::persistence::is_disk_full(&error.to_string()) {
            self.mark_storage_paused(session_id.to_string()).await;
        }
    }

    /// Persist ONE hook-run `attachment` transcript line.
    ///
    /// claude-code writes exactly one `type:"attachment"` line per hook run
    /// (26 048 such records mined from real 2.1.220 transcripts under
    /// `~/.claude/projects`). The outer envelope puts the payload BEFORE the
    /// discriminator — `parentUuid, isSidechain, attachment, type, uuid,
    /// timestamp, …trailer` — and carries NO inner `message`; that ordering is
    /// implemented by the attachment arm of
    /// [`session::jsonl::schema::JsonlMessage`]'s hand-written `Serialize`.
    ///
    /// `payload` is the value built by [`hooks::attachment`] (`hook_success` /
    /// `hook_non_blocking_error` / `hook_cancelled`). Best-effort like every
    /// other JSONL append; advances `last_jsonl_uuid` on success so the next
    /// line chains off it.
    pub async fn persist_hook_attachment_to_jsonl(&self, payload: Source) {
        self.persist_attachment_to_jsonl(payload, source_object([]), "hook_attachment")
            .await;
    }

    /// Persist a hook attachment under the identity of its derived model
    /// message. This keeps the live additional-context row aligned with cold
    /// JSONL restoration, whose renderer keys that message by attachment UUID.
    pub(crate) async fn persist_hook_attachment_to_jsonl_with_id(
        &self,
        id: lingxi_core::types::MessageId,
        payload: Source,
    ) {
        self.persist_attachment_to_jsonl_with_id(
            id.as_uuid(),
            payload,
            source_object([]),
            "hook_attachment",
        )
        .await;
    }

    /// Persist one raw UserPromptExpansion row with its checkpointed UUID.
    /// The transaction compares its exact source attachment and returns the
    /// actual transcript tail so a retry cannot move the live cursor backward.
    pub(super) async fn persist_pending_prompt_attachment_once(
        &self,
        id: lingxi_core::types::MessageId,
        payload: Source,
    ) -> Result<bool, OrchestratorError> {
        if active_mod_result_stage_is_virtual().await {
            return Err(OrchestratorError::Internal(
                "pending prompt attachment cannot be persisted in a virtual Mod stage".into(),
            ));
        }
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return Err(OrchestratorError::Internal(
                "pending prompt attachment requires a durable transcript writer".into(),
            ));
        };
        let session_id = self.session.lock().await.session_id.as_uuid().to_string();
        let row = session::JsonlMessage {
            message_type: "attachment".into(),
            uuid: id.as_uuid().to_string(),
            parent_uuid: None,
            session_id: session_id.clone(),
            timestamp: chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
            cwd: self.current_cwd().to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").into(),
            message: Source::parse("null").expect("null is valid JSON"),
            attachment_projection: Some(payload),
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch: self.resolve_git_branch().await,
            entrypoint: Some(entrypoint_value()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: source_object([]),
        };
        let (outcome, last_uuid) = writer
            .append_pending_prompt_message_once(&row)
            .await
            .map_err(|error| {
                OrchestratorError::Internal(format!(
                    "pending prompt attachment append failed: {error}"
                ))
            })?;
        let last_uuid = last_uuid.ok_or_else(|| {
            OrchestratorError::Internal(
                "pending prompt attachment append completed without a transcript tip".into(),
            )
        })?;
        *self.transcript.last_jsonl_uuid.lock().await = Some(last_uuid);
        let appended = outcome == session::jsonl::TranscriptAppendOutcome::Appended;
        if appended {
            telemetry::emit_session_appended(&session_id, &row.uuid);
        }
        Ok(appended)
    }

    /// Persist a future-owned request-context sidecar with Native's rendered
    /// transcript view. The attachment stays outside conversational history.
    pub(crate) async fn persist_request_context_attachment_to_jsonl(
        &self,
        attachment: crate::RequestContextTranscriptAttachment,
    ) {
        let extra = source_object([
            ("rendered", attachment.rendered),
            ("renderedRole", Source::from(attachment.rendered_role)),
        ]);
        self.persist_attachment_to_jsonl_with_id(
            attachment.message_id.as_uuid(),
            attachment.payload,
            extra,
            "request_context_attachment",
        )
        .await;
    }

    /// Persist a model-facing prompt-ingress row with the exact Native rendered
    /// cache. Unlike a request-only sidecar, its JSONL UUID is also the
    /// `ConversationMessage` identity retained in history and outgoing request
    /// projections.
    pub(crate) async fn persist_prompt_ingress_attachment_to_jsonl(
        &self,
        id: lingxi_core::types::MessageId,
        payload: Source,
        rendered: Source,
        rendered_role: &'static str,
    ) {
        if active_mod_result_stage_is_virtual().await {
            return;
        }
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return;
        };
        let (session_id, parent_uuid) = {
            let session = self.session.lock().await;
            (
                session.session_id.as_uuid().to_string(),
                self.transcript.last_jsonl_uuid.lock().await.clone(),
            )
        };
        let git_branch = self.resolve_git_branch().await;
        let marked_before_closing_line = {
            let session = self.session.lock().await;
            session
                .native_request_history
                .before_closing_line_ids
                .contains(&id)
                && payload
                    .string_at("/type")
                    .is_some_and(crate::conversation::closing_placement::is_tee_attachment_type)
        };
        let mut extra = source_object([
            ("rendered", rendered),
            ("renderedRole", Source::from(rendered_role)),
        ]);
        if marked_before_closing_line {
            extra
                .set_field("placement", Source::from("before_closing_line"))
                .expect("extra metadata is an object");
        }
        let row = session::JsonlMessage {
            message_type: "attachment".into(),
            uuid: id.as_uuid().to_string(),
            parent_uuid,
            session_id: session_id.clone(),
            timestamp: chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
            cwd: self.current_cwd().to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").into(),
            message: Source::parse("null").expect("null is valid JSON"),
            attachment_projection: Some(payload),
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch,
            entrypoint: Some(entrypoint_value()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra,
        };
        match self.append_transcript_row(&writer, &row).await {
            Ok(()) => {
                #[cfg(test)]
                crate::request_context::pause_after_first_ingress_append_for_test().await;
                *self.transcript.last_jsonl_uuid.lock().await = Some(row.uuid.clone());
                telemetry::emit_session_appended(&session_id, &row.uuid);
            }
            Err(error) => {
                self.record_transcript_append_failure(
                    &session_id,
                    "prompt_ingress_attachment",
                    &error,
                )
                .await;
            }
        }
    }

    async fn persist_attachment_to_jsonl(
        &self,
        payload: Source,
        extra: Source,
        operation: &'static str,
    ) {
        self.persist_attachment_to_jsonl_with_id(uuid::Uuid::new_v4(), payload, extra, operation)
            .await;
    }

    async fn persist_attachment_to_jsonl_with_id(
        &self,
        uuid: uuid::Uuid,
        payload: Source,
        extra: Source,
        operation: &'static str,
    ) {
        if active_mod_result_stage_is_virtual().await {
            return;
        }
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return;
        };
        let session_id_str = self.session.lock().await.session_id.as_uuid().to_string();
        let parent_uuid = self.transcript.last_jsonl_uuid.lock().await.clone();
        let git_branch = self.resolve_git_branch().await;
        let jmsg = session::JsonlMessage {
            message_type: "attachment".to_string(),
            uuid: uuid.to_string(),
            parent_uuid,
            session_id: session_id_str.clone(),
            timestamp: chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
            cwd: self.current_cwd().to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            // Attachment lines carry NO inner `message`.
            message: lingxi_core::types::utf16_json::Utf16JsonProjection::parse("null")
                .expect("null is valid JSON"),
            attachment_projection: Some(payload),
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch,
            entrypoint: Some(entrypoint_value()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra,
        };
        let line_uuid = jmsg.uuid.clone();
        match self.append_transcript_row(&writer, &jmsg).await {
            Ok(()) => {
                #[cfg(test)]
                crate::request_context::pause_after_first_ingress_append_for_test().await;
                *self.transcript.last_jsonl_uuid.lock().await = Some(line_uuid.clone());
                telemetry::emit_session_appended(&session_id_str, &line_uuid);
            }
            Err(error) => {
                self.record_transcript_append_failure(&session_id_str, operation, &error)
                    .await;
            }
        }
    }

    /// A host-queued human persists as a rendered attachment, independently
    /// of its transient model/history projection. No synthetic USER row is written.
    pub(super) async fn persist_queued_input_to_jsonl(
        &self,
        prompt: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        source_uuid: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
        delivery: &crate::prompt::mid_turn_input::MidTurnInputDelivery,
        rendered: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) {
        if active_mod_result_stage_is_virtual().await {
            return;
        }
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return;
        };
        let session_id = self.session.lock().await.session_id.as_uuid().to_string();
        let mut attachment = source_object([
            ("type", Source::from("queued_command")),
            ("prompt", prompt.clone()),
        ]);
        if let Some(source_uuid) = source_uuid {
            attachment
                .set_field("source_uuid", source_uuid.clone())
                .expect("validated source UUID");
        }
        attachment
            .set_field("delivery_id", delivery.delivery_id.as_str().into())
            .expect("delivery identity");
        attachment
            .set_field("commandMode", "prompt".into())
            .expect("prompt mode");
        attachment
            .set_field("timestamp", delivery.timestamp.as_str().into())
            .expect("admission timestamp");
        let extra = source_object([
            ("rendered", rendered.clone()),
            ("renderedRole", Source::from("system")),
        ]);
        let row = session::JsonlMessage {
            message_type: "attachment".into(),
            uuid: uuid::Uuid::new_v4().to_string(),
            parent_uuid: self.transcript.last_jsonl_uuid.lock().await.clone(),
            session_id: session_id.clone(),
            timestamp: delivery.timestamp.clone(),
            cwd: self.current_cwd().to_string_lossy().into_owned(),
            version: delivery.reference_version.clone(),
            message: source_null(),
            attachment_projection: Some(attachment),
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch: self.resolve_git_branch().await,
            entrypoint: Some("sdk-cli".into()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra,
        };
        match self.append_transcript_row(&writer, &row).await {
            Ok(()) => {
                *self.transcript.last_jsonl_uuid.lock().await = Some(row.uuid.clone());
                telemetry::emit_session_appended(&session_id, &row.uuid);
            }
            Err(error) => {
                self.record_transcript_append_failure(&session_id, "queued_command", &error)
                    .await
            }
        }
    }

    pub(super) async fn persist_absorbed_queue_input(
        &self,
        input: &crate::prompt::mid_turn_input::MidTurnInput,
    ) {
        let (Some(writer), Some(delivery)) = (
            self.transcript.jsonl_writer.as_ref(),
            input.queue_delivery.as_ref(),
        ) else {
            return;
        };
        let session_id = self.session.lock().await.session_id.as_uuid().to_string();
        let prompt = input
            .projected_content
            .clone()
            .unwrap_or_else(|| input.text.clone());
        let mut record = source_object([
            ("type", "queue-operation".into()),
            ("operation", "remove".into()),
            (
                "timestamp",
                chrono::Utc::now()
                    .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                    .to_string()
                    .into(),
            ),
            ("sessionId", session_id.clone().into()),
            ("content", prompt),
            ("reason", "absorbed_mid_turn".into()),
        ]);
        if let Some(uuid) = &input.source_message_uuid {
            record
                .set_field("commandUuid", uuid.clone())
                .expect("source UUID");
        }
        record
            .set_field("deliveryId", delivery.delivery_id.as_str().into())
            .expect("delivery identity");
        if let Err(error) = writer.append_queue_operation(&record).await {
            self.record_transcript_append_failure(&session_id, "queue-operation", &error)
                .await;
        }
    }

    /// Persist before retry, while the rejected snapshot is still the transcript
    /// tail. The scope owns these handles so lazy streams can await durability
    /// after the caller's task-local scope has ended.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn persist_thinking_recovery_snapshot(
        writer: Arc<JsonlWriter>,
        last_uuid: Arc<Mutex<Option<String>>>,
        session: Arc<Mutex<SessionState>>,
        expected_session: lingxi_core::types::SessionId,
        cwd: std::path::PathBuf,
        git_branch: Option<String>,
        mut ranges: std::collections::HashMap<MessageId, usize>,
    ) {
        let mut state = session.lock().await;
        // An in-place resume may replace the owning session while an older
        // stream is being cancelled. Its recovery cannot extend the new chain.
        if state.session_id != expected_session {
            return;
        }
        ranges.retain(|id, _| state.history.iter().any(|message| message.id() == *id));
        if !ranges.iter().any(|(id, from)| {
            state
                .thinking_stripped_messages
                .get(id)
                .is_none_or(|current| from < current)
        }) {
            return;
        }
        let mut parent = last_uuid.lock().await;
        let session_id = expected_session.as_uuid().to_string();
        let row = session::JsonlMessage {
            message_type: "attachment".into(),
            uuid: uuid::Uuid::new_v4().to_string(),
            parent_uuid: parent.clone(),
            session_id: session_id.clone(),
            timestamp: chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").into(),
            message: lingxi_core::types::utf16_json::Utf16JsonProjection::parse("null")
                .expect("null is valid JSON"),
            attachment_projection: Some(source_object([
                ("type", "thinking_stripped".into()),
                ("scope", "all".into()),
            ])),
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch,
            entrypoint: Some(entrypoint_value()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: source_object([]),
        };
        match writer.append(&row).await {
            Ok(()) => {
                *parent = Some(row.uuid.clone());
                // Updating the session only after durable append also prevents
                // the normal post-call synchronization from writing a duplicate.
                state.thinking_signature_stripped = true;
                for (id, from) in ranges {
                    state
                        .thinking_stripped_messages
                        .entry(id)
                        .and_modify(|current| *current = (*current).min(from))
                        .or_insert(from);
                }
                telemetry::emit_session_appended(&session_id, &row.uuid);
            }
            Err(error) => {
                tracing::error!(%error, "thinking recovery transcript append failed");
                telemetry::emit_session_persistence_failed();
            }
        }
    }

    /// Atomically persist oversized hook output below this session's
    /// root-confined `tool-results` directory. Native 2.1.291's local fallback
    /// defaults to 1 GiB when no StorageV5 store is available; this runtime has
    /// no StorageV5 producer, so a separate configured `maxBytes` value is not
    /// represented here.
    pub(crate) async fn persist_large_hook_output(
        &self,
        text: &hooks::ExactHookText,
    ) -> Result<hooks::attachment::PersistedHookOutput, String> {
        const NATIVE_LOCAL_HOOK_OUTPUT_CAP_BYTES: usize = 1_073_741_824;
        let config_home = self
            .config_home
            .clone()
            .ok_or_else(|| "tool result was not saved".to_string())?;
        let (session_uuid, cwd) = {
            let session = self.session.lock().await;
            (
                session.session_id.as_uuid().to_string(),
                self.current_cwd().to_string_lossy().into_owned(),
            )
        };
        let relative = std::path::PathBuf::from("projects")
            .join(session::jsonl::path::project_dir_name(&cwd))
            .join(session_uuid)
            .join("tool-results")
            .join(format!("hook-{}.txt", uuid::Uuid::new_v4()));
        let absolute = config_home.join(&relative);
        let persisted_text = text.truncate_utf8_bytes(NATIVE_LOCAL_HOOK_OUTPUT_CAP_BYTES);
        let truncated_at_bytes = (persisted_text.utf16_code_units != text.utf16_code_units)
            .then_some(NATIVE_LOCAL_HOOK_OUTPUT_CAP_BYTES);
        let bytes = persisted_text.display.as_bytes().to_vec();
        let write = tokio::task::spawn_blocking(move || {
            lingxi_core::host::rooted_fs::atomic_write(
                &config_home,
                &relative,
                &bytes,
                lingxi_core::host::AtomicWriteOptions {
                    overwrite: false,
                    ..lingxi_core::host::AtomicWriteOptions::default()
                },
            )
        })
        .await;
        match write {
            Ok(Ok(())) => Ok(hooks::attachment::PersistedHookOutput {
                path: absolute.display().to_string(),
                persisted_text,
                truncated_at_bytes,
            }),
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "failed to persist oversized hook output");
                Err(error.to_string())
            }
            Err(error) => {
                tracing::warn!(error = %error, "oversized hook output writer task failed");
                Err(error.to_string())
            }
        }
    }

    /// Retract a rejected attempt from live display, active history and disk.
    pub(crate) async fn discard_retry_attempt(
        &self,
        assistant_id: MessageId,
        persisted_row_ids: &[MessageId],
    ) {
        self.session
            .lock()
            .await
            .history
            .retain(|message| message.id() != assistant_id);
        self.output.emit_message_retracted(&assistant_id).await;
        if let Some(writer) = self.transcript.jsonl_writer.as_ref() {
            let mut message_ids = Vec::with_capacity(persisted_row_ids.len() + 1);
            message_ids.push(assistant_id.as_uuid().to_string());
            message_ids.extend(persisted_row_ids.iter().map(|id| id.as_uuid().to_string()));
            match writer.remove_retry_attempt_rows(&message_ids).await {
                Ok(tail) => *self.transcript.last_jsonl_uuid.lock().await = tail,
                Err(error) => tracing::warn!(%error, "failed to remove rejected retry attempt"),
            }
        }
    }

    /// Persist the latest active-goal snapshot as a transcript metadata line.
    ///
    /// This keeps `/goal` resumable on non-compacted transcripts; compact
    /// boundaries also snapshot the active goal in `compactMetadata` so a later
    /// compaction cannot summarize away the only copy.
    pub(crate) async fn persist_active_goal_state_to_jsonl(
        &self,
        active_goal: Option<&lingxi_core::session::ActiveGoalState>,
    ) {
        let status = if active_goal.is_some() {
            lingxi_core::host::GoalStatusKind::Set
        } else {
            lingxi_core::host::GoalStatusKind::Cleared
        };
        self.persist_goal_status_attachment(status, active_goal)
            .await;
    }

    pub(super) async fn persist_goal_status_attachment(
        &self,
        status: lingxi_core::host::GoalStatusKind,
        active_goal: Option<&lingxi_core::session::ActiveGoalState>,
    ) {
        let Some(goal) = active_goal else {
            return;
        };
        let total_tokens = self.snapshot_cost_real().await.total_tokens;
        let duration_ms = std::time::SystemTime::now()
            .duration_since(goal.set_at)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let snapshot = lingxi_core::host::ActiveGoalSnapshot {
            condition: goal.condition.clone(),
            set_at: goal.set_at,
            last_reason: goal.last_reason.clone(),
            iterations: goal.iterations,
            tokens_at_start: goal.tokens_at_start,
        };
        let condition = goal.condition.clone();
        let reason = goal.last_reason.clone();
        let tokens = total_tokens.saturating_sub(goal.tokens_at_start);
        let attachment = match status {
            lingxi_core::host::GoalStatusKind::Set => {
                lingxi_core::host::GoalStatusAttachment::sentinel_set(condition, Some(snapshot))
            }
            lingxi_core::host::GoalStatusKind::Cleared => {
                lingxi_core::host::GoalStatusAttachment::sentinel_cleared(condition)
            }
            lingxi_core::host::GoalStatusKind::Achieved => {
                lingxi_core::host::GoalStatusAttachment::achieved(
                    condition,
                    reason,
                    goal.iterations,
                    duration_ms,
                    tokens,
                )
            }
            lingxi_core::host::GoalStatusKind::Failed => {
                lingxi_core::host::GoalStatusAttachment::failed(
                    condition,
                    reason,
                    goal.iterations,
                    duration_ms,
                    tokens,
                )
            }
            // The goal survives a not-met turn, so the resume snapshot rides
            // along with it; upstream's record carries only condition+reason.
            lingxi_core::host::GoalStatusKind::NotMet => {
                lingxi_core::host::GoalStatusAttachment::not_met(condition, reason, Some(snapshot))
            }
        };
        match serde_json::to_string(&attachment) {
            Ok(value) => {
                self.persist_hook_attachment_to_jsonl(
                    Source::parse(&value).expect("serialized attachment is valid JSON"),
                )
                .await
            }
            Err(error) => tracing::warn!(%error, "failed to encode goal status attachment"),
        }
    }

    /// Shared append body for [`Self::persist_message_to_jsonl_with_parent`],
    /// [`Self::persist_api_error_message_to_jsonl`] and
    /// [`Self::persist_compact_summary_to_jsonl`].
    pub(super) async fn persist_message_to_jsonl_inner(
        &self,
        msg: &ConversationMessage,
        parent_override: Option<String>,
        api_error: Option<ApiErrorEnvelope>,
        compact_summary: bool,
    ) {
        self.persist_message_to_jsonl_inner_with_queue_metadata(
            msg,
            parent_override,
            api_error,
            compact_summary,
            None,
            None,
            false,
        )
        .await;
    }

    async fn persist_message_to_jsonl_inner_with_queue_metadata(
        &self,
        msg: &ConversationMessage,
        parent_override: Option<String>,
        api_error: Option<ApiErrorEnvelope>,
        compact_summary: bool,
        queued_input: Option<&QueuedPromptInput>,
        preappended: Option<PreappendedRowMetadata<'_>>,
        session_append_preaccepted: bool,
    ) {
        let _ = self
            .persist_message_to_jsonl_inner_with_pending_prompt_identity(
                msg,
                parent_override,
                api_error,
                compact_summary,
                queued_input,
                preappended,
                session_append_preaccepted,
                None,
            )
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn persist_message_to_jsonl_inner_with_pending_prompt_identity(
        &self,
        msg: &ConversationMessage,
        parent_override: Option<String>,
        api_error: Option<ApiErrorEnvelope>,
        compact_summary: bool,
        queued_input: Option<&QueuedPromptInput>,
        preappended: Option<PreappendedRowMetadata<'_>>,
        session_append_preaccepted: bool,
        pending_prompt_id: Option<&str>,
    ) -> Result<Option<ConversationMessage>, OrchestratorError> {
        if pending_prompt_id.is_some() && !matches!(msg, ConversationMessage::User { .. }) {
            return Err(OrchestratorError::Internal(
                "pending prompt append requires a user message".into(),
            ));
        }
        let rewritten = if preappended.is_some() || session_append_preaccepted {
            msg.clone()
        } else {
            self.mod_session_append_row(msg, None, true, None).await
        };
        let compact_summary_content =
            compact_summary_content_source_if_unmodified(msg, &rewritten, compact_summary);
        let msg = &rewritten;
        self.note_assistant_commit(msg).await;
        if matches!(msg, ConversationMessage::Assistant { .. }) {
            if api_error.is_some()
                || preappended
                    .as_ref()
                    .and_then(|metadata| metadata.api_error)
                    .is_some_and(|row| row.is_api_error_message)
            {
                self.mark_prompt_suggestion_api_error(msg.id());
            }
            // The accepted assistant owns terminal API-error metadata even
            // when the embedding host has no durable transcript writer.
            // Native's final isApiErrorMessage flag belongs to the accepted
            // assistant row, including writer-less embedded sessions.
            let error_stop_reason = api_error
                .as_ref()
                .map(|env| env.inner_stop_reason.unwrap_or("stop_sequence"))
                .or_else(|| {
                    preappended
                        .as_ref()
                        .and_then(|metadata| metadata.api_error)
                        .filter(|row| row.is_api_error_message)
                        .map(|row| row.message.stop_reason.as_str())
                });
            self.note_turn_api_error_stop_reason(error_stop_reason);
        }
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            // These side tables live only until the corresponding tool-result
            // line is persisted. In in-memory/no-writer sessions there is no
            // line to consume them below, so drain them here after callers have
            // had their pre-persist audience-note read.
            let _ = self.take_tool_use_result(msg).await;
            let _ = self.take_tool_denial_kind(msg).await;
            let _ = self.take_tool_use_mcp_meta(msg).await;
            let _ = self.take_source_tool_assistant_uuid(msg).await;
            return if pending_prompt_id.is_some() {
                Err(OrchestratorError::Internal(
                    "pending prompt append requires a durable transcript writer".into(),
                ))
            } else {
                Ok(None)
            };
        };
        let (session_id_str, plan_mode, parent_uuid) = {
            let session = self.session.lock().await;
            let parent = if pending_prompt_id.is_some() {
                None
            } else {
                match parent_override {
                    Some(p) => Some(p),
                    None => self.transcript.last_jsonl_uuid.lock().await.clone(),
                }
            };
            (
                session.session_id.as_uuid().to_string(),
                session.plan_mode,
                parent,
            )
        };
        // Writer-field fidelity (§G gap 4):
        // - gitBranch: once-per-session `getBranch()` (cached).
        // - entrypoint: `getEntrypoint()` → `CLAUDE_CODE_ENTRYPOINT` env or "cli"
        //   (mirrors the UA builder, `model/user_agent.rs:73`).
        // - promptId: `getPromptId()` on USER lines. We mint a fresh id when this
        //   is a genuine new user prompt and reuse it for the turn's tool_result
        //   `user` lines, matching TS where `getPromptId()` is stable across a
        //   turn. `to_jsonl_message` drops it on non-user lines.
        let git_branch = self.resolve_git_branch().await;
        let entrypoint = Some(entrypoint_value());
        let mut prompt_id = self.prompt_id_for_message(msg).await;
        if let Some(stable_prompt_id) = pending_prompt_id {
            *self.prompt_runtime.current_prompt_id.lock().await =
                Some(stable_prompt_id.to_string());
            prompt_id = Some(stable_prompt_id.to_string());
        }
        let mut jmsg = self.to_jsonl_message_with_inner_id(
            msg,
            &session_id_str,
            parent_uuid,
            git_branch,
            entrypoint,
            prompt_id,
            None,
            None,
            None,
            None,
            api_error.as_ref(),
        );
        if let Some(stop_details) = api_error
            .as_ref()
            .and_then(|envelope| envelope.inner_stop_details.as_ref())
        {
            jmsg.message
                .set_field(
                    "stop_details",
                    Source::parse(
                        &serde_json::to_string(stop_details)
                            .expect("refusal stop details serialize as exact JSON"),
                    )
                    .expect("refusal stop details are valid JSON"),
                )
                .expect("synthetic API-error message is an object");
        }
        if let ConversationMessage::User { id, .. } = msg {
            if jmsg.message_type == "user" {
                let (task_delivery, envelope) = {
                    let session = self.session.lock().await;
                    (
                        session
                            .native_request_history
                            .task_deliveries
                            .get(id)
                            .cloned(),
                        session
                            .native_request_history
                            .task_delivery_envelopes
                            .get(id)
                            .cloned(),
                    )
                };
                if let Some(content) = sdk_plain_prompt_content_source(
                    msg,
                    envelope
                        .as_ref()
                        .and_then(|value| value.prompt_source.as_ref()),
                    queued_input.and_then(|input| input.sdk_content_is_json_string),
                ) {
                    jmsg.message
                        .set_field("content", content)
                        .expect("SDK plain prompt content is a valid JSON string");
                }
                if let Some(envelope) = envelope {
                    for (key, projection) in [
                        ("verifiedSlackHumanTurn", envelope.verified_slack_human_turn),
                        ("promptSource", envelope.prompt_source),
                    ] {
                        if let Some(projection) = projection {
                            jmsg.extra
                                .set_field(key, projection)
                                .expect("user envelope metadata is an object");
                        }
                    }
                    if let Some(tool_use_result) = envelope.tool_use_result {
                        jmsg.tool_use_result_projection = Some(tool_use_result);
                    }
                }
                if let Some(task_delivery) = task_delivery {
                    jmsg.extra
                        .set_field("taskDelivery", task_delivery)
                        .expect("extra metadata is an object");
                }
            }
        }
        if let Some(content) = compact_summary_content {
            jmsg.message
                .set_field("content", content)
                .expect("compact summary message is an object");
        }
        if let Some(metadata) = preappended {
            jmsg.timestamp = metadata.timestamp.to_owned();
            if let Some(row) = metadata.api_error {
                jmsg.uuid = row.uuid.as_uuid().to_string();
                let mut message = row.message.clone();
                if let ConversationMessage::Assistant { content, .. } = msg {
                    message.content = content.clone();
                }
                jmsg.message = Source::parse(
                    &serde_json::to_string(&message)
                        .expect("synthetic API-error message contains serializable host fields"),
                )
                .expect("synthetic message is valid JSON");
                jmsg.extra
                    .set_field(
                        "isApiErrorMessage",
                        Source::from_serde_value(&serde_json::Value::from(
                            row.is_api_error_message,
                        ))
                        .expect("generated transcript metadata is valid JSON"),
                    )
                    .expect("extra metadata is an object");
                jmsg.extra
                    .set_field(
                        "error",
                        Source::from_serde_value(&serde_json::Value::from(row.error.clone()))
                            .expect("generated transcript metadata is valid JSON"),
                    )
                    .expect("extra metadata is an object");
                jmsg.extra
                    .remove_field("requestId")
                    .expect("extra metadata is an object");
                if let Some(request_id) = &row.request_id {
                    jmsg.extra
                        .set_field(
                            "requestId",
                            Source::from_serde_value(&serde_json::Value::from(request_id.clone()))
                                .expect("generated transcript metadata is valid JSON"),
                        )
                        .expect("extra metadata is an object");
                }
            }
        }
        if jmsg.message_type == "user" {
            if let Some(input) = queued_input {
                if let Some(priority) = &input.queue_priority {
                    jmsg.extra
                        .set_field(
                            "queuePriority",
                            Source::from_serde_value(&serde_json::Value::String(priority.clone()))
                                .expect("generated transcript metadata is valid JSON"),
                        )
                        .expect("extra metadata is an object");
                }
                if let Some(task_id) = &input.scheduled_task_id {
                    jmsg.extra
                        .set_field(
                            "scheduledTaskId",
                            Source::from_serde_value(&serde_json::Value::String(task_id.clone()))
                                .expect("generated transcript metadata is valid JSON"),
                        )
                        .expect("extra metadata is an object");
                    if let Some(fire_id) = &input.scheduled_fire_id {
                        jmsg.extra
                            .set_field(
                                "scheduledFireId",
                                Source::from_serde_value(&serde_json::Value::String(
                                    fire_id.clone(),
                                ))
                                .expect("generated transcript metadata is valid JSON"),
                            )
                            .expect("extra metadata is an object");
                    }
                }
            }
            let permission_mode = if plan_mode {
                "plan".to_string()
            } else {
                self.permission_mode()
                    .unwrap_or_else(|| "default".to_string())
            };
            jmsg.extra
                .set_field(
                    "permissionMode",
                    Source::from_serde_value(&serde_json::Value::String(permission_mode))
                        .expect("generated transcript metadata is valid JSON"),
                )
                .expect("extra metadata is an object");
        }
        if let Some(stable_prompt_id) = pending_prompt_id {
            jmsg.prompt_id = Some(stable_prompt_id.to_string());
        }
        // Compaction summary user line: stamp the top-level envelope flags in
        // claude's on-disk order (`isVisibleInTranscriptOnly` before
        // `isCompactSummary`, between `message` and `uuid` — the schema's user
        // arm emits them there).
        // Denial provenance: claude stamps `toolDenialKind` on the tool_result
        // user line for a tool that was denied rather than run. The schema's
        // tool-result head places it after `timestamp` and before the common
        // trailer; the exactly-one-tool_result guard lives in
        // `take_tool_denial_kind`.
        //
        // O1: the same line also carries `toolUseResult` (the tool's raw
        // structured result / the `Error: …` string), the MCP `mcpMeta`
        // sibling, and `sourceToolAssistantUUID` (the assistant line that
        // carried the `tool_use`). All four share the single-block guard and
        // are emitted in `TOOL_RESULT_HEAD_EXTRA` order regardless of the
        // order they are inserted here.
        if let Some(result) = self.take_tool_use_result(msg).await {
            jmsg.tool_use_result_projection = Some(result.clone());
            if let ConversationMessage::User { id, .. } = msg {
                let mut session = self.session.lock().await;
                session
                    .native_request_history
                    .task_delivery_envelopes
                    .entry(*id)
                    .or_default()
                    .tool_use_result = Some(result);
            }
        }
        if let Some(kind) = self.take_tool_denial_kind(msg).await {
            jmsg.extra
                .set_field(
                    "toolDenialKind",
                    Source::from_serde_value(&serde_json::Value::String(kind))
                        .expect("generated transcript metadata is valid JSON"),
                )
                .expect("extra metadata is an object");
        }
        if let Some(meta) = self.take_tool_use_mcp_meta(msg).await {
            jmsg.mcp_meta_projection = Some(meta);
        }
        if self.tool_result_message_ends_turn(msg).await {
            jmsg.extra
                .set_field(
                    "toolEndsTurn",
                    Source::from_serde_value(&serde_json::Value::Bool(true))
                        .expect("generated transcript metadata is valid JSON"),
                )
                .expect("extra metadata is an object");
        }
        if let Some(src) = self.take_source_tool_assistant_uuid(msg).await {
            jmsg.extra
                .set_field(
                    "sourceToolAssistantUUID",
                    Source::from_serde_value(&serde_json::Value::String(src))
                        .expect("generated transcript metadata is valid JSON"),
                )
                .expect("extra metadata is an object");
        }
        if msg.is_visible_in_transcript_only() || compact_summary {
            jmsg.extra
                .set_field(
                    "isVisibleInTranscriptOnly",
                    Source::from_serde_value(&serde_json::Value::Bool(true))
                        .expect("generated transcript metadata is valid JSON"),
                )
                .expect("extra metadata is an object");
        }
        if msg.is_compact_summary() || compact_summary {
            jmsg.extra
                .set_field(
                    "isCompactSummary",
                    Source::from_serde_value(&serde_json::Value::Bool(true))
                        .expect("generated transcript metadata is valid JSON"),
                )
                .expect("extra metadata is an object");
        }
        let uuid_for_chain = jmsg.uuid.clone();
        let append_result = if pending_prompt_id.is_some() {
            self.append_pending_transcript_row(writer, &jmsg)
                .await
                .map(|(outcome, last_uuid)| {
                    (
                        last_uuid,
                        outcome == session::jsonl::TranscriptAppendOutcome::Appended,
                    )
                })
                .map_err(|error| error.to_string())
        } else {
            self.append_transcript_row(writer, &jmsg)
                .await
                .map(|()| (Some(uuid_for_chain.clone()), true))
                .map_err(|error| error.to_string())
        };
        match append_result {
            Ok((last_uuid, newly_appended)) => {
                if pending_prompt_id.is_some() && last_uuid.is_none() {
                    return Err(OrchestratorError::Internal(
                        "pending prompt append completed without a transcript tip".into(),
                    ));
                }
                *self.transcript.last_jsonl_uuid.lock().await = last_uuid;
                if newly_appended {
                    telemetry::emit_session_appended(&session_id_str, &uuid_for_chain);
                }
                if let Some(id) = crate::resume::native_zce_message_id(&jmsg) {
                    self.session
                        .lock()
                        .await
                        .native_request_history
                        .synthetic_closing_ids
                        .insert(id);
                }
                if let Some(row_token) = queued_input
                    .and_then(|input| input.transcript_row_token.as_deref())
                    .filter(|_| matches!(msg, ConversationMessage::User { .. }))
                {
                    self.output
                        .emit_user_transcript_row_identity(row_token, &uuid_for_chain)
                        .await;
                }
                if let ConversationMessage::Assistant { id, .. } = msg {
                    self.output
                        .emit_assistant_transcript_row_uuids(id, &[Some(uuid_for_chain.clone())])
                        .await;
                }
            }
            Err(error) => {
                if pending_prompt_id.is_some() {
                    return Err(OrchestratorError::Internal(format!(
                        "pending prompt transcript append failed: {error}"
                    )));
                }
                self.record_transcript_append_failure(&session_id_str, "message", &error)
                    .await;
            }
        }
        Ok(Some(rewritten))
    }

    /// Persist a meta user message to one exact session transcript and mark it
    /// as excluded from model context. This never retargets the live writer.
    pub(crate) async fn persist_model_excluded_meta_to_session(
        &self,
        target_session: lingxi_core::types::SessionId,
        msg: &ConversationMessage,
    ) -> Result<Option<String>, lingxi_core::host::HandleError> {
        self.persist_conversation_message_to_session(target_session, msg, true)
            .await
    }

    pub(crate) async fn persist_conversation_message_to_session(
        &self,
        target_session: lingxi_core::types::SessionId,
        msg: &ConversationMessage,
        exclude_from_model: bool,
    ) -> Result<Option<String>, lingxi_core::host::HandleError> {
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return Ok(None);
        };

        let target_uuid = target_session.as_uuid();
        let target_bare = target_uuid.to_string();
        let cwd = self.cwd.to_string_lossy().into_owned();
        let durable = writer.durable_transcript_enabled();
        let target_path = if durable {
            writer.session_target_path(target_session).ok_or_else(|| {
                lingxi_core::host::HandleError::ActionFailed(format!(
                    "durable transcript target is not bound for session {target_bare}"
                ))
            })?
        } else {
            match self.config_home.as_ref() {
                Some(home) => {
                    match session::jsonl::resolve_session_path_across_worktrees(
                        home,
                        &cwd,
                        target_uuid,
                    )
                    .await
                    {
                        Ok(path) => path,
                        Err(
                            session::jsonl::LoaderError::SessionNotFound { .. }
                            | session::jsonl::LoaderError::EmptyDirectory,
                        ) => session::jsonl::session_path(home, &cwd, &target_bare),
                        Err(error) => {
                            return Err(lingxi_core::host::HandleError::ActionFailed(format!(
                                "could not resolve target session transcript: {error}"
                            )));
                        }
                    }
                }
                None => {
                    let current = self.session.lock().await.session_id;
                    if current != target_session {
                        return Err(lingxi_core::host::HandleError::ActionFailed(
                            "target-session persistence requires a configured session store".into(),
                        ));
                    }
                    writer.active_path()
                }
            }
        };

        // Production resolves parentage inside the pinned transcript
        // transaction. Compatibility writers retain the historical loader
        // lookup because they have no durable target map/lock.
        let parent_uuid = if durable {
            None
        } else {
            let fs = writer.filesystem_handle();
            match self.config_home.as_ref() {
                Some(home) => {
                    match session::jsonl::load_session_across_worktrees(home, &cwd, target_uuid, fs)
                        .await
                    {
                        Ok(messages) => messages.last().map(|message| message.uuid.clone()),
                        Err(
                            session::jsonl::LoaderError::SessionNotFound { .. }
                            | session::jsonl::LoaderError::EmptyDirectory,
                        ) => None,
                        Err(error) => {
                            return Err(lingxi_core::host::HandleError::ActionFailed(format!(
                                "could not read target session transcript: {error}"
                            )));
                        }
                    }
                }
                None => self.transcript.last_jsonl_uuid.lock().await.clone(),
            }
        };

        let persisted_session_id = if durable {
            target_bare.clone()
        } else {
            target_session.as_uuid().to_string()
        };
        let mut persisted = self.to_jsonl_message_with_inner_id(
            msg,
            &persisted_session_id,
            parent_uuid,
            self.resolve_git_branch().await,
            Some(entrypoint_value()),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        if exclude_from_model {
            persisted
                .extra
                .set_field(
                    "isModelContextExcluded",
                    Source::from_serde_value(&serde_json::Value::Bool(true))
                        .expect("generated transcript metadata is valid JSON"),
                )
                .expect("extra metadata is an object");
        }
        if durable {
            persisted.cwd = writer
                .session_target_cwd(target_session)
                .unwrap_or_else(|| self.current_cwd())
                .to_string_lossy()
                .into_owned();
        }
        if matches!(msg, ConversationMessage::User { .. }) {
            persisted
                .extra
                .set_field(
                    "permissionMode",
                    Source::from_serde_value(&serde_json::Value::String(
                        self.permission_mode()
                            .unwrap_or_else(|| "default".to_string()),
                    ))
                    .expect("generated transcript metadata is valid JSON"),
                )
                .expect("extra metadata is an object");
        }
        let uuid = persisted.uuid.clone();
        writer
            .append_to_path(&target_path, &persisted)
            .await
            .map_err(|error| {
                lingxi_core::host::HandleError::ActionFailed(format!(
                    "could not append target session transcript: {error}"
                ))
            })?;
        telemetry::emit_session_appended(&target_session.to_string(), &uuid);
        Ok(Some(uuid))
    }

    /// Persist an assistant turn as ONE single-block JSONL line PER content block
    /// (claude-code's per-`content_block_stop` writer — `claude.ts:2171-2211`).
    ///
    /// claude-code builds an `AssistantMessage` at each `content_block_stop` from
    /// a SINGLE content block (`content: normalizeContentFromAPI([contentBlock])`)
    /// with distinct top-level `uuid`s and the SAME inner `message.id` shared
    /// across all blocks of the turn. So an assistant turn `[text, tool_use A,
    /// tool_use B]` becomes THREE assistant JSONL lines: one shared inner
    /// `message.id`, three distinct top-level `uuid`s, one block each.
    ///
    /// This is a WRITE-side (transcript) split ONLY — the caller keeps the single
    /// merged `ConversationMessage::Assistant` in `session.history` for
    /// request-building (the Anthropic request needs one assistant turn carrying
    /// all blocks). Native streaming rows keep their stop-time row ids. The
    /// non-streaming fallback derives a stable top-level UUID from the turn id,
    /// block index, and source block before `session.append`, then chooses the
    /// current parent only inside the durable append lease. The originating turn
    /// id (`msg.id().as_uuid()`) remains the shared inner `message.id` so the
    /// loader's sibling-grouping reconstructs the DAG.
    ///
    /// Returns a `tool_use_id -> that block's line uuid` map so the caller can
    /// parent EACH `tool_result` to ITS specific `tool_use` line (TS
    /// `sourceToolAssistantUUID`), not one shared per-turn parent. On an
    /// assistant with no `tool_use` blocks the map is empty. The lines chain off
    /// `last_jsonl_uuid` (advancing it per line), so the LAST block's uuid ends
    /// up as `last_jsonl_uuid` and any subsequent non-tool message chains
    /// correctly. An empty-content assistant persists nothing (no line, empty
    /// map) — faithful to streaming, which never emits a zero-block turn.
    pub(crate) async fn persist_assistant_per_block(
        &self,
        msg: &ConversationMessage,
        // Raw Anthropic `usage` object for the BetaMessage envelope (the codec's
        // `Usage::provider_metadata`); `None` writes `usage: null`.
        usage: Option<&JsonPayload>,
        // The Anthropic `request-id` response header for this turn → the
        // top-level `requestId` on every per-block assistant line. `None` when
        // the adapter recorded no request-id (e.g. a mock that does not surface
        // headers) — the line then omits `requestId`, like claude-code.
        request_id: Option<&str>,
        publication_fence: Option<crate::autonomous_tool_scheduler::ToolDispatchPublicationFence>,
    ) -> std::collections::HashMap<lingxi_core::types::ToolUseId, String> {
        self.persist_assistant_per_block_with_fence(msg, usage, request_id, true, publication_fence)
            .await
    }

    /// Write per-block rows for a merged row whose `session.append` pass has
    /// already run before W1 dispatch. Do not dispatch the hook again while
    /// serializing the accepted query row.
    pub(crate) async fn persist_preappended_assistant_per_block(
        &self,
        msg: &ConversationMessage,
        usage: Option<&JsonPayload>,
        request_id: Option<&str>,
        publication_fence: crate::autonomous_tool_scheduler::ToolDispatchPublicationFence,
    ) -> std::collections::HashMap<lingxi_core::types::ToolUseId, String> {
        self.persist_assistant_per_block_with_fence(
            msg,
            usage,
            request_id,
            false,
            Some(publication_fence),
        )
        .await
    }

    async fn persist_assistant_per_block_with_fence(
        &self,
        msg: &ConversationMessage,
        usage: Option<&JsonPayload>,
        request_id: Option<&str>,
        run_session_append: bool,
        publication_fence: Option<crate::autonomous_tool_scheduler::ToolDispatchPublicationFence>,
    ) -> std::collections::HashMap<lingxi_core::types::ToolUseId, String> {
        self.persist_assistant_per_block_inner(
            msg,
            usage,
            request_id,
            run_session_append,
            publication_fence,
        )
        .await
    }

    async fn persist_assistant_per_block_inner(
        &self,
        msg: &ConversationMessage,
        usage: Option<&JsonPayload>,
        request_id: Option<&str>,
        run_session_append: bool,
        publication_fence: Option<crate::autonomous_tool_scheduler::ToolDispatchPublicationFence>,
    ) -> std::collections::HashMap<lingxi_core::types::ToolUseId, String> {
        let mut map: std::collections::HashMap<lingxi_core::types::ToolUseId, String> =
            std::collections::HashMap::new();
        let ConversationMessage::Assistant {
            id: turn_id,
            content,
            stop_reason,
            resumable,
            per_turn_effort,
        } = msg
        else {
            // Defensive: non-assistant messages fall back to the normal single
            // line (no split applies). Should not happen in practice.
            let persist = self.persist_message_to_jsonl(msg);
            if let Some(fence) = publication_fence.as_ref() {
                fence.commit_if_current(Box::pin(persist)).await;
            } else {
                persist.await;
            }
            return map;
        };
        let writer = self.transcript.jsonl_writer.clone();
        // Shared inner Anthropic `message.id` for every block.
        let inner_id = turn_id.as_uuid().to_string();

        let (session_id_str, mut model, mut model_profile) = {
            let s = self.session.lock().await;
            (
                s.session_id.as_uuid().to_string(),
                s.model.clone(),
                s.model_profile.clone(),
            )
        };
        if let Some(settings) = crate::scheduled_turn::current() {
            model = settings.model;
            model_profile = Some(settings.provider);
        }
        let git_branch = self.resolve_git_branch().await;
        let entrypoint = Some(entrypoint_value());
        let persisted_usage = usage.map(without_host_usage_metadata);

        let mut text_row_uuids = Vec::new();
        let mut block_groups = content
            .iter()
            .cloned()
            .map(|block| vec![block])
            .collect::<Vec<_>>();
        for (block_index, block) in content.iter().enumerate() {
            // The fallback identity is fixed before append-through. The current
            // parent is selected later, inside the short durable commit lease.
            let original_single = ConversationMessage::Assistant {
                per_turn_effort: per_turn_effort.clone(),
                id: MessageId::new(),
                content: vec![block.clone()],
                stop_reason: stop_reason.clone(),
                resumable: resumable.clone(),
            };
            let line_uuid = Self::assistant_block_derived_uuid(&inner_id, block_index, block);
            let single = if run_session_append {
                self.mod_session_append_row(
                    &original_single,
                    Some(&line_uuid),
                    false,
                    publication_fence.clone().map(erase_tool_dispatch_fence),
                )
                .await
            } else {
                original_single
            };
            let ConversationMessage::Assistant {
                content: rewritten_blocks,
                ..
            } = &single
            else {
                continue;
            };
            let rewritten_blocks = rewritten_blocks.clone();
            let mut next_block_groups = block_groups.clone();
            next_block_groups[block_index] = rewritten_blocks.clone();
            let rewritten_merged = ConversationMessage::Assistant {
                per_turn_effort: per_turn_effort.clone(),
                id: *turn_id,
                content: next_block_groups.iter().flatten().cloned().collect(),
                stop_reason: stop_reason.clone(),
                resumable: resumable.clone(),
            };
            let writer_for_commit = writer.clone();
            let single_for_commit = single.clone();
            let session_id_for_commit = session_id_str.clone();
            let git_branch_for_commit = git_branch.clone();
            let entrypoint_for_commit = entrypoint.clone();
            let inner_id_for_commit = inner_id.clone();
            let model_for_commit = model.clone();
            let usage_for_commit = persisted_usage.clone();
            let request_id_for_commit = request_id.map(str::to_owned);
            let model_profile_for_commit = model_profile.clone();
            let line_uuid_for_commit = line_uuid.clone();
            let block_for_commit = block.clone();
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            let persist = async {
                self.replace_session_history_row(*turn_id, rewritten_merged)
                    .await;
                let Some(writer) = writer_for_commit else {
                    let _ = result_tx.send(None::<String>);
                    return;
                };
                let parent_uuid = self.transcript.last_jsonl_uuid.lock().await.clone();
                let mut jmsg = self.to_jsonl_message_with_inner_id(
                    &single_for_commit,
                    &session_id_for_commit,
                    parent_uuid.clone(),
                    git_branch_for_commit,
                    entrypoint_for_commit,
                    None,
                    Some(&inner_id_for_commit),
                    Some(&model_for_commit),
                    usage_for_commit.as_ref(),
                    request_id_for_commit.as_deref(),
                    None,
                );
                jmsg.uuid.clone_from(&line_uuid_for_commit);
                jmsg.extra
                    .set_field(
                        "modelProfile",
                        Source::from_serde_value(
                            &model_profile_for_commit.map_or(serde_json::Value::Null, |profile| {
                                serde_json::Value::String(profile)
                            }),
                        )
                        .expect("generated transcript metadata is valid JSON"),
                    )
                    .expect("extra metadata is an object");
                match self.append_transcript_row(&writer, &jmsg).await {
                    Ok(()) => {
                        *self.transcript.last_jsonl_uuid.lock().await =
                            Some(line_uuid_for_commit.clone());
                        telemetry::emit_session_appended(
                            &session_id_for_commit,
                            &line_uuid_for_commit,
                        );
                        if let lingxi_core::types::ContentBlock::ToolUse { id, .. } =
                            &block_for_commit
                        {
                            self.record_source_tool_assistant_uuid(
                                id,
                                line_uuid_for_commit.clone(),
                            )
                            .await;
                        }
                        let _ = result_tx.send(Some(line_uuid_for_commit));
                    }
                    Err(error) => {
                        self.record_transcript_append_failure(
                            &session_id_for_commit,
                            "assistant_block",
                            &error,
                        )
                        .await;
                        let _ = result_tx.send(None);
                    }
                }
            };
            let committed = if let Some(fence) = publication_fence.as_ref() {
                fence.commit_if_current(Box::pin(persist)).await
            } else {
                persist.await;
                true
            };
            if !committed {
                return map;
            }
            block_groups = next_block_groups;
            let written_uuid = result_rx.await.ok().flatten();
            match written_uuid {
                Some(uuid) => {
                    if matches!(
                        block,
                        lingxi_core::types::ContentBlock::Text { .. }
                            | lingxi_core::types::ContentBlock::TextJsUtf16 { .. }
                    ) {
                        text_row_uuids.push(Some(uuid.clone()));
                    }
                    if let lingxi_core::types::ContentBlock::ToolUse { id, .. } = block {
                        map.insert(id.clone(), uuid);
                    }
                }
                None if matches!(
                    block,
                    lingxi_core::types::ContentBlock::Text { .. }
                        | lingxi_core::types::ContentBlock::TextJsUtf16 { .. }
                ) =>
                {
                    text_row_uuids.push(None);
                }
                None => {}
            }
        }
        let rewritten_merged = ConversationMessage::Assistant {
            per_turn_effort: per_turn_effort.clone(),
            id: *turn_id,
            content: block_groups.into_iter().flatten().collect(),
            stop_reason: stop_reason.clone(),
            resumable: resumable.clone(),
        };
        let note = self.note_assistant_commit(&rewritten_merged);
        if let Some(fence) = publication_fence.as_ref() {
            if !fence.commit_if_current(Box::pin(note)).await {
                return map;
            }
        } else {
            note.await;
        }
        if writer.is_some() {
            let output = self.output.clone();
            let message_id = *turn_id;
            let publish = output.emit_assistant_transcript_row_uuids(&message_id, &text_row_uuids);
            if let Some(fence) = publication_fence.as_ref() {
                fence.publish_if_current(Box::pin(publish)).await;
            } else {
                publish.await;
            }
        }
        map
    }

    /// Await Native's append bridge before dispatching a completed tool row.
    /// The accepted content becomes the current query row and is shared by
    /// W1, retained query rows, and the eventual transcript write.
    pub(crate) async fn append_completed_assistant_row(
        &self,
        row: &mut crate::streaming_loop::CompletedAssistantRow,
        publication_guard: Option<Arc<dyn HookPublicationGuard>>,
    ) {
        if publication_guard
            .as_ref()
            .is_some_and(|guard| !guard.is_current())
        {
            return;
        }
        if row.session_append_dispatched || row.is_api_error {
            return;
        }
        let row_uuid = row.row_id.as_uuid().to_string();
        let message = ConversationMessage::Assistant {
            per_turn_effort: (!row.is_api_error)
                .then(|| row.per_turn_effort.clone())
                .flatten(),
            id: row.row_id,
            content: row.content.clone(),
            stop_reason: row.stop_reason.clone(),
            resumable: row.resumable.clone(),
        };
        if let ConversationMessage::Assistant { content, .. } = self
            .mod_session_append_row(&message, Some(&row_uuid), false, publication_guard.clone())
            .await
        {
            // Native's append-through yields the same row object that the
            // query and tool executor consume. Keep one accepted content value
            // for W1, K, retained-row handling, and eventual JSONL; q/D
            // reconstruction preserves the source ToolUse if the hook returns
            // Text only.
            row.content = content;
        }
        if publication_guard
            .as_ref()
            .is_some_and(|guard| !guard.is_current())
        {
            return;
        }
        row.session_append_dispatched = true;
    }

    /// Commit one content-block row after terminal metadata is available.
    /// Native's interactive recorder excludes the incomplete assistant tail.
    /// Its event journal cursor and the headless shared-message write queue
    /// require separate handling; this helper does not model those queues.
    pub(crate) async fn persist_completed_assistant_row(
        &self,
        row: &mut crate::streaming_loop::CompletedAssistantRow,
        publication_guard: Option<Arc<dyn HookPublicationGuard>>,
    ) -> Option<crate::streaming_loop::PersistedAssistantRowLink> {
        if publication_guard
            .as_ref()
            .is_some_and(|guard| !guard.is_current())
        {
            return None;
        }
        if let Some(link) = &row.persisted_link {
            return Some(link.clone());
        }
        if row.stop_reason.is_none() || row.is_api_error {
            return None;
        }
        let inner_id = if self.is_persistent_app_chat() {
            // Replay groups the per-block rows into the exact live assistant
            // identity. A provider id is neither that host UUID nor a replay
            // authority; retain it separately as response metadata.
            row.logical_message_id?.as_uuid().to_string()
        } else {
            row.provider_message_id.clone()
        };

        self.append_completed_assistant_row(row, publication_guard.clone())
            .await;
        if publication_guard
            .as_ref()
            .is_some_and(|guard| !guard.is_current())
        {
            return None;
        }

        let writer = self.transcript.jsonl_writer.as_ref()?;

        let single = ConversationMessage::Assistant {
            per_turn_effort: (!row.is_api_error)
                .then(|| row.per_turn_effort.clone())
                .flatten(),
            id: row.row_id,
            content: row.content.clone(),
            stop_reason: row.stop_reason.clone(),
            resumable: row.resumable.clone(),
        };
        let session_id = self.session.lock().await.session_id.as_uuid().to_string();
        let usage = row.usage.as_ref().map(persisted_assistant_usage);
        let mut jmsg = self.to_jsonl_message_with_inner_id(
            &single,
            &session_id,
            None,
            self.resolve_git_branch().await,
            Some(entrypoint_value()),
            None,
            Some(&inner_id),
            Some(&row.model),
            usage.as_ref(),
            row.request_id.as_deref(),
            None,
        );
        let row_uuid = row.row_id.as_uuid().to_string();
        jmsg.uuid.clone_from(&row_uuid);
        jmsg.timestamp.clone_from(&row.timestamp);
        if self.is_persistent_app_chat() {
            jmsg.extra
                .set_field("providerMessageId", row.provider_message_id.clone().into())
                .expect("extra metadata is an object");
        }
        jmsg.extra
            .set_field(
                "modelProfile",
                Source::from_serde_value(
                    &row.model_profile
                        .as_ref()
                        .map_or(serde_json::Value::Null, |profile| {
                            serde_json::Value::String(profile.clone())
                        }),
                )
                .expect("generated transcript metadata is valid JSON"),
            )
            .expect("extra metadata is an object");
        if let Some(details) = row.stop_details.as_ref() {
            jmsg.message
                .set_field(
                    "stop_details",
                    Source::parse(&serde_json::to_string(details).expect("stop details serialize"))
                        .expect("stop details are valid JSON"),
                )
                .expect("assistant message is an object");
        }
        if !row.supersedes_row_ids.is_empty() {
            jmsg.extra
                .set_field(
                    "supersedesUuids",
                    Source::from_serde_value(&serde_json::Value::Array(
                        row.supersedes_row_ids
                            .iter()
                            .map(|id| serde_json::Value::String(id.as_uuid().to_string()))
                            .collect(),
                    ))
                    .expect("generated transcript metadata is valid JSON"),
                )
                .expect("extra metadata is an object");
        }

        let writer = Arc::clone(writer);
        let session_id_for_write = session_id.clone();
        let mut jmsg_for_write = jmsg;
        let row_content = row.content.clone();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let persist = async move {
            // Fenced attachment writers use the same lease. Pick the current
            // parent only after admission, so an attachment that committed
            // during session.append cannot leave this row with a stale parent.
            let parent_uuid = self.transcript.last_jsonl_uuid.lock().await.clone();
            jmsg_for_write.parent_uuid.clone_from(&parent_uuid);
            let result = match self.append_transcript_row(&writer, &jmsg_for_write).await {
                Ok(()) => {
                    *self.transcript.last_jsonl_uuid.lock().await = Some(row_uuid.clone());
                    telemetry::emit_session_appended(&session_id_for_write, &row_uuid);
                    for block in &row_content {
                        if let lingxi_core::types::ContentBlock::ToolUse { id, .. } = block {
                            self.record_source_tool_assistant_uuid(id, row_uuid.clone())
                                .await;
                        }
                    }
                    Some(crate::streaming_loop::PersistedAssistantRowLink {
                        uuid: row_uuid,
                        parent_uuid,
                    })
                }
                Err(error) => {
                    self.record_transcript_append_failure(
                        &session_id_for_write,
                        "assistant_stream_row",
                        &error,
                    )
                    .await;
                    None
                }
            };
            let _ = result_tx.send(result);
        };
        if let Some(fence) = publication_guard {
            if !fence.commit_if_current(Box::pin(persist)).await {
                return None;
            }
        } else {
            persist.await;
        }
        result_rx.await.ok().flatten()
    }

    /// Physically remove transcript rows named by Native tombstones. Children
    /// keep their original parent UUID; transcript resume handles the resulting
    /// missing-parent chain according to Native's timestamp fallback.
    pub(crate) async fn remove_assistant_stream_rows(
        &self,
        rows: &[crate::streaming_loop::PersistedAssistantRowLink],
    ) {
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return;
        };
        let mut removed = std::collections::HashMap::new();
        for row in rows {
            match writer.remove_message_by_uuid(&row.uuid).await {
                Ok(true) => {
                    removed.insert(row.uuid.clone(), row.parent_uuid.clone());
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    uuid = %row.uuid,
                    %error,
                    "failed to remove a tombstoned assistant transcript row"
                ),
            }
        }
        if removed.is_empty() {
            return;
        }

        // The live append cursor follows the surviving transcript tail, but
        // the JSONL rows themselves are not re-parented by tombstone removal.
        let mut cursor = self.transcript.last_jsonl_uuid.lock().await;
        let mut tail = cursor.clone();
        let mut seen = std::collections::HashSet::new();
        while let Some(uuid) = tail.as_ref() {
            if !seen.insert(uuid.clone()) {
                break;
            }
            let Some(parent) = removed.get(uuid) else {
                break;
            };
            tail.clone_from(parent);
        }
        *cursor = tail;
    }

    /// Persist a NON-streaming (batched) assistant turn as ONE merged JSONL line
    /// carrying the full BetaMessage envelope — the real `model` (the live
    /// session model, same source [`Self::persist_assistant_per_block`] uses),
    /// the Anthropic `usage` object, and the `requestId`. This is the batched
    /// counterpart of `persist_assistant_per_block`: claude-code's non-streaming
    /// handler (`claude.ts:2571`) emits one merged `AssistantMessage` WITH the
    /// response model/usage, and the batched `run_turn` path must match.
    ///
    /// Previously the batched path used the model-less
    /// [`Self::persist_message_to_jsonl`], so every real `--print` / `--bg`
    /// reply was recorded as `model:"<synthetic>"` with `usage` dropped (cost
    /// lost, telemetry/resume mis-attributed) even though the API call
    /// succeeded. Genuine SYNTHETIC api-error lines still use the model-less
    /// path (`persist_api_error_message_to_jsonl` / `persist_message_to_jsonl`).
    pub(crate) async fn persist_assistant_merged(
        &self,
        msg: &ConversationMessage,
        // Typed response usage → the Anthropic `usage` JSON (via
        // `assistant_usage_value`). `None` writes `usage: null`.
        usage: Option<&llm_runtime::ExecutionUsage>,
        request_id: Option<&str>,
        publication_fence: Option<crate::autonomous_tool_scheduler::ToolDispatchPublicationFence>,
    ) -> ConversationMessage {
        let rewritten = self
            .mod_session_append_row(
                msg,
                None,
                true,
                publication_fence.clone().map(erase_tool_dispatch_fence),
            )
            .await;
        if publication_fence
            .as_ref()
            .is_some_and(|fence| !fence.is_current())
        {
            return rewritten;
        }
        let msg = &rewritten;
        let ConversationMessage::Assistant { id: turn_id, .. } = msg else {
            // Defensive: non-assistant messages take the plain single-line path.
            let persist = self.persist_message_to_jsonl(msg);
            if let Some(fence) = publication_fence {
                fence.commit_if_current(Box::pin(persist)).await;
            } else {
                persist.await;
            }
            return rewritten.clone();
        };
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            let note = self.note_assistant_commit(msg);
            if let Some(fence) = publication_fence {
                fence.commit_if_current(Box::pin(note)).await;
            } else {
                note.await;
            }
            return rewritten.clone();
        };
        let inner_id = turn_id.as_uuid().to_string();
        let (session_id_str, mut model, mut model_profile) = {
            let s = self.session.lock().await;
            (
                s.session_id.as_uuid().to_string(),
                s.model.clone(),
                s.model_profile.clone(),
            )
        };
        if let Some(settings) = crate::scheduled_turn::current() {
            model = settings.model;
            model_profile = Some(settings.provider);
        }
        let git_branch = self.resolve_git_branch().await;
        let entrypoint = Some(entrypoint_value());
        let usage_json = usage.map(persisted_assistant_usage);
        let mut jmsg = self.to_jsonl_message_with_inner_id(
            msg,
            &session_id_str,
            None,
            git_branch,
            entrypoint,
            None,
            Some(&inner_id),
            Some(&model),
            usage_json.as_ref(),
            request_id,
            None,
        );
        jmsg.extra
            .set_field(
                "modelProfile",
                Source::from_serde_value(
                    &model_profile.map_or(serde_json::Value::Null, serde_json::Value::String),
                )
                .expect("generated transcript metadata is valid JSON"),
            )
            .expect("extra metadata is an object");
        let line_uuid = jmsg.uuid.clone();
        let writer = Arc::clone(writer);
        let assistant_id = *turn_id;
        let assistant_content = match msg {
            ConversationMessage::Assistant { content, .. } => content.clone(),
            _ => Vec::new(),
        };
        let session_id_for_write = session_id_str.clone();
        let line_uuid_for_write = line_uuid.clone();
        let assistant_content_for_write = assistant_content.clone();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let persist = async move {
            self.note_assistant_commit(msg).await;
            let parent_uuid = self.transcript.last_jsonl_uuid.lock().await.clone();
            let mut jmsg = jmsg;
            jmsg.parent_uuid.clone_from(&parent_uuid);
            let persisted = match self.append_transcript_row(&writer, &jmsg).await {
                Ok(()) => {
                    *self.transcript.last_jsonl_uuid.lock().await =
                        Some(line_uuid_for_write.clone());
                    telemetry::emit_session_appended(&session_id_for_write, &line_uuid_for_write);
                    // O1: the batched path writes ONE merged line, so every
                    // `tool_use` block in it shares that line's uuid as its
                    // `sourceToolAssistantUUID`.
                    for block in &assistant_content_for_write {
                        if let lingxi_core::types::ContentBlock::ToolUse { id, .. } = block {
                            self.record_source_tool_assistant_uuid(id, line_uuid_for_write.clone())
                                .await;
                        }
                    }
                    true
                }
                Err(error) => {
                    self.record_transcript_append_failure(
                        &session_id_for_write,
                        "assistant_merged",
                        &error,
                    )
                    .await;
                    false
                }
            };
            let _ = result_tx.send(persisted);
        };
        let admitted = if let Some(fence) = publication_fence.as_ref() {
            fence.commit_if_current(Box::pin(persist)).await
        } else {
            persist.await;
            true
        };
        if admitted && result_rx.await.unwrap_or(false) {
            let output = self.output.clone();
            let published_uuids = [Some(line_uuid)];
            let publish =
                output.emit_assistant_transcript_row_uuids(&assistant_id, &published_uuids);
            if let Some(fence) = publication_fence.as_ref() {
                fence.publish_if_current(Box::pin(publish)).await;
            } else {
                publish.await;
            }
        }
        rewritten
    }

    /// Update the out-of-band assistant timestamp without changing message wire
    /// shape. Every production assistant commit passes through one of the JSONL
    /// persistence seams, including writer-less runtimes.
    pub(crate) async fn note_assistant_commit(&self, msg: &ConversationMessage) {
        if matches!(msg, ConversationMessage::Assistant { .. }) {
            // An ordinary later assistant replaces a recoverable API-error row.
            // API persistence stamps its explicit marker after this common seam.
            self.note_turn_api_error_stop_reason(None);
            self.session.lock().await.message_timing.last_assistant_at =
                Some(std::time::SystemTime::now());
        }
    }

    /// Mark one completed streamed assistant turn after its per-block rows
    /// have been written. The block writer intentionally does not update this
    /// once-per-turn timestamp for every individual row.
    pub(crate) async fn note_assistant_stream_commit(&self, msg: &ConversationMessage) {
        self.note_assistant_commit(msg).await;
    }
}

/// Claude 2.1.270 `npe`: strip ANSI, normalize whitespace, remove invisible
/// controls, then truncate without splitting a UTF-16 surrogate pair.
fn scheduled_fire_prompt(prompt: &str) -> String {
    use std::sync::LazyLock;
    static ANSI: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
        r"[\x{001B}\x{009B}][\[\]()#;?]*(?:(?:(?:(?:;[-a-zA-Z0-9/#&.:=?%@~_]+)*|[a-zA-Z0-9]+(?:;[-a-zA-Z0-9/#&.:=?%@~_]*)*)?(?:\x{0007}|\x{001B}\x{005C}|\x{009C}))|(?:(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-PR-TZcf-nq-uy=><~]))"
    ).expect("oracle ANSI pattern")
    });
    static CONTROLS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"[\p{Cc}\p{Cf}\x{2028}\x{2029}]").expect("control pattern")
    });
    let stripped = ANSI.replace_all(prompt, "");
    let stripped: String = stripped
        .chars()
        .filter(|c| {
            !matches!(*c as u32,
        0..=8 | 14..=31 | 127..=159)
        })
        .collect();
    static SPACE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
        r"[ \t\n\v\f\r\x{00A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]+"
    ).expect("ECMAScript whitespace")
    });
    let normalized = SPACE.replace_all(&stripped, " ");
    let ansi_clean = ANSI.replace_all(&normalized, "");
    let clean = CONTROLS.replace_all(&ansi_clean, "");
    let mut units = 0;
    clean
        .trim()
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= 200
        })
        .collect()
}

#[cfg(test)]
#[test]
fn scheduled_fire_prompt_matches_oracle_sanitization_and_utf16_limit() {
    let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../../session/tests/fixtures/loop-2.1.270/prompt.json"
    ))
    .unwrap();
    for case in cases {
        assert_eq!(
            scheduled_fire_prompt(case["input"].as_str().unwrap()),
            case["expected"].as_str().unwrap()
        );
    }
}

#[cfg(test)]
#[test]
fn mod_append_content_block_preserves_optional_wire_presence() {
    use lingxi_core::types::{ContentBlock, ToolUseId};

    let text = ContentBlock::Text {
        text: "answer".into(),
        citations: Some(None),
    };
    let projected_text = mod_append_content_block(&text);
    assert!(projected_text.get_property("citations").is_some());
    assert!(projected_text.get_property("citations").unwrap().is_null());

    let omitted = ContentBlock::ToolResult {
        tool_use_id: ToolUseId::from("toolu_1"),
        content: "ok".into(),
        is_error: None,
        provider_tool_use_id: None,
        content_blocks: None,
    };
    assert!(mod_append_content_block(&omitted)
        .get_property("is_error")
        .is_none());

    let explicit_false = ContentBlock::ToolResult {
        tool_use_id: ToolUseId::from("toolu_1"),
        content: "ok".into(),
        is_error: Some(false),
        provider_tool_use_id: None,
        content_blocks: None,
    };
    assert_eq!(
        mod_append_content_block(&explicit_false)
            .get_property("is_error")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    let accepted_text = rewrite_mod_append_blocks(
        &[],
        &[Source::parse(r#"{"type":"text","text":"new answer","citations":null}"#).unwrap()],
    );
    assert!(matches!(
        accepted_text.as_slice(),
        [ContentBlock::Text {
            citations: Some(None),
            ..
        }]
    ));
    let projected_accepted_text = mod_append_content_block(&accepted_text[0]);
    assert!(projected_accepted_text.get_property("citations").is_some());
    assert!(projected_accepted_text
        .get_property("citations")
        .unwrap()
        .is_null());

    let old_error = ContentBlock::ToolResult {
        tool_use_id: ToolUseId::from("toolu_1"),
        content: "failed".into(),
        is_error: Some(true),
        provider_tool_use_id: None,
        content_blocks: None,
    };
    let omitted_error = rewrite_mod_append_tool_result(
        &old_error,
        &Source::parse(r#"{"type":"tool_result","content":"accepted"}"#).unwrap(),
    );
    assert!(matches!(
        omitted_error,
        ContentBlock::ToolResult { is_error: None, .. }
    ));
}

#[cfg(test)]
#[test]
fn opaque_text_append_echo_preserves_source_but_replacement_drops_metadata() {
    use lingxi_core::types::ContentBlock;
    let source = ContentBlock::ProviderContent {
        protocol: "anthropic_messages".into(),
        value: Source::parse(r#"{"type":"text","text":"before","citations":[{"type":"future_citation"}],"extra_native":{"kept":289}}"#).unwrap(),
    };
    let projected = mod_append_content_block(&source);
    assert_eq!(
        rewrite_mod_append_blocks(
            std::slice::from_ref(&source),
            std::slice::from_ref(&projected),
        ),
        vec![source.clone()]
    );
    let mut changed = projected;
    changed.set_field("text", "after".into()).unwrap();
    // Native q/D and the enabled-Mod loopback both replace modified text with
    // one canonical Text:null, rather than retaining the old source metadata.
    assert_eq!(
        rewrite_mod_append_blocks(&[source], &[changed]),
        vec![ContentBlock::Text {
            text: "after".into(),
            citations: Some(None)
        }]
    );
}

#[cfg(test)]
#[tokio::test]
async fn native_session_append_presence_survives_durable_resume_and_anthropic_wire() {
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, SessionId};
    use llm_runtime::services::sdk::{self, WireCodec};
    use serde_json::json;
    use std::sync::Arc;

    let root = tempfile::tempdir().expect("temporary session root");
    let cwd = root.path().to_string_lossy().into_owned();
    let session_id = SessionId::new();
    let citations = json!([{
        "type":"web_search_result_location",
        "url":"https://example.test/source",
        "title":"Source",
        "cited_text":"source text"
    }]);
    let profile: sdk::protocol::ProviderProfile = serde_json::from_value(json!({
        "provider_id":"anthropic", "profile_name":"test", "base_url":"https://api.anthropic.com",
        "protocol":"anthropic_messages", "auth":"none", "models":[]
    }))
    .expect("build public Anthropic codec profile");
    let decode_context =
        sdk::CodecContext::new(&profile, "claude-sonnet-4-5", sdk::RequestMode::Complete);
    let decoded = sdk::AnthropicMessagesCodec
        .decode_response(
            &sdk::transport::HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&json!({
                    "id":"msg-presence-source","type":"message","role":"assistant","model":"claude-sonnet-4-5",
                    "content":[
                        {"type":"text","text":"source text"},
                        {"type":"text","text":"source cited text","citations":citations,"provider_metadata":{"keep":"native"}},
                        {"type":"tool_use","id":"toolu_presence_e2e","name":"Example","input":{}}
                    ],
                    "stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":6}
                }))
                .expect("encode fake provider response")
                .into(),
            },
            &decode_context,
        )
        .expect("decode provider assistant response");
    let decoded_history = llm_runtime::HistoryResponse::from_model(
        decoded,
        sdk::protocol::ProtocolFamily::AnthropicMessages,
    )
    .expect("project decoded SDK response to history");
    let source_content = crate::turn_loop::translate_response_blocks(&decoded_history.content);
    assert_eq!(source_content.len(), 3, "two text blocks and one ToolUse");
    assert!(matches!(
        source_content.get(1),
        Some(ContentBlock::ProviderContent { protocol, value })
            if protocol == "anthropic_messages"
                && value.string_at("/text") == Some("source cited text")
                && value.subprojection("/citations").unwrap() == Source::from_serde_value(&citations).unwrap()
                && value.string_at("/provider_metadata/keep") == Some("native")
    ));
    let transcript_path =
        session::jsonl::session_path(root.path(), &cwd, &session_id.as_uuid().to_string());
    let state_root = root.path().join("durable-state");
    std::fs::create_dir_all(&state_root).expect("create durable state root");
    let durable = Arc::new(
        session::jsonl::DurableTranscriptWriter::open(&state_root)
            .expect("open durable transcript writer"),
    );
    let writer = Arc::new(
        session::jsonl::JsonlWriter::new(
            transcript_path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            )),
        )
        .with_durable_lock(durable),
    );
    writer
        .activate_session_target(
            session_id,
            transcript_path.clone(),
            root.path().to_path_buf(),
        )
        .expect("bind durable transcript target");

    let module_path = root.path().join("session-append.js");
    std::fs::write(
        &module_path,
        r#"
        export function register(on) {
          on('session.append', ($, event, next) => {
            if (event.message.type === 'assistant'
                && event.message.content.some(block => block.type === 'text')) {
              if (Object.prototype.hasOwnProperty.call(event.message, 'stop_reason')) {
                throw new Error('stop_reason is outside the Native append preview');
              }
              return next({ ...event, message: { ...event.message, content:
                event.message.content.map(block => {
                  if (block.type !== 'text') return block;
                  if (block.text === 'source text') {
                    return { ...block, text: 'accepted Mod text', citations: null };
                  }
                  if (block.text === 'source cited text') {
                    if (!Array.isArray(block.citations)) {
                      throw new Error('fixture expected typed source citations');
                    }
                    return { ...block, text: 'accepted Mod cited text' };
                  }
                  return block;
                })
              } });
            }
            if (event.message.type === 'user'
                && event.message.content.some(block => block.type === 'tool_result')) {
              return next({ ...event, message: { ...event.message, content:
                event.message.content.map(block => {
                  if (block.type !== 'tool_result') return block;
                  if (block.is_error !== true) {
                    throw new Error('fixture expected the source error flag');
                  }
                  const { is_error, ...withoutError } = block;
                  return withoutError;
                })
              } });
            }
            return next(event);
          });
        }
        "#,
    )
    .expect("write Native session.append fixture");
    let host = hooks::mods::ModHost::start(None)
        .await
        .expect("start Mod host");
    host.load(
        "wire-presence",
        root.path(),
        &module_path,
        Source::parse("{}").unwrap(),
    )
    .await
    .expect("load Native session.append Mod");
    let mut hook_registry = hooks::HookRegistry::new();
    hook_registry.set_mod_host(host);
    let orchestrator = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(crate::test_support::MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        crate::test_support::noop_hook_executor(),
        Arc::new(crate::test_support::NoOpPermissionGate),
        Arc::new(crate::test_support::MockOutputStream::new()),
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hook_registry)))
    .with_jsonl_writer(writer)
    .with_session_id(session_id)
    .with_config_home(root.path().to_path_buf());

    let prompt = ConversationMessage::user(MessageId::new(), "start presence replay".into());
    orchestrator.persist_message_to_jsonl(&prompt).await;

    let tool_use_id = source_content
        .iter()
        .find_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("decoded ToolUse has a paired result");
    let tool_use_wire_id = tool_use_id.to_string();
    let mut assistant_row = crate::streaming_loop::CompletedAssistantRow {
        per_turn_effort: None,
        stream_order: 0,
        row_id: MessageId::new(),
        logical_message_id: None,
        provider_message_id: "assistant-presence-row".into(),
        model: "claude-sonnet-4-5".into(),
        model_profile: None,
        is_api_error: false,
        stop_reason: Some("tool_use".into()),
        resumable: None,
        stop_details: None,
        usage: None,
        request_id: None,
        timestamp: "2026-10-05T12:00:00.000Z".into(),
        persisted_link: None,
        content: source_content,
        session_append_dispatched: false,
        supersedes_row_ids: Vec::new(),
    };
    let assistant_link = orchestrator
        .persist_completed_assistant_row(&mut assistant_row, None)
        .await
        .expect("persist completed Mod-edited assistant row");
    assert_eq!(assistant_row.content.len(), 3);
    assert!(matches!(
        assistant_row.content.first(),
        Some(ContentBlock::Text {
            text,
            citations: Some(None),
        }) if text == "accepted Mod text"
    ));
    assert!(matches!(
        assistant_row.content.get(1),
        Some(ContentBlock::Text { text, citations: Some(None) })
            if text == "accepted Mod cited text"
    ));
    assert_eq!(
        assistant_row
            .content
            .iter()
            .filter_map(ContentBlock::visible_text)
            .collect::<String>(),
        "accepted Mod textaccepted Mod cited text"
    );

    let tool_result = ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: MessageId::new(),
        content: vec![ContentBlock::ToolResult {
            tool_use_id,
            content: "original tool result".into(),
            is_error: Some(true),
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    };
    orchestrator.persist_message_to_jsonl(&tool_result).await;

    let loaded = session::jsonl::load_session(
        root.path(),
        &cwd,
        session_id.as_uuid(),
        Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        )),
    )
    .await
    .expect("reload durable transcript");
    assert_eq!(loaded.len(), 3, "prompt, accepted assistant, paired result");
    let accepted_assistant = &loaded[1].message;
    assert_eq!(
        accepted_assistant.string_at("/stop_reason"),
        Some("tool_use")
    );
    assert_eq!(accepted_assistant.array_len("/content"), Some(3));
    assert!(accepted_assistant
        .subprojection("/content/0/citations")
        .unwrap()
        .at_pointer("")
        .unwrap()
        .is_null());
    assert_eq!(
        accepted_assistant.string_at("/content/1/type"),
        Some("text")
    );
    assert_eq!(
        accepted_assistant.string_at("/content/1/text"),
        Some("accepted Mod cited text")
    );
    assert!(accepted_assistant
        .subprojection("/content/1/citations")
        .unwrap()
        .at_pointer("")
        .unwrap()
        .is_null());
    assert!(accepted_assistant
        .subprojection("/content/1/provider_metadata")
        .is_err());
    assert_eq!(
        accepted_assistant.string_at("/content/2/id"),
        Some(tool_use_wire_id.as_str())
    );
    assert!(loaded[2]
        .message
        .subprojection("/content/0/is_error")
        .is_err());
    assert_eq!(
        loaded[2].message.string_at("/content/0/tool_use_id"),
        Some(tool_use_wire_id.as_str())
    );

    let resumed = crate::resume::state_from_messages(session_id.as_uuid(), &loaded);
    assert!(matches!(
        resumed.history.get(1),
        Some(ConversationMessage::Assistant { content, .. })
            if content.len() == 3
                && matches!(content.first(), Some(ContentBlock::Text {
                citations: Some(None), ..
            }))
                && matches!(content.get(1), Some(ContentBlock::Text {
                    text, citations: Some(None),
                }) if text == "accepted Mod cited text")
    ));
    assert!(matches!(
        resumed.history.get(2),
        Some(ConversationMessage::User { content, .. })
            if matches!(content.first(), Some(ContentBlock::ToolResult {
                is_error: None, ..
            }))
    ));

    let llm_messages = llm_runtime::convert::to_llm_messages(resumed.history)
        .expect("convert resumed core history");
    let (request, exact_string_overrides) = llm_runtime::convert::history_input(
        "claude-sonnet-4-5",
        &llm_messages,
        &[],
        &[],
        sdk::protocol::ProtocolFamily::AnthropicMessages,
    )
    .expect("project Anthropic history input");
    assert!(exact_string_overrides.is_empty());
    let encoded = sdk::AnthropicMessagesCodec
        .encode_request(
            sdk::EncodeRequest::new(&request),
            &sdk::CodecContext::new(&profile, &request.model, sdk::RequestMode::Complete),
        )
        .expect("encode public Anthropic request");
    let wire_body = String::from_utf8(encoded.body.to_vec()).expect("Anthropic body is UTF-8 JSON");
    assert!(wire_body.contains(r#""citations":null"#), "{wire_body}");
    assert!(!wire_body.contains("provider_metadata"), "{wire_body}");
    let body: serde_json::Value = serde_json::from_str(&wire_body).unwrap();
    let accepted_cited = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| block["text"] == "accepted Mod cited text")
        .unwrap();
    assert_eq!(
        accepted_cited,
        &serde_json::json!({
            "type":"text", "text":"accepted Mod cited text", "citations":null
        })
    );
    assert_eq!(wire_body.matches("accepted Mod cited text").count(), 1);
    assert!(!wire_body.contains(r#""is_error""#), "{wire_body}");
    assert!(wire_body.contains(r#""type":"tool_use""#), "{wire_body}");
    assert!(wire_body.contains(r#""type":"tool_result""#), "{wire_body}");
    assert!(
        wire_body.contains(&format!(r#""id":"{tool_use_wire_id}""#)),
        "{wire_body}"
    );
    assert!(
        wire_body.contains(&format!(r#""tool_use_id":"{tool_use_wire_id}""#)),
        "{wire_body}"
    );
    assert_eq!(assistant_link.uuid, loaded[1].uuid);
}
#[cfg(test)]
#[tokio::test]
async fn mod_utf16_text_is_exact_live_and_raw_but_native_cold_resume_projects_replacements() {
    use hooks::mods::ModHost;
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, SessionId};
    use llm_runtime::services::sdk::{self, WireCodec};
    use serde_json::{json, Value};
    use std::sync::Arc;

    let root = tempfile::tempdir().expect("temporary session root");
    let cwd = root.path().to_string_lossy().into_owned();
    let session_id = SessionId::new();
    let transcript_path =
        session::jsonl::session_path(root.path(), &cwd, &session_id.as_uuid().to_string());
    let state_root = root.path().join("durable-state");
    std::fs::create_dir_all(&state_root).expect("create durable state root");
    let durable = Arc::new(
        session::jsonl::DurableTranscriptWriter::open(&state_root)
            .expect("open durable transcript writer"),
    );
    let writer = Arc::new(
        session::jsonl::JsonlWriter::new(
            transcript_path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            )),
        )
        .with_durable_lock(durable),
    );
    writer
        .activate_session_target(
            session_id,
            transcript_path.clone(),
            root.path().to_path_buf(),
        )
        .expect("bind durable transcript target");

    let accepted_units = "accepted Mod text "
        .encode_utf16()
        .chain([0xd800, 0x03a9, 0xdc00])
        .collect::<Vec<_>>();
    let accepted_text = String::from_utf16_lossy(&accepted_units);
    let replacement_text = String::from_utf16_lossy(
        &accepted_units
            .iter()
            .map(|unit| match unit {
                0xd800 | 0xdc00 => 0xfffd,
                unit => *unit,
            })
            .collect::<Vec<_>>(),
    );
    let source_units = "source Mod text "
        .encode_utf16()
        .chain([0xd800, 0x03a9, 0xdc00])
        .collect::<Vec<_>>();
    let source_text = String::from_utf16_lossy(&source_units);

    let module_path = root.path().join("utf16-session-append.js");
    std::fs::write(
        &module_path,
        r#"
const units = text => Array.from({length:text.length}, (_, index) => text.charCodeAt(index));
export function register(on) {
  on('session.append', async ($, event, next) => {
    if (event.message.type !== 'assistant') return next(event);
    const before = event.message.content.filter(block => block.type === 'text').map(block => ({text:block.text.toWellFormed(), units:units(block.text)}));
    const content = event.message.content.map(block => block.type === 'text'
      ? {...block, text:'accepted Mod text ' + String.fromCharCode(0xd800) + 'Ω' + String.fromCharCode(0xdc00), citations:null}
      : block);
    const forwarded = {...event, message:{...event.message, content}};
    const result = await next(forwarded);
    const resultUnits = result.message.content.filter(block => block.type === 'text').map(block => ({text:block.text.toWellFormed(), units:units(block.text)}));
    await $.fs.write('append-units.json', JSON.stringify({before, forwarded:units(content[0].text), result:resultUnits, citations:result.message.content[0].citations}));
    return result;
  });
  on('turn.complete', async ($, event, next) => {
    // Direct session API calls: this module registers no `session.messages`
    // middleware, so both forms cross the nested worker API result boundary.
    const summary = await $.session.messages();
    const api = await $.session.messages({as:'api'});
    const summaryRow = summary.find(row => row.role === 'assistant' && row.text.startsWith('accepted Mod text '));
    const apiBlock = api.flatMap(row => row.content ?? []).find(block => block.type === 'text' && block.text.startsWith('accepted Mod text '));
    const observe = row => row && ({text:row.text.toWellFormed(), units:units(row.text)});
    const observeBlock = block => block && ({text:block.text.toWellFormed(), units:units(block.text), citations:block.citations});
    const suffix = event.answer === 'cold-resume' ? 'resume' : 'live';
    await $.fs.write('session-views-' + suffix + '.json', JSON.stringify({summary:observe(summaryRow), api:observeBlock(apiBlock)}));
    return next(event);
  });
}
"#,
    )
    .expect("write UTF-16 Mod fixture");
    let host = ModHost::start(None).await.expect("start Mod host");
    host.load(
        "utf16-session-append",
        root.path(),
        &module_path,
        Source::parse("{}").unwrap(),
    )
    .await
    .expect("load UTF-16 session.append Mod");
    let mut hook_registry = hooks::HookRegistry::new();
    hook_registry.set_mod_host(host.clone());
    let orchestrator = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(crate::test_support::MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        crate::test_support::noop_hook_executor(),
        Arc::new(crate::test_support::NoOpPermissionGate),
        Arc::new(crate::test_support::MockOutputStream::new()),
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hook_registry)))
    .with_jsonl_writer(writer)
    .with_session_id(session_id)
    .with_config_home(root.path().to_path_buf());

    let prompt =
        ConversationMessage::user(MessageId::new(), "live UTF-16 session projection".into());
    orchestrator
        .session
        .lock()
        .await
        .history
        .push(prompt.clone());
    orchestrator.persist_message_to_jsonl(&prompt).await;
    let assistant = ConversationMessage::Assistant {
        per_turn_effort: None,
        id: MessageId::new(),
        content: vec![ContentBlock::TextJsUtf16 {
            text: source_text.clone(),
            utf16_code_units: source_units.clone(),
            citations: Some(Some(
                Source::parse(r#"[{"type":"source","label":"before"}]"#).unwrap(),
            )),
        }],
        stop_reason: Some("end_turn".into()),
        resumable: None,
    };
    orchestrator
        .session
        .lock()
        .await
        .history
        .push(assistant.clone());
    let accepted = orchestrator
        .persist_assistant_merged(&assistant, None, None, None)
        .await;
    assert!(matches!(
        &accepted,
        ConversationMessage::Assistant { content, .. }
            if matches!(content.as_slice(), [ContentBlock::TextJsUtf16 {
                text, utf16_code_units, citations: Some(None),
            }] if text == &accepted_text && utf16_code_units == &accepted_units)
    ));
    let append_views: Value = serde_json::from_str(
        &std::fs::read_to_string(root.path().join("append-units.json"))
            .expect("read session.append observations"),
    )
    .expect("parse session.append observations");
    assert_eq!(append_views["before"][0]["units"], json!(&source_units));
    assert_eq!(append_views["forwarded"], json!(&accepted_units));
    assert_eq!(append_views["result"][0]["units"], json!(&accepted_units));
    assert_eq!(append_views["citations"], Value::Null);

    let fire_turn_complete = |answer: &'static str| {
        let host = host.clone();
        let orchestrator = &orchestrator;
        async move {
            host.dispatch_with_ui_meta_at_session(
                "turn.complete",
                Source::from_serde_value(&json!({"answer":answer,"turnId":"utf16-live-test","durationMs":0,"isAborted":false,"reason":"answer"})).unwrap(),
                orchestrator,
                |event| async move { Ok(Some(source_object([("text", event.expect("turn event").subprojection("/answer").unwrap())]))) },
                |_, _| async {},
                |_, _, _| async {},
                |_, _| async {},
            )
            .await
            .expect("dispatch session.messages through a real Mod hook")
        }
    };
    fire_turn_complete("live").await;
    let live_views: Value = serde_json::from_str(
        &std::fs::read_to_string(root.path().join("session-views-live.json"))
            .expect("read live session.messages observation"),
    )
    .expect("parse live session.messages observation");
    assert_eq!(live_views["summary"]["units"], json!(&accepted_units));
    assert_eq!(live_views["api"]["units"], json!(&accepted_units));
    assert_eq!(live_views["api"]["citations"], Value::Null);

    let live_history = orchestrator.session.lock().await.history.clone();
    let llm_messages =
        llm_runtime::convert::to_llm_messages(live_history).expect("convert exact live history");
    let (request, live_overrides) = llm_runtime::convert::history_input(
        "claude-sonnet-4-5",
        &llm_messages,
        &[],
        &[],
        sdk::protocol::ProtocolFamily::AnthropicMessages,
    )
    .expect("project exact live history");
    assert!(live_overrides
        .values()
        .any(|units| units == &accepted_units));
    let profile: sdk::protocol::ProviderProfile = serde_json::from_value(json!({
        "provider_id":"anthropic", "profile_name":"test", "base_url":"https://api.anthropic.com",
        "protocol":"anthropic_messages", "auth":"none", "models":[]
    }))
    .expect("build public Anthropic codec profile");
    let encoded = sdk::AnthropicMessagesCodec
        .encode_request(
            sdk::EncodeRequest::new(&request),
            &sdk::CodecContext::new(&profile, &request.model, sdk::RequestMode::Complete),
        )
        .expect("public SDK encodes the safe Anthropic body");
    let encoded_json = JsonPayload::parse(std::str::from_utf8(&encoded.body).unwrap())
        .expect("parse exact public SDK request projection");
    let live_wire = String::from_utf8(
        sdk::exact_json::serialize_json_payload_with_overrides(
            &encoded_json,
            &live_overrides,
            sdk::exact_json::JsonEncoding::JavaScript,
        )
        .expect("public SDK serializes host UTF-16 sidecars"),
    )
    .expect("exact public SDK body remains UTF-8 JSON");
    assert!(
        live_wire.contains(r#""text":"accepted Mod text \ud800Ω\udc00""#),
        "{live_wire}"
    );
    assert!(live_wire.contains(r#""citations":null"#), "{live_wire}");

    let raw_jsonl =
        std::fs::read_to_string(&transcript_path).expect("read durable transcript bytes");
    assert!(raw_jsonl.contains(r#"\ud800Ω\udc00"#), "{raw_jsonl}");
    assert!(!raw_jsonl.contains("utf16_code_units"), "{raw_jsonl}");
    assert!(
        !raw_jsonl.contains("__lingxiModUtf16StringsV1"),
        "{raw_jsonl}"
    );
    let loaded = session::jsonl::load_session(
        root.path(),
        &cwd,
        session_id.as_uuid(),
        Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        )),
    )
    .await
    .expect("load durable UTF-16 transcript");
    let assistant_row = loaded.last().expect("loaded assistant row");
    assert_eq!(
        session::jsonl::exact_json::message_utf16_overrides(assistant_row)
            ["/message/content/0/text"],
        accepted_units
    );
    let resumed = crate::resume::state_from_messages(session_id.as_uuid(), &loaded);
    let ConversationMessage::Assistant { content, .. } = &resumed.history[1] else {
        panic!("resumed accepted row stays assistant history");
    };
    assert!(matches!(
        content.as_slice(),
        [ContentBlock::Text { text, citations: Some(None) }]
            if text == &replacement_text
    ));
    orchestrator.session.lock().await.history = resumed.history;
    fire_turn_complete("cold-resume").await;
    let resumed_views: Value = serde_json::from_str(
        &std::fs::read_to_string(root.path().join("session-views-resume.json"))
            .expect("read resumed session.messages observation"),
    )
    .expect("parse resumed session.messages observation");
    let replacement_units = replacement_text.encode_utf16().collect::<Vec<_>>();
    assert_eq!(resumed_views["summary"]["units"], json!(&replacement_units));
    assert_eq!(resumed_views["api"]["units"], json!(&replacement_units));

    let resumed_llm =
        llm_runtime::convert::to_llm_messages(orchestrator.session.lock().await.history.clone())
            .expect("convert cold-resumed history");
    let (resumed_request, resumed_overrides) = llm_runtime::convert::history_input(
        "claude-sonnet-4-5",
        &resumed_llm,
        &[],
        &[],
        sdk::protocol::ProtocolFamily::AnthropicMessages,
    )
    .expect("project cold-resumed history");
    assert!(resumed_overrides.is_empty());
    let resumed_encoded = sdk::AnthropicMessagesCodec
        .encode_request(
            sdk::EncodeRequest::new(&resumed_request),
            &sdk::CodecContext::new(&profile, &resumed_request.model, sdk::RequestMode::Complete),
        )
        .expect("public SDK encodes the cold-resume projection");
    let resumed_wire = String::from_utf8(resumed_encoded.body.to_vec())
        .expect("resumed public SDK body is UTF-8 JSON");
    assert!(resumed_wire.contains(&replacement_text), "{resumed_wire}");
}

#[cfg(test)]
mod rich_append_projection_tests {
    use super::*;
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};

    fn worker_frame(message: &ConversationMessage) -> Source {
        source_object([("message", mod_append_message(message))])
    }

    #[tokio::test]
    async fn pending_prompt_history_accepts_restored_text_and_rejects_invalid_same_uuid() {
        let root = tempfile::tempdir().unwrap();
        let orchestrator = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(crate::test_support::MockApiClient::new(vec![])),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            crate::test_support::noop_hook_executor(),
            Arc::new(crate::test_support::NoOpPermissionGate),
            Arc::new(crate::test_support::MockOutputStream::new()),
            Arc::new(crate::test_support::StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        );

        let prompt_id = MessageId::new();
        let mut restored = ConversationMessage::user(prompt_id, "prompt".into());
        let refused_message_id =
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#""msg_\ud800""#).unwrap();
        if let ConversationMessage::User {
            refused_message_id: carrier,
            ..
        } = &mut restored
        {
            *carrier = Some(refused_message_id.clone());
        }
        let mut retry = restored.clone();
        if let ConversationMessage::User { content, .. } = &mut retry {
            content[0] = ContentBlock::TextJsUtf16 {
                text: "prompt".into(),
                utf16_code_units: "prompt".encode_utf16().collect(),
                citations: None,
            };
        }
        orchestrator.session.lock().await.history.push(restored);
        orchestrator
            .append_pending_prompt_message_to_history_once(&retry)
            .await
            .expect("same source units across restored text carriers are idempotent");
        assert_eq!(orchestrator.session.lock().await.history.len(), 1);

        if let ConversationMessage::User {
            refused_message_id: carrier,
            ..
        } = &mut retry
        {
            *carrier = Some(
                lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#""other-id""#)
                    .unwrap(),
            );
        }
        let error = orchestrator
            .append_pending_prompt_message_to_history_once(&retry)
            .await
            .expect_err("the same UUID with another refusal id is not idempotent");
        assert!(matches!(
            error,
            OrchestratorError::Internal(message)
                if message.contains("has different source content")
        ));
        assert_eq!(orchestrator.session.lock().await.history.len(), 1);

        let invalid_id = MessageId::new();
        let mut invalid = ConversationMessage::user(invalid_id, "wrong display".into());
        if let ConversationMessage::User { content, .. } = &mut invalid {
            content[0] = ContentBlock::TextJsUtf16 {
                text: "wrong display".into(),
                utf16_code_units: "actual source".encode_utf16().collect(),
                citations: None,
            };
        }
        orchestrator
            .session
            .lock()
            .await
            .history
            .push(invalid.clone());
        let error = orchestrator
            .append_pending_prompt_message_to_history_once(&invalid)
            .await
            .expect_err("same UUID with an inconsistent text carrier is not idempotent");
        assert!(matches!(
            error,
            OrchestratorError::Internal(message)
                if message.contains("has different source content")
        ));
        assert_eq!(orchestrator.session.lock().await.history.len(), 2);
    }

    #[test]
    fn tool_input_keeps_exact_source_and_changed_surrogate_keys_do_not_match() {
        let input = Source::parse(r#"{"\ud800":"\udfff"}"#).unwrap();
        let original = ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            stop_reason: Some("tool_use".into()),
            resumable: None,
            content: vec![ContentBlock::ToolUse {
                id: ToolUseId::from("toolu_exact"),
                name: "Echo".into(),
                input: input.clone(),
                provider_id: None,
            }],
        };

        let forwarded = worker_frame(&original);
        let forwarded_message = forwarded.subprojection("/message").unwrap();
        let rewritten = rewrite_mod_append_message(&original, &forwarded_message).unwrap();
        assert!(mod_append_projection_matches(
            &rewritten,
            &forwarded_message
        ));
        assert_eq!(
            mod_append_message(&rewritten)
                .subprojection("/content/0/input")
                .unwrap()
                .to_json_string()
                .unwrap(),
            input.to_json_string().unwrap()
        );

        let mut tampered = mod_append_message(&original);
        let mut blocks = source_array_items(&tampered.subprojection("/content").unwrap()).unwrap();
        blocks[0]
            .set_field("input", Source::parse(r#"{"\ud801":"\udfff"}"#).unwrap())
            .unwrap();
        tampered
            .set_field("content", Source::array(blocks).unwrap())
            .unwrap();
        assert!(!mod_append_projection_matches(&rewritten, &tampered));
    }

    #[test]
    fn tool_result_source_survives_omission_and_block_repositioning() {
        let data = Source::parse(r#"[{"type":"text","text":"\udc00","\udfff":"\ud800"}]"#).unwrap();
        let original = ConversationMessage::User {
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
            id: MessageId::new(),
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: ToolUseId::from("toolu_exact"),
                content: "�".into(),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: Some(data.clone()),
            }],
        };

        let omitted = source_object([(
            "content",
            Source::array(vec![source_object([
                ("type", "tool_result".into()),
                ("tool_use_id", "toolu_exact".into()),
            ])])
            .unwrap(),
        )]);
        let retained = rewrite_mod_append_message(&original, &omitted).unwrap();
        assert_eq!(
            mod_append_message(&retained)
                .subprojection("/content/0/content")
                .unwrap()
                .to_json_string()
                .unwrap(),
            data.to_json_string().unwrap()
        );

        let reordered = source_object([(
            "content",
            Source::array(vec![
                source_object([
                    ("type", "text".into()),
                    ("text", "added before the result".into()),
                ]),
                source_object([
                    ("type", "tool_result".into()),
                    ("tool_use_id", "toolu_exact".into()),
                    ("content", data.clone()),
                    ("is_error", Source::parse("false").unwrap()),
                ]),
            ])
            .unwrap(),
        )]);
        let rewritten = rewrite_mod_append_message(&original, &reordered).unwrap();
        let rewritten_source = mod_append_message(&rewritten);
        assert_eq!(
            rewritten_source
                .subprojection("/content/0/content")
                .unwrap()
                .to_json_string()
                .unwrap(),
            data.to_json_string().unwrap()
        );
        assert!(rewritten_source
            .to_json_string()
            .unwrap()
            .contains("added before the result"));
    }
}
