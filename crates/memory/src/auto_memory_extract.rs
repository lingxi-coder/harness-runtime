//! Claude Code 2.1.319 local-file post-turn memory extraction protocol.
//!
//! This is a tool-executing fork that maintains the ordinary memory directory.
//! The older `session_memory` module is a distinct LingXi document-extraction
//! extension; its token gates and return-document prompt are not this protocol.

use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use std::collections::HashSet;
use std::sync::OnceLock;

/// Native file-extraction turn limit.
pub const FILE_EXTRACTION_MAX_TURNS: u32 = 5;
/// Native file-extraction cadence without a server override.
pub const FILE_EXTRACTION_EVERY_N_TURNS: u32 = 1;
/// Native query-source and fork labels are identical.
pub const FILE_EXTRACTION_QUERY_SOURCE: &str = "extract_memories";
/// Native file-inventory frontmatter/head line budget.
pub const FILE_INVENTORY_HEAD_LINES: usize = 30;
/// Native file-inventory head read budget, in bytes.
pub const FILE_INVENTORY_HEAD_BYTES: u64 = 65_536;

/// Native memory-agent denial for an unavailable/out-of-scope tool.
#[must_use]
pub fn memory_tool_denial(directory: &str, posix: bool) -> String {
    format!(
        "only Read, Grep, Glob, read-only {}, and Edit/Write within {directory} are allowed",
        if posix { "Bash" } else { "PowerShell" }
    )
}

/// Native memory-agent denial for a shell command outside its permitted forms.
#[must_use]
pub fn memory_shell_denial(directory: &str, posix: bool) -> String {
    format!("Only read-only shell commands and {} of .md files under {directory} (not protected subdirectories like .git or agents) are permitted in this context ({})", if posix { "rm (no flags except -f)" } else { "Remove-Item" }, if posix { "ls, find, grep, cat, stat, wc, head, tail, and similar" } else { "Get-ChildItem, Get-Content, Select-Object -First/-Last, and similar" })
}

/// Native memory-inventory metadata, independent of LingXi's legacy selector
/// frontmatter aliases and its strict persisted-document delimiter grammar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileInventoryMetadata {
    /// One of the four native memory types.
    pub memory_type: Option<String>,
    /// A nonempty string description, preserved exactly.
    pub description: Option<String>,
}

fn inventory_yaml_source(head: &str) -> Option<&str> {
    static DELIMITER: OnceLock<regex::Regex> = OnceLock::new();
    let regex = DELIMITER.get_or_init(|| {
        // Native td/mv uses ECMAScript \s, which includes FEFF and excludes
        // 0085. This intentionally retains Native's loose closing delimiter.
        let space = r"[\x09-\x0d \u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]";
        regex::Regex::new(&format!(r"\A---{space}*\n(?s:(.*?))---{space}*\n?")).expect("native delimiter regex")
    });
    regex
        .captures(head.strip_prefix('\u{feff}').unwrap_or(head))?
        .get(1)
        .map(|capture| capture.as_str())
}

