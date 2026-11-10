//! Main-loop transcript view for the Mod `session.messages` operation.

use base64::Engine as _;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ConversationMessage};
use llm_runtime::history::ContentBlock as ModelBlock;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

const MAX_SESSION_MESSAGES: usize = 4096;

struct ToolOutcome {
    text: Utf16JsonProjection,
    is_error: bool,
}

fn projection_object(fields: Vec<(String, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(fields).expect("session.messages object is valid JSON")
}

fn projection_array(values: Vec<Utf16JsonProjection>) -> Utf16JsonProjection {
    Utf16JsonProjection::array(values).expect("session.messages array is valid JSON")
}

fn project_provider_payload(
    value: lingxi_llm_client::protocol::JsonPayload,
) -> Result<Utf16JsonProjection, String> {
    Utf16JsonProjection::parse(value.as_json_source()).map_err(|error| error.to_string())
}

fn projection_string(value: impl Into<String>) -> Utf16JsonProjection {
    value.into().into()
}

fn projection_value(value: &Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(value).expect("generated message JSON is valid")
}

#[cfg(test)]
fn projection_to_value(projection: &Utf16JsonProjection) -> Value {
    serde_json::from_str(
        &projection
            .to_json_string()
            .expect("session.messages projection stringifies"),
    )
    .expect("test assertions use JSON-compatible strings")
}

fn exact_text_projection(blocks: &[ContentBlock]) -> Utf16JsonProjection {
    crate::streaming_loop::joined_visible_text_projection(blocks)
}

pub(super) fn summarize_projection(history: &[ConversationMessage]) -> Utf16JsonProjection {
    // Read backwards so a tool_result is known when its earlier tool_use is
    // turned into the public summary. Claude Code applies the same 4096-row
    // cap after skipping system and synthetic user entries.
    let mut outcomes = HashMap::<String, ToolOutcome>::new();
    let mut rows = Vec::new();
    for message in history.iter().rev() {
        if rows.len() == MAX_SESSION_MESSAGES {
            break;
        }
        match message {
            ConversationMessage::System { .. } => {}
            ConversationMessage::User {
                content, is_meta, ..
            } if !is_meta => {
                let text = exact_text_projection(content);
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
                        let result_text = content.clone();
                        outcomes.insert(
                            id.clone(),
                            ToolOutcome {
                                text: result_text.clone(),
                                is_error: is_error.unwrap_or(false),
                            },
                        );
                        let mut fields = vec![
                            ("tool_use_id".into(), projection_string(id)),
                            ("text".into(), result_text),
                        ];
                        if is_error.unwrap_or(false) {
                            fields.push(("isError".into(), projection_value(&Value::Bool(true))));
                        }
                        tool_results.push(projection_object(fields));
                    }
                }
                let mut fields = vec![
                    ("role".into(), projection_string("user")),
                    ("text".into(), text),
                    ("toolUses".into(), projection_array(Vec::new())),
                ];
                if !tool_results.is_empty() {
                    fields.push(("toolResults".into(), projection_array(tool_results)));
                }
                rows.push(projection_object(fields));
            }
            ConversationMessage::User { .. } => {}
            ConversationMessage::Assistant { content, .. } => {
                let text = exact_text_projection(content);
                let mut tool_uses = Vec::new();
                for block in content {
                    if let ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        provider_id,
                    } = block
                    {
                        let tool_id = provider_id.clone().unwrap_or_else(|| id.to_string());
                        let mut call = projection_object(vec![
                            ("tool_use_id".into(), projection_string(tool_id.clone())),
                            ("tool".into(), projection_string(name.clone())),
                            ("input".into(), input.clone()),
                        ]);
                        if let Some(outcome) = outcomes.get(&tool_id) {
                            call.set_field("text", outcome.text.clone())
                                .expect("tool outcome row is an object");
                            if outcome.is_error {
                                call.set_field("isError", projection_value(&Value::Bool(true)))
                                    .expect("tool outcome row is an object");
                            }
                        }
                        tool_uses.push(call);
                    }
                }
                rows.push(projection_object(vec![
                    ("role".into(), projection_string("assistant")),
                    ("text".into(), text),
                    ("toolUses".into(), projection_array(tool_uses)),
                ]));
            }
        }
    }
    rows.reverse();
    projection_array(rows)
}

