//! `/export` — write the conversation transcript to a plain-text file.
//!
//! Port of the claude-code `type: 'local-jsx'` `ExportDialog`
//! (`src/commands/export/export.tsx`). The TS command renders the messages to
//! plain text (`renderMessagesToPlainText`) and, when an explicit `[filename]`
//! arg is given, writes the file directly and reports
//! `"Conversation exported to: {filepath}"` (or
//! `"Failed to export conversation: {error}"`). With no arg it opens an
//! interactive React dialog seeded with a default filename derived from the
//! first prompt (`extractFirstPrompt` + `sanitizeFilename`) or a timestamp.
//!
//! The interactive dialog has no CLI text analogue, so this port keeps the
//! direct-write behavior for both cases: with no arg it writes to the
//! TS-derived default filename in the cwd. The transcript is rendered from the
//! additive [`OrchestratorHandle::conversation_transcript`]; the full
//! React/Ink-rendered layout (`exportRenderer.tsx`) is out of scope for a CLI
//! port, so a plain role-prefixed text rendering is used instead.

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::OrchestratorHandle;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ConversationMessage};
use std::path::Path;
use std::sync::Arc;

/// `/export` handler — renders + writes the conversation transcript.
#[derive(Clone)]
pub struct ExportHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl ExportHandler {
    /// Construct an `ExportHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for ExportHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let Some(raw_args) = args.raw_args_utf8() else {
            return CommandResult::Done {
                display: Some("Export filename must be valid Unicode text.".into()),
            };
        };
        let messages = self.handle.conversation_transcript().await;
        let snap = self.handle.get_status_snapshot().await;
        let content = render_messages_to_plain_text(&messages);

        let filename_arg = raw_args.trim();
        let filename = if filename_arg.is_empty() {
            default_filename(&messages)
        } else {
            ensure_txt_extension(filename_arg)
        };
        let filepath = snap.cwd.join(&filename);

        let file_text = String::from_utf16_lossy(
            &content
                .string_units("")
                .expect("transcript renderer returns a root string"),
        );
        let display = match std::fs::write(&filepath, file_text) {
            Ok(()) => format!("Conversation exported to: {}", filepath.display()),
            Err(e) => format!("Failed to export conversation: {e}"),
        };
        CommandResult::Done {
            display: Some((display).into()),
        }
    }
    fn name(&self) -> &str {
        "export"
    }
    fn description(&self) -> &str {
        "Export the current conversation to a file or clipboard"
    }
}

/// Render the transcript to plain text: one role-prefixed block per message,
/// blocks separated by a blank line.
///
/// This is the CLI analogue of the TS `renderMessagesToPlainText` (which
/// drives the full React/Ink renderer — out of scope for a text port). Tool
/// calls and results are rendered as labeled lines so the export remains a
/// faithful record of the turn.
#[must_use]
fn render_messages_to_plain_text(messages: &[ConversationMessage]) -> Utf16JsonProjection {
    let mut units = Vec::new();
    for (index, msg) in messages.iter().enumerate() {
        if index > 0 {
            units.extend("\n\n".encode_utf16());
        }
        let (label, body) = match msg {
            ConversationMessage::User { content, .. } => ("User", render_blocks(content)),
            ConversationMessage::Assistant { content, .. } => ("Assistant", render_blocks(content)),
            ConversationMessage::System { content, .. } => ("System", source_string_units(content)),
        };
        units.extend(label.encode_utf16());
        units.extend(": ".encode_utf16());
        units.extend(body);
    }
    source_string(units)
}