/// Read the native inventory's BOM/delimiter shape and string metadata fields.
/// YAML decoding uses the host's existing YAML parser; this does not change the
/// legacy session-document writer or selector's frontmatter precedence.
#[must_use]
pub fn parse_file_inventory_head(head: &str) -> FileInventoryMetadata {
    let Some(yaml) = inventory_yaml_source(head) else {
        return FileInventoryMetadata::default();
    };
    let parsed = yaml_serde::from_str::<yaml_serde::Value>(yaml).or_else(|_| {
        // Native vHn's retry expands only leading YAML indentation tabs.
        let normalized = yaml
            .split_inclusive('\n')
            .map(|line| {
                let tabs = line.bytes().take_while(|byte| *byte == b'\t').count();
                format!("{}{}", "  ".repeat(tabs), &line[tabs..])
            })
            .collect::<String>();
        yaml_serde::from_str::<yaml_serde::Value>(&normalized)
    });
    let Ok(parsed) = parsed else {
        return FileInventoryMetadata::default();
    };
    let Some(fields) = parsed.as_mapping() else {
        return FileInventoryMetadata::default();
    };
    let field = |name: &str| fields.get(yaml_serde::Value::String(name.to_owned()));
    let nested_type = field("metadata")
        .and_then(yaml_serde::Value::as_mapping)
        .and_then(|fields| fields.get(yaml_serde::Value::String("type".to_owned())));
    let memory_type = nested_type
        .or_else(|| field("type"))
        .and_then(yaml_serde::Value::as_str)
        .filter(|kind| matches!(*kind, "user" | "feedback" | "project" | "reference"))
        .map(str::to_owned);
    let description = field("description")
        .and_then(yaml_serde::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    FileInventoryMetadata {
        memory_type,
        description,
    }
}

/// Host facts selected by the native `Vt` prompt renderer.
#[derive(Debug, Clone, Copy)]
pub struct FileExtractionPrompt<'a> {
    /// Number of user/assistant rows after the extraction cursor.
    pub message_count: usize,
    /// Rendered existing-file list, or an empty string for no files.
    pub existing_memory_files: &'a str,
    /// Whether the parent's Memory section provides scope guidance.
    pub has_scope_guidance: bool,
    /// Whether project-skill upkeep appears in the parent's Memory section.
    pub project_skill_upkeep: bool,
    /// Whether shared-memory tools are described but unavailable to this fork.
    pub shared_memory_tools_unavailable: bool,
    /// `true` for the native POSIX shell, `false` for PowerShell.
    pub posix: bool,
}

/// Render the complete native file-extractor directive without whitespace folding.
#[must_use]
pub fn render_file_extraction_prompt(input: FileExtractionPrompt<'_>) -> String {
    let shell = if input.posix { "Bash" } else { "PowerShell" };
    render_file_extraction_prompt_with_shell(input, shell)
}

/// Host-specific shell identity; only the tool name changes on Mobile. The
/// same native POSIX/PowerShell constraints and prompt ordering still apply.
#[must_use]
pub fn render_file_extraction_prompt_with_shell(
    input: FileExtractionPrompt<'_>,
    shell: &str,
) -> String {
    let FileExtractionPrompt {
        message_count,
        existing_memory_files,
        has_scope_guidance,
        project_skill_upkeep,
        shared_memory_tools_unavailable,
        posix,
    } = input;
    let readers = if posix {
        "ls/find/cat/stat/wc/head/tail and similar"
    } else {
        "Get-ChildItem/Get-Content/Select-Object -First/-Last and similar"
    };
    let remove = if posix { "rm" } else { "Remove-Item" };
    let remove_flags = if posix {
        "; rm takes no flags except -f"
    } else {
        ""
    };
    let mut rows = vec![
        format!("You are now acting as the memory extraction subagent. Analyze the most recent ~{message_count} messages above and use them to update your persistent memory systems."),
        String::new(),
        format!("Available tools: Read, Grep, Glob, read-only {shell} ({readers}), and Edit/Write for paths inside the memory directory only, and {shell} {remove} of .md files inside the memory directory only (outside protected subdirectories like .git or agents{remove_flags}). All other tools — MCP, Agent, write-capable {shell}, etc — will be denied."),
        String::new(),
        "You have a limited turn budget. Edit requires a prior Read of the same file, so the efficient strategy is: turn 1 — issue all Read calls in parallel for every file you might update; turn 2 — issue all Write/Edit calls in parallel. Do not interleave reads and writes across multiple turns.".to_owned(),
        String::new(),
        format!("You MUST only use content from the last ~{message_count} messages to update your persistent memories. Do not waste any turns attempting to investigate or verify that content further — no grepping source files, no reading code to confirm a pattern exists, no git commands."),
    ];
    if !existing_memory_files.is_empty() {
        rows.last_mut().expect("directive exists").push_str(&format!(
            "\n\n## Existing memory files\n\n{existing_memory_files}\n\nCheck this list before writing — update an existing file rather than creating a duplicate."
        ));
    }
    rows.extend([
        String::new(),
        "If nothing is worth saving, output only 'Nothing to save.' Do not explain why.".to_owned(),
        String::new(),
        "If the user explicitly asks you to remember something, save it immediately as whichever type fits best. If they ask you to forget something, find and remove the relevant entry.".to_owned(),
        String::new(),
        format!("Apply the memory types, {}what-not-to-save criteria, and frontmatter format from the Memory section of your system prompt — it is already in your context above.", if has_scope_guidance { "scope guidance, " } else { "" }),
    ]);
    if project_skill_upkeep {
        rows.extend([String::new(), "Skip the project-skill upkeep step here: your writes are restricted to the memory directory, so record the correction as a feedback memory only.".to_owned()]);
    }
    if shared_memory_tools_unavailable {
        rows.extend([String::new(), "The memory_list / memory_read / memory_write tools are unavailable here, so skip anything the scope guidance marks as shared with the project — the main conversation saves those; never file them in the personal directory instead. Save only what belongs in your personal memory directory.".to_owned()]);
    }
    rows.join("\n")
}