fn api_block(block: ModelBlock) -> Result<Utf16JsonProjection, String> {
    let object = |fields| Utf16JsonProjection::object(fields).map_err(|error| error.to_string());
    let string = |value: String| Utf16JsonProjection::from(value);
    let json_value = |value: Value| {
        Utf16JsonProjection::from_serde_value(&value).map_err(|error| error.to_string())
    };
    let mut fields = Vec::<(String, Utf16JsonProjection)>::new();
    let kind = match block {
        ModelBlock::ProviderContent { protocol, value } => {
            if protocol != "anthropic_messages" {
                return Err(format!(
                    "session.messages as api cannot project {protocol} provider content into Claude messages"
                ));
            }
            return project_provider_payload(value);
        }
        ModelBlock::Text {
            text, citations, ..
        } => {
            fields.push(("text".into(), string(text)));
            if let Some(citations) = citations {
                let citations = citations.unwrap_or_else(|| {
                    lingxi_llm_client::protocol::JsonPayload::parse("null")
                        .expect("null is valid JSON")
                });
                fields.push(("citations".into(), project_provider_payload(citations)?));
            }
            "text"
        }
        ModelBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            citations,
            ..
        } => {
            let text = Utf16JsonProjection::root_string(text, utf16_code_units)
                .map_err(|error| error.to_string())?;
            fields.push(("text".into(), text));
            if let Some(citations) = citations {
                let citations = citations.unwrap_or_else(|| {
                    lingxi_llm_client::protocol::JsonPayload::parse("null")
                        .expect("null is valid JSON")
                });
                fields.push(("citations".into(), project_provider_payload(citations)?));
            }
            "text"
        }
        ModelBlock::Image { media_type, bytes } => {
            fields.push((
                "source".into(),
                object(vec![
                    ("type".into(), string("base64".into())),
                    ("media_type".into(), string(media_type)),
                    (
                        "data".into(),
                        string(base64::engine::general_purpose::STANDARD.encode(bytes)),
                    ),
                ])?,
            ));
            "image"
        }
        ModelBlock::ImageUrl { url } => {
            fields.push((
                "source".into(),
                object(vec![
                    ("type".into(), string("url".into())),
                    ("url".into(), string(url)),
                ])?,
            ));
            "image"
        }
        ModelBlock::Document { media_type, bytes } => {
            fields.push((
                "source".into(),
                object(vec![
                    ("type".into(), string("base64".into())),
                    ("media_type".into(), string(media_type)),
                    (
                        "data".into(),
                        string(base64::engine::general_purpose::STANDARD.encode(bytes)),
                    ),
                ])?,
            ));
            "document"
        }
        ModelBlock::ToolCall { id, name, input } => {
            fields.extend([
                ("id".into(), string(id)),
                ("name".into(), string(name)),
                (
                    "input".into(),
                    Utf16JsonProjection::parse(input.as_json_source())
                        .map_err(|error| error.to_string())?,
                ),
            ]);
            "tool_use"
        }
        ModelBlock::ToolResult {
            tool_call_id,
            output,
            is_error,
            ..
        } => {
            let content = if output.is_root_string() || output.is_root_array() {
                Utf16JsonProjection::parse(output.as_json_source())
                    .map_err(|error| error.to_string())?
            } else {
                let source = Utf16JsonProjection::parse(output.as_json_source())
                    .map_err(|error| error.to_string())?
                    .to_json_string()
                    .map_err(|error| error.to_string())?;
                string(source)
            };
            fields.extend([
                ("tool_use_id".into(), string(tool_call_id)),
                ("content".into(), content),
            ]);
            if let Some(is_error) = is_error {
                fields.push(("is_error".into(), json_value(Value::Bool(is_error))?));
            }
            "tool_result"
        }
        ModelBlock::Reasoning { text, signature } => {
            fields.push(("thinking".into(), string(text)));
            if let Some(signature) = signature {
                fields.push(("signature".into(), string(signature)));
            }
            "thinking"
        }
        ModelBlock::RedactedThinking { data } => {
            fields.push(("data".into(), string(data)));
            "redacted_thinking"
        }
        ModelBlock::ServerToolUse { id, name, input } => {
            fields.extend([
                ("id".into(), string(id)),
                ("name".into(), string(name)),
                (
                    "input".into(),
                    Utf16JsonProjection::parse(input.as_json_source())
                        .map_err(|error| error.to_string())?,
                ),
            ]);
            "server_tool_use"
        }
        ModelBlock::ConnectorText {
            connector_text,
            signature,
        } => {
            fields.push(("connector_text".into(), string(connector_text)));
            if let Some(signature) = signature {
                fields.push(("signature".into(), string(signature)));
            }
            "connector_text"
        }
        ModelBlock::AdvisorToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            fields.extend([
                ("tool_use_id".into(), string(tool_use_id)),
                (
                    "content".into(),
                    Utf16JsonProjection::parse(content.as_json_source())
                        .map_err(|error| error.to_string())?,
                ),
                ("is_error".into(), json_value(Value::Bool(is_error))?),
            ]);
            "advisor_tool_result"
        }
        ModelBlock::CacheEdits { edits } => {
            fields.push((
                "edits".into(),
                json_value(serde_json::to_value(edits).map_err(|error| error.to_string())?)?,
            ));
            "cache_edits"
        }
    };
    fields.insert(0, ("type".into(), string(kind.into())));
    object(fields)
}