/// Render a message's content blocks to plain text. Text blocks pass through;
/// tool-use / tool-result / thinking / image blocks render a labeled summary.
fn render_blocks(content: &[ContentBlock]) -> Vec<u16> {
    let mut units = Vec::new();
    let mut first = true;
    for block in content {
        let line = match block {
            ContentBlock::Text { text, .. } => text.encode_utf16().collect(),
            ContentBlock::TextJsUtf16 {
                utf16_code_units, ..
            } => utf16_code_units.clone(),
            ContentBlock::ToolUse { name, input, .. } => {
                labeled_source_units("[tool: ", name, "] ", input)
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                let tag = if is_error.unwrap_or(false) {
                    "[tool error] "
                } else {
                    "[tool result] "
                };
                let mut line = tag.encode_utf16().collect::<Vec<_>>();
                line.extend(source_string_units(content));
                line
            }
            ContentBlock::Thinking { thinking, .. } => {
                let mut line = "[thinking] ".encode_utf16().collect::<Vec<_>>();
                line.extend(thinking.encode_utf16());
                line
            }
            ContentBlock::Image { .. } => "[image]".encode_utf16().collect(),
            ContentBlock::Document { .. } => "[document]".encode_utf16().collect(),
            ContentBlock::RedactedThinking { .. } => "[redacted thinking]".encode_utf16().collect(),
            ContentBlock::ServerToolUse { name, input, .. } => {
                labeled_source_units("[server tool: ", name, "] ", input)
            }
            ContentBlock::ConnectorText { connector_text, .. } => {
                let mut line = "[connector] ".encode_utf16().collect::<Vec<_>>();
                line.extend(connector_text.encode_utf16());
                line
            }
            ContentBlock::AdvisorToolResult { content, .. } => {
                let mut line = "[advisor result] ".encode_utf16().collect::<Vec<_>>();
                line.extend(source_string_units(content));
                line
            }
            ContentBlock::MediaAnalysis { .. } | ContentBlock::ProviderContent { .. } => continue,
        };
        if !first {
            units.push(b'\n' as u16);
        }
        first = false;
        units.extend(line);
    }
    units
}

fn labeled_source_units(
    prefix: &str,
    name: &str,
    middle: &str,
    source: &Utf16JsonProjection,
) -> Vec<u16> {
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend(name.encode_utf16());
    units.extend(middle.encode_utf16());
    units.extend(source_string_units(source));
    units
}

fn source_string_units(source: &Utf16JsonProjection) -> Vec<u16> {
    source.string_units("").unwrap_or_else(|| {
        source
            .to_json_string()
            .expect("source JSON projection serializes")
            .encode_utf16()
            .collect()
    })
}

fn source_string(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display string is derived from exact UTF-16 units")
}

/// Replace any trailing extension on `name` with `.txt`, 1:1 with the TS
/// `filename.replace(/\.[^.]+$/, '') + '.txt'` (no early-return: an existing
/// `.txt` strips and re-appends to the same value, matching the TS exactly).
fn ensure_txt_extension(name: &str) -> String {
    let path = Path::new(name);
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or(name);
    // Strip a trailing `.ext` exactly like the TS regex `/\.[^.]+$/`: the last
    // dot followed by >=1 non-dot char, anchored to the end. So `.gitignore`
    // strips to "" (TS yields `.txt`), `a.b.c` -> `a.b`, and a name ending in
    // `.` keeps the dot. `file_stem` would instead treat a leading dot as the
    // stem, diverging from TS — hence the explicit `rfind`.
    let stem = match file_name.rfind('.') {
        Some(pos) if pos + 1 < file_name.len() => &file_name[..pos],
        _ => file_name,
    };
    // Preserve any parent directory in the supplied name.
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(
            || format!("{stem}.txt"),
            |parent| parent.join(format!("{stem}.txt")).display().to_string(),
        )
}

/// Derive the default export filename, mirroring the TS no-arg branch:
/// `{timestamp}-{sanitized-first-prompt}.txt` when a first prompt exists,
/// else `conversation-{timestamp}.txt`. The timestamp uses the same
/// `YYYY-MM-DD-HHMMSS` shape as the TS `formatTimestamp`.
fn default_filename(messages: &[ConversationMessage]) -> String {
    let timestamp = timestamp_now();
    let first = extract_first_prompt(messages);
    if first.is_empty() {
        return format!("conversation-{timestamp}.txt");
    }
    let sanitized = sanitize_filename(&first);
    if sanitized.is_empty() {
        format!("conversation-{timestamp}.txt")
    } else {
        format!("{timestamp}-{sanitized}.txt")
    }
}