fn cursor_index(history: &[ConversationMessage], cursor: Option<&MessageId>) -> Option<usize> {
    cursor.and_then(|id| history.iter().position(|message| message.id() == *id))
}

/// Native `Ct`: count user/assistant rows; a missing cursor reviews all rows.
#[must_use]
pub fn file_extraction_message_count(
    history: &[ConversationMessage],
    cursor: Option<&MessageId>,
) -> usize {
    let start = cursor_index(history, cursor).map_or(0, |index| index + 1);
    history[start..]
        .iter()
        .filter(|message| !matches!(message, ConversationMessage::System { .. }))
        .count()
}

fn js_space(unit: &u16) -> bool {
    matches!(*unit, 0x0009..=0x000D | 0x0020 | 0x00A0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF)
}

fn three_words(units: &[u16]) -> bool {
    units
        .split(js_space)
        .filter(|part| !part.is_empty())
        .take(3)
        .count()
        >= 3
}

/// Native file-mode `$t`/`Rt`: an ordinary user text block needs three words.
/// Separate text blocks are not joined; meta users never qualify. Like Native,
/// this deliberately does not add the MCP mode's CJK/chat-prose heuristics.
#[must_use]
pub fn has_file_extraction_user_prose(
    history: &[ConversationMessage],
    cursor: Option<&MessageId>,
) -> bool {
    let start = cursor_index(history, cursor).map_or(0, |index| index + 1);
    history[start..].iter().any(|message| match message {
        ConversationMessage::User {
            content,
            is_meta: false,
            ..
        } => content.iter().any(|block| match block {
            ContentBlock::Text { text, .. } => {
                three_words(&text.encode_utf16().collect::<Vec<_>>())
            }
            ContentBlock::TextJsUtf16 {
                utf16_code_units, ..
            } => three_words(utf16_code_units),
            _ => false,
        }),
        _ => false,
    })
}