/// The Claude API-shaped request history, including meta messages that the
/// summary view intentionally excludes. The request normalizer merges adjacent
/// role messages before the upstream 4096-row cut is applied.
pub(super) fn api_projection(
    history: Vec<ConversationMessage>,
) -> Result<Utf16JsonProjection, String> {
    let normalized = llm_runtime::convert::normalize_messages_for_api(history);
    let messages =
        llm_runtime::convert::to_llm_messages(normalized).map_err(|error| error.to_string())?;
    let mut rows = Vec::with_capacity(messages.len());
    for message in messages {
        let content = message
            .content
            .into_iter()
            .map(api_block)
            .collect::<Result<Vec<_>, _>>()?;
        rows.push(projection_object(vec![
            ("role".into(), projection_string(message.role)),
            ("content".into(), projection_array(content)),
        ]));
    }
    if rows.len() > MAX_SESSION_MESSAGES {
        let start = rows.len() - MAX_SESSION_MESSAGES;
        let first_complete_user = rows[start..]
            .iter()
            .position(|row| {
                row.string_at("/role") == Some("user")
                    && !row.array_len("/content").is_some_and(|len| {
                        (0..len).any(|index| {
                            row.string_at(&format!("/content/{index}/type")) == Some("tool_result")
                        })
                    })
            })
            .unwrap_or(0);
        let removed_rows = start + first_complete_user;
        rows.drain(..removed_rows);
    }
    Ok(projection_array(rows))
}

// Cache byte offsets rather than raw tool bodies. Metadata includes inode and
// ctime on Unix so append, truncate, replacement and same-size rewrites retire
// an index. Bound both the number of sessions and indexed tool IDs.
#[derive(Clone, PartialEq, Eq)]
struct ResultFileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

fn result_file_stamp(metadata: &std::fs::Metadata) -> ResultFileStamp {
    ResultFileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        #[cfg(unix)]
        identity: {
            use std::os::unix::fs::MetadataExt as _;
            (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            )
        },
    }
}

struct ResultOffsetIndex {
    stamp: ResultFileStamp,
    offsets: HashMap<Vec<u16>, u64>,
}

type ResultOffsetCache =
    std::collections::VecDeque<(std::path::PathBuf, std::sync::Arc<ResultOffsetIndex>)>;

fn result_offset_cache() -> &'static std::sync::Mutex<ResultOffsetCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<ResultOffsetCache>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn transcript_result_id(record: &Utf16JsonProjection) -> Option<Vec<u16>> {
    if !record.contains_property("toolUseResult") {
        return None;
    }
    let len = record.array_len("/message/content")?;
    let mut result_index = None;
    for index in 0..len {
        if record.string_at(&format!("/message/content/{index}/type")) == Some("tool_result") {
            if result_index.is_some() {
                return None;
            }
            result_index = Some(index);
        }
    }
    record.string_units(&format!("/message/content/{}/tool_use_id", result_index?))
}

