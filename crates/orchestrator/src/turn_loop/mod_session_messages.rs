//! Main-loop transcript view for the Mod `session.messages` operation.

use base64::Engine as _;
use hooks::mods::{ModUtf16StringSidecar, ModUtf16ValueProjection};
use lingxi_core::types::{ContentBlock, ConversationMessage};
use llm_runtime::history::ContentBlock as ModelBlock;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;

const MAX_SESSION_MESSAGES: usize = 4096;

struct ToolOutcome {
    text: String,
    is_error: bool,
}

fn text_of(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => Some(text),
        _ => None,
    }
}

fn exact_text_of(blocks: &[ContentBlock]) -> (String, Option<Vec<u16>>) {
    let mut code_units = Vec::new();
    let mut has_exact_units = false;
    for block in blocks {
        match block {
            ContentBlock::Text { text, .. } => code_units.extend(text.encode_utf16()),
            ContentBlock::TextJsUtf16 {
                utf16_code_units, ..
            } => {
                has_exact_units = true;
                code_units.extend(utf16_code_units);
            }
            _ => {}
        }
    }
    if !has_exact_units {
        return (
            blocks.iter().filter_map(text_of).collect::<String>(),
            None,
        );
    }
    let text = String::from_utf16_lossy(&code_units);
    let sidecar = String::from_utf16(&code_units)
        .is_err()
        .then_some(code_units);
    (text, sidecar)
}

pub(super) fn summarize_projection(history: &[ConversationMessage]) -> ModUtf16ValueProjection {
    // Read backwards so a tool_result is known when its earlier tool_use is
    // turned into the public summary. Claude Code applies the same 4096-row
    // cap after skipping system and synthetic user entries.
    let mut outcomes = HashMap::<String, ToolOutcome>::new();
    let mut rows = Vec::new();
    let mut reversed_sidecars = Vec::<(usize, Vec<u16>)>::new();
    for message in history.iter().rev() {
        if rows.len() == MAX_SESSION_MESSAGES {
            break;
        }
        match message {
            ConversationMessage::System { .. } => {}
            ConversationMessage::User {
                content, is_meta, ..
            } if !is_meta => {
                let (text, exact_units) = exact_text_of(content);
                if let Some(exact_units) = exact_units {
                    reversed_sidecars.push((rows.len(), exact_units));
                }
                let mut tool_results = Vec::new();
                for block in content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        provider_tool_use_id,
                        ..
                    } = block
                    {
                        let id = provider_tool_use_id
                            .clone()
                            .unwrap_or_else(|| tool_use_id.to_string());
                        outcomes.insert(
                            id.clone(),
                            ToolOutcome {
                                text: content.clone(),
                                is_error: is_error.unwrap_or(false),
                            },
                        );
                        tool_results.push(json!({
                            "tool_use_id": id,
                            "text": content,
                            "isError": is_error.unwrap_or(false),
                        }));
                    }
                }
                let mut row = json!({"role":"user","text":text,"toolUses":[]});
                if !tool_results.is_empty() {
                    row["toolResults"] = Value::Array(tool_results);
                }
                rows.push(row);
            }
            ConversationMessage::User { .. } => {}
            ConversationMessage::Assistant { content, .. } => {
                let (text, exact_units) = exact_text_of(content);
                if let Some(exact_units) = exact_units {
                    reversed_sidecars.push((rows.len(), exact_units));
                }
                let mut tool_uses = Vec::new();
                for block in content {
                    if let ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        provider_id,
                     .. } = block
                    {
                        let tool_id = provider_id.clone().unwrap_or_else(|| id.to_string());
                        let mut call = json!({
                            "tool_use_id": tool_id,
                            "tool": name,
                            "input": input,
                        });
                        if let Some(outcome) = outcomes.get(&tool_id) {
                            call["text"] = Value::String(outcome.text.clone());
                            if outcome.is_error {
                                call["isError"] = Value::Bool(true);
                            }
                        }
                        tool_uses.push(call);
                    }
                }
                rows.push(json!({"role":"assistant","text":text,"toolUses":tool_uses}));
            }
        }
    }
    rows.reverse();
    let row_count = rows.len();
    let strings = reversed_sidecars
        .into_iter()
        .map(|(reverse_index, code_units)| ModUtf16StringSidecar {
            pointer: format!("/{}/text", row_count - reverse_index - 1),
            code_units,
        })
        .collect();
    ModUtf16ValueProjection {
        value: Value::Array(rows),
        strings,
        keys: Vec::new(),
    }
}