/// Native file-mode `Nt`/`ze`: skip a range already written by the main agent.
/// A defined cursor absent after compaction does not suppress the next pass.
/// The caller supplies the active memory-directory containment predicate.
#[must_use]
pub fn main_wrote_memory_since(
    history: &[ConversationMessage],
    cursor: Option<&MessageId>,
    is_memory_path: impl Fn(&str) -> bool,
) -> bool {
    let start = match (cursor, cursor_index(history, cursor)) {
        (Some(_), None) => return false,
        (_, Some(index)) => index + 1,
        (None, None) => 0,
    };
    let mut shared_writes = HashSet::new();
    for message in &history[start..] {
        match message {
            ConversationMessage::Assistant { content, .. } => {
                for block in content {
                    let ContentBlock::ToolUse {
                        name, id, input, ..
                    } = block
                    else {
                        continue;
                    };
                    if name == "memory_write" {
                        shared_writes.insert(id.clone());
                    } else if matches!(name.as_str(), "Write" | "Edit")
                        && input.string_at("/file_path").is_some_and(&is_memory_path)
                    {
                        return true;
                    }
                }
            }
            ConversationMessage::User { content, .. } => {
                if content.iter().any(|block| {
                    matches!(block,
                    ContentBlock::ToolResult { tool_use_id, is_error, .. }
                    if *is_error != Some(true) && shared_writes.contains(tool_use_id))
                }) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Native `GJr` inventory row. The host supplies the file mtime in JavaScript
/// `Date.toISOString()` form; descriptions are preserved without trimming.
#[must_use]
pub fn render_existing_memory_file(
    filename: &str,
    iso_timestamp: &str,
    memory_type: Option<&str>,
    description: Option<&str>,
) -> String {
    let kind = memory_type
        .filter(|kind| !kind.is_empty())
        .map_or_else(String::new, |kind| format!("[{kind}] "));
    let description = description
        .filter(|description| !description.is_empty())
        .map_or_else(String::new, |description| format!(": {description}"));
    format!("- {kind}{filename} ({iso_timestamp}){description}")
}

/// Native `Qt` summary for the exact successful write paths, in first-write order.
#[must_use]
pub fn extraction_write_summary(paths: &[std::path::PathBuf]) -> String {
    let count = paths
        .iter()
        .filter(|path| path.file_name().is_none_or(|name| name != "MEMORY.md"))
        .count();
    match count {
        0 => "updated the memory index".to_owned(),
        1 => "saved 1 memory".to_owned(),
        _ => format!("saved {count} memories"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../tests/fixtures/session_memory_2_1_319.json"
        ))
        .unwrap()
    }

    fn history(rows: &Value) -> Vec<ConversationMessage> {
        rows.as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let role = row["type"].as_str().unwrap();
                let mut content = row["message"]["content"].clone();
                if content.is_string() && role != "system" {
                    content = json!([{ "type": "text", "text": content }]);
                }
                serde_json::from_value(json!({
                    "role": role, "id": row["uuid"], "content": content,
                    "is_meta": row["isMeta"].as_bool().unwrap_or(false)
                }))
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn mobile_shell_name_uses_the_advertised_capability_without_rewriting_inventory_data() {
        let tools = vec![lingxi_core::host::SourceToolWireDefinition {
            name: "Shell".into(),
            description: "mobile shell".into(),
            input_schema: lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}").unwrap(),
            strict: false,
            defer_loading: false,
        }];
        let shell = crate::agent_tools::MemoryAgentShell::from_tools(&tools);
        assert_eq!(shell, crate::agent_tools::MemoryAgentShell::Mobile);
        let prompt = render_file_extraction_prompt_with_shell(
            FileExtractionPrompt {
                message_count: 3,
                existing_memory_files: "- note.md: Bash preference remains literal",
                has_scope_guidance: false,
                project_skill_upkeep: false,
                shared_memory_tools_unavailable: false,
                posix: shell.is_posix(),
            },
            shell.name(),
        );
        assert!(prompt.contains("read-only Shell"));
        assert!(prompt.contains("write-capable Shell"));
        assert!(prompt.contains("Bash preference remains literal"));
        assert!(!prompt.contains("read-only Bash"));
    }

    #[test]
    fn native_extraction_prompt_byte_vectors() {
        for case in fixture()["prompts"].as_array().unwrap() {
            let input = &case["input"];
            let actual = render_file_extraction_prompt(FileExtractionPrompt {
                message_count: input["message_count"].as_u64().unwrap() as usize,
                existing_memory_files: input["existing_memory_files"].as_str().unwrap(),
                has_scope_guidance: input["has_scope_guidance"].as_bool().unwrap(),
                project_skill_upkeep: input["project_skill_upkeep"].as_bool().unwrap(),
                shared_memory_tools_unavailable: input["shared_memory_tools_unavailable"]
                    .as_bool()
                    .unwrap(),
                posix: input["posix"].as_bool().unwrap(),
            });
            assert_eq!(actual, case["expected"].as_str().unwrap(), "{input}");
        }
    }

    #[test]
    fn native_extraction_cursor_prose_and_write_skip_vectors() {
        for case in fixture()["triggers"].as_array().unwrap() {
            let history = history(&case["messages"]);
            let cursor = case["cursor"].as_str().and_then(MessageId::parse_prefixed);
            assert_eq!(
                file_extraction_message_count(&history, cursor.as_ref()),
                case["expected"]["message_count"].as_u64().unwrap() as usize,
                "{case}"
            );
            assert_eq!(
                has_file_extraction_user_prose(&history, cursor.as_ref()),
                case["expected"]["has_user_prose"].as_bool().unwrap(),
                "{case}"
            );
            assert_eq!(
                main_wrote_memory_since(&history, cursor.as_ref(), |path| path
                    .starts_with("/memory/")
                    && !path.contains("/../")),
                case["expected"]["main_wrote_memory"].as_bool().unwrap(),
                "{case}"
            );
        }
    }

    #[test]
    fn native_extraction_write_summary_vectors() {
        for case in fixture()["summaries"].as_array().unwrap() {
            let paths = case["paths"]
                .as_array()
                .unwrap()
                .iter()
                .map(|path| path.as_str().unwrap().into())
                .collect::<Vec<_>>();
            assert_eq!(
                extraction_write_summary(&paths),
                case["expected"]["summary"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn native_inventory_and_file_pass_options() {
        let fixture = fixture();
        assert_eq!(
            FILE_EXTRACTION_QUERY_SOURCE,
            fixture["file_pass"]["query_source"].as_str().unwrap()
        );
        assert_eq!(
            u64::from(FILE_EXTRACTION_MAX_TURNS),
            fixture["file_pass"]["max_turns"].as_u64().unwrap()
        );
        assert_eq!(
            u64::from(FILE_EXTRACTION_EVERY_N_TURNS),
            fixture["file_pass"]["default_every_n_turns"]
                .as_u64()
                .unwrap()
        );
        assert_eq!(
            FILE_INVENTORY_HEAD_LINES as u64,
            fixture["file_pass"]["inventory_head_lines"]
                .as_u64()
                .unwrap()
        );
        assert_eq!(
            FILE_INVENTORY_HEAD_BYTES,
            fixture["file_pass"]["inventory_head_bytes"]
                .as_u64()
                .unwrap()
        );
        for case in fixture["inventories"].as_array().unwrap() {
            let actual = case["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| {
                    render_existing_memory_file(
                        entry["filename"].as_str().unwrap(),
                        entry["iso_timestamp"].as_str().unwrap(),
                        entry["type"].as_str(),
                        entry["description"].as_str(),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(actual, case["expected"].as_str().unwrap());
        }
    }

    #[test]
    fn native_inventory_frontmatter_delimiter_and_bom_vectors() {
        for case in fixture()["frontmatter_heads"].as_array().unwrap() {
            assert_eq!(
                inventory_yaml_source(case["head"].as_str().unwrap()),
                case["expected_yaml"].as_str(),
                "{case}"
            );
        }
        assert_eq!(parse_file_inventory_head("---\r\nlegacy: true\r\ndescription: Keep spaces  \r\ntype: reference\r\nmetadata:\r\n  type: feedback\r\n---"), FileInventoryMetadata {
            memory_type: Some("feedback".into()), description: Some("Keep spaces".into()),
        });
        assert_eq!(
            parse_file_inventory_head(
                "---\nname: topic\ndescription: 7\ntype: reference\nmetadata:\n  type: null\n---"
            ),
            FileInventoryMetadata::default()
        );
    }

    #[test]
    fn native_memory_agent_denial_byte_vectors() {
        for case in fixture()["denials"].as_array().unwrap() {
            let root = case["root"].as_str().unwrap();
            let posix = case["posix"].as_bool().unwrap();
            assert_eq!(
                memory_shell_denial(root, posix),
                case["expected_shell"].as_str().unwrap()
            );
            assert_eq!(
                memory_tool_denial(root, posix),
                case["expected_tool"].as_str().unwrap()
            );
        }
    }
}