fn read_transcript_results(
    path: &Path,
    needed: &std::collections::HashSet<Vec<u16>>,
) -> HashMap<Vec<u16>, Utf16JsonProjection> {
    use std::io::{Seek, SeekFrom};
    let mut found = HashMap::new();
    let Ok(file) = std::fs::File::open(path) else {
        return found;
    };
    let Ok(metadata) = file.metadata() else {
        return found;
    };
    let stamp = result_file_stamp(&metadata);
    let cached = {
        let cache = result_offset_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .iter()
            .find(|(cached_path, index)| cached_path == path && index.stamp == stamp)
            .map(|(_, index)| std::sync::Arc::clone(index))
    };
    let mut reader = BufReader::new(file);
    if let Some(index) = cached {
        let mut valid = true;
        for id in needed {
            let Some(offset) = index.offsets.get(id) else {
                continue;
            };
            let mut line = String::new();
            if reader.seek(SeekFrom::Start(*offset)).is_err()
                || reader.read_line(&mut line).is_err()
            {
                valid = false;
                break;
            }
            let Ok(record) = Utf16JsonProjection::parse(&line) else {
                valid = false;
                break;
            };
            if transcript_result_id(&record).as_ref() != Some(id) {
                valid = false;
                break;
            }
            if let Ok(raw) = record.subprojection("/toolUseResult") {
                found.insert(id.clone(), raw);
            }
        }
        if valid && std::fs::metadata(path).is_ok_and(|meta| result_file_stamp(&meta) == stamp) {
            return found;
        }
        found.clear();
    }
    if reader.rewind().is_err() {
        return found;
    }
    let mut offsets = HashMap::new();
    let mut indexed_units = 0usize;
    let mut complete = true;
    let mut position = 0;
    let mut line = String::new();
    loop {
        line.clear();
        let Ok(count) = reader.read_line(&mut line) else {
            complete = false;
            break;
        };
        if count == 0 {
            break;
        }
        let offset = position;
        position += count as u64;
        if !line.contains("\"toolUseResult\"") {
            continue;
        }
        let Ok(record) = Utf16JsonProjection::parse(&line) else {
            continue;
        };
        let Some(id) = transcript_result_id(&record) else {
            continue;
        };
        let already_indexed = offsets.contains_key(&id);
        if already_indexed
            || (offsets.len() < 32_768 && indexed_units.saturating_add(id.len()) <= 1_048_576)
        {
            if !already_indexed {
                indexed_units += id.len();
            }
            offsets.insert(id.clone(), offset);
        } else {
            complete = false;
        }
        if needed.contains(&id) {
            if let Ok(raw) = record.subprojection("/toolUseResult") {
                found.insert(id, raw);
            }
        }
    }
    if complete
        && position == stamp.len
        && std::fs::metadata(path).is_ok_and(|meta| result_file_stamp(&meta) == stamp)
    {
        let mut cache = result_offset_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|(cached_path, _)| cached_path != path);
        cache.push_back((
            path.to_owned(),
            std::sync::Arc::new(ResultOffsetIndex {
                stamp,
                offsets,
            }),
        ));
        while cache.len() > 4 {
            cache.pop_front();
        }
    }
    found
}