fn api_block(
    block: ModelBlock,
    pointer: &str,
    strings: &mut Vec<ModUtf16StringSidecar>,
) -> Result<Value, String> {
    Ok(match block {
        ModelBlock::ProviderContent { protocol, value } => {
            if protocol != "anthropic_messages" {
                return Err(format!(
                    "session.messages as api cannot project {protocol} provider content into Claude messages"
                ));
            }
            value
        }
        ModelBlock::Text { text, citations, .. } => {
            let mut block = json!({"type":"text","text":text});
            if let Some(citations) = citations {
                block["citations"] = citations.unwrap_or(Value::Null);
            }
            block
        }
        ModelBlock::TextJsUtf16 {
            utf16_code_units,
            citations,
            ..
        } => {
            let text = String::from_utf16_lossy(&utf16_code_units);
            if String::from_utf16(&utf16_code_units).is_err() {
                strings.push(ModUtf16StringSidecar {
                    pointer: pointer.to_owned(),
                    code_units: utf16_code_units,
                });
            }
            let mut block = json!({"type":"text","text":text});
            if let Some(citations) = citations {
                block["citations"] = citations.unwrap_or(Value::Null);
            }
            block
        }
        ModelBlock::Image { media_type, bytes } => json!({
            "type":"image",
            "source":{"type":"base64","media_type":media_type,
                "data":base64::engine::general_purpose::STANDARD.encode(bytes)}
        }),
        ModelBlock::ImageUrl { url } => {
            json!({"type":"image","source":{"type":"url","url":url}})
        }
        ModelBlock::Document { media_type, bytes } => json!({
            "type":"document",
            "source":{"type":"base64","media_type":media_type,
                "data":base64::engine::general_purpose::STANDARD.encode(bytes)}
        }),
        ModelBlock::ToolCall { id, name, input , .. } => {
            json!({"type":"tool_use","id":id,"name":name,"input":input})
        }
        ModelBlock::ToolResult {
            tool_call_id,
            output,
            is_error,
            ..
        } => {
            let content = if output.is_string() || output.is_array() {
                output
            } else {
                Value::String(output.to_string())
            };
            let mut block = json!({"type":"tool_result","tool_use_id":tool_call_id,
                "content":content});
            if let Some(is_error) = is_error {
                block["is_error"] = Value::Bool(is_error);
            }
            block
        }
        ModelBlock::Reasoning { text, signature } => {
            let mut block = json!({"type":"thinking","thinking":text});
            if let Some(signature) = signature {
                block["signature"] = Value::String(signature);
            }
            block
        }
        ModelBlock::RedactedThinking { data } => {
            json!({"type":"redacted_thinking","data":data})
        }
        ModelBlock::ServerToolUse { id, name, input } => {
            json!({"type":"server_tool_use","id":id,"name":name,"input":input})
        }
        ModelBlock::ConnectorText {
            connector_text,
            signature,
        } => {
            let mut block = json!({"type":"connector_text","connector_text":connector_text});
            if let Some(signature) = signature {
                block["signature"] = Value::String(signature);
            }
            block
        }
        ModelBlock::AdvisorToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({"type":"advisor_tool_result","tool_use_id":tool_use_id,
            "content":content,"is_error":is_error}),
        ModelBlock::CacheEdits { edits } => json!({"type":"cache_edits","edits":edits}),
    })
}