/// First non-empty text of the first user message, first line only, truncated
/// to 50 chars with a trailing `…` (1:1 with the TS `extractFirstPrompt`).
fn extract_first_prompt(messages: &[ConversationMessage]) -> String {
    let Some(ConversationMessage::User { content, .. }) = messages
        .iter()
        .find(|m| matches!(m, ConversationMessage::User { .. }))
    else {
        return String::new();
    };
    let text = content
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.trim()),
            _ => None,
        })
        .unwrap_or("");
    let first_line = text.lines().next().unwrap_or("");
    if first_line.chars().count() > 50 {
        let truncated: String = first_line.chars().take(49).collect();
        format!("{truncated}…")
    } else {
        first_line.to_string()
    }
}

/// Lowercase, drop non-`[a-z0-9 -]`, collapse whitespace runs to `-`, collapse
/// repeated `-`, trim leading/trailing `-` (1:1 with the TS `sanitizeFilename`).
fn sanitize_filename(text: &str) -> String {
    let lower = text.to_lowercase();
    // Drop special chars (keep alnum, whitespace, hyphen).
    let kept: String = lower
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || c.is_whitespace() || *c == '-')
        .collect();
    // Whitespace runs -> single hyphen.
    let mut out = String::with_capacity(kept.len());
    let mut prev_hyphen = false;
    for ch in kept.chars() {
        if ch.is_whitespace() || ch == '-' {
            if !prev_hyphen {
                out.push('-');
                prev_hyphen = true;
            }
        } else {
            out.push(ch);
            prev_hyphen = false;
        }
    }
    out.trim_matches('-').to_string()
}

/// `YYYY-MM-DD-HHMMSS` UTC timestamp, matching the TS `formatTimestamp` shape.
/// Computed from the Unix epoch with a minimal civil-date conversion so no new
/// dependency is required.
fn timestamp_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    civil_timestamp(now)
}

