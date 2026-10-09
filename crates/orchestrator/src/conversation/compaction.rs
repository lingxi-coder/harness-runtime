//! Manual and automatic compaction plus session-memory coordination.

use super::*;

/// Current Ake/aW text read: bound disk bytes, decode lossily, then normalize
/// the complete retained body before the caller applies its token budget.
pub(super) async fn read_restored_text_prefix(
    path: &std::path::Path,
    max_bytes: usize,
) -> std::io::Result<(String, bool)> {
    use tokio::io::AsyncReadExt as _;
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    file.take(u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .await?;
    let truncated = bytes.len() > max_bytes;
    bytes.truncate(max_bytes);
    let read = String::from_utf8_lossy(&bytes);
    let mut content = read
        .strip_prefix('\u{feff}')
        .unwrap_or(&read)
        .replace("\r\n", "\n");
    if content.ends_with('\r') {
        content.pop();
    }
    Ok((content, truncated))
}

/// Which side of the surviving messages the fresh summary lands on.
///
/// 🚨 There is exactly ONE `AfterKept` caller — `/rewind`'s "Summarize from
/// here" — and the distinction is not cosmetic: it decides whether the model
/// reads the summary as "here is everything before this point" or "here is
/// everything after it". Both orders are well-formed histories, so a wrong
/// choice produces no error, just a model that has the conversation backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummaryPlacement {
    /// `[marker, summary, kept…]` — the summary replaces the older messages.
    /// Every automatic, reactive and `/compact` pass, plus `summarize_up_to`.
    BeforeKept,
    /// `[marker, kept…, summary]` — the earlier messages survive and the
    /// summary covers what came after them. `summarize` ("from here") only.
    AfterKept,
}

/// The two boundary fields oracle `mne` sets only on the message-selector path
/// (`mne("manual", preTokens, lastUuid, userContext, messagesSummarized)`).
#[derive(Debug, Clone)]
pub(crate) struct SelectorBoundaryMetadata {
    /// `messagesSummarized` — how many messages the summary replaced.
    pub messages_summarized: u32,
    /// `userContext` — the free text the user typed into the picker row, if
    /// any. `None` when they submitted the row empty.
    pub user_context: Option<String>,
}

// ⛔ No `logicalParentUuid` field here, deliberately. The oracle's `hn` is a
// CONVERSATION message uuid (`up_to` takes `e.slice(0,n).findLast(…)`, `from`
// takes `Se.at(-1)` — the same message either way, the last one before the
// split). This port derives it from `transcript.last_jsonl_uuid` instead,
// because the value has to match an on-disk JSONL line uuid, and
// `apply_post_compact` overwrites whatever the boundary constructor was given.
// Passing `hn` in here would read as wired and be discarded two hundred lines
// later.

#[derive(Clone)]
struct SessionCompactRowRecord {
    projection: serde_json::Value,
    messages: Vec<ConversationMessage>,
}

/// Converts between Harness conversation messages and Claude's compact-event
/// rows. Native Mods see `{ role, text, toolUses, toolResults?, handle? }`
/// rows, not the host's full ConversationMessage representation. The handle
/// table lets unchanged rows recover their original structured messages.
#[derive(Default)]
struct SessionCompactRowCodec {
    records: std::collections::HashMap<String, SessionCompactRowRecord>,
}

impl SessionCompactRowCodec {
    fn rows_for_messages(&mut self, messages: &[ConversationMessage]) -> Vec<serde_json::Value> {
        let mut tool_result_details = std::collections::HashMap::new();
        for message in messages {
            let Some(row) = session_compact_row(message) else {
                continue;
            };
            if let Some(tool_results) = row.get("toolResults").and_then(serde_json::Value::as_array)
            {
                for tool_result in tool_results {
                    if let Some(tool_use_id) = tool_result
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                    {
                        tool_result_details.insert(tool_use_id.to_string(), tool_result.clone());
                    }
                }
            }
        }
        let mut rows = Vec::new();
        let mut leading_unrepresented = Vec::new();
        for message in messages {
            let Some(mut row) = session_compact_row(message) else {
                leading_unrepresented.push(message.clone());
                continue;
            };
            if let Some(tool_uses) = row
                .get_mut("toolUses")
                .and_then(serde_json::Value::as_array_mut)
            {
                for tool_use in tool_uses {
                    let Some(tool_use_id) = tool_use
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned)
                    else {
                        continue;
                    };
                    let Some(tool_result) = tool_result_details.get(&tool_use_id) else {
                        continue;
                    };
                    if let Some(text) = tool_result.get("text") {
                        tool_use["text"] = text.clone();
                    }
                    if tool_result
                        .get("isError")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                    {
                        tool_use["isError"] = serde_json::Value::Bool(true);
                    }
                }
            }
            let Some(handle) = row.get("handle").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let mut original_messages = std::mem::take(&mut leading_unrepresented);
            original_messages.push(message.clone());
            self.records.insert(
                handle.to_string(),
                SessionCompactRowRecord {
                    projection: session_compact_row_projection(&row),
                    messages: original_messages,
                },
            );
            rows.push(row);
        }
        rows
    }

    fn messages_of_rows(
        &self,
        rows: &[serde_json::Value],
    ) -> Result<Vec<ConversationMessage>, hooks::mods::ModError> {
        let mut messages = Vec::new();
        let mut restored_handles = std::collections::HashSet::new();
        for row in rows {
            let handle = row.get("handle").and_then(serde_json::Value::as_str);
            if let Some(handle) = handle {
                if let Some(record) = self.records.get(handle) {
                    if record.projection == session_compact_row_projection(row)
                        && restored_handles.insert(recorded_handle_key(handle))
                    {
                        messages.extend(record.messages.iter().cloned());
                        continue;
                    }
                }
            }
            messages.push(session_compact_message_from_row(row)?);
        }
        Ok(messages)
    }
}

fn recorded_handle_key(handle: &str) -> String {
    handle.to_string()
}

fn session_compact_row_projection(row: &serde_json::Value) -> serde_json::Value {
    let mut projection = serde_json::Map::new();
    for key in ["role", "text", "toolUses", "toolResults"] {
        if let Some(value) = row.get(key) {
            projection.insert(key.to_string(), value.clone());
        }
    }
    serde_json::Value::Object(projection)
}

fn validate_session_compact_rows(rows: &[serde_json::Value]) -> Result<(), hooks::mods::ModError> {
    let invalid = || {
        hooks::mods::ModError::Hook(
            "session.compact messages must be a non-empty list of native rows".to_string(),
        )
    };
    if rows.is_empty() {
        return Err(invalid());
    }
    for row in rows {
        let Some(object) = row.as_object() else {
            return Err(invalid());
        };
        if !matches!(
            object.get("role").and_then(serde_json::Value::as_str),
            Some("user" | "assistant")
        ) || object
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_none()
            || object.get("handle").is_some_and(|value| !value.is_string())
        {
            return Err(invalid());
        }
        let Some(tool_uses) = object.get("toolUses").and_then(serde_json::Value::as_array) else {
            return Err(invalid());
        };
        if tool_uses.iter().any(|tool_use| {
            !tool_use.is_object()
                || tool_use
                    .get("tool_use_id")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
                || tool_use
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
                || tool_use
                    .get("input")
                    .and_then(serde_json::Value::as_object)
                    .is_none()
        }) {
            return Err(invalid());
        }
        if let Some(tool_results) = object.get("toolResults") {
            let Some(tool_results) = tool_results.as_array() else {
                return Err(invalid());
            };
            if tool_results.iter().any(|tool_result| {
                !tool_result.is_object()
                    || tool_result
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                        .is_none()
                    || tool_result
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .is_none()
            }) {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

fn parse_native_session_compact_number(
    result: &serde_json::Value,
    field: &str,
) -> Result<Option<f64>, hooks::mods::ModError> {
    let Some(value) = result.get(field) else {
        return Ok(None);
    };
    value
        .as_f64()
        .filter(|number| number.is_finite() && *number >= 0.0)
        .map(Some)
        .ok_or_else(|| {
            hooks::mods::ModError::Hook(format!(
                "session.compact {field} must be a non-negative number"
            ))
        })
}

fn parse_native_session_compact_usage(
    result: &serde_json::Value,
) -> Result<Option<serde_json::Value>, hooks::mods::ModError> {
    let Some(usage) = result.get("usage") else {
        return Ok(None);
    };
    let fields = [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ];
    if !usage.is_object()
        || fields.iter().any(|field| {
            usage
                .get(*field)
                .and_then(serde_json::Value::as_f64)
                .is_none_or(|number| !number.is_finite() || number < 0.0)
        })
    {
        return Err(hooks::mods::ModError::Hook(
            "session.compact usage must contain four non-negative token counts".to_string(),
        ));
    }
    Ok(Some(usage.clone()))
}

fn parse_session_compact_event_result(
    result: &serde_json::Value,
    codec: &SessionCompactRowCodec,
) -> Result<SessionCompactEventResult, hooks::mods::ModError> {
    let rows = result
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            hooks::mods::ModError::Hook(
                "session.compact result must contain messages or skip".to_string(),
            )
        })?;
    validate_session_compact_rows(rows)?;
    Ok(SessionCompactEventResult {
        messages: codec.messages_of_rows(rows)?,
        tokens_before: parse_native_session_compact_number(result, "tokensBefore")?,
        tokens_after: parse_native_session_compact_number(result, "tokensAfter")?,
        usage: parse_native_session_compact_usage(result)?,
    })
}

fn session_compact_rows_equal(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    let (Some(left), Some(right)) = (left.as_array(), right.as_array()) else {
        return false;
    };
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.get("handle") == right.get("handle")
                && session_compact_row_projection(left) == session_compact_row_projection(right)
        })
}

fn fresh_session_compact_message_id(message: ConversationMessage) -> ConversationMessage {
    let Ok(mut value) = serde_json::to_value(&message) else {
        return message;
    };
    let Some(object) = value.as_object_mut() else {
        return message;
    };
    let Ok(id) = serde_json::to_value(lingxi_core::types::MessageId::new()) else {
        return message;
    };
    object.insert("id".to_string(), id);
    serde_json::from_value(value).unwrap_or(message)
}

fn session_compact_row(message: &ConversationMessage) -> Option<serde_json::Value> {
    let id = serde_json::to_value(message.id()).ok()?;
    let handle = id.as_str()?;
    let (role, text, tool_uses, tool_results) = match message {
        ConversationMessage::Assistant { content, .. } => {
            let mut text = String::new();
            let mut tool_uses = Vec::new();
            for block in content {
                match block {
                    lingxi_core::types::ContentBlock::Text { text: part, .. }
                    | lingxi_core::types::ContentBlock::TextJsUtf16 { text: part, .. } => {
                        text.push_str(part);
                    }
                    lingxi_core::types::ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        provider_id,
                        ..
                    } => {
                        let tool_use_id = provider_id.clone().unwrap_or_else(|| {
                            serde_json::to_value(id)
                                .ok()
                                .and_then(|value| value.as_str().map(ToOwned::to_owned))
                                .unwrap_or_default()
                        });
                        tool_uses.push(serde_json::json!({
                            "tool_use_id": tool_use_id,
                            "tool": name,
                            "input": input,
                        }));
                    }
                    _ => {}
                }
            }
            ("assistant", text, tool_uses, Vec::new())
        }
        ConversationMessage::User {
            content, is_meta, ..
        } if !is_meta => {
            let mut text = String::new();
            let mut tool_results = Vec::new();
            for block in content {
                match block {
                    lingxi_core::types::ContentBlock::Text { text: part, .. }
                    | lingxi_core::types::ContentBlock::TextJsUtf16 { text: part, .. } => {
                        text.push_str(part);
                    }
                    lingxi_core::types::ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        provider_tool_use_id,
                        ..
                    } => {
                        let tool_use_id = provider_tool_use_id.clone().unwrap_or_else(|| {
                            serde_json::to_value(tool_use_id)
                                .ok()
                                .and_then(|value| value.as_str().map(ToOwned::to_owned))
                                .unwrap_or_default()
                        });
                        let mut row = serde_json::json!({
                            "tool_use_id": tool_use_id,
                            "text": content,
                        });
                        if is_error.unwrap_or(false) {
                            row["isError"] = serde_json::Value::Bool(true);
                        }
                        tool_results.push(row);
                    }
                    _ => {}
                }
            }
            ("user", text, Vec::new(), tool_results)
        }
        ConversationMessage::User { .. } | ConversationMessage::System { .. } => return None,
    };
    let mut row = serde_json::json!({
        "role": role,
        "text": text,
        "toolUses": tool_uses,
        "handle": handle,
    });
    if !tool_results.is_empty() {
        row["toolResults"] = serde_json::Value::Array(tool_results);
    }
    Some(row)
}