/// The Claude API-shaped request history, including meta messages that the
/// summary view intentionally excludes. The request normalizer merges adjacent
/// role messages before the upstream 4096-row cut is applied.
pub(super) fn api_projection(
    history: Vec<ConversationMessage>,
) -> Result<ModUtf16ValueProjection, String> {
    let normalized = llm_runtime::convert::normalize_messages_for_api(history);
    let messages =
        llm_runtime::convert::to_llm_messages(normalized).map_err(|error| error.to_string())?;
    let mut rows = Vec::with_capacity(messages.len());
    let mut strings = Vec::new();
    for (row_index, message) in messages.into_iter().enumerate() {
        let content = message
            .content
            .into_iter()
            .enumerate()
            .map(|(block_index, block)| {
                api_block(
                    block,
                    &format!("/{row_index}/content/{block_index}/text"),
                    &mut strings,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        rows.push(json!({"role":message.role,"content":content}));
    }
    if rows.len() > MAX_SESSION_MESSAGES {
        let start = rows.len() - MAX_SESSION_MESSAGES;
        let first_complete_user = rows[start..]
            .iter()
            .position(|row| {
                row["role"] == "user"
                    && !row["content"].as_array().is_some_and(|content| {
                        content.iter().any(|block| block["type"] == "tool_result")
                    })
            })
            .unwrap_or(0);
        let removed_rows = start + first_complete_user;
        rows.drain(..removed_rows);
        strings.retain_mut(|sidecar| {
            let Some((row, rest)) = sidecar.pointer.strip_prefix('/').and_then(|path| path.split_once('/')) else {
                return false;
            };
            let Some(row) = row.parse::<usize>().ok() else {
                return false;
            };
            if row < removed_rows {
                return false;
            }
            sidecar.pointer = format!("/{}/{}", row - removed_rows, rest);
            true
        });
    }
    Ok(ModUtf16ValueProjection {
        value: Value::Array(rows),
        strings,
        keys: Vec::new(),
    })
}

/// `toolUseResult` is an on-disk transcript field rather than a model content
/// block. Read only the requested ids, without loading a long JSONL file into
/// memory, after the Mod result stage has committed the final tool outcome.
pub(super) async fn hydrate_results(path: &Path, rows: &mut Value) {
    let Some(array) = rows.as_array() else { return };
    let needed = array
        .iter()
        .filter_map(|row| row.get("toolResults").and_then(Value::as_array))
        .flatten()
        .filter_map(|result| result.get("tool_use_id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<HashSet<_>>();
    if needed.is_empty() || path.as_os_str().is_empty() {
        return;
    }
    let path = path.to_path_buf();
    let Ok(results) = tokio::task::spawn_blocking(move || {
        let mut found = HashMap::<String, Value>::new();
        let Ok(file) = std::fs::File::open(path) else {
            return found;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if !line.contains("\"toolUseResult\"") {
                continue;
            }
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(raw) = record.get("toolUseResult") else {
                continue;
            };
            let Some(content) = record
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            let mut results = content
                .iter()
                .filter(|block| block["type"] == "tool_result");
            let Some(block) = results.next() else {
                continue;
            };
            if results.next().is_some() {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            if needed.contains(id) {
                found.insert(id.to_owned(), raw.clone());
            }
        }
        found
    })
    .await
    else {
        return;
    };
    let Some(array) = rows.as_array_mut() else {
        return;
    };
    for row in array {
        for field in ["toolResults", "toolUses"] {
            let Some(entries) = row.get_mut(field).and_then(Value::as_array_mut) else {
                continue;
            };
            for entry in entries {
                if let Some(raw) = entry
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .and_then(|id| results.get(id))
                {
                    entry["result"] = raw.clone();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{ImageSource, MessageId, ToolUseId};

    fn user(content: Vec<ContentBlock>, is_meta: bool) -> ConversationMessage {
        ConversationMessage::User { api_message_override: None,
            id: MessageId::new(),
            content,
            is_meta,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }
    }

    #[test]
    fn joins_text_skips_meta_and_pairs_tool_outcomes() {
        let id = ToolUseId::from("toolu_example");
        let history = vec![
            user(
                vec![ContentBlock::Text {
                    text: "question".into(),
                    citations: None,
                }],
                false,
            ),
            user(
                vec![ContentBlock::Text {
                    text: "hidden".into(),
                    citations: None,
                }],
                true,
            ),
            ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                content: vec![
                    ContentBlock::Text {
                        text: "before".into(),
                        citations: None,
                    },
                    ContentBlock::ToolUse { input_projection: None,
                        id: id.clone(),
                        name: "Read".into(),
                        input: json!({"file_path":"a.txt"}),
                        provider_id: Some("toolu_example".into()),
                    },
                    ContentBlock::Text {
                        text: "after".into(),
                        citations: None,
                    },
                ],
                stop_reason: None,
            },
            user(
                vec![ContentBlock::ToolResult { content_projection: None,
                    tool_use_id: id,
                    content: "not found".into(),
                    is_error: Some(true),
                    provider_tool_use_id: Some("toolu_example".into()),
                    content_blocks: None,
                }],
                false,
            ),
        ];
        let result = summarize_projection(&history).value;
        assert_eq!(result.as_array().unwrap().len(), 3);
        assert_eq!(
            result[0],
            json!({"role":"user","text":"question","toolUses":[]})
        );
        assert_eq!(result[1]["text"], "beforeafter");
        assert_eq!(
            result[1]["toolUses"][0],
            json!({
                "tool_use_id":"toolu_example",
                "tool":"Read",
                "input":{"file_path":"a.txt"},
                "text":"not found",
                "isError":true,
            })
        );
        assert_eq!(
            result[2]["toolResults"][0],
            json!({
                "tool_use_id":"toolu_example",
                "text":"not found",
                "isError":true,
            })
        );
    }

    #[test]
    fn api_view_keeps_request_blocks_and_meta_content() {
        let id = ToolUseId::from("toolu_api_view");
        let history = vec![
            user(
                vec![ContentBlock::Text {
                    text: "hello".into(),
                    citations: None,
                }],
                false,
            ),
            user(
                vec![ContentBlock::Text {
                    text: "hidden".into(),
                    citations: None,
                }],
                true,
            ),
            ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "reason".into(),
                        signature: Some("signed".into()),
                    },
                    ContentBlock::ToolUse { input_projection: None,
                        id: id.clone(),
                        name: "Read".into(),
                        input: json!({"file_path":"a.txt"}),
                        provider_id: Some("toolu_api_view".into()),
                    },
                ],
                stop_reason: None,
            },
            user(
                vec![ContentBlock::ToolResult { content_projection: None,
                    tool_use_id: id,
                    content: "found".into(),
                    is_error: Some(false),
                    provider_tool_use_id: Some("toolu_api_view".into()),
                    content_blocks: None,
                }],
                false,
            ),
        ];
        let rows = api_projection(history).unwrap().value;
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["role"], "user");
        assert_eq!(
            rows[0]["content"][0],
            json!({"type":"text","text":"hello\n"})
        );
        assert_eq!(
            rows[0]["content"][1],
            json!({"type":"text","text":"hidden"})
        );
        assert_eq!(
            rows[1]["content"][0],
            json!({"type":"thinking","thinking":"reason","signature":"signed"})
        );
        assert_eq!(
            rows[1]["content"][1],
            json!({"type":"tool_use","id":"toolu_api_view","name":"Read","input":{"file_path":"a.txt"}})
        );
        assert_eq!(
            rows[2]["content"][0],
            json!({"type":"tool_result","tool_use_id":"toolu_api_view","content":"found","is_error":false})
        );
    }

    #[test]
    fn api_view_keeps_media_and_structured_tool_result_blocks() {
        let id = ToolUseId::from("toolu_media");
        let history = vec![
            user(
                vec![ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AQID".into(),
                    },
                }],
                false,
            ),
            ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                content: vec![ContentBlock::ToolUse { input_projection: None,
                    id: id.clone(),
                    name: "View".into(),
                    input: json!({}),
                    provider_id: None,
                }],
                stop_reason: None,
            },
            user(
                vec![ContentBlock::ToolResult { content_projection: None,
                    tool_use_id: id,
                    content: "image".into(),
                    is_error: Some(true),
                    provider_tool_use_id: None,
                    content_blocks: Some(vec![
                        json!({"type":"text","text":"caption"}),
                        json!({"type":"image","source":{"type":"base64",
                        "media_type":"image/png","data":"AQID"}}),
                    ]),
                }],
                false,
            ),
        ];
        let rows = api_projection(history).unwrap().value;
        assert_eq!(
            rows[0]["content"][0],
            json!({"type":"image","source":{
            "type":"base64","media_type":"image/png","data":"AQID"}})
        );
        assert_eq!(
            rows[2]["content"][0],
            json!({"type":"tool_result",
            "tool_use_id":"toolu_media","is_error":true,"content":[
                {"type":"text","text":"caption"},
                {"type":"image","source":{"type":"base64",
                    "media_type":"image/png","data":"AQID"}}
            ]})
        );
    }

    #[test]
    fn api_view_cuts_at_next_complete_user_after_the_4096_row_window() {
        let id = ToolUseId::from("toolu_cut");
        let mut history = vec![
            user(
                vec![ContentBlock::Text {
                    text: "old".into(),
                    citations: None,
                }],
                false,
            ),
            ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                content: vec![ContentBlock::ToolUse { input_projection: None,
                    id: id.clone(),
                    name: "Read".into(),
                    input: json!({}),
                    provider_id: Some("toolu_cut".into()),
                }],
                stop_reason: None,
            },
            user(
                vec![ContentBlock::ToolResult { content_projection: None,
                    tool_use_id: id,
                    content: "result".into(),
                    is_error: Some(false),
                    provider_tool_use_id: Some("toolu_cut".into()),
                    content_blocks: None,
                }],
                false,
            ),
            ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                    citations: None,
                }],
                stop_reason: None,
            },
            user(
                vec![ContentBlock::Text {
                    text: "fresh".into(),
                    citations: None,
                }],
                false,
            ),
        ];
        while history.len() < MAX_SESSION_MESSAGES + 2 {
            let assistant = history.len() % 2 == 1;
            let content = vec![ContentBlock::Text {
                text: "filler".into(),
                citations: None,
            }];
            if assistant {
                history.push(ConversationMessage::Assistant { per_turn_effort: None,
                    id: MessageId::new(),
                    content,
                    stop_reason: None,
                });
            } else {
                history.push(user(content, false));
            }
        }
        let rows = api_projection(history).unwrap().value;
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), MAX_SESSION_MESSAGES - 2);
        assert_eq!(rows[0]["role"], "user");
        assert_eq!(rows[0]["content"][0]["text"], "fresh");
    }

    #[test]
    fn keeps_only_newest_4096_visible_messages() {
        let history = (0..4097)
            .map(|n| {
                user(
                    vec![ContentBlock::Text {
                        text: n.to_string(),
                        citations: None,
                    }],
                    false,
                )
            })
            .collect::<Vec<_>>();
        let result = summarize_projection(&history).value;
        assert_eq!(result.as_array().unwrap().len(), 4096);
        assert_eq!(result[0]["text"], "1");
        assert_eq!(result[4095]["text"], "4096");
    }

    #[test]
    fn compact_summary_marked_transcript_only_is_still_a_message() {
        let mut summary = user(
            vec![ContentBlock::Text {
                text: "summary".into(),
                citations: None,
            }],
            false,
        );
        if let ConversationMessage::User {
            is_compact_summary,
            is_visible_in_transcript_only,
            ..
        } = &mut summary
        {
            *is_compact_summary = true;
            *is_visible_in_transcript_only = true;
        }
        assert_eq!(summarize_projection(&[summary]).value[0]["text"], "summary");
    }

    #[tokio::test]
    async fn reads_committed_raw_tool_result_from_jsonl() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        let id = ToolUseId::from("toolu_example");
        let history = [
            ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                content: vec![ContentBlock::ToolUse { input_projection: None,
                    id: id.clone(),
                    name: "Read".into(),
                    input: json!({}),
                    provider_id: None,
                }],
                stop_reason: None,
            },
            user(
                vec![ContentBlock::ToolResult { content_projection: None,
                    tool_use_id: id,
                    content: "file text".into(),
                    is_error: Some(false),
                    provider_tool_use_id: None,
                    content_blocks: None,
                }],
                false,
            ),
        ];
        let line = json!({
            "type":"user",
            "message":{"content":[{"type":"tool_result","tool_use_id":"toolu_example","content":"file text"}]},
            "toolUseResult":{"file":{"content":"file text"}},
        });
        std::fs::write(&path, format!("{line}\n")).unwrap();
        let mut rows = summarize_projection(&history).value;
        hydrate_results(&path, &mut rows).await;
        assert_eq!(
            rows[0]["toolUses"][0]["result"],
            json!({"file":{"content":"file text"}})
        );
        assert_eq!(
            rows[1]["toolResults"][0]["result"],
            json!({"file":{"content":"file text"}})
        );
    }
}