/// `toolUseResult` is an on-disk transcript field rather than a model content
/// block. Read only the requested ids, without loading a long JSONL file into
/// memory, after the Mod result stage has committed the final tool outcome.
pub(super) async fn hydrate_results(path: &Path, rows: &mut Utf16JsonProjection) {
    let Some(row_count) = rows.array_len("") else {
        return;
    };
    let mut needed = std::collections::HashSet::<Vec<u16>>::new();
    for row_index in 0..row_count {
        for field in ["toolResults", "toolUses"] {
            let pointer = format!("/{row_index}/{field}");
            for index in 0..rows.array_len(&pointer).unwrap_or_default() {
                if let Some(id) = rows.string_units(&format!("{pointer}/{index}/tool_use_id")) {
                    needed.insert(id);
                }
            }
        }
    }
    if needed.is_empty() || path.as_os_str().is_empty() {
        return;
    }
    let path = path.to_path_buf();
    let Ok(results) =
        tokio::task::spawn_blocking(move || read_transcript_results(&path, &needed)).await
    else {
        return;
    };
    let mut hydrated_rows = Vec::with_capacity(row_count);
    for row_index in 0..row_count {
        let Ok(mut row) = rows.subprojection(&format!("/{row_index}")) else {
            continue;
        };
        for field in ["toolResults", "toolUses"] {
            let Some(entry_count) = row.array_len(&format!("/{field}")) else {
                continue;
            };
            let mut entries = Vec::with_capacity(entry_count);
            for index in 0..entry_count {
                let Ok(mut entry) = row.subprojection(&format!("/{field}/{index}")) else {
                    continue;
                };
                if let Some(raw) = row
                    .string_units(&format!("/{field}/{index}/tool_use_id"))
                    .and_then(|id| results.get(&id))
                {
                    if entry.set_field("result", raw.clone()).is_err() {
                        continue;
                    }
                }
                entries.push(entry);
            }
            if row.set_field(field, projection_array(entries)).is_err() {
                return;
            }
        }
        hydrated_rows.push(row);
    }
    *rows = projection_array(hydrated_rows);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{ImageSource, MessageId, ToolUseId};
    use serde_json::json;

    fn user(content: Vec<ContentBlock>, is_meta: bool) -> ConversationMessage {
        ConversationMessage::User {
            id: MessageId::new(),
            content,
            is_meta,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
        }
    }

    #[tokio::test]
    async fn default_mod_summary_keeps_opaque_visible_text_and_exact_utf16() {
        use crate::conversation::ConversationOrchestrator;
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use crate::OrchestratorConfig;
        use std::sync::Arc;
        use tool_api::registry::ToolRegistry;

        let cwd = tempfile::tempdir().unwrap();
        let orchestrator = ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.path().to_path_buf(),
        );
        let opaque = ContentBlock::ProviderContent {
            protocol: "anthropic_messages".into(),
            value: Utf16JsonProjection::parse(
                r#"{"type":"text","text":"opaque\ud800","native_field":true}"#,
            )
            .unwrap(),
        };
        let source_units = vec![u16::from(b'!'), 0xdc00];
        let content = vec![
            ContentBlock::Text {
                text: "before".into(),
                citations: None,
            },
            opaque.clone(),
            ContentBlock::Thinking {
                thinking: "private reasoning".into(),
                signature: None,
            },
            ContentBlock::TextJsUtf16 {
                text: String::from_utf16_lossy(&source_units),
                utf16_code_units: source_units.clone(),
                citations: None,
            },
        ];
        let history = vec![
            user(vec![opaque], false),
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content,
                stop_reason: None,
                resumable: None,
                per_turn_effort: None,
            },
        ];
        orchestrator.session.lock().await.history = history.clone();
        let summary = hooks::mods::ModSessionContext::messages(&orchestrator, None)
            .await
            .unwrap();
        summary.validate().unwrap();
        assert_eq!(summary.array_len(""), Some(2));
        let mut opaque_units = "opaque".encode_utf16().collect::<Vec<_>>();
        opaque_units.push(0xd800);
        assert_eq!(summary.string_at("/0/role"), Some("user"));
        assert_eq!(summary.string_units("/0/text"), Some(opaque_units.clone()));
        let mut expected = "before".encode_utf16().collect::<Vec<_>>();
        expected.extend(opaque_units);
        expected.extend(source_units);
        assert_eq!(summary.string_at("/1/role"), Some("assistant"));
        assert_eq!(summary.string_units("/1/text"), Some(expected));
        assert_eq!(
            orchestrator.session.lock().await.history,
            history,
            "the default summary must preserve original provider payloads and source units"
        );
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
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![
                    ContentBlock::Text {
                        text: "before".into(),
                        citations: None,
                    },
                    ContentBlock::ToolUse {
                        id: id.clone(),
                        name: "Read".into(),
                        input: projection_value(&json!({"file_path":"a.txt"})),
                        provider_id: Some("toolu_example".into()),
                    },
                    ContentBlock::Text {
                        text: "after".into(),
                        citations: None,
                    },
                ],
                stop_reason: None,
                resumable: None,
                per_turn_effort: None,
            },
            user(
                vec![ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: "not found".into(),
                    is_error: Some(true),
                    provider_tool_use_id: Some("toolu_example".into()),
                    content_blocks: None,
                }],
                false,
            ),
        ];
        let result = projection_to_value(&summarize_projection(&history));
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
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "reason".into(),
                        signature: Some("signed".into()),
                    },
                    ContentBlock::ToolUse {
                        id: id.clone(),
                        name: "Read".into(),
                        input: projection_value(&json!({"file_path":"a.txt"})),
                        provider_id: Some("toolu_api_view".into()),
                    },
                ],
                stop_reason: None,
                resumable: None,
                per_turn_effort: None,
            },
            user(
                vec![ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: "found".into(),
                    is_error: Some(false),
                    provider_tool_use_id: Some("toolu_api_view".into()),
                    content_blocks: None,
                }],
                false,
            ),
        ];
        let rows = projection_to_value(&api_projection(history).unwrap());
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
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::ToolUse {
                    id: id.clone(),
                    name: "View".into(),
                    input: projection_value(&json!({})),
                    provider_id: None,
                }],
                stop_reason: None,
                resumable: None,
                per_turn_effort: None,
            },
            user(
                vec![ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: "image".into(),
                    is_error: Some(true),
                    provider_tool_use_id: None,
                    content_blocks: Some(projection_value(&json!([
                        json!({"type":"text","text":"caption"}),
                        json!({"type":"image","source":{"type":"base64",
                        "media_type":"image/png","data":"AQID"}}),
                    ]))),
                }],
                false,
            ),
        ];
        let rows = projection_to_value(&api_projection(history).unwrap());
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
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::ToolUse {
                    id: id.clone(),
                    name: "Read".into(),
                    input: projection_value(&json!({})),
                    provider_id: Some("toolu_cut".into()),
                }],
                stop_reason: None,
                resumable: None,
                per_turn_effort: None,
            },
            user(
                vec![ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: "result".into(),
                    is_error: Some(false),
                    provider_tool_use_id: Some("toolu_cut".into()),
                    content_blocks: None,
                }],
                false,
            ),
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                    citations: None,
                }],
                stop_reason: None,
                resumable: None,
                per_turn_effort: None,
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
                history.push(ConversationMessage::Assistant {
                    id: MessageId::new(),
                    content,
                    stop_reason: None,
                    resumable: None,
                    per_turn_effort: None,
                });
            } else {
                history.push(user(content, false));
            }
        }
        let rows = projection_to_value(&api_projection(history).unwrap());
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
        let result = projection_to_value(&summarize_projection(&history));
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
        assert_eq!(
            projection_to_value(&summarize_projection(&[summary]))[0]["text"],
            "summary"
        );
    }

    #[test]
    fn result_offset_cache_invalidates_append_rewrite_and_replacement() {
        use std::io::Write as _;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        let line = |value| {
            format!(
                "{}\n",
                json!({
                    "message":{"content":[{"type":"tool_result","tool_use_id":"id"}]},
                    "toolUseResult":{"value":value}
                })
            )
        };
        let needed = ["id".encode_utf16().collect::<Vec<_>>()]
            .into_iter()
            .collect();
        let value = || {
            read_transcript_results(&path, &needed)
                .values()
                .next()
                .unwrap()
                .number_at("/value")
                .unwrap()
        };
        std::fs::write(&path, line(1)).unwrap();
        assert_eq!(value(), 1.0);
        assert_eq!(value(), 1.0, "unchanged file uses indexed offsets");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(line(2).as_bytes())
            .unwrap();
        assert_eq!(value(), 2.0, "last raw result wins after append");
        std::fs::write(&path, line(3)).unwrap();
        assert_eq!(value(), 3.0, "truncation invalidates the prior offsets");
        std::fs::write(&path, line(4)).unwrap();
        assert_eq!(value(), 4.0, "same-length rewrite invalidates the index");
        let replacement = temp.path().join("replacement");
        std::fs::write(&replacement, line(5)).unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert_eq!(value(), 5.0, "replacement inode invalidates the index");
        std::fs::remove_file(&path).unwrap();
        assert!(read_transcript_results(&path, &needed).is_empty());
    }

    #[tokio::test]
    async fn reads_committed_raw_tool_result_from_jsonl() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        let id = ToolUseId::from("toolu_example");
        let history = [
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::ToolUse {
                    id: id.clone(),
                    name: "Read".into(),
                    input: projection_value(&json!({})),
                    provider_id: None,
                }],
                stop_reason: None,
                resumable: None,
                per_turn_effort: None,
            },
            user(
                vec![ContentBlock::ToolResult {
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
        let mut rows = summarize_projection(&history);
        hydrate_results(&path, &mut rows).await;
        let rows = projection_to_value(&rows);
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