fn session_compact_message_from_row(
    row: &serde_json::Value,
) -> Result<ConversationMessage, hooks::mods::ModError> {
    let invalid = || {
        hooks::mods::ModError::Hook(
            "session.compact messages must be native role/text rows".to_string(),
        )
    };
    let role = row
        .get("role")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(invalid)?;
    let text = row
        .get("text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(invalid)?;
    // `hat(row)` creates a fresh native message; row.handle is only an index
    // into the original row map used to preserve an unchanged message.
    let id = lingxi_core::types::MessageId::new();
    let mut content = Vec::new();
    let text_block = (!text.is_empty()).then(|| lingxi_core::types::ContentBlock::Text {
        text: text.to_string(),
        citations: None,
    });
    match role {
        "assistant" => {
            if let Some(block) = text_block {
                content.push(block);
            }
            if let Some(tool_uses) = row.get("toolUses").and_then(serde_json::Value::as_array) {
                for tool_use in tool_uses {
                    let id_text = tool_use
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(invalid)?;
                    let name = tool_use
                        .get("tool")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(invalid)?;
                    let input = tool_use.get("input").cloned().ok_or_else(invalid)?;
                    content.push(lingxi_core::types::ContentBlock::ToolUse {
                        input_projection: None,
                        id: lingxi_core::types::ToolUseId::from(id_text),
                        name: name.to_string(),
                        input,
                        provider_id: Some(id_text.to_string()),
                    });
                }
            }
            Ok(ConversationMessage::Assistant { per_turn_effort: None,
                id,
                content,
                stop_reason: None,
            })
        }
        "user" => {
            if let Some(tool_results) = row.get("toolResults").and_then(serde_json::Value::as_array)
            {
                for tool_result in tool_results {
                    let id_text = tool_result
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(invalid)?;
                    let text = tool_result
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(invalid)?;
                    let is_error = tool_result
                        .get("isError")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    content.push(lingxi_core::types::ContentBlock::ToolResult {
                        content_projection: None,
                        tool_use_id: lingxi_core::types::ToolUseId::from(id_text),
                        content: text.to_string(),
                        is_error: Some(is_error),
                        provider_tool_use_id: Some(id_text.to_string()),
                        content_blocks: None,
                    });
                }
            }
            if let Some(block) = text_block {
                content.push(block);
            }
            Ok(ConversationMessage::User { api_message_override: None,
                id,
                content,
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
            })
        }
        _ => Err(invalid()),
    }
}

pub(crate) struct SessionCompactEventResult {
    pub(crate) messages: Vec<ConversationMessage>,
    pub(crate) tokens_before: Option<f64>,
    pub(crate) tokens_after: Option<f64>,
    pub(crate) usage: Option<serde_json::Value>,
}

pub(crate) struct SessionCompactCoreOutput {
    pub(crate) result: compaction::IterationCompactionResult,
    pub(crate) source_messages: Vec<ConversationMessage>,
    pub(crate) event_result: SessionCompactEventResult,
    pub(crate) api_duration: std::time::Duration,
}

fn apply_session_compact_result(
    result: &mut compaction::IterationCompactionResult,
    event_result: &SessionCompactEventResult,
    source_messages: &[ConversationMessage],
) {
    let preserved = result.messages_to_preserve.clone();
    let split = (!preserved.is_empty()
        && event_result.messages.len() >= preserved.len()
        && event_result.messages[event_result.messages.len() - preserved.len()..] == preserved)
        .then(|| event_result.messages.len() - preserved.len());
    if let Some(split) = split {
        result.messages = event_result.messages[..split].to_vec();
        result.messages_to_preserve = preserved;
    } else {
        result.messages = event_result.messages.clone();
        result.messages_to_preserve.clear();
    }
    // Native `wat` gives every replacement row a fresh UUID. Preserve the
    // summary/tail split used by the host's boundary metadata, but refresh IDs
    // on both sides only when Mods actually replaced the core rows.
    result.messages = result
        .messages
        .drain(..)
        .map(fresh_session_compact_message_id)
        .collect();
    result.messages_to_preserve = result
        .messages_to_preserve
        .drain(..)
        .map(fresh_session_compact_message_id)
        .collect();
    result.raw_summary_text = result
        .messages
        .iter()
        .map(ConversationMessage::text_content)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    // `tokensBefore` / `tokensAfter` are part of the Mod event envelope, not
    // the host's freed-token accounting. Match the native replacement path,
    // which estimates from its source and replacement messages.
    let tokens_before = compaction::grouping::estimate_tokens_for_range(source_messages);
    let tokens_after = compaction::grouping::estimate_tokens_for_range(&event_result.messages);
    result.total_tokens_freed = tokens_before.saturating_sub(tokens_after);
}

impl SessionCompactCoreOutput {
    pub(crate) fn new(
        result: compaction::IterationCompactionResult,
        source_messages: Vec<ConversationMessage>,
        api_duration: std::time::Duration,
    ) -> Self {
        let mut messages = result.messages.clone();
        messages.extend(result.messages_to_preserve.iter().cloned());
        let tokens_before =
            Some(compaction::grouping::estimate_tokens_for_range(&source_messages) as f64);
        let tokens_after = Some(compaction::grouping::estimate_tokens_for_range(&messages) as f64);
        let usage = result.compaction_usage.map(|usage| {
            serde_json::json!({
                "input_tokens": usage.tokens.input,
                "output_tokens": usage.tokens.output,
                "cache_read_input_tokens": usage.tokens.cache_read,
                "cache_creation_input_tokens": usage.tokens.cache_write
                    .saturating_add(usage.tokens.cache_write_1h),
            })
        });
        Self {
            result,
            source_messages,
            event_result: SessionCompactEventResult {
                messages,
                tokens_before,
                tokens_after,
                usage,
            },
            api_duration,
        }
    }

    fn event_value(&self) -> Result<serde_json::Value, hooks::mods::ModError> {
        let messages =
            SessionCompactRowCodec::default().rows_for_messages(&self.event_result.messages);
        let mut value = serde_json::json!({
            "messages": messages,
        });
        if let Some(tokens_before) = self.event_result.tokens_before {
            value["tokensBefore"] = serde_json::json!(tokens_before);
        }
        if let Some(tokens_after) = self.event_result.tokens_after {
            value["tokensAfter"] = serde_json::json!(tokens_after);
        }
        if let Some(usage) = &self.event_result.usage {
            value["usage"] = usage.clone();
        }
        Ok(value)
    }
}

pub(crate) type SessionCompactCore = std::sync::Arc<
    dyn Fn(
            Vec<ConversationMessage>,
            Option<String>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<SessionCompactCoreOutput, hooks::mods::ModError>,
                    > + Send
                    + 'static,
            >,
        > + Send
        + Sync,
>;

/// Outcome of running the actual compaction callback through session.compact.
pub(crate) enum SessionCompactDecision {
    Continue {
        result: compaction::IterationCompactionResult,
        source_messages: Vec<ConversationMessage>,
        api_duration: std::time::Duration,
        core_ran: bool,
    },
    Skip {
        reason: String,
    },
}

impl ConversationOrchestrator {
    /// Run the route-specific compactor inside the Mods middleware's core
    /// continuation. The hook sees the real compact result from next(e), and
    /// can rewrite the source messages/instructions before that callback runs.
    pub(crate) async fn dispatch_session_compact_mods(
        &self,
        trigger: &str,
        messages: Vec<ConversationMessage>,
        instructions: Option<&str>,
        compact: SessionCompactCore,
    ) -> Result<SessionCompactDecision, hooks::mods::ModError> {
        let original_instructions = instructions.map(ToString::to_string);
        let Some(registry) = self.lifecycle_runtime.hook_registry.as_ref() else {
            let output = compact(messages, original_instructions).await?;
            return Ok(SessionCompactDecision::Continue {
                result: output.result,
                source_messages: output.source_messages,
                api_duration: output.api_duration,
                core_ran: true,
            });
        };
        let Some(host) = registry.read().await.mod_host() else {
            let output = compact(messages, original_instructions).await?;
            return Ok(SessionCompactDecision::Continue {
                result: output.result,
                source_messages: output.source_messages,
                api_duration: output.api_duration,
                core_ran: true,
            });
        };
        if !host.has_event("session.compact") {
            let output = compact(messages, original_instructions).await?;
            return Ok(SessionCompactDecision::Continue {
                result: output.result,
                source_messages: output.source_messages,
                api_duration: output.api_duration,
                core_ran: true,
            });
        }

        let original_source_messages = messages.clone();
        let row_codec =
            std::sync::Arc::new(std::sync::Mutex::new(SessionCompactRowCodec::default()));
        let messages_rows = row_codec.lock().unwrap().rows_for_messages(&messages);
        validate_session_compact_rows(&messages_rows)?;
        let mut input = serde_json::json!({
            "trigger": trigger,
            "messages": messages_rows,
        });
        if let Some(instructions) = instructions {
            input["instructions"] = serde_json::Value::String(instructions.to_string());
        }
        let original_trigger = trigger.to_string();
        let original_agent_id = input.get("agentId").cloned();
        let core_result: std::sync::Arc<
            tokio::sync::Mutex<Option<(SessionCompactCoreOutput, serde_json::Value)>>,
        > = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let core_result_for_dispatch = core_result.clone();
        let compact_for_dispatch = compact.clone();
        let row_codec_for_dispatch = row_codec.clone();
        let log_output = self.output.clone();
        let toast_output = self.output.clone();
        let status_output = self.output.clone();
        let result = host
            .dispatch_with_ui_at_session(
                "session.compact",
                input,
                self,
                move |event| {
                    let core_result = core_result_for_dispatch.clone();
                    let compact = compact_for_dispatch.clone();
                    let row_codec = row_codec_for_dispatch.clone();
                    let expected_trigger = original_trigger.clone();
                    let expected_agent_id = original_agent_id.clone();
                    async move {
                        if event.get("trigger").and_then(serde_json::Value::as_str)
                            != Some(expected_trigger.as_str())
                            || event.get("agentId").cloned() != expected_agent_id
                        {
                            return Err(hooks::mods::ModError::Hook(
                                "session.compact trigger and agentId are pinned".into(),
                            ));
                        }
                        let instructions = match event.get("instructions") {
                            None => None,
                            Some(serde_json::Value::String(text)) => Some(text.clone()),
                            _ => {
                                return Err(hooks::mods::ModError::Hook(
                                    "session.compact instructions must be a string".into(),
                                ));
                            }
                        };
                        let messages_value = event
                            .get("messages")
                            .and_then(serde_json::Value::as_array)
                            .cloned()
                            .ok_or_else(|| {
                                hooks::mods::ModError::Hook(
                                    "session.compact messages must be a native row list".into(),
                                )
                            })?;
                        validate_session_compact_rows(&messages_value)?;
                        let messages = row_codec
                            .lock()
                            .unwrap()
                            .messages_of_rows(&messages_value)?;
                        let instructions = instructions.filter(|text| !text.trim().is_empty());
                        let output = compact(messages, instructions).await?;
                        let event_result = output.event_value()?;
                        row_codec
                            .lock()
                            .unwrap()
                            .rows_for_messages(&output.event_result.messages);
                        *core_result.lock().await = Some((output, event_result.clone()));
                        Ok(event_result)
                    }
                },
                move |plugin, text| {
                    let output = log_output.clone();
                    async move { output.emit_mod_log(&plugin, &text).await }
                },
                move |plugin, text, timeout_ms| {
                    let output = toast_output.clone();
                    async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
                },
                move |plugin, text| {
                    let output = status_output.clone();
                    async move { output.emit_mod_status(&plugin, text.as_deref()).await }
                },
            )
            .await?;

        if let Some(reason) = result.get("skip").and_then(serde_json::Value::as_str) {
            let ran_core = core_result.lock().await.take().is_some();
            if reason.is_empty() || result.get("messages").is_some() {
                return Err(hooks::mods::ModError::Hook(
                    "session.compact skip must be non-empty and cannot include messages".into(),
                ));
            }
            if ran_core && trigger != "precompute" {
                return Err(hooks::mods::ModError::Hook(
                    "session.compact cannot skip after its compact callback ran".into(),
                ));
            }
            return Ok(SessionCompactDecision::Skip {
                reason: reason.to_string(),
            });
        }

        let compact_output = core_result.lock().await.take();
        let source_messages_for_estimates = compact_output
            .as_ref()
            .map(|(output, _)| output.source_messages.clone())
            .unwrap_or_else(|| original_source_messages.clone());
        let event_result = parse_session_compact_event_result(&result, &row_codec.lock().unwrap())?;
        match compact_output {
            Some((mut output, last_core_value)) => {
                let uses_last_core_result = session_compact_rows_equal(
                    result.get("messages").unwrap_or(&serde_json::Value::Null),
                    last_core_value
                        .get("messages")
                        .unwrap_or(&serde_json::Value::Null),
                );
                if !uses_last_core_result {
                    apply_session_compact_result(
                        &mut output.result,
                        &event_result,
                        &source_messages_for_estimates,
                    );
                }
                Ok(SessionCompactDecision::Continue {
                    result: output.result,
                    source_messages: output.source_messages,
                    api_duration: output.api_duration,
                    core_ran: true,
                })
            }
            None => {
                // Native Mods may return a complete replacement without ever
                // calling next(e). Such an outcome has no summarizer record,
                // but its messages still flow through the normal replacement
                // boundary path.
                let mut replacement = compaction::IterationCompactionResult {
                    messages: Vec::new(),
                    raw_summary_text: String::new(),
                    layers_applied: Vec::new(),
                    total_tokens_freed: 0,
                    cache_hit: false,
                    consecutive_failures: 0,
                    was_compacted: true,
                    rapid_refill_breaker_tripped: false,
                    consecutive_rapid_refills: 0,
                    messages_to_preserve: Vec::new(),
                    media_analysis_to_preserve: Vec::new(),
                    compaction_usage: None,
                    compaction_model: None,
                    compaction_profile: None,
                };
                apply_session_compact_result(
                    &mut replacement,
                    &event_result,
                    &source_messages_for_estimates,
                );
                Ok(SessionCompactDecision::Continue {
                    result: replacement,
                    source_messages: original_source_messages,
                    api_duration: std::time::Duration::ZERO,
                    core_ran: false,
                })
            }
        }
    }

    /// Clone the current session request state and run the shared pre-call
    /// preparation hook, if one is wired.
    pub(crate) async fn prepare_model_call_snapshot(
        &self,
        path: ModelCallPath,
        system_prompt: Option<&str>,
        cancel: Option<&CancellationToken>,
    ) -> Result<PreparedModelCall, OrchestratorError> {
        let mut draft = {
            let s = self.session.lock().await;
            PreparedModelCall {
                history_snapshot: s.model_context_history(),
                model: s.model.clone(),
                model_profile: s.model_profile.clone(),
                outgoing_history_rewriter: None,
            }
        };
        if let Some(settings) = crate::scheduled_turn::current() {
            draft.model = settings.model;
            draft.model_profile = Some(settings.provider);
        }
        if let Some(route) = crate::query_model::current() {
            draft.model = route.model;
            draft.model_profile = route.profile;
        }
        let prepared = match self.model_runtime.model_call_preparer.as_ref() {
            Some(preparer) => {
                preparer
                    .prepare(self, path, system_prompt, cancel, draft)
                    .await
            }
            None => Ok(draft),
        }?;
        Ok(self.apply_context_collapse_projection(prepared))
    }

    pub(super) fn apply_context_collapse_projection(
        &self,
        mut prepared: PreparedModelCall,
    ) -> PreparedModelCall {
        let Some(compactor) = self.compaction_runtime.compaction.clone() else {
            return prepared;
        };
        if !compaction::is_context_collapse_enabled() {
            return prepared;
        }

        prepared.history_snapshot = compactor
            .context_collapse
            .apply_collapses_if_needed(prepared.history_snapshot)
            .messages;
        prepared.outgoing_history_rewriter = Some(Arc::new(ContextCollapseHistoryRewriter {
            inner: prepared.outgoing_history_rewriter.take(),
            compactor,
        }));
        prepared
    }

    /// Apply a retry-safe outgoing-history rewrite when a recovery path rebuilds
    /// the request from raw `session.history`.
    pub(crate) async fn rewrite_outgoing_history(
        &self,
        raw_history: Vec<ConversationMessage>,
        rewriter: Option<&Arc<dyn OutgoingHistoryRewriter>>,
    ) -> Result<Vec<ConversationMessage>, OrchestratorError> {
        let mut rewritten = match rewriter {
            Some(rewriter) => rewriter.rewrite(self, raw_history).await,
            None => Ok(raw_history),
        }?;
        let excluded = self
            .session
            .lock()
            .await
            .model_context_excluded_messages
            .clone();
        rewritten.retain(|message| !excluded.contains(&message.id()));
        Ok(rewritten)
    }

    /// Persist the append-only commits followed by the last-wins staged-state
    /// snapshot produced by a collapse drain. Failures are best-effort, matching
    /// transcript side-record writes: the in-memory projection remains usable
    /// for the current retry even if durable persistence is unavailable.
    pub(crate) async fn persist_context_collapse_drain(&self, drain: &compaction::DrainResult) {
        if !drain.commits.is_empty() {
            self.expect_prompt_cache_rebuild().await;
        }
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return;
        };
        let session_id = self.session.lock().await.session_id.as_uuid().to_string();
        for commit in &drain.commits {
            if let Err(error) = writer
                .append_context_collapse_commit(
                    &session_id,
                    &commit.collapse_id,
                    &commit.summary_uuid,
                    &commit.summary_content,
                    &commit.summary,
                    &commit.first_archived_uuid,
                    &commit.last_archived_uuid,
                )
                .await
            {
                tracing::warn!(%error, "failed to persist context-collapse commit");
            }
        }
        let staged = serde_json::to_value(&drain.snapshot.staged)
            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
        if let Err(error) = writer
            .append_context_collapse_snapshot(
                &session_id,
                &staged,
                drain.snapshot.armed,
                drain.snapshot.last_spawn_tokens,
            )
            .await
        {
            tracing::warn!(%error, "failed to persist context-collapse snapshot");
        }
    }

    /// Restore context-collapse side records from a tolerant transcript load.
    /// Strict no-op when no compactor is wired; callers may invoke this
    /// unconditionally during resume so an empty record set clears stale state.
    pub fn restore_context_collapse_from_json(
        &self,
        commits: &[serde_json::Value],
        snapshot: Option<&serde_json::Value>,
    ) -> Result<(), serde_json::Error> {
        let Some(compactor) = self.compaction_runtime.compaction.as_ref() else {
            return Ok(());
        };
        compactor
            .context_collapse
            .restore_from_json_entries(commits, snapshot)
    }

    async fn reset_context_collapse_after_compact(&self) {
        self.transcript
            .thinking_recovery
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .reset_beta_rejections();
        if !compaction::is_context_collapse_enabled() {
            return;
        }
        let Some(compactor) = self.compaction_runtime.compaction.as_ref() else {
            return;
        };
        let reset = compactor.context_collapse.reset("compact");
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return;
        };
        let session_id = self.session.lock().await.session_id.as_uuid().to_string();
        if let Err(error) = writer
            .append_context_collapse_reset(&session_id, &reset.reason)
            .await
        {
            tracing::warn!(%error, "failed to persist context-collapse reset");
        }
    }

    /// P1 (§6.5): when a session-memory handle is wired AND the extractor's
    /// tool-call threshold is crossed, BACKGROUND-fork a distillation of the
    /// session history and write `<configHome>/agents/session-memory/<id>.md`
    /// (which the next session re-loads via the Session-tier memdir scan). A
    /// strict no-op when no handle is wired, no cache-safe params are available
    /// yet, or the threshold is not crossed — so the locked fixtures stay
    /// byte-identical by default. Never blocks the turn (the fork runs on the
    /// handle's runtime); a failed extraction is swallowed, never surfaced.
    pub(crate) async fn maybe_extract_session_memory(&self) {
        let safety_observer = self.model_safety_observer().await;
        let Some(handle) = self.compaction_runtime.session_memory.clone() else {
            return;
        };
        if handle
            .in_flight
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        // Own the reservation immediately. If any await below is cancelled,
        // this guard drops and releases the flag; after spawn succeeds the
        // spawned future owns the same guard until extraction completes.
        let in_flight_reset = SessionMemoryInFlightReset(handle.clone());
        // The fork shares the parent's cache-safe prefix; without it (early in a
        // session) skip — a later turn re-checks.
        let Some(slot) = self.model_runtime.cache_safe_slot.as_ref() else {
            handle
                .in_flight
                .store(false, std::sync::atomic::Ordering::Release);
            return;
        };
        let Some(mut params) = slot.get_last().await else {
            handle
                .in_flight
                .store(false, std::sync::atomic::Ordering::Release);
            return;
        };
        // Snapshot history + id without holding the session lock across the fork.
        let (history, session_id) = {
            let s = self.session.lock().await;
            (s.model_context_history(), s.session_id)
        };
        // Keep the same context-size baseline Claude records after a
        // successful extraction. The token count is computed from the
        // immutable snapshot so later turns cannot move the watermark for an
        // in-flight fork.
        let extraction_token_count = memory::session_memory::token_count_with_estimation(&history);
        extend_session_memory_fork_context(&mut params.fork_context_messages, &history);
        let runtime = handle.runtime.clone();
        let generation = handle.generation.load(std::sync::atomic::Ordering::Acquire);
        let task_handle = handle.clone();
        let spawn_result = runtime
            .spawn(
                "session-memory-extract",
                Box::pin(lingxi_core::host::model_safety::bind_model_safety(
                    safety_observer,
                    async move {
                        let _in_flight = in_flight_reset;
                        let covered_through = params
                            .fork_context_messages
                            .last()
                            .map(ConversationMessage::id);
                        let revision = {
                            let mut ex = task_handle.extractor.lock().await;
                            if task_handle
                                .generation
                                .load(std::sync::atomic::Ordering::Acquire)
                                != generation
                                || !ex.should_extract(&history)
                            {
                                return;
                            }
                            ex.state_revision()
                        };

                        let Ok(content) =
                            memory::session_memory::SessionMemoryExtractor::run_extraction(
                                &task_handle.runner,
                                params,
                            )
                            .await
                        else {
                            return;
                        };

                        let mut ex = task_handle.extractor.lock().await;
                        if task_handle
                            .generation
                            .load(std::sync::atomic::Ordering::Acquire)
                            != generation
                            || ex.state_revision() != revision
                        {
                            return;
                        }
                        if ex
                            .commit_extraction(
                                &content,
                                &session_id.to_string(),
                                covered_through,
                                &task_handle.config_home,
                            )
                            .is_ok()
                        {
                            ex.record_extraction_token_count(extraction_token_count);
                        }
                    },
                )),
            )
            .await;
        if spawn_result.is_err() {
            handle
                .in_flight
                .store(false, std::sync::atomic::Ordering::Release);
        }
    }

    /// Real `force_compact` body — cancelable via a [`CancellationToken`].
    ///
    /// Behavior:
    /// 1. Snapshot `session.history` (clone — we don't hold the lock
    ///    across `process_iteration`).
    /// 2. Race `compactor.process_iteration(snapshot, 0)` against
    ///    `cancel.cancelled()`. On cancel, drop the future and return
    ///    `HandleError::ActionFailed("compaction cancelled")` —
    ///    history is left untouched.
    /// 3. On success: append a `[Compacted N → M messages]` System
    ///    marker, swap history under the same lock, emit
    ///    `OutputEvent::CompactionCompleted`, return a
    ///    `CompactionSummary` with real numbers.
    ///
    /// When no compactor is wired (`compaction == None`), returns an explicit
    /// error. Manual compaction must never report success without a real model
    /// summary and history transition.
    ///
    /// (M6-08)
    pub async fn force_compact_with_cancel(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<lingxi_core::host::CompactionSummary, lingxi_core::host::HandleError> {
        self.force_compact_with_instructions_and_cancel(None, cancel)
            .await
    }

    /// `/rewind` → "Summarize from here" / "Summarize up to here" — oracle
    /// `zir` (`src_169588164.js`).
    ///
    /// Summarizes ONE side of the conversation at `message_uuid` and keeps the
    /// other verbatim. Unlike `/rewind`'s restore scopes this does NOT unwind
    /// the app loop: it is a live summarizer call over the CURRENT conversation
    /// followed by a history swap, exactly like `/compact`.
    ///
    /// `user_context` is the free text typed into the picker row. Upstream
    /// wraps it as `User context: <text>` and merges it with any PreCompact
    /// hook stdout before it reaches the prompt — it is the user telling the
    /// summarizer what to keep, not a second system instruction.
    ///
    /// # Errors
    /// The byte-exact `Nothing to summarize …` sentence when the chosen point
    /// leaves nothing on the summarize side, plus every failure
    /// [`Self::force_compact_with_instructions_and_cancel`] can produce.
    pub async fn summarize_at(
        &self,
        message_uuid: &str,
        user_context: Option<&str>,
        direction: compaction::prompt::SummarizeDirection,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<lingxi_core::host::CompactionSummary, lingxi_core::host::HandleError> {
        let Some(compactor) = self.compaction_runtime.compaction.clone() else {
            return Err(lingxi_core::host::HandleError::ActionFailed(
                "compaction unavailable".into(),
            ));
        };

        let _turn_guard = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(lingxi_core::host::HandleError::ActionFailed(
                    "Compaction canceled.".into(),
                ));
            }
            guard = self.turn_gate.lock() => guard,
        };

        let (history_before, model) = {
            let session = self.session.lock().await;
            (session.model_context_history(), session.model.clone())
        };
        if history_before.is_empty() {
            return Err(lingxi_core::host::HandleError::ActionFailed(
                "No messages to compact".into(),
            ));
        }
        let Some(index) = compaction::selector::index_of(&history_before, message_uuid) else {
            return Err(lingxi_core::host::HandleError::ActionFailed(
                "Message not found.".into(),
            ));
        };
        compaction::selector::split_at(&history_before, index, direction)
            .map_err(|message| lingxi_core::host::HandleError::ActionFailed(message.to_string()))?;

        let compact_started = std::time::Instant::now();
        self.output.emit_compaction_started().await;

        let outcome = async {
            let pre_compact = self.fire_pre_compact("manual", user_context).await;
            if let Some(detail) = pre_compact.blocked_by {
                let msg = if detail.is_empty() {
                    "Compaction blocked by PreCompact hook".to_string()
                } else {
                    format!("Compaction blocked by PreCompact hook: {detail}")
                };
                tracing::warn!("{msg}");
                return Err(lingxi_core::host::HandleError::ActionFailed(msg));
            }

            // `xe = lRe(we.newCustomInstructions, m ? `User context: ${m}` : undefined)`.
            // The label matters: without it the summarizer reads the user's note as
            // another instruction from the system rather than as the user saying
            // what they care about.
            let labelled_context = user_context
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| format!("User context: {text}"));
            let merged_instructions = merge_compact_instructions(
                pre_compact.additional_instructions.as_deref(),
                labelled_context.as_deref(),
            );

            let system_prompt = self.effective_system_prompt().await;
            let tools = self.build_wire_tools().await.0;
            self.save_cache_safe_params(Some(&system_prompt), &model, &tools)
                .await;

            let cost_scope = self.compaction_cost_scope().await.map_err(|error| {
                lingxi_core::host::HandleError::ActionFailed(format!(
                    "Compaction cost preflight failed: {error}"
                ))
            })?;
            let safety_observer = cost_scope
                .as_ref()
                .map(cost::CostSessionScope::model_safety_observer);
            let phase_output = self.output.clone();
            let compact_for_core = compactor.clone();
            let cancel_for_core = cancel.clone();
            let message_uuid_for_core = message_uuid.to_string();
            let direction_for_core = direction;
            let core: SessionCompactCore =
                std::sync::Arc::new(move |source_messages, instructions| {
                    let compactor = compact_for_core.clone();
                    let cancel = cancel_for_core.clone();
                    let message_uuid = message_uuid_for_core.clone();
                    let phase_output = phase_output.clone();
                    let direction = direction_for_core;
                    let safety_observer = safety_observer.clone();
                    Box::pin(lingxi_core::host::model_safety::bind_model_safety(
                        safety_observer,
                        async move {
                            let index =
                                compaction::selector::index_of(&source_messages, &message_uuid)
                                    .ok_or_else(|| {
                                        hooks::mods::ModError::Native("Message not found.".into())
                                    })?;
                            let split =
                                compaction::selector::split_at(&source_messages, index, direction)
                                    .map_err(|message| {
                                        hooks::mods::ModError::Native(message.to_string())
                                    })?;
                            let context = split.summarizer_context(&source_messages);
                            phase_output.emit_compaction_phase("summarizing").await;
                            let api_started = std::time::Instant::now();
                            let result = tokio::select! {
                                biased;
                                () = cancel.cancelled() => {
                                    return Err(hooks::mods::ModError::Native(
                                        "Compaction canceled.".into(),
                                    ));
                                }
                                result = compactor.summarize_selection(
                                    context,
                                    &split.to_summarize,
                                    instructions.as_deref(),
                                    direction,
                                ) => result.map_err(|error| {
                                    hooks::mods::ModError::Native(
                                        Self::summarize_error(error).to_string(),
                                    )
                                })?,
                            };
                            let api_duration = api_started.elapsed();
                            let mut result = result;
                            result.messages_to_preserve =
                                compaction::partial::zero_preserved_tail_usage(split.to_keep);
                            Ok(SessionCompactCoreOutput::new(
                                result,
                                source_messages,
                                api_duration,
                            ))
                        },
                    ))
                });
            let compacted = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Err(lingxi_core::host::HandleError::ActionFailed(
                        "Compaction canceled.".into(),
                    ));
                }
                result = self.dispatch_session_compact_mods(
                    "manual",
                    history_before,
                    merged_instructions.as_deref(),
                    core,
                ) => result.map_err(|error| {
                    lingxi_core::host::HandleError::ActionFailed(error.to_string())
                })?,
            };
            let (result, history_before, compact_duration) = match compacted {
                SessionCompactDecision::Continue {
                    result,
                    source_messages,
                    api_duration,
                    ..
                } => (result, source_messages, api_duration),
                SessionCompactDecision::Skip { reason, .. } => {
                    return Err(lingxi_core::host::HandleError::ActionFailed(reason));
                }
            };
            let split_index = compaction::selector::index_of(&history_before, message_uuid)
                .ok_or_else(|| {
                    lingxi_core::host::HandleError::ActionFailed("Message not found.".into())
                })?;
            let split = compaction::selector::split_at(&history_before, split_index, direction)
                .map_err(|message| {
                    lingxi_core::host::HandleError::ActionFailed(message.to_string())
                })?;
            let messages_before = u32::try_from(history_before.len()).unwrap_or(u32::MAX);
            let bytes_before: u64 = history_before
                .iter()
                .map(lingxi_core::types::text_byte_size)
                .sum();
            let pre_tokens_estimate =
                compaction::grouping::estimate_tokens_for_range(&split.to_summarize);
            let messages_summarized = u32::try_from(split.to_summarize.len()).unwrap_or(u32::MAX);
            let cost_receipt =
                self.begin_compaction_usage(cost_scope.as_ref(), &result, compact_duration);
            if cancel.is_cancelled() {
                return Err(lingxi_core::host::HandleError::ActionFailed(
                    "Compaction canceled.".into(),
                ));
            }
            self.settle_compaction_usage(cost_receipt)
                .await
                .map_err(|error| {
                    lingxi_core::host::HandleError::ActionFailed(format!(
                        "Compaction cost settlement failed: {error}"
                    ))
                })?;

            // The kept half rides through `apply_post_compact`'s preserved-tail
            // slot; `placement` decides which side of the summary it lands on.
            let placement = match direction {
                compaction::prompt::SummarizeDirection::UpTo => SummaryPlacement::BeforeKept,
                compaction::prompt::SummarizeDirection::From => SummaryPlacement::AfterKept,
            };

            let Some(summary_out) = self
                .apply_post_compact_placed(
                    result,
                    compaction::CompactTrigger::Manual,
                    pre_tokens_estimate,
                    messages_before,
                    bytes_before,
                    compact_started,
                    Some(&cancel),
                    placement,
                    Some(SelectorBoundaryMetadata {
                        messages_summarized,
                        user_context: user_context
                            .map(str::trim)
                            .filter(|text| !text.is_empty())
                            .map(ToString::to_string),
                    }),
                )
                .await
            else {
                return Err(lingxi_core::host::HandleError::ActionFailed(
                    "Compaction canceled.".into(),
                ));
            };

            Ok(summary_out)
        }
        .await;
        if let Err(error) = &outcome {
            let detail = match error {
                lingxi_core::host::HandleError::ActionFailed(detail) => detail.as_str(),
                lingxi_core::host::HandleError::Unimplemented(_) => "compaction failed",
            };
            self.output.emit_compaction_finished(Some(detail)).await;
        }
        outcome
    }

    /// Map a compactor error onto the user-visible sentence, shared by the
    /// summarize path with `/compact`'s mapping.
    fn summarize_error(
        error: compaction::autocompact::CompactionError,
    ) -> lingxi_core::host::HandleError {
        use compaction::autocompact::CompactionError;
        lingxi_core::host::HandleError::ActionFailed(match error {
            CompactionError::MaxRetriesExceeded => {
                "Compaction failed \u{b7} conversation could not be reduced below the context limit"
                    .to_string()
            }
            CompactionError::NotEnoughMessages => "Not enough messages to compact.".to_string(),
            CompactionError::MediaUnstrippable => {
                "Compaction failed \u{b7} attached media exceeds size limits".to_string()
            }
            CompactionError::Summary(detail) | CompactionError::Internal(detail) => {
                format!("Error during compaction: {detail}")
            }
            other => format!("Error during compaction: {other}"),
        })
    }

    /// Manual `/compact` with optional focus text and cooperative cancellation.
    pub async fn force_compact_with_instructions_and_cancel(
        &self,
        custom_instructions: Option<&str>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<lingxi_core::host::CompactionSummary, lingxi_core::host::HandleError> {
        let Some(compactor) = self.compaction_runtime.compaction.clone() else {
            return Err(lingxi_core::host::HandleError::ActionFailed(
                "compaction unavailable".into(),
            ));
        };

        // Manual compaction runs outside the model turn, but its history
        // snapshot and boundary/summary commit must share chain ownership with
        // background Fusion publication and session switches. Cancellation may
        // abandon the queue, never a partially persisted commit. Automatic
        // compaction already runs under its caller's turn gate.
        let _turn_guard = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(lingxi_core::host::HandleError::ActionFailed(
                    "Compaction canceled.".into(),
                ));
            }
            guard = self.turn_gate.lock() => guard,
        };

        // Snapshot history (clone — we don't hold the lock across the
        // network call inside `process_iteration`).
        let (history_before, model) = {
            let s = self.session.lock().await;
            (s.model_context_history(), s.model.clone())
        };
        // Capture the token estimate before `history_before` is consumed by
        // `process_iteration` — used for the boundary `preTokens`.

        // Fast-path: if already cancelled, exit without invoking the
        // compactor. tokio::select! random-polls between ready arms,
        // so this explicit check keeps the cancel-first contract
        // deterministic even when process_iteration completes synchronously
        // (e.g. the M3 stub Autocompactor path).
        if cancel.is_cancelled() {
            return Err(lingxi_core::host::HandleError::ActionFailed(
                "Compaction canceled.".into(),
            ));
        }

        // Binary-verified guard ordering: only an EMPTY history short-circuits
        // before hooks (`if(n.length===0) throw Error("No messages to
        // compact")`, thrown by /compact's caller before `Juy`). A short-but-
        // non-empty conversation still fires PreCompact hooks and flashes the
        // compacting status — it fails LATER inside the group compactor
        // (`Nto`'s `too_few_groups` → `Not enough messages to compact.`),
        // which the port surfaces via `process_forced`'s
        // `CompactionError::NotEnoughMessages` mapping below.
        if history_before.is_empty() {
            return Err(lingxi_core::host::HandleError::ActionFailed(
                "No messages to compact".into(),
            ));
        }

        // Claude starts manual-compaction timing before PreCompact hooks and
        // freezes durationMs after attachment/SessionStart restoration.
        let compact_started = std::time::Instant::now();

        // Once a real pass begins, emit status before PreCompact hooks so slow
        // hooks are visible too (`Juy` emits `sdk_status: compacting` first).
        self.output.emit_compaction_started().await;

        let outcome = async {
            // hooks compaction lifecycle: PreCompact fires before the summary pass.
            // This is the explicit `/compact` entry point, so the trigger is
            // `manual` (TS `isAutoCompact ? 'auto' : 'manual'`). TS `VJn`: a
            // blocking PreCompact hook ABORTS the compaction, throwing
            // `"Compaction blocked by PreCompact hook: <blockedBy>"`. We surface the
            // same message as the `/compact` failure result.
            let pre_compact = self.fire_pre_compact("manual", custom_instructions).await;
            if let Some(detail) = pre_compact.blocked_by {
                let msg = if detail.is_empty() {
                    "Compaction blocked by PreCompact hook".to_string()
                } else {
                    format!("Compaction blocked by PreCompact hook: {detail}")
                };
                tracing::warn!("{msg}");
                return Err(lingxi_core::host::HandleError::ActionFailed(msg));
            }

            let merged_instructions = merge_compact_instructions(
                custom_instructions,
                pre_compact.additional_instructions.as_deref(),
            );

            // A resumed session may not have made a live API call yet, leaving the
            // shared cache-safe slot empty. Seed it from the current system prompt
            // and live history so manual compaction works immediately after resume.
            // Autocompactor replaces the slot's potentially stale message clone with
            // `history_before`'s selected prefix before issuing the request.
            let system_prompt = self.effective_system_prompt().await;
            let tools = self.build_wire_tools().await.0;
            self.save_cache_safe_params(Some(&system_prompt), &model, &tools)
                .await;

            // Run the 5-layer compactor, racing against the cancel token.
            // process_iteration takes no CancellationToken; drop-on-cancel
            // leaves history untouched because we have not written back.
            // `biased` so the cancel arm wins a tie — preferred when both
            // arms are immediately ready.
            // The API-duration clock starts HERE (summarizer round-trip only) —
            // separate from `compact_started` (pre-hooks), which feeds the
            // boundary's durationMs.
            let cost_scope = self.compaction_cost_scope().await.map_err(|error| {
                lingxi_core::host::HandleError::ActionFailed(format!(
                    "Compaction cost preflight failed: {error}"
                ))
            })?;
            let safety_observer = cost_scope
                .as_ref()
                .map(cost::CostSessionScope::model_safety_observer);
            let phase_output = self.output.clone();
            let compact_for_core = compactor.clone();
            let cancel_for_core = cancel.clone();
            let core: SessionCompactCore =
                std::sync::Arc::new(move |source_messages, instructions| {
                    let compactor = compact_for_core.clone();
                    let cancel = cancel_for_core.clone();
                    let phase_output = phase_output.clone();
                    let safety_observer = safety_observer.clone();
                    Box::pin(lingxi_core::host::model_safety::bind_model_safety(
                        safety_observer,
                        async move {
                            phase_output.emit_compaction_phase("summarizing").await;
                            let api_started = std::time::Instant::now();
                            let result = tokio::select! {
                                biased;
                                () = cancel.cancelled() => {
                                    return Err(hooks::mods::ModError::Native(
                                        "Compaction canceled.".into(),
                                    ));
                                }
                                result = compactor.process_forced(
                                    source_messages.clone(),
                                    instructions.as_deref(),
                                ) => result.map_err(|error| {
                                    hooks::mods::ModError::Native(
                                        Self::summarize_error(error).to_string(),
                                    )
                                })?,
                            };
                            Ok(SessionCompactCoreOutput::new(
                                result,
                                source_messages,
                                api_started.elapsed(),
                            ))
                        },
                    ))
                });
            let compacted = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Err(lingxi_core::host::HandleError::ActionFailed(
                        "Compaction canceled.".into(),
                    ));
                }
                result = self.dispatch_session_compact_mods(
                    "manual",
                    history_before,
                    merged_instructions.as_deref(),
                    core,
                ) => result.map_err(|error| {
                    lingxi_core::host::HandleError::ActionFailed(error.to_string())
                })?,
            };
            let (result, history_before, compact_duration) = match compacted {
                SessionCompactDecision::Continue {
                    result,
                    source_messages,
                    api_duration,
                    ..
                } => (result, source_messages, api_duration),
                SessionCompactDecision::Skip { reason, .. } => {
                    return Err(lingxi_core::host::HandleError::ActionFailed(reason));
                }
            };
            let messages_before = u32::try_from(history_before.len()).unwrap_or(u32::MAX);
            let bytes_before: u64 = history_before
                .iter()
                .map(lingxi_core::types::text_byte_size)
                .sum();
            let pre_tokens_estimate =
                compaction::grouping::estimate_tokens_for_range(&history_before);
            let cost_receipt =
                self.begin_compaction_usage(cost_scope.as_ref(), &result, compact_duration);
            // CC re-checks `signal.aborted` between compaction phases: an Esc that
            // lands while the summarizer response was already resolving must still
            // abort BEFORE the post-compact transition (file re-reads, SessionStart
            // hooks, history swap) — otherwise the cancelled task swaps history out
            // from under a prompt the user has since submitted.
            if cancel.is_cancelled() {
                return Err(lingxi_core::host::HandleError::ActionFailed(
                    "Compaction canceled.".into(),
                ));
            }
            // API duration = the summarizer round-trip only. `compact_started`
            // (above, pre-hooks) feeds the boundary's user-visible durationMs;
            // feeding it here would fold PreCompact hook wall-time into
            // /cost's total_api_duration_ms.
            self.settle_compaction_usage(cost_receipt)
                .await
                .map_err(|error| {
                    lingxi_core::host::HandleError::ActionFailed(format!(
                        "Compaction cost settlement failed: {error}"
                    ))
                })?;

            // Apply the post-compact transition (boundary marker + history swap +
            // CompactionCompleted emit) via the shared helper reused by the
            // proactive trigger (Batch 4) and the reactive 413 fallback (Batch 5).
            // `bytes_before` / `pre_tokens_estimate` were computed from the same
            // `history_before` snapshot (consumed by `process_iteration` above).
            let Some(summary_out) = self
                .apply_post_compact(
                    result,
                    compaction::CompactTrigger::Manual,
                    pre_tokens_estimate,
                    messages_before,
                    bytes_before,
                    compact_started,
                    Some(&cancel),
                )
                .await
            else {
                // Esc landed before the post-compact commit phase: no auxiliary
                // state or history has been changed.
                return Err(lingxi_core::host::HandleError::ActionFailed(
                    "Compaction canceled.".into(),
                ));
            };

            Ok(summary_out)
        }
        .await;
        if let Err(error) = &outcome {
            let detail = match error {
                lingxi_core::host::HandleError::ActionFailed(detail) => detail.as_str(),
                lingxi_core::host::HandleError::Unimplemented(_) => "compaction failed",
            };
            self.output.emit_compaction_finished(Some(detail)).await;
        }
        outcome
    }

    /// Apply a completed compaction pass to the live session: append the
    /// `[Compacted N → M messages]` boundary marker, swap `session.history`
    /// under the lock, persist the marker to the optional JSONL writer, and
    /// emit [`lingxi_core::host::OutputStream::emit_compaction_completed`].
    ///
    /// Factored out of [`Self::force_compact_with_cancel`] (Batch 4) so the
    /// manual `/compact` path, the proactive pre-call trigger
    /// ([`crate::turn_loop::maybe_compact_before_call`] via
    /// [`Self::maybe_compact_before_call`]), and the reactive 413 fallback
    /// (Batch 5) all replace history identically. Mirrors TS
    /// `buildPostCompactMessages` + the `CompactionCompleted` yield
    /// (`query.ts:528-534`).
    ///
    /// `messages_before` / `bytes_before` are computed by the caller from the
    /// pre-compaction snapshot (the same snapshot fed to the compactor); the
    /// helper does NOT re-read history before swapping because the caller has
    /// not mutated it between snapshot and apply.
    /// Snapshot the read-file-state, clear it, and restore the most-recent
    /// files as post-compact attachment messages.
    ///
    /// #59 / `K2p`+`Pqn` (`bin/claude.exe` offsets 202820676 / 203001477): after
    /// a compaction the read-file-state is cleared (its entries no longer match
    /// the summarized history) and up to
    /// [`compaction::POST_COMPACT_MAX_FILES_TO_RESTORE`] of the most-recently-read
    /// files are re-attached — content capped at
    /// [`compaction::POST_COMPACT_MAX_TOKENS_PER_FILE`] each (oversized reads
    /// become `compact_file_reference` attachments) and a running
    /// [`compaction::POST_COMPACT_TOKEN_BUDGET`] total — so the model keeps the
    /// freshest file context across the boundary. Selection is the pure
    /// [`compaction::select_post_compact_files`]; each survivor is then RE-READ
    /// from disk (the byte-faithful `eRg`/`XQn` behaviour — see below), budgeted,
    /// and rendered as a `<system-reminder>` meta user message.
    ///
    /// P2-12 / `eRg` (`bin/claude.exe` offset ~91938880): the binary re-reads
    /// each selected file at compact time via `XQn(filename,
    /// {...ctx,fileReadingLimits:{maxTokens:z0g}}, "…_success", "…_error",
    /// "compact")` rather than reusing the stale `readFileState` snapshot. A
    /// successful re-read fires `tengu_post_compact_file_restore_success` (empty
    /// payload, `N(r,{})`) and attaches the FRESH content; an unreadable/deleted
    /// file fires `tengu_post_compact_file_restore_error` (`N(n,{})`) and is
    /// dropped — the model never sees stale/since-deleted content. This differs
    /// observably from the snapshot only when a file changed or was deleted after
    /// its last read.
    ///
    /// SKILL restoration (`rRg`/`kGo`) IS wired here (P2-12): the Skill tool
    /// records each invocation in the process-global but session-scoped
    /// [`compaction::invoked_skills`] registry (`zSr`), and after the file arm we
    /// [`compaction::invoked_skills::filter_for_scope`] the current session's
    /// main-thread rows (`agentId = None`), run
    /// [`compaction::restore_post_compact_skills`]
    /// (`rRg`: `invokedAt` DESC, per-skill truncate 5000, budget 25000, registry
    /// write-back on truncation/overflow), and emit the survivors as ONE `isMeta`
    /// user message in the byte-faithful `invoked_skills` attachment shape
    /// (`render_invoked_skills_attachment`). The registry outlives the compaction
    /// (never cleared by `run_post_compact_cleanup`), so a skill invoked before
    /// compaction re-enters the model's context afterwards.
    ///
    pub(super) async fn restore_post_compact_attachments_against(
        &self,
        boundary_context: &[lingxi_core::types::ConversationMessage],
    ) -> Vec<lingxi_core::types::ConversationMessage> {
        // Native g_n snapshots every cache entry, including nested-memory
        // seeds, before readFileState.clear(). Keep the cache's MRU order so
        // Evo's stable timestamp sort preserves ties.
        let snapshot: Vec<(std::path::PathBuf, tool_api::read_file_state::ReadFileEntry)> = {
            let mut map = self
                .prompt_runtime
                .read_state_map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let snapshot = map
                .keys()
                .into_iter()
                .filter_map(|path| map.remove(&path).map(|entry| (path, entry)))
                .collect::<Vec<_>>();
            map.drain().for_each(drop);
            snapshot
        };

        // The two arms are independent: skills restore from the process-global
        // registry even when no file was read this session, so we do NOT early-
        // return on an empty file snapshot.
        let mut out: Vec<lingxi_core::types::ConversationMessage> = Vec::new();

        if !snapshot.is_empty() {
            let already_attached = Self::post_compact_attached_file_paths(boundary_context);
            let session_id = self.session.lock().await.session_id;
            let plan_file = std::path::PathBuf::from(Self::plan_file_path(
                &session_id,
                &self.cwd,
                self.config.plans_directory.as_deref(),
            ));
            out.extend(
                self.restore_post_compact_files_arm(snapshot, &already_attached, &plan_file)
                    .await,
            );
        }

        // ── SKILL restoration (`rRg`) ──
        // Source candidates from this session's rows in the process-global
        // invoked-skill registry (`kGo` on the main thread, `agentId = None`),
        // budget them (`rRg`), and
        // emit the survivors as ONE `isMeta` user message in the byte-faithful
        // `invoked_skills` attachment shape. Preserve the binary's LQn
        // deduplication against both plain message bodies and skill content
        // that already survived in an earlier invoked-skills attachment.
        let session_id = self.session.lock().await.session_id.to_string();
        let skill_candidates = compaction::invoked_skills::filter_for_scope(
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None),
        );
        let already_attached_skills = self.post_compact_attached_skill_contents(boundary_context);
        let restored_skills =
            compaction::restore_post_compact_skills(skill_candidates, &already_attached_skills);
        if let Some(rendered) =
            compaction::render_invoked_skills_attachment_with_sidecar(&restored_skills)
        {
            let message_id = lingxi_core::types::MessageId::new();
            let contents = restored_skills
                .iter()
                .map(|skill| skill.content.clone())
                .collect();
            self.transcript
                .post_compact_skill_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(message_id, contents);
            out.push(lingxi_core::types::ConversationMessage::User { api_message_override: None,
                id: message_id,
                content: vec![rendered.into_content_block()],
                is_meta: true,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
            });
        }

        // 2.1.266 `_ts` (@4123349), assembled by `KOe` as
        // `[...files, ...skills, ...planFileReference, …]` — a plan file that
        // survived the boundary is re-attached so the model can pick the plan
        // back up after a compaction.
        if let Some(message) = self.plan_file_reference_attachment().await {
            out.push(message);
        }

        out
    }

    /// `_ts(agentId, storageV5)` — the `plan_file_reference` attachment.
    ///
    /// ```js
    /// async function _ts(e,n){ if(i8()!==void 0) Bp(ay(e));
    ///   let r=await Kke(e,n); if(!r)return null; let o=ay(e);
    ///   return un({type:"plan_file_reference",planFilePath:o,planContent:r}) }
    /// ```
    ///
    /// `if(!r)` is JS-falsy, so an EMPTY plan file produces no attachment — not
    /// just a missing one. The `Bp(ay(e))` cache-invalidation half has no Rust
    /// home (the port reads the file directly rather than through a plan-file
    /// cache).
    pub(super) async fn plan_file_reference_attachment(
        &self,
    ) -> Option<lingxi_core::types::ConversationMessage> {
        let path = {
            let session = self.session.lock().await;
            self.session_plan_file_path(&session.session_id)
        };
        let content = std::fs::read_to_string(&path).ok()?;
        if content.is_empty() {
            return None;
        }
        let body = crate::prompt::plan_reminder::render_plan_file_reference(&path, &content);
        Some(lingxi_core::types::ConversationMessage::user_meta(
            lingxi_core::types::MessageId::new(),
            format!("<system-reminder>\n{body}\n</system-reminder>"),
        ))
    }

    fn post_compact_attached_file_paths(
        messages: &[lingxi_core::types::ConversationMessage],
    ) -> Vec<String> {
        // Native 2.1.261 `kXo`: only preserved Read tool calls establish that
        // a file is still in context. A Read that returned an unchanged-file
        // stub depends on an earlier result and cannot establish that itself.
        let dedup_reads = messages
            .iter()
            .flat_map(|message| {
                let lingxi_core::types::ConversationMessage::User { content, .. } = message else {
                    return Vec::new();
                };
                content
                    .iter()
                    .filter_map(|block| match block {
                        lingxi_core::types::ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            content_blocks: None,
                            provider_tool_use_id,
                            ..
                        } if tool_file::read::is_dedup_result(content) => Some(
                            provider_tool_use_id
                                .clone()
                                .unwrap_or_else(|| tool_use_id.to_string()),
                        ),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<HashSet<_>>();
        messages
            .iter()
            .flat_map(|message| {
                let lingxi_core::types::ConversationMessage::Assistant { content, .. } = message
                else {
                    return Vec::new();
                };
                content
                    .iter()
                    .filter_map(|block| {
                        let lingxi_core::types::ContentBlock::ToolUse {
                            id,
                            name,
                            input,
                            provider_id,
                            ..
                        } = block
                        else {
                            return None;
                        };
                        if name != "Read"
                            || dedup_reads
                                .contains(&provider_id.clone().unwrap_or_else(|| id.to_string()))
                        {
                            return None;
                        }
                        let path = input.get("file_path")?.as_str()?;
                        Some(
                            crate::turn_loop::normalize_lexically(std::path::Path::new(path))
                                .to_string_lossy()
                                .into_owned(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn post_compact_attached_skill_contents(
        &self,
        messages: &[lingxi_core::types::ConversationMessage],
    ) -> Vec<compaction::AttachedSkillContent> {
        let surviving_ids = messages
            .iter()
            .map(lingxi_core::types::ConversationMessage::id)
            .collect::<std::collections::HashSet<_>>();
        let mut known_attachments = self
            .transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        known_attachments.retain(|message_id, _| surviving_ids.contains(message_id));

        let mut attached = Vec::new();
        for message in messages {
            if let Some(contents) = known_attachments.get(&message.id()) {
                attached.extend(
                    contents
                        .iter()
                        .cloned()
                        .map(compaction::AttachedSkillContent::Attachment),
                );
                continue;
            }
            // Native `Lle` accepts only meta-user messages made entirely of
            // text, joining multiple blocks with a blank line. Ordinary user
            // or assistant text must not suppress registry write-backs.
            if let lingxi_core::types::ConversationMessage::User {
                content,
                is_meta: true,
                ..
            } = message
            {
                let text = content
                    .iter()
                    .map(|block| match block {
                        lingxi_core::types::ContentBlock::Text { text, .. }
                        | lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. } => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>();
                if let Some(text) = text {
                    attached.push(compaction::AttachedSkillContent::Body(text.join("\n\n")));
                }
            }
        }
        attached
    }

    /// The FILE restoration arm of [`Self::restore_post_compact_attachments`]
    /// (`eRg`/`XQn`): select recent files, RE-READ each from disk, fire the
    /// per-file restore telemetry, budget the fresh contents, and render each as
    /// a `<system-reminder>` meta user message. Split out so the skill arm can
    /// run even when the file snapshot is empty.
    async fn restore_post_compact_files_arm(
        &self,
        snapshot: Vec<(std::path::PathBuf, tool_api::read_file_state::ReadFileEntry)>,
        already_attached: &[String],
        plan_file: &std::path::Path,
    ) -> Vec<lingxi_core::types::ConversationMessage> {
        let candidates: Vec<compaction::FileRestoreCandidate> = snapshot
            .into_iter()
            .map(|(path, entry)| compaction::FileRestoreCandidate {
                path,
                content: entry.content,
                timestamp_ms: entry.mtime_ms,
            })
            .collect();

        // Selection half of `eRg`: filter the plan file (`aRg`) and files that
        // already survived in the preserved boundary context, then sort mtime
        // DESC and take the top five.
        let plan_file = crate::turn_loop::normalize_lexically(plan_file);
        // Native Ovo / Dvo excludes the five eager entrypoints; folder memory
        // files and imported files remain ordinary restore candidates.
        let roots = self
            .prompt_runtime
            .nested_memory_roots
            .clone()
            .or_else(|| self.memory.hierarchy_roots());
        let managed = roots
            .as_ref()
            .and_then(|(_, managed)| managed.clone())
            .unwrap_or_else(memory::lingxi_md::hierarchy::managed_path);
        let mut memory_entrypoints = vec![
            self.cwd.join(branding::MEMORY_FILE),
            self.cwd.join(branding::MEMORY_LOCAL_FILE),
            managed.join(branding::MEMORY_FILE),
        ];
        let config_home = self.config_home.clone().or_else(|| {
            roots
                .as_ref()
                .map(|(home, _)| memory::lingxi_md::user_config_dir(home))
        });
        if let Some(config_home) = config_home.as_ref() {
            memory_entrypoints.push(config_home.join(branding::MEMORY_FILE));
        }
        let auto_memory_dir = self
            .prompt_runtime
            .memory_prefetch
            .as_ref()
            .and_then(|p| p.user_memdir())
            .map(std::path::Path::to_path_buf)
            .or_else(|| {
                config_home
                    .as_ref()
                    .map(|home| memory::memdir::paths::user_memdir_for_project(home, &self.cwd))
            });
        if let Some(dir) = auto_memory_dir {
            memory_entrypoints.push(dir.join("MEMORY.md"));
        }
        let candidates = candidates
            .into_iter()
            .filter(|candidate| {
                let path = crate::turn_loop::normalize_lexically(&candidate.path);
                // Native x$o excludes persisted artifact-* files only directly
                // inside tool-results, with a case-insensitive basename check.
                let artifact = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.to_ascii_lowercase().starts_with("artifact-"))
                    && path
                        .parent()
                        .and_then(std::path::Path::file_name)
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.eq_ignore_ascii_case("tool-results"));
                path != plan_file
                    && !artifact
                    && !memory_entrypoints
                        .iter()
                        .any(|entrypoint| crate::turn_loop::normalize_lexically(entrypoint) == path)
                    && !already_attached
                        .iter()
                        .any(|attached| attached == path.to_string_lossy().as_ref())
            })
            .collect();
        let selected = compaction::select_post_compact_files(candidates, &[]);

        enum FreshFileAttachment {
            Content {
                path: std::path::PathBuf,
                content: String,
            },
            Reference {
                path: std::path::PathBuf,
            },
        }

        // RE-READ each selected file from disk (`XQn`), firing the restore
        // telemetry per file. A file that changed since its last read yields the
        // FRESH content; a deleted/unreadable file is dropped (never restoring the
        // stale snapshot content the model would otherwise have carried across the
        // boundary).
        let mut fresh = Vec::with_capacity(selected.len());
        for candidate in selected {
            let read = read_restored_text_prefix(
                &candidate.path,
                tool_file::read::MAX_FILE_READ_SIZE as usize,
            )
            .await;
            match read {
                Ok((content, truncated)) => {
                    // Native Ake/aW normalizes a full text read before token
                    // budgeting and counting lines, including its empty final
                    // fragment. Apply the same body to the restored Read state.
                    let units = content.encode_utf16().count() as u64;
                    let extension = candidate
                        .path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(str::to_ascii_lowercase);
                    let divisor = match extension.as_deref() {
                        Some("json" | "jsonl" | "jsonc") => 2,
                        _ => 4,
                    };
                    if truncated
                        || (units + divisor / 2) / divisor
                            > compaction::POST_COMPACT_MAX_TOKENS_PER_FILE
                    {
                        // y5e's compact-reference fallback returns directly,
                        // before emitting the successful-file-read event.
                        fresh.push(FreshFileAttachment::Reference {
                            path: candidate.path,
                        });
                    } else {
                        self.fire_post_compact_file_restore(true).await;
                        if let Ok(metadata) = tokio::fs::metadata(&candidate.path).await {
                            let mtime_ms = metadata
                                .modified()
                                .map(tool_api::read_file_state::mtime_ms_floor)
                                .unwrap_or(0);
                            tool_api::read_file_state::set_with_requested_path(
                                &self.prompt_runtime.read_state_map,
                                candidate.path.clone(),
                                tool_api::read_file_state::ReadFileEntry {
                                    content: content.clone(),
                                    mtime_ms,
                                    offset: None,
                                    limit: None,
                                    from_read: true,
                                    seeded_from_context: false,
                                    is_partial_view: false,
                                },
                                true,
                                Some(candidate.path.clone()),
                            );
                        }
                        fresh.push(FreshFileAttachment::Content {
                            path: candidate.path,
                            content,
                        });
                    }
                }
                Err(_) => {
                    // Unreadable/deleted at compact time → drop; `XQn` returns
                    // null and the file is filtered out of the attachment set.
                    self.fire_post_compact_file_restore(false).await;
                }
            }
        }

        let mut running_tokens = 0u64;
        let mut restored = Vec::new();
        for attachment in fresh {
            let (path, data) = match attachment {
                FreshFileAttachment::Content { path, content } => {
                    let lines = content.bytes().filter(|&byte| byte == b'\n').count() + 1;
                    let data = serde_json::json!({
                        "type": "file", "filename": path,
                        "content": { "type": "text", "file": {
                            "filePath": path, "content": content,
                            "numLines": lines, "startLine": 1,
                            "totalLines": lines,
                        }},
                    });
                    (path, data)
                }
                FreshFileAttachment::Reference { path } => {
                    let data = serde_json::json!({
                        "type": "compact_file_reference", "filename": path,
                    });
                    (path, data)
                }
            };
            // yXo budgets JSON.stringify(pn(attachment)), including JSON
            // escapes and the attachment envelope, before rendering its
            // call/result messages. A body-only estimate undercounts
            // control characters and can exceed the 50k aggregate budget.
            let mut data = data;
            let cwd = crate::turn_loop::normalize_lexically(&self.cwd);
            let normalized_path = crate::turn_loop::normalize_lexically(&path);
            let common = cwd
                .components()
                .zip(normalized_path.components())
                .take_while(|(left, right)| left == right)
                .count();
            let mut display_path = std::path::PathBuf::new();
            for _ in cwd.components().skip(common) {
                display_path.push("..");
            }
            for component in normalized_path.components().skip(common) {
                display_path.push(component.as_os_str());
            }
            data["displayPath"] = serde_json::json!(display_path.to_string_lossy());
            let id = MessageId::new();
            let envelope = serde_json::json!({
                "attachment": data, "type": "attachment",
                "uuid": id.as_uuid().to_string(),
                "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            });
            let cost = compaction::estimate_content_tokens(&envelope.to_string());
            if running_tokens.saturating_add(cost) > compaction::POST_COMPACT_TOKEN_BUDGET {
                continue;
            }
            running_tokens = running_tokens.saturating_add(cost);
            let message = Self::file_attachment_projection(id, &data)
                .expect("new current file restore payload");
            self.transcript
                .model_reminder_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id, data);
            restored.push(message);
        }
        restored
    }

    /// Fire the post-compact file-restore telemetry — `N(r,{})` / `N(n,{})` in
    /// the binary's `XQn`: an EMPTY payload, one event per re-read attempt.
    ///
    /// `true` → `tengu_post_compact_file_restore_success` (the file re-read
    /// cleanly); `false` → `tengu_post_compact_file_restore_error` (unreadable /
    /// deleted). No-op when no analytics bus is wired (library/test callers),
    /// mirroring the other `fire_*` compaction telemetry helpers.
    async fn fire_post_compact_file_restore(&self, success: bool) {
        let Some(bus) = self.model_runtime.analytics_bus.as_ref() else {
            return;
        };
        let event = if success {
            "tengu_post_compact_file_restore_success"
        } else {
            "tengu_post_compact_file_restore_error"
        };
        // Empty metadata — the binary fires `N(r,{})` / `N(n,{})` with no fields.
        bus.log_event(event, telemetry::LogEventMetadata::new())
            .await;
    }

    /// `cancel`: the manual `/compact` path threads its Esc token to the
    /// post-summary commit boundary. Cancellation before that boundary leaves
    /// both history and auxiliary state untouched; after commit begins the
    /// transition finishes atomically. `None` (auto/reactive callers, which
    /// have no user-cancellable surface) never returns `None`.
    pub(crate) async fn apply_post_compact(
        &self,
        result: compaction::IterationCompactionResult,
        trigger: compaction::CompactTrigger,
        pre_tokens_estimate: u64,
        messages_before: u32,
        bytes_before: u64,
        compact_started: std::time::Instant,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Option<lingxi_core::host::CompactionSummary> {
        self.apply_post_compact_placed(
            result,
            trigger,
            pre_tokens_estimate,
            messages_before,
            bytes_before,
            compact_started,
            cancel,
            SummaryPlacement::BeforeKept,
            None,
        )
        .await
    }

    /// [`Self::apply_post_compact`] with the two knobs only the `/rewind`
    /// message-selector summarize needs.
    ///
    /// `placement` decides which side of the kept messages the summary lands
    /// on, and `selector_metadata` carries the 5-arg boundary fields
    /// (`messagesSummarized` / `userContext`) that oracle `mne` sets ONLY on
    /// that path — every other path passes the 3-arg shape, and adding the two
    /// fields there would put keys in persisted boundaries that a real
    /// claude-code transcript never carries.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn apply_post_compact_placed(
        &self,
        result: compaction::IterationCompactionResult,
        trigger: compaction::CompactTrigger,
        pre_tokens_estimate: u64,
        messages_before: u32,
        bytes_before: u64,
        compact_started: std::time::Instant,
        cancel: Option<&tokio_util::sync::CancellationToken>,
        placement: SummaryPlacement,
        selector_metadata: Option<SelectorBoundaryMetadata>,
    ) -> Option<lingxi_core::host::CompactionSummary> {
        self.output.emit_compaction_phase("restoring").await;
        // Preserve the transcript-only summary before `result.messages` is
        // consumed into the replacement history. The TUI carries this on the
        // compact boundary so Ctrl-O can reveal the same summary sent to the
        // continuation turn.
        let visible_summary = Self::compaction_summary_text(&result);
        // The manual/reactive group compactor (2.1.261 Ajt) freezes boundary
        // duration before attachment restoration; full auto (Ejt) freezes it
        // afterwards. The group compactor always carries a preserved tail.
        let preserved_duration_ms = (!result.messages_to_preserve.is_empty())
            .then(|| u64::try_from(compact_started.elapsed().as_millis()).unwrap_or(u64::MAX));
        // #58: the usage-zeroed verbatim tail the autocompact layer preserved
        // (`messagesToPreserve` → `messagesToKeep`). Empty on the
        // full-replacement path (short conversation / snip-micro-only /
        // under-threshold), keeping the post-compact history byte-identical to
        // before this finding.
        let preserved_tail = match crate::native_computer::preserve_pending_compaction_tail(
            self,
            &result.messages_to_preserve,
        )
        .await
        {
            Ok(tail) => tail,
            Err(error) => {
                tracing::warn!(%error, "compaction cannot preserve an unfinished native round");
                self.output
                    .emit_compaction_finished(Some(&error.to_string()))
                    .await;
                return None;
            }
        };
        let kept_context_types = self
            .context_attachment_history(&preserved_tail)
            .into_iter()
            .filter_map(|attachment| {
                let kind = attachment.get("type")?.as_str()?;
                if !matches!(
                    kind,
                    "instructions" | "session_context" | "context_sections" | "date"
                ) || (kind == "instructions"
                    && attachment
                        .get("changed")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true))
                {
                    return None;
                }
                Some(kind.to_owned())
            })
            .collect::<HashSet<_>>();
        let tail_ids = preserved_tail
            .iter()
            .map(|message| message.id().to_string())
            .collect::<HashSet<_>>();
        let preserved_media_analysis = result
            .media_analysis_to_preserve
            .into_iter()
            .filter(|message| !tail_ids.contains(&message.id().to_string()))
            .collect::<Vec<_>>();

        // CSM.4: build the TS-faithful compact boundary (`createCompactBoundaryMessage`,
        // the byte-exact `"Conversation compacted"` sentinel) instead of the ad-hoc
        // `[Compacted N → M]` marker, so the TUI scrollback + next-turn system-prompt
        // assembly see the same boundary TS emits. The rich `CompactBoundaryMetadata`
        // is persisted below as the boundary line's `compactMetadata` (P1-05), so a
        // cold `--resume` reconstructs the post-compact transition.
        //
        // #58: when a tail was preserved, the boundary carries a
        // `preserved_segment` (`WAo`): head = first kept msg, anchor = the LAST
        // summary message (suffix-preserving splice point), tail = last kept msg —
        // plus the loader's `preserved_messages` re-splice list (`E$_`). The
        // anchor is the last of `result.messages` (the summary set). When the
        // tail is empty, `create_compact_boundary_with_preserved_tail` yields
        // `preserved_segment: None`, identical to the plain constructor.
        // `preserved_segment` exists so a session loader can re-splice a
        // preserved TAIL back after the summary (`WAo`). Under `AfterKept`
        // there is no tail: the kept messages are the conversation's own HEAD
        // and they are already in their original order. Describing them as a
        // preserved tail would make a cold resume splice them after the summary
        // — the exact reordering this placement exists to avoid — so the
        // boundary is built with no preserved segment at all.
        let boundary_tail: &[lingxi_core::types::ConversationMessage] = match placement {
            SummaryPlacement::BeforeKept => &preserved_tail,
            SummaryPlacement::AfterKept => &[],
        };
        let anchor_uuid = if boundary_tail.is_empty() {
            None
        } else {
            result
                .messages
                .last()
                .map(lingxi_core::types::ConversationMessage::id)
        };
        // RV6 (parity 2.1.208): the full auto/manual compaction path — the only
        // one this port implements — builds its boundary via the 3-arg
        // `U6r(trigger, preTokens, lastUuid)`, leaving `messagesSummarized` (and
        // `userContext`) undefined, so `JSON.stringify` drops both fields. CC only
        // sets `messagesSummarized:f.length` on the 5-arg message-selector
        // (`up_to`/`from`) path (`tHu`), which LingXi has no feature for. Passing
        // `None` here keeps the persisted boundary byte-identical to a real
        // 2.1.208 transcript (no extra field, and no spurious "Summarized N
        // messages" TUI line when a genuine CC client cold-resumes it).
        // `preCompactDiscoveredTools` carries the ToolSearch-loaded set so a cold
        // `--resume` re-marks them loaded (empty ⇒ field omitted on the wire).
        let discovered_tools = self.tools.deferral().loaded_tool_names();
        let (mut marker, mut metadata) = compaction::create_compact_boundary_with_preserved_tail(
            trigger,
            pre_tokens_estimate,
            // `logicalParentUuid` is set from the transcript below, not here.
            None,
            selector_metadata
                .as_ref()
                .and_then(|selector| selector.user_context.clone()),
            selector_metadata
                .as_ref()
                .map(|selector| selector.messages_summarized),
            &discovered_tools,
            boundary_tail,
            anchor_uuid.as_ref(),
        );

        // This is the commit boundary for the post-compact transition. Every
        // phase below mutates live auxiliary state (read-state, invoked-skill
        // budgets, cleanup registries, and lifecycle hooks), some of which
        // cannot be rolled back. Honour cancellation before entering that phase;
        // once it starts, finish the transition so history and auxiliary state
        // cannot disagree.
        if cancel.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            return None;
        }
        // Query auto/PTL success (`DCr(..., u4(w))`, 2.1.286) rolls the
        // complete last-assistant usage before replacing the model context.
        // Manual compact and selector summaries do not call that producer.
        if matches!(trigger, compaction::CompactTrigger::Auto) {
            self.compaction_runtime.roll_over_total_tokens_context();
        }
        let session_memory_progress =
            if let Some(handle) = self.compaction_runtime.session_memory.clone() {
                let history = self.session.lock().await.history.clone();
                let mut extractor = handle.extractor.lock().await;
                let pending = extractor.pending_tool_calls_for_history(&history);
                let revision = extractor.state_revision();
                drop(extractor);
                Some((handle, pending, revision))
            } else {
                None
            };

        // #59: snapshot the read-file-state BEFORE clearing it, then restore the
        // most-recent files as post-compact attachments. Mirrors `Iqn`
        // (`bin/claude.exe` offset 202817825): `let f=eOt(d.readFileState);
        // d.readFileState.clear(); ...; K2p(f,...)`. The restored attachments are
        // appended AFTER the summary, in the `messagesToKeep`/`attachments`
        // position of `buildPostCompactMessages` order. Internal vision
        // sidecars are inserted between the kept tail and these attachments.
        let restored_attachments = self
            .restore_post_compact_attachments_against(&preserved_tail)
            .await;

        // Claude Code 2.1.212 `gio(...)` builds the post-compact attachment
        // set before `executePostCompactHooks`: reload instruction files with
        // `load_reason:"compact"`, then run `SessionStart(source:"compact")`
        // and append its model-facing hook results after the restored files /
        // skills. Run the module-state cleanup first so the reload observes a
        // fresh post-compact state rather than the pre-compact caches.
        compaction::run_post_compact_cleanup(None);
        self.prompt_runtime.instruction_cache.apply_compaction();
        self.reset_context_collapse_after_compact().await;
        self.fire_instructions_loaded_with_reason(hooks::events::InstructionsLoadReason::Compact)
            .await;
        let session_start_messages = self.collect_session_start_messages("compact").await;
        let duration_ms = preserved_duration_ms.unwrap_or_else(|| {
            u64::try_from(compact_started.elapsed().as_millis()).unwrap_or(u64::MAX)
        });

        self.fire_post_compact(
            match trigger {
                compaction::CompactTrigger::Manual => "manual",
                compaction::CompactTrigger::Auto => "auto",
            },
            result.raw_summary_text.clone(),
            result.total_tokens_freed,
        )
        .await;

        // COMPACT.1 / #58: the boundary marker leads the post-compact history,
        // matching TS `buildPostCompactMessages` / `Iqn` order
        // `[boundaryMarker, ...summaryMessages, ...messagesToKeep, ...sidecars,
        // ...attachments, ...hookResults]` (compact.ts:330). The preserved
        // verbatim tail (`messagesToKeep`) rides AFTER the summary; internal
        // vision sidecars then retain reusable media evidence before restored
        // attachments. Empty `preserved_tail` and sidecar set ⇒ the order is
        // identical to before (`[marker, ...summary, ...attachments]`).
        let mut history_after = Vec::with_capacity(
            result.messages.len()
                + 1
                + preserved_tail.len()
                + preserved_media_analysis.len()
                + restored_attachments.len()
                + session_start_messages.len(),
        );
        let tail_preserved = !preserved_tail.is_empty();
        history_after.push(marker.clone());
        // `BeforeKept` is every path but one: the summary replaces the older
        // messages, so it leads and the preserved tail follows. `AfterKept` is
        // `/rewind`'s "Summarize from here", where the EARLIER messages survive
        // and the summary describes what came after them — putting it first
        // would tell the model the conversation ended before it began.
        match placement {
            SummaryPlacement::BeforeKept => {
                history_after.extend(result.messages.iter().cloned());
                // #58: the usage-zeroed verbatim tail (`messagesToKeep`).
                history_after.extend(preserved_tail);
            }
            SummaryPlacement::AfterKept => {
                history_after.extend(preserved_tail);
                history_after.extend(result.messages.iter().cloned());
            }
        }
        // Vision sidecars from the summarized prefix are internal messages,
        // not prose that may be dropped by the summary model. Keep them after
        // the summary so future turns can reuse their fingerprints.
        history_after.extend(preserved_media_analysis.iter().cloned());
        // Restored file attachments ride after the summary + kept tail (the
        // `attachments` slot in `buildPostCompactMessages`).
        history_after.extend(restored_attachments.iter().cloned());
        // `SessionStart(source:"compact")` hook results are the final
        // `hookResults` slot in Claude's `buildPostCompactMessages` order.
        history_after.extend(session_start_messages.iter().cloned());

        // .286 Gr / Cn: only the normal auto/PTL retained-tail transition
        // inserts missing announcement families immediately after the boundary,
        // before the summary. The original Or snapshot is reused here.
        let summary_last_uuid = result
            .messages
            .last()
            .map(|message| message.id().as_uuid().to_string());
        let anchor_matches = metadata
            .preserved_messages
            .as_ref()
            .and_then(|preserved| preserved.anchor_uuid.as_ref())
            .is_none_or(|anchor| Some(anchor) == summary_last_uuid.as_ref());
        let retained_context_announcements = if tail_preserved
            && matches!(trigger, compaction::CompactTrigger::Auto)
            && matches!(placement, SummaryPlacement::BeforeKept)
            && anchor_matches
            && telemetry::feature_flags::flag_bool("tengu_calm_noodle", true)
        {
            self.context_announcement_rows_for_history(
                &history_after,
                self.frozen_instruction_refresh_reason().as_str(),
            )
            .await
            .into_iter()
            .filter(|(_, attachment)| {
                attachment
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| !kept_context_types.contains(kind))
            })
            .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        {
            let mut attachments = self
                .transcript
                .model_reminder_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (message, attachment) in &retained_context_announcements {
                attachments.insert(message.id(), attachment.clone());
            }
        }
        history_after.splice(
            1..1,
            retained_context_announcements
                .iter()
                .map(|(message, _)| message.clone()),
        );

        if let Some((handle, pending_tool_calls, revision)) = session_memory_progress {
            let mut extractor = handle.extractor.lock().await;
            // A background extraction may have committed while post-compact
            // hooks/files were being assembled. In that case its newer state
            // owns the watermark; do not overwrite it with the stale snapshot.
            if extractor.state_revision() == revision {
                extractor.record_compaction_boundary(
                    history_after.last().map(ConversationMessage::id),
                    pending_tool_calls,
                );
            }
        }

        // Claude mutates the boundary metadata only after the complete
        // post-compact message set has been assembled. This includes the
        // boundary, summary, preserved tail, vision sidecars, restored
        // attachments, and SessionStart hook results.
        let post_tokens = compaction::grouping::estimate_tokens_for_range(&history_after);
        metadata.post_tokens = Some(post_tokens);
        metadata.duration_ms = Some(duration_ms);
        let dropped_this_pass = pre_tokens_estimate.saturating_sub(post_tokens);
        let previous_dropped = self
            .compaction_runtime
            .compaction_cumulative_dropped_tokens
            .fetch_add(dropped_this_pass, std::sync::atomic::Ordering::Relaxed);
        metadata.cumulative_dropped_tokens =
            Some(previous_dropped.saturating_add(dropped_this_pass));
        let pre_boundary_last_uuid = self.transcript.last_jsonl_uuid.lock().await.clone();
        metadata.logical_parent_uuid = pre_boundary_last_uuid.clone();

        let messages_after = u32::try_from(history_after.len()).unwrap_or(u32::MAX);
        let bytes_after: u64 = history_after
            .iter()
            .map(lingxi_core::types::text_byte_size)
            .sum();
        let bytes_saved = bytes_before.saturating_sub(bytes_after);

        // Finalize compact metadata before the transcript row passes through
        // session.append. The Mod may rewrite the visible boundary content,
        // while the typed compact metadata and message identity remain owned
        // by the compaction core.
        {
            let s = self.session.lock().await;
            metadata.active_goal = s
                .active_goal
                .as_ref()
                .map(compaction::compact_active_goal_from_engine);
            debug_assert!(marker.set_compact_metadata(metadata.clone()));
        }
        marker = self
            .mod_session_append_row(&marker, None, false, None)
            .await;
        history_after[0] = marker.clone();
        // Swap model-visible history under the same lock while retaining
        // transcript-only completion envelopes. They were excluded from the
        // compactor input and remain excluded after the transition.
        {
            let mut s = self.session.lock().await;
            s.replace_model_context_history(history_after);
            self.compaction_runtime
                .total_tokens_reminder_usage
                .store(0, std::sync::atomic::Ordering::Relaxed);
        }
        self.expect_prompt_cache_rebuild().await;
        // Relevant-memory and skill reminders live only in outgoing request
        // snapshots. Once compaction discards those snapshots, they may surface
        // again; restored file attachments remain deduplicated by
        // `read_state_map`. Drop in-flight pre-compact queries as well so stale
        // selections cannot be injected against the new history.
        self.prompt_runtime
            .surfaced_memory_paths
            .lock()
            .await
            .clear();
        self.prompt_runtime
            .surfaced_skill_names
            .lock()
            .await
            .clear();
        // Compaction rewrites history; a frozen git-status snapshot from the
        // pre-compact conversation would otherwise ride into the next turn.
        *self.prompt_runtime.git_status_snapshot.lock().await = None;
        self.prompt_runtime.invalidate_mod_prompt_context().await;
        *self.prompt_runtime.pending_memory_prefetch.lock().await = None;
        *self.prompt_runtime.pending_skill_prefetch.lock().await = None;
        // P1-05: persist the full compaction transition (claude 2.1.207
        // `insertMessageChain` + the compact flow), so a cold `--resume`
        // reconstructs exactly the post-compact state. Best-effort — a write
        // failure never fails the turn.
        //
        // 1. The boundary line: `parentUuid: null` (chain reset — a tip→root
        //    walk stops here) with the real parent stashed in
        //    `logicalParentUuid`, plus the flattened `subtype:"compact_boundary"`
        //    / `content` / `level:"info"` / `compactMetadata` envelope.
        // 2. The summary user line(s) (`isCompactSummary` +
        //    `isVisibleInTranscriptOnly`), chained off the boundary.
        // 3. The preserved verbatim tail is NOT rewritten — its lines are
        //    already on disk (claude doesn't rewrite them either); the
        //    boundary's `preservedSegment`/`preservedMessages` metadata carries
        //    the loader's re-splice info. The chain pointer is reset to the
        //    tail's LAST on-disk line so subsequent lines parent off the kept
        //    tail, exactly like claude (whose writer chains off the in-memory
        //    array `[boundary, ...summary, ...messagesToKeep, ...]`, skipping
        //    already-persisted members).
        // 4. Preserved vision sidecars, then restored file attachments, chained
        //    after the tail (the `attachments` slot of
        //    `buildPostCompactMessages`).
        self.persist_compact_boundary_to_jsonl(&marker, &metadata)
            .await;
        for (message, _) in &retained_context_announcements {
            self.persist_message_to_jsonl(message).await;
        }
        for m in &result.messages {
            self.persist_compact_summary_to_jsonl(m).await;
        }
        if tail_preserved {
            if let Some(tail_last) = pre_boundary_last_uuid {
                *self.transcript.last_jsonl_uuid.lock().await = Some(tail_last);
            }
        }
        for m in &preserved_media_analysis {
            self.persist_message_to_jsonl(m).await;
        }
        for m in &restored_attachments {
            self.persist_message_to_jsonl(m).await;
        }
        for m in &session_start_messages {
            self.persist_message_to_jsonl(m).await;
        }

        // Claude closes the compact command status before the query driver
        // publishes its boundary. Preserve this order for stream-json clients.
        self.output.emit_compaction_finished(None).await;
        // The `/loop` fold's `blocking_system_in_span` veto: a compaction
        // landed inside the tick, so it was not a quiet one.
        self.turn_span.note_compaction();
        self.output
            .emit_compact_boundary(&marker.id().as_uuid().to_string(), &metadata)
            .await;
        for summary in &result.messages {
            self.output
                .emit_compact_summary(&summary.id().as_uuid().to_string(), &summary.text_content())
                .await;
        }

        // Best-effort emit so the TUI hears about it.
        self.output
            .emit_compaction_completed(
                messages_before,
                messages_after,
                bytes_saved,
                &visible_summary,
            )
            .await;

        Some(lingxi_core::host::CompactionSummary {
            messages_before,
            messages_after,
            bytes_saved,
            summary: visible_summary,
        })
    }

    /// Proactive pre-call compaction trigger (In-Loop Compaction Batch 4).
    ///
    /// Invoked at the TOP of both the batched
    /// ([`crate::turn_loop::execute_one_turn_with_recovery_tracked`]) and the
    /// streaming turn-driver loop bodies, BEFORE the history snapshot that
    /// feeds the model call — so a proactive compact this turn makes the
    /// subsequent snapshot read the NEW, compacted history. 1:1 with the TS
    /// pre-call pipeline (`query.ts:365-467`, `autoCompactIfNeeded` contract at
    /// `autoCompact.ts:241-351`).
    ///
    /// Behavior:
    /// - **No compactor wired** (`compaction == None`): strict NO-OP — history
    ///   untouched, no event. Keeps the locked turn-loop fixtures behaviour-
    ///   neutral until the CLI wires a real compactor (Batch 6).
    /// - **Under threshold**: strict NO-OP. We gate on
    ///   [`compaction::should_auto_compact`] against the snapshot estimate
    ///   BEFORE calling the orchestrator so that snip/microcompact do not
    ///   silently rewrite history below threshold (the proactive trigger is
    ///   an autocompact gate, not an unconditional snip pass).
    /// - **Over threshold**: run `process_iteration_tracked` threading the
    ///   per-conversation [`Self::compaction_tracking`] circuit-breaker state;
    ///   when `was_compacted`, apply via [`Self::apply_post_compact`] (history
    ///   swap + boundary marker + JSONL persist + `CompactionCompleted` emit).
    ///   The updated `consecutive_failures` is persisted back into
    ///   `compaction_tracking` regardless of success so the breaker survives
    ///   across turns.
    ///
    /// **Recursion note** (parity with the TS `querySource === 'compact'`
    /// recursion guard rationale, `query.ts:365`): the autocompact summarizer
    /// runs as a SEPARATE stateless side-query (`ForkedAgentRunner` /
    /// `SideQueryClient` inside `Autocompactor::compact`) — it does NOT
    /// re-enter `execute_one_turn`/this trigger — so no explicit guard flag is
    /// needed here. Documented to make the absence intentional.
    /// Record the last API response's total input-token count
    /// (`input_tokens + cache_read + cache_creation`) for the fixed-prefix
    /// overflow guard. Called by both turn drivers after every successful call.
    /// Mirrors claude-code's `Xtt` last-usage snapshot (see
    /// [`Self::last_response_input_tokens`]).
    pub(crate) fn record_response_input_tokens(&self, usage: &llm_runtime::ExecutionUsage) {
        let total_input = usage
            .counts()
            .input_tokens
            .saturating_add(usage.counts().cache_read_tokens)
            .saturating_add(usage.counts().cache_write_tokens);
        self.compaction_runtime
            .last_response_input_tokens
            .store(total_input, std::sync::atomic::Ordering::Relaxed);
        // `hoe(messages)` (2.1.238 @294688350) sums the last assistant usage
        // INCLUDING output tokens; the `total_tokens_reminder` needs that total,
        // so cache the output half here at the same chokepoint.
        self.compaction_runtime.last_response_output_tokens.store(
            usage
                .counts()
                .output_tokens
                .saturating_sub(usage.counts().reasoning_tokens),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.compaction_runtime.total_tokens_reminder_usage.store(
            total_input.saturating_add(
                usage
                    .counts()
                    .output_tokens
                    .saturating_sub(usage.counts().reasoning_tokens),
            ),
            std::sync::atomic::Ordering::Relaxed,
        );
        // Feed the shared workflow `budget.spent()` pool: this is the single
        // per-response chokepoint both turn drivers call, so adding the
        // response's output tokens here accumulates the main-loop side of the
        // pool. A launched workflow's subagents add their output tokens to the
        // same `Arc`, so `budget.spent()` reads main loop + all workflows.
        if self.model_runtime.output_scopes.is_none() {
            self.compaction_runtime.output_token_pool.fetch_add(
                usage
                    .counts()
                    .output_tokens
                    .saturating_sub(usage.counts().reasoning_tokens),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    pub(crate) async fn compaction_cost_scope(
        &self,
    ) -> Result<Option<cost::CostSessionScope>, cost::CostPersistError> {
        let Some(tracker) = self.model_runtime.cost_tracker.as_ref() else {
            return Ok(None);
        };
        let mut scope = self
            .model_runtime
            .cost_scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if scope.is_none() {
            let session_id = self.session.lock().await.session_id;
            scope = Some(tracker.session_scope(session_id));
        }
        if let Some(scope) = scope.as_ref() {
            scope.preflight().await?;
        }
        Ok(scope)
    }

    /// Transfer a known compaction response before any post-response await.
    pub(crate) fn begin_compaction_usage(
        &self,
        scope: Option<&cost::CostSessionScope>,
        result: &compaction::IterationCompactionResult,
        duration: std::time::Duration,
    ) -> Option<cost::CostResponseReceipt> {
        let (Some(scope), Some(usage), Some(model)) = (
            scope,
            result.compaction_usage,
            result.compaction_model.as_deref(),
        ) else {
            return None;
        };
        let model_ref =
            crate::cost_wiring::model_ref_from_string(model, result.compaction_profile.as_deref());
        Some(scope.submit_model_response(cost::CostModelResponse {
            model_ref,
            usage,
            duration,
            retries: 0,
            cache_read_input_tokens: usage.tokens.cache_read,
            cache_creation_input_tokens: usage.tokens.cache_write,
            is_batch_request: false,
            bus: self.model_runtime.analytics_bus.clone(),
        }))
    }

    pub(crate) async fn settle_compaction_usage(
        &self,
        receipt: Option<cost::CostResponseReceipt>,
    ) -> Result<(), cost::CostPersistError> {
        let Some(receipt) = receipt else {
            return Ok(());
        };
        let settlement = receipt.settle().await;
        self.model_runtime
            .api_calls_recorded
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        settlement.persistence_result().clone().map(|_| ())
    }

    /// The shared output-token pool backing a launched workflow's
    /// `budget.spent()`. The composition root hands the returned `Arc` to the
    /// `LocalWorkflowHandler` so the script's `spent()` reflects the union of
    /// main-loop and all-workflow output tokens. See [`Self::output_token_pool`].
    #[must_use]
    pub fn output_token_pool(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.compaction_runtime.output_token_pool.clone()
    }

    /// The turn-start output baseline (claude-code `xtr`) backing a launched
    /// workflow's turn-relative `budget.spent()`. The composition root hands this
    /// `Arc` to the `LocalWorkflowHandler` (snapshotted at workflow spawn). See
    /// [`Self::turn_start_output_baseline`].
    #[must_use]
    pub fn turn_start_output_baseline(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.compaction_runtime.turn_start_output_baseline.clone()
    }

    /// Fire the `tengu_auto_compact_prefix_overflow` telemetry event for a
    /// detected fixed-prefix overflow.
    ///
    /// 1:1 with claude-code v2.1.183 (`bin/claude.exe` offset 203006250): the
    /// auto path on a non-`null` `a3p` result emits
    /// `j("tengu_auto_compact_prefix_overflow", {...u, wouldHaveBlocked:!0})`
    /// — the spread `...u` carries the overflow descriptor fields, and the
    /// path WARNS but STILL PROCEEDS (the guard never aborts the auto compact;
    /// only the reactive PTL path surfaces `compactionImpossible`).
    async fn fire_prefix_overflow_telemetry(&self, overflow: compaction::PrefixOverflow) {
        let Some(bus) = self.model_runtime.analytics_bus.as_ref() else {
            return;
        };
        #[allow(clippy::cast_possible_wrap)]
        fn int(v: u64) -> telemetry::AnalyticsValue {
            telemetry::AnalyticsValue::Int(i64::try_from(v).unwrap_or(i64::MAX))
        }
        let mut metadata = telemetry::LogEventMetadata::new();
        metadata.insert("prefixTokens".into(), int(overflow.prefix_tokens));
        metadata.insert("thresholdTokens".into(), int(overflow.threshold_tokens));
        metadata.insert("totalInputTokens".into(), int(overflow.total_input_tokens));
        metadata.insert("messagesEstimate".into(), int(overflow.messages_estimate));
        metadata.insert("snipTokensFreed".into(), int(overflow.snip_tokens_freed));
        metadata.insert(
            "documentBlockCount".into(),
            int(u64::from(overflow.document_block_count)),
        );
        metadata.insert(
            "imageBlockCount".into(),
            int(u64::from(overflow.image_block_count)),
        );
        metadata.insert(
            "wouldHaveBlocked".into(),
            telemetry::AnalyticsValue::Bool(true),
        );
        bus.log_event("tengu_auto_compact_prefix_overflow", metadata)
            .await;
    }

    /// Fire the `tengu_auto_compact_rapid_refill_breaker` telemetry event when
    /// the thrashing breaker trips.
    ///
    /// 1:1 with claude-code v2.1.183 (`bin/claude.exe` offset 202942256 /
    /// 203006250): `j("tengu_auto_compact_rapid_refill_breaker",
    /// {consecutiveRapidRefills, turnsSincePreviousCompact, ..., reactive})`.
    /// The reactive arm sets `reactive:!0`; the proactive arm omits it.
    async fn fire_rapid_refill_breaker_telemetry_with_reactive(
        &self,
        consecutive_rapid_refills: u32,
        turns_since_previous_compact: i64,
        reactive: bool,
    ) {
        let Some(bus) = self.model_runtime.analytics_bus.as_ref() else {
            return;
        };
        let mut metadata = telemetry::LogEventMetadata::new();
        metadata.insert(
            "consecutiveRapidRefills".into(),
            telemetry::AnalyticsValue::Int(i64::from(consecutive_rapid_refills)),
        );
        metadata.insert(
            "turnsSincePreviousCompact".into(),
            telemetry::AnalyticsValue::Int(turns_since_previous_compact),
        );
        // queryTracking fields (binary v2.1.193 rapid-refill breaker payload
        // `{…,queryChainId:se,queryDepth:ne.depth}`). `queryDepth` is always 0 in
        // this port (subagents never run through `ConversationOrchestrator`).
        metadata.insert(
            "queryChainId".into(),
            telemetry::AnalyticsValue::String(self.query_chain_id.clone()),
        );
        metadata.insert("queryDepth".into(), telemetry::AnalyticsValue::Int(0));
        if reactive {
            metadata.insert("reactive".into(), telemetry::AnalyticsValue::Bool(true));
        }
        bus.log_event("tengu_auto_compact_rapid_refill_breaker", metadata)
            .await;
    }

    /// Proactive-arm rapid-refill breaker telemetry (no `reactive` flag).
    async fn fire_rapid_refill_breaker_telemetry(
        &self,
        consecutive_rapid_refills: u32,
        turns_since_previous_compact: i64,
    ) {
        self.fire_rapid_refill_breaker_telemetry_with_reactive(
            consecutive_rapid_refills,
            turns_since_previous_compact,
            false,
        )
        .await;
    }

    /// Reactive-arm rapid-refill breaker telemetry (`reactive:true`). Called by
    /// the reactive PTL recovery path in `turn_loop`.
    pub(crate) async fn fire_rapid_refill_breaker_telemetry_reactive(
        &self,
        consecutive_rapid_refills: u32,
        turns_since_previous_compact: i64,
    ) {
        self.fire_rapid_refill_breaker_telemetry_with_reactive(
            consecutive_rapid_refills,
            turns_since_previous_compact,
            true,
        )
        .await;
    }

    /// `tengu_post_autocompact_turn` (binary main loop, offset ~209133741):
    /// fired once per turn that follows an auto-compact, alongside the
    /// `turnCounter++` increment. Payload `{turnId, turnCounter, queryChainId,
    /// queryDepth}`; `queryDepth` is always 0 (subagents never run through
    /// `ConversationOrchestrator`). Strict no-op when no analytics bus is wired.
    async fn fire_post_autocompact_turn(&self, turn_id: &str, turn_counter: u32) {
        let Some(bus) = self.model_runtime.analytics_bus.as_ref() else {
            return;
        };
        let mut metadata = telemetry::LogEventMetadata::new();
        metadata.insert(
            "turnId".into(),
            telemetry::AnalyticsValue::String(turn_id.to_string()),
        );
        metadata.insert(
            "turnCounter".into(),
            telemetry::AnalyticsValue::Int(i64::from(turn_counter)),
        );
        metadata.insert(
            "queryChainId".into(),
            telemetry::AnalyticsValue::String(self.query_chain_id.clone()),
        );
        metadata.insert("queryDepth".into(), telemetry::AnalyticsValue::Int(0));
        bus.log_event(
            telemetry::tengu::orchestrator::POST_AUTOCOMPACT_TURN,
            metadata,
        )
        .await;
    }

    pub(crate) async fn maybe_compact_before_call(&self) {
        let Some(compactor) = self.compaction_runtime.compaction.clone() else {
            // No compactor wired — strict no-op (history untouched).
            return;
        };

        // Context collapse owns proactive headroom while enabled. Its committed
        // view is applied later by `prepare_model_call_snapshot`; prompt-too-long
        // recovery drains staged collapses before the ordinary compact fallback.
        // The gate is default-off, so the established autocompact path remains
        // byte-identical unless explicitly enabled.
        if compaction::is_context_collapse_enabled() {
            return;
        }

        // #54 per-turn turn-counter increment + `tengu_post_autocompact_turn`
        // emit (binary `if(le?.compacted)le.turnCounter++,G(
        // "tengu_post_autocompact_turn",{turnId,turnCounter,queryChainId,queryDepth})`,
        // offsets 202951666 / ~209133741). Runs on EVERY turn after a compact
        // (regardless of the threshold below) so the rapid-refill window
        // (`turn_counter < RAPID_REFILL_TURN_WINDOW`) measures
        // turns-since-previous-compact correctly. Capture under the lock, then
        // emit after releasing it (the analytics emit is async).
        let post_autocompact = {
            let mut tracking = self.compaction_runtime.compaction_tracking.lock().await;
            if tracking.compacted {
                tracking.turn_counter = tracking.turn_counter.saturating_add(1);
                Some((tracking.turn_id.clone(), tracking.turn_counter))
            } else {
                None
            }
        };
        if let Some((turn_id, turn_counter)) = post_autocompact {
            self.fire_post_autocompact_turn(&turn_id, turn_counter)
                .await;
        }

        // Snapshot history + estimate tokens WITHOUT holding the lock across
        // the (possibly networked) compaction call. The session's CURRENT model
        // rides along for the threshold resolution below.
        let (snapshot, last_assistant_at, model) = {
            let s = self.session.lock().await;
            (
                s.model_context_history(),
                s.message_timing.last_assistant_at,
                s.model.clone(),
            )
        };
        let estimate = compaction::grouping::estimate_tokens_for_range(&snapshot);

        // The gate's ceiling belongs to the model, not to the engine:
        // claude-code resolves `getAutoCompactThreshold(model)`
        // (`autoCompact.ts:72-91`) at its `autoCompactIfNeeded` call site, so a
        // 1M-token catalog model must not be compacted at a boot-time constant
        // sized for the 200k Claude window, and a mid-session model switch must
        // move the gate with it. `refresh_autocompact_threshold` is a no-op for
        // fixed-threshold callers (tests, embedded hosts) — it returns their
        // configured value.
        let threshold = compactor.refresh_autocompact_threshold(&model, &self.api.active_betas());

        // Threshold gate: under threshold ⇒ strict no-op. `snip_freed = 0`
        // because we have done no snip work yet at the call site.
        if !compaction::should_auto_compact(estimate, 0, threshold) {
            return;
        }

        // #7 consecutive-failures breaker (`bin/claude.exe` `ewo` step 2): a
        // tripped breaker (`consecutiveFailures >= MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES`)
        // makes compaction a guaranteed no-op, so RETURN HERE — BEFORE the
        // fixed-prefix overflow probe below. The binary's `ewo` runs
        // `if(o?.consecutiveFailures>=Swo) return {wasCompacted:!1}` (Swo=3)
        // ahead of `Wom` (the prefix-overflow probe that emits
        // `tengu_auto_compact_prefix_overflow`), so a breaker-tripped,
        // over-threshold session emits NO prefix-overflow event. Without this
        // early return our code fired a spurious `tengu_auto_compact_prefix_overflow`
        // (`wouldHaveBlocked:true`) every turn for such a session.
        {
            let tracking = self.compaction_runtime.compaction_tracking.lock().await;
            if tracking.consecutive_failures
                >= compaction::thresholds::MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES
            {
                return;
            }
        }

        // #55 fixed-prefix overflow guard (`a3p`, `bin/claude.exe` offset
        // 203004969). If the immovable prefix (system prompt + tools +
        // attachments = `totalInput − messages`) already exceeds the autocompact
        // threshold, compaction can NEVER bring usage below threshold. Mirror the
        // auto path exactly: WARN + emit `tengu_auto_compact_prefix_overflow`
        // (`wouldHaveBlocked:true`) but STILL PROCEED — the guard is observational
        // on the auto path; only the reactive PTL path surfaces it to the user as
        // `compactionImpossible`. `totalInput` is the last response's input total
        // (the `Xtt` snapshot); `0` until the first call ⇒ a strict no-op.
        let total_input = self
            .compaction_runtime
            .last_response_input_tokens
            .load(std::sync::atomic::Ordering::Relaxed);
        let (doc_blocks, img_blocks) = count_document_and_image_blocks(&snapshot);
        if let Some(overflow) = compaction::compaction_prefix_overflow(
            total_input,
            estimate,
            threshold,
            0,
            doc_blocks,
            img_blocks,
        ) {
            tracing::warn!(
                prefix_tokens = overflow.prefix_tokens,
                threshold_tokens = overflow.threshold_tokens,
                "autocompact: fixed prefix ~{} > threshold {} — compaction cannot help",
                overflow.prefix_tokens,
                overflow.threshold_tokens,
            );
            self.fire_prefix_overflow_telemetry(overflow).await;
            // Fall through — the auto path STILL PROCEEDS (parity with the binary).
        }

        // hooks compaction lifecycle: PreCompact fires once we have crossed the
        // autocompact threshold and are about to run the summary pass (TS
        // `executePreCompactHooks` BEFORE the summary request, `compact.ts:413`).
        // The proactive trigger is always the `auto` arm. TS precomputed arm: a
        // blocking PreCompact hook logs `Precomputed compact blocked by
        // PreCompact hook: <blockedBy>` and skips compaction (history untouched).
        let compact_started = std::time::Instant::now();
        self.output.emit_compaction_started().await;
        let pre_compact = self.fire_pre_compact("auto", None).await;
        if let Some(detail) = pre_compact.blocked_by {
            tracing::warn!("Precomputed compact blocked by PreCompact hook: {detail}");
            self.output
                .emit_compaction_finished(Some(&format!(
                    "Compaction blocked by PreCompact hook: {detail}"
                )))
                .await;
            return;
        }

        let cost_scope = match self.compaction_cost_scope().await {
            Ok(scope) => scope,
            Err(error) => {
                tracing::error!(%error, "proactive compaction cost preflight failed");
                self.output
                    .emit_compaction_finished(Some(&format!(
                        "Compaction cost preflight failed: {error}"
                    )))
                    .await;
                return;
            }
        };
        let tracking_state = std::sync::Arc::new(tokio::sync::Mutex::new(
            self.compaction_runtime
                .compaction_tracking
                .lock()
                .await
                .clone(),
        ));
        let tracking_for_core = tracking_state.clone();
        let compact_for_core = compactor.clone();
        let phase_output = self.output.clone();
        let last_assistant_at_for_core = last_assistant_at;
        let core: SessionCompactCore = std::sync::Arc::new(move |source_messages, instructions| {
            let tracking = tracking_for_core.clone();
            let compactor = compact_for_core.clone();
            let phase_output = phase_output.clone();
            let last_assistant_at = last_assistant_at_for_core;
            Box::pin(async move {
                phase_output.emit_compaction_phase("summarizing").await;
                let api_started = std::time::Instant::now();
                let mut tracking = tracking.lock().await;
                let result = compactor
                    .process_iteration_tracked_with_instructions_and_timing(
                        source_messages.clone(),
                        0,
                        &mut tracking,
                        instructions.as_deref(),
                        last_assistant_at,
                        std::time::SystemTime::now(),
                    )
                    .await
                    .map_err(|error| hooks::mods::ModError::Native(error.to_string()))?;
                Ok(SessionCompactCoreOutput::new(
                    result,
                    source_messages,
                    api_started.elapsed(),
                ))
            })
        });
        let compacted = self
            .dispatch_session_compact_mods(
                "auto",
                snapshot,
                pre_compact.additional_instructions.as_deref(),
                core,
            )
            .await;
        let tracking_after_core = tracking_state.lock().await.clone();
        let turns_since = i64::from(tracking_after_core.turn_counter);
        *self.compaction_runtime.compaction_tracking.lock().await = tracking_after_core;
        let (result, snapshot, compact_duration, core_ran) = match compacted {
            Ok(SessionCompactDecision::Continue {
                result,
                source_messages,
                api_duration,
                core_ran,
                ..
            }) => (result, source_messages, api_duration, core_ran),
            Ok(SessionCompactDecision::Skip { reason, .. }) => {
                tracing::info!(%reason, "proactive compact skipped by session.compact Mod");
                self.output.emit_compaction_skipped().await;
                self.output.emit_compaction_finished(None).await;
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "proactive autocompact failed; continuing un-compacted");
                self.output
                    .emit_compaction_finished(Some(&error.to_string()))
                    .await;
                return;
            }
        };
        if !core_ran {
            tracing::debug!(
                "proactive session.compact Mod supplied replacement without running core"
            );
        }
        let estimate = compaction::grouping::estimate_tokens_for_range(&snapshot);
        let messages_before = u32::try_from(snapshot.len()).unwrap_or(u32::MAX);
        let bytes_before: u64 = snapshot
            .iter()
            .map(lingxi_core::types::text_byte_size)
            .sum();
        // The response facts become session-owned before any telemetry,
        // output, or cancellation-sensitive await below.
        let cost_receipt =
            self.begin_compaction_usage(cost_scope.as_ref(), &result, compact_duration);
        if let Err(error) = self.settle_compaction_usage(cost_receipt).await {
            tracing::error!(%error, "proactive compaction cost settlement failed");
            self.output
                .emit_compaction_finished(Some(&format!(
                    "Compaction cost settlement failed: {error}"
                )))
                .await;
            return;
        }

        // #54 rapid-refill (thrashing) breaker (proactive trip): when the
        // breaker tripped, the orchestrator SKIPPED the summarizer (history
        // untouched). Emit `tengu_auto_compact_rapid_refill_breaker` and warn,
        // then return — re-summarizing cannot help (a file/tool output is too
        // large), so the proactive path leaves history alone. Mirrors `Eho`
        // (`bin/claude.exe` offset 203006250).
        if result.rapid_refill_breaker_tripped {
            let consecutive = result.consecutive_rapid_refills;
            // `turnsSincePreviousCompact` = the tracking turn counter (the
            // binary reports `oe?.turnCounter ?? -1`).
            tracing::warn!(
                consecutive_rapid_refills = consecutive,
                turns_since_previous_compact = turns_since,
                "autocompact: rapid-refill breaker tripped — {consecutive} consecutive refills within <{} turns each",
                compaction::RAPID_REFILL_TURN_WINDOW,
            );
            self.fire_rapid_refill_breaker_telemetry(consecutive, turns_since)
                .await;
            self.output
                .emit_compaction_finished(Some(compaction::RAPID_REFILL_THRASHING_MESSAGE))
                .await;
            return;
        }

        if !result.was_compacted {
            // Snip/micro may have fired but autocompact did not (circuit
            // breaker tripped, or the post-snip estimate fell under threshold).
            // Keep history untouched so the proactive trigger stays a strict
            // no-op whenever autocompact itself did not run — matching the
            // manual-path contract that only the autocompact transition emits a
            // boundary marker.
            self.output.emit_compaction_skipped().await;
            return;
        }

        // `cancel: None` — the proactive trigger has no user-cancellable
        // surface, so the apply is infallible.
        self.apply_post_compact(
            result,
            compaction::CompactTrigger::Auto,
            estimate,
            messages_before,
            bytes_before,
            compact_started,
            None,
        )
        .await;
    }

    /// Append an SDK/stream-json compact boundary with its structured metadata.
    ///
    /// Claude's SDK uses snake_case inside `compact_metadata`, while the JSONL
    /// transcript stores camelCase `compactMetadata`. Keeping this path
    /// separate from generic system-history append prevents informational
    /// system frames from masquerading as model context and preserves cold
    /// resume semantics for externally replayed compacted sessions.
    pub async fn append_external_compact_boundary(
        &self,
        mut marker: ConversationMessage,
        sdk_compact_metadata: serde_json::Value,
    ) {
        let compact_metadata = camelize_json_keys(sdk_compact_metadata);
        if let Ok(typed_metadata) = serde_json::from_value::<
            lingxi_core::types::CompactBoundaryMetadata,
        >(compact_metadata.clone())
        {
            let _ = marker.set_compact_metadata(typed_metadata);
        }
        marker = self
            .mod_session_append_row(&marker, None, false, None)
            .await;
        {
            let mut session = self.session.lock().await;
            session.history.push(marker.clone());
        }
        self.persist_compact_boundary_jsonl_value(&marker, compact_metadata)
            .await;
    }

    /// Persist a compaction summary user line, stamping the top-level
    /// `isVisibleInTranscriptOnly: true` + `isCompactSummary: true` envelope
    /// flags. 1:1 with claude 2.1.207's summary persist
    /// (`$r({content: v9r(...), isCompactSummary: !0,
    /// isVisibleInTranscriptOnly: !0})`, round-tripped by the writer's
    /// `...f.isCompactSummary===!0&&{isCompactSummary:!0}`); the line chains
    /// off `last_jsonl_uuid` (the compact-boundary line).
    pub(crate) async fn persist_compact_summary_to_jsonl(&self, msg: &ConversationMessage) {
        self.persist_message_to_jsonl_inner(msg, None, None, true)
            .await;
    }

    /// Persist the compact-boundary system line (P1-05) — claude 2.1.207's
    /// `insertMessageChain` chain reset: the boundary gets `parentUuid: null`
    /// (`CC(d)` ⇒ `{parentUuid: p?null:f, logicalParentUuid: p?i:void 0}`) with
    /// the real parent (the last pre-compact on-disk line) stashed in
    /// `logicalParentUuid`, plus the flattened system envelope
    /// (`subtype:"compact_boundary"`, `content:"Conversation compacted"`,
    /// `level:"info"`, camelCase `compactMetadata`) and NO inner `message`.
    /// Best-effort like every other JSONL append; advances `last_jsonl_uuid`
    /// to the boundary's uuid on success so the summary line chains off it.
    pub(super) async fn persist_compact_boundary_to_jsonl(
        &self,
        marker: &ConversationMessage,
        metadata: &compaction::CompactBoundaryMetadata,
    ) {
        // `compactMetadata` wire value: CompactBoundaryMetadata serializes
        // claude's camelCase keys; `logicalParentUuid` is a TOP-LEVEL line
        // field (`x9r` spreads it as a message-level sibling), never a
        // `compactMetadata` member — strip it defensively.
        let marker_metadata = match marker {
            ConversationMessage::System {
                compact_metadata: Some(compact_metadata),
                ..
            } => compact_metadata,
            _ => metadata,
        };
        debug_assert_eq!(marker_metadata, metadata);
        let mut compact_metadata =
            serde_json::to_value(marker_metadata).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = compact_metadata.as_object_mut() {
            obj.remove("logicalParentUuid");
        }
        self.persist_compact_boundary_jsonl_value(marker, compact_metadata)
            .await;
    }

    /// Shared compact-boundary JSONL writer for locally generated and SDK
    /// replayed boundaries. The marker resets the physical chain and retains
    /// the previous tail in `logicalParentUuid`, matching Claude's chain model.
    async fn persist_compact_boundary_jsonl_value(
        &self,
        marker: &ConversationMessage,
        mut compact_metadata: serde_json::Value,
    ) {
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return;
        };
        let session_id_str = self.session.lock().await.session_id.to_string();
        let explicit_parent = match marker {
            ConversationMessage::System {
                compact_metadata: Some(metadata),
                ..
            } => metadata.logical_parent_uuid.clone(),
            _ => None,
        };
        let logical_parent =
            explicit_parent.or(self.transcript.last_jsonl_uuid.lock().await.clone());
        if let Some(metadata) = compact_metadata.as_object_mut() {
            metadata.remove("logicalParentUuid");
        }
        let git_branch = self.resolve_git_branch().await;
        let content = match marker {
            ConversationMessage::System { content, .. } => content.clone(),
            _ => compaction::BOUNDARY_CONTENT.to_string(),
        };
        let mut extra = serde_json::Map::new();
        extra.insert(
            "subtype".to_string(),
            serde_json::Value::String("compact_boundary".to_string()),
        );
        extra.insert("content".to_string(), serde_json::Value::String(content));
        extra.insert(
            "level".to_string(),
            serde_json::Value::String("info".to_string()),
        );
        extra.insert("compactMetadata".to_string(), compact_metadata);

        let jmsg = session::JsonlMessage {
            json_projection: None,
            message_type: "system".to_string(),
            uuid: marker.id().as_uuid().to_string(),
            parent_uuid: None,
            session_id: session_id_str.clone(),
            timestamp: chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
            cwd: self.current_cwd().to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            // Boundary lines carry NO inner `message` (the schema's
            // compact-boundary arm skips the field entirely).
            message: serde_json::Value::Null,
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch,
            entrypoint: Some(entrypoint_value()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: logical_parent,
            extra,
        };
        let uuid_for_chain = jmsg.uuid.clone();
        match writer.append(&jmsg).await {
            Ok(()) => {
                *self.transcript.last_jsonl_uuid.lock().await = Some(uuid_for_chain.clone());
                telemetry::emit_session_appended(&session_id_str, &uuid_for_chain);
            }
            Err(e) => {
                self.record_transcript_append_failure(&session_id_str, "compact_boundary", &e)
                    .await;
            }
        }
    }

    /// Fire the `PreCompact` lifecycle hooks immediately BEFORE a compaction
    /// pass runs (hooks compaction lifecycle, TS `executePreCompactHooks` called
    /// from `services/compact/compact.ts:413` BEFORE the summary request).
    ///
    /// `trigger` is the `auto` / `manual` discriminator carried verbatim into
    /// the wire payload's `trigger` field (TS `compactData.trigger`): `auto` for
    /// the proactive pre-call autocompact and the reactive 413/PTL fallback,
    /// `manual` for an explicit `/compact`. Strict no-op when no `PreCompact`
    /// hook is registered.
    ///
    /// Returns both a possible blocking reason and the successful hooks' stdout.
    /// A block ABORTS the pass in every path (TS `VJn` throws
    /// `"Compaction blocked by PreCompact hook: <blockedBy>"` on the manual
    /// route; the proactive / reactive routes log and skip). The returned
    /// string is the port's aggregate `reason` (the closest equivalent of TS's
    /// `[cmd]: output` join); callers own the surfacing so the log wording
    /// matches each route (manual / `Precomputed` / `Reactive`).
    ///
    /// `custom_instructions` is the caller's current `/compact <focus>` text and
    /// is exposed in the hook payload. Exit-code-0 stdout is returned in hook
    /// order and appended to the same summary prompt.
    pub(crate) async fn fire_pre_compact(
        &self,
        trigger: &str,
        custom_instructions: Option<&str>,
    ) -> PreCompactHookOutcome {
        let ctx = self.lifecycle_hook_ctx(false).await;
        let agg = self
            .hooks
            .execute(
                HookEvent::PreCompact {
                    reason: trigger.to_string(),
                    custom_instructions: custom_instructions.map(str::to_owned),
                },
                ctx,
            )
            .await;
        let blocked_by = matches!(agg.decision, Some(hooks::response::HookDecision::Block))
            .then(|| agg.reason.clone().unwrap_or_default());
        let stdout: Vec<&str> = agg
            .all_results
            .iter()
            .filter(|(_, result)| result.outcome == hooks::response::HookOutcome::Success)
            .map(|(_, result)| compaction::prompt::trim_compact_text(&result.stdout))
            .filter(|text| !text.is_empty())
            .collect();
        PreCompactHookOutcome {
            blocked_by,
            additional_instructions: (!stdout.is_empty()).then(|| stdout.join("\n\n")),
        }
    }

    /// Fire the `PostCompact` lifecycle hooks immediately AFTER a compaction pass
    /// has been applied to the live session (hooks compaction lifecycle, TS
    /// `executePostCompactHooks` called from `services/compact/compact.ts:723`
    /// AFTER the summary is produced).
    ///
    /// `summary` carries the compaction summary text (TS `compactData.compactSummary`
    /// = `getAssistantMessageText(summaryResponse)`); `tokens_freed` is the
    /// approximate reclaimed-token count. Best-effort, exactly like
    /// [`Self::fire_pre_compact`]: the aggregate is discarded so a failing
    /// `PostCompact` hook never breaks the turn. Strict no-op when no
    /// `PostCompact` hook is registered.
    /// Derive the `PostCompact` `summary` payload text from a completed
    /// compaction pass. Mirrors TS `compactData.compactSummary =
    /// getAssistantMessageText(summaryResponse)`: the autocompact layer replaces
    /// history with the summarizer's output message(s)
    /// ([`compaction::IterationCompactionResult::messages`] ==
    /// `summary_messages`), so concatenating their text reconstitutes the
    /// summary the model produced. Empty when the pass produced no text.
    pub(crate) fn compaction_summary_text(
        result: &compaction::IterationCompactionResult,
    ) -> String {
        result
            .messages
            .iter()
            .map(lingxi_core::types::ConversationMessage::text_content)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) async fn fire_post_compact(
        &self,
        trigger: &str,
        summary: String,
        tokens_freed: u64,
    ) {
        // `trigger` (`manual` / `auto`) is threaded onto the `PostCompact`
        // event so it becomes the TS `matchQuery` (claude `getMatchingHooks`
        // `i = r.trigger`) AND rides the wire payload — a hook matcher of
        // `"manual"` / `"auto"` now filters correctly.
        let ctx = self.lifecycle_hook_ctx(false).await;
        let _ = self
            .hooks
            .execute(
                HookEvent::PostCompact {
                    summary,
                    tokens_freed,
                    trigger: trigger.to_string(),
                },
                ctx,
            )
            .await;
    }
}

#[cfg(test)]
mod session_compact_mod_tests {
    use super::*;

    fn empty_compaction_result() -> compaction::IterationCompactionResult {
        compaction::IterationCompactionResult {
            messages: Vec::new(),
            raw_summary_text: String::new(),
            layers_applied: Vec::new(),
            total_tokens_freed: 0,
            cache_hit: false,
            consecutive_failures: 0,
            was_compacted: true,
            rapid_refill_breaker_tripped: false,
            consecutive_rapid_refills: 0,
            messages_to_preserve: Vec::new(),
            media_analysis_to_preserve: Vec::new(),
            compaction_usage: None,
            compaction_model: None,
            compaction_profile: None,
        }
    }

    #[test]
    fn native_token_counts_allow_fractional_optional_values_but_reject_negative() {
        assert_eq!(
            parse_native_session_compact_number(
                &serde_json::json!({"tokensBefore": 1.5}),
                "tokensBefore"
            )
            .unwrap(),
            Some(1.5)
        );
        assert_eq!(
            parse_native_session_compact_number(&serde_json::json!({}), "tokensBefore").unwrap(),
            None
        );
        assert!(
            parse_native_session_compact_number(
                &serde_json::json!({"tokensBefore": -0.5}),
                "tokensBefore"
            )
            .is_err()
        );
        assert!(
            parse_native_session_compact_usage(&serde_json::json!({
                "usage": {
                    "input_tokens": 0.5,
                    "output_tokens": 1,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                }
            }))
            .is_ok()
        );
        assert!(
            parse_native_session_compact_usage(&serde_json::json!({
                "usage": {
                    "input_tokens": -0.5,
                    "output_tokens": 1,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                }
            }))
            .is_err()
        );
    }

    #[test]
    fn replacement_freed_tokens_use_source_and_replacement_estimates() {
        let source = vec![ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "a long original conversation row".into(),
        )];
        let replacement = vec![ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "short".into(),
        )];
        let expected = compaction::grouping::estimate_tokens_for_range(&source).saturating_sub(
            compaction::grouping::estimate_tokens_for_range(&replacement),
        );
        let mut result = empty_compaction_result();

        apply_session_compact_result(
            &mut result,
            &SessionCompactEventResult {
                messages: replacement,
                tokens_before: Some(999.5),
                tokens_after: Some(0.25),
                usage: None,
            },
            &source,
        );

        assert_eq!(result.total_tokens_freed, expected);
    }

    #[test]
    fn replacement_keeps_core_usage_and_direct_replacement_does_not_charge_mod_usage() {
        let source = vec![ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "original message".into(),
        )];
        let replacement = vec![ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "replacement message".into(),
        )];
        let core_usage = cost::Usage {
            tokens: cost::TokenUsage {
                input: 13,
                output: 5,
                cache_read: 2,
                cache_write: 1,
                reasoning_output: 0,
                cache_write_1h: 0,
            },
            server_tool_use: None,
            speed: None,
        };
        let hook_usage = serde_json::json!({
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 20,
            "cache_creation_input_tokens": 10,
        });
        let event_result = SessionCompactEventResult {
            messages: replacement,
            tokens_before: Some(900.0),
            tokens_after: Some(1.0),
            usage: Some(hook_usage),
        };

        let mut core_ran = empty_compaction_result();
        core_ran.compaction_usage = Some(core_usage);
        apply_session_compact_result(&mut core_ran, &event_result, &source);
        assert_eq!(core_ran.compaction_usage, Some(core_usage));

        let mut direct = empty_compaction_result();
        apply_session_compact_result(&mut direct, &event_result, &source);
        assert_eq!(direct.compaction_usage, None);
    }

    #[test]
    fn native_row_codec_roundtrips_tool_messages_with_following_result_details() {
        let tool_use_id = lingxi_core::types::ToolUseId::from("toolu_row_codec_test");
        let messages = vec![
            ConversationMessage::Assistant { per_turn_effort: None,
                id: lingxi_core::types::MessageId::new(),
                content: vec![
                    lingxi_core::types::ContentBlock::Text {
                        text: "reading a file".into(),
                        citations: None,
                    },
                    lingxi_core::types::ContentBlock::ToolUse {
                        input_projection: None,
                        id: tool_use_id.clone(),
                        name: "Read".into(),
                        input: serde_json::json!({"file_path":"/tmp/example"}),
                        provider_id: Some(tool_use_id.to_string()),
                    },
                ],
                stop_reason: Some("tool_use".into()),
            },
            ConversationMessage::User { api_message_override: None,
                id: lingxi_core::types::MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::ToolResult {
                    content_projection: None,
                    tool_use_id: tool_use_id.clone(),
                    content: "file contents".into(),
                    is_error: Some(true),
                    provider_tool_use_id: Some(tool_use_id.to_string()),
                    content_blocks: None,
                }],
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
            },
        ];
        let mut codec = SessionCompactRowCodec::default();

        let rows = codec.rows_for_messages(&messages);
        assert_eq!(rows[0]["toolUses"][0]["text"], "file contents");
        assert_eq!(rows[0]["toolUses"][0]["isError"], true);
        assert_eq!(rows[1]["toolResults"][0]["isError"], true);
        assert_eq!(codec.messages_of_rows(&rows).unwrap(), messages);
    }
}