/// Convert Unix seconds to a `YYYY-MM-DD-HHMMSS` UTC string. Uses the
/// algorithm from Howard Hinnant's `days_from_civil` (public domain), inverse
/// direction, to avoid a chrono/time dependency.
// `doe`/`doy`/`yoe` are the canonical variable names from Hinnant's algorithm;
// renaming for `similar_names` would only obscure the well-known derivation.
#[allow(clippy::cast_possible_wrap, clippy::similar_names)]
fn civil_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;

    // civil_from_days (Hinnant). z = days since 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02}-{hour:02}{minute:02}{second:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::MessageId;

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_string())
    }

    fn assistant(text: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                citations: None,
            }],
            stop_reason: Some("end_turn".to_string()),
            resumable: None,
        }
    }

    #[test]
    fn renders_role_prefixed_transcript() {
        let msgs = vec![user("hello"), assistant("hi there")];
        let s = render_messages_to_plain_text(&msgs);
        assert_eq!(s.string_at(""), Some("User: hello\n\nAssistant: hi there"));
    }

    #[test]
    fn renders_tool_blocks() {
        let msgs = vec![ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "running".to_string(),
                    citations: None,
                },
                ContentBlock::ToolUse {
                    id: lingxi_core::types::ToolUseId::new(),
                    name: "Read".into(),
                    input: Utf16JsonProjection::from_serde_value(
                        &serde_json::json!({"file_path": "/x"}),
                    )
                    .unwrap(),
                    provider_id: None,
                },
            ],
            stop_reason: None,
            resumable: None,
        }];
        let s = render_messages_to_plain_text(&msgs);
        let rendered = s.string_at("").expect("valid display string");
        assert!(rendered.contains("Assistant: running"));
        assert!(rendered.contains("[tool: Read]"));
    }

    #[test]
    fn skips_internal_media_analysis_blocks() {
        let msgs = vec![ConversationMessage::User {
            id: MessageId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "hello".to_string(),
                    citations: None,
                },
                ContentBlock::MediaAnalysis {
                    analysis: lingxi_core::types::MediaAnalysis {
                        question_key: "internal-question-key".to_string(),
                        media_fingerprints: vec!["fp".to_string()],
                        model: "delegate".to_string(),
                        prompt_version: 1,
                        created_at: std::time::SystemTime::UNIX_EPOCH,
                        task_findings: vec!["internal".to_string()],
                        media: Vec::new(),
                        cross_media_findings: Vec::new(),
                        truncated: false,
                    },
                },
            ],
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }];
        let rendered = render_messages_to_plain_text(&msgs);
        assert_eq!(rendered.string_at(""), Some("User: hello"));
        assert!(!rendered
            .string_at("")
            .expect("valid display string")
            .contains("internal-question-key"));
    }

    #[test]
    fn ensure_txt_extension_cases() {
        assert_eq!(ensure_txt_extension("notes"), "notes.txt");
        assert_eq!(ensure_txt_extension("notes.txt"), "notes.txt");
        assert_eq!(ensure_txt_extension("notes.md"), "notes.txt");
        // Multi-extension: only the last `.ext` is stripped (TS regex parity).
        assert_eq!(ensure_txt_extension("archive.tar.gz"), "archive.tar.txt");
        // Dotfile: TS `/\.[^.]+$/` strips the whole `.gitignore` -> `.txt`.
        assert_eq!(ensure_txt_extension(".gitignore"), ".txt");
    }

    #[test]
    fn sanitize_filename_matches_ts() {
        assert_eq!(sanitize_filename("Hello, World!"), "hello-world");
        assert_eq!(sanitize_filename("  spaced   out  "), "spaced-out");
        assert_eq!(sanitize_filename("a--b__c"), "a-bc");
        assert_eq!(sanitize_filename("***"), "");
    }

    #[test]
    fn extract_first_prompt_truncates_and_first_line_only() {
        let long = "x".repeat(100);
        let msgs = vec![user(&format!("{long}\nsecond line"))];
        let p = extract_first_prompt(&msgs);
        assert_eq!(p.chars().count(), 50); // 49 chars + ellipsis
        assert!(p.ends_with('…'));
    }

    #[test]
    fn extract_first_prompt_empty_when_no_user() {
        let msgs = vec![assistant("only assistant")];
        assert_eq!(extract_first_prompt(&msgs), "");
    }

    #[test]
    fn default_filename_uses_first_prompt() {
        let msgs = vec![user("Fix the bug")];
        let f = default_filename(&msgs);
        assert!(f.ends_with("-fix-the-bug.txt"), "got {f}");
    }

    #[test]
    fn default_filename_falls_back_to_conversation_prefix() {
        let msgs = vec![assistant("no user prompt")];
        let f = default_filename(&msgs);
        assert!(f.starts_with("conversation-"), "got {f}");
        assert_eq!(
            Path::new(&f).extension().and_then(|e| e.to_str()),
            Some("txt")
        );
    }

    #[test]
    fn civil_timestamp_known_epoch() {
        // 0 -> 1970-01-01 00:00:00 UTC.
        assert_eq!(civil_timestamp(0), "1970-01-01-000000");
        // 1_700_000_000 -> 2023-11-14 22:13:20 UTC.
        assert_eq!(civil_timestamp(1_700_000_000), "2023-11-14-221320");
    }

    #[tokio::test]
    async fn writes_file_with_explicit_name() {
        use orchestrator::test_support::MockOrchestratorHandle;
        let dir = std::env::temp_dir().join(format!("lingxi-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_status_snapshot(lingxi_core::host::StatusSnapshot {
            cwd: dir.clone(),
            ..lingxi_core::host::StatusSnapshot::default()
        });
        let h = ExportHandler::new(mock);
        let args = ParsedSlashCommand {
            name: "export".into(),
            raw_args: "out".into(),
            positional_args: vec!["out".into()],
        };
        let display = match h.handle(&args).await {
            CommandResult::Done { display: Some(s) } => s,
            other => panic!("{other:?}"),
        };
        let expected = dir.join("out.txt");
        let expected_display = format!("Conversation exported to: {}", expected.display());
        assert!(
            display.string_at("") == Some(expected_display.as_str()),
            "got {display:?}"
        );
        assert!(expected.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn name_and_description() {
        use orchestrator::test_support::MockOrchestratorHandle;
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ExportHandler::new(mock);
        assert_eq!(h.name(), "export");
        assert_eq!(
            h.description(),
            "Export the current conversation to a file or clipboard"
        );
    }
}
