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
use platform_api::OrchestratorHandle;
use protocol::{ContentBlock, ConversationMessage};
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
        let messages = self.handle.conversation_transcript().await;
        let snap = self.handle.get_status_snapshot().await;
        let content = render_messages_to_plain_text(&messages);

        let filename_arg = args.raw_args.trim();
        let filename = if filename_arg.is_empty() {
            default_filename(&messages)
        } else {
            ensure_txt_extension(filename_arg)
        };
        let filepath = snap.cwd.join(&filename);

        let display = match std::fs::write(&filepath, content) {
            Ok(()) => format!("Conversation exported to: {}", filepath.display()),
            Err(e) => format!("Failed to export conversation: {e}"),
        };
        CommandResult::Done {
            display: Some(display),
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
fn render_messages_to_plain_text(messages: &[ConversationMessage]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(messages.len());
    for msg in messages {
        let (label, body) = match msg {
            ConversationMessage::User { content, .. } => ("User", render_blocks(content)),
            ConversationMessage::Assistant { content, .. } => ("Assistant", render_blocks(content)),
            ConversationMessage::System { content, .. } => ("System", content.clone()),
        };
        parts.push(format!("{label}: {body}"));
    }
    parts.join("\n\n")
}

/// Render a message's content blocks to plain text. Text blocks pass through;
/// tool-use / tool-result / thinking / image blocks render a labeled summary.
fn render_blocks(content: &[ContentBlock]) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(content.len());
    for block in content {
        match block {
            ContentBlock::Text { text } | ContentBlock::TextJsUtf16 { text, .. } => {
                lines.push(text.clone())
            }
            ContentBlock::ToolUse { name, input, .. } => {
                lines.push(format!("[tool: {name}] {input}"));
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                let tag = if *is_error {
                    "tool error"
                } else {
                    "tool result"
                };
                lines.push(format!("[{tag}] {content}"));
            }
            ContentBlock::Thinking { thinking, .. } => {
                lines.push(format!("[thinking] {thinking}"));
            }
            ContentBlock::Image { .. } => lines.push("[image]".to_string()),
            ContentBlock::Document { .. } => lines.push("[document]".to_string()),
            ContentBlock::RedactedThinking { .. } => {
                lines.push("[redacted thinking]".to_string());
            }
            ContentBlock::ServerToolUse { name, input, .. } => {
                lines.push(format!("[server tool: {name}] {input}"));
            }
            ContentBlock::ConnectorText { connector_text, .. } => {
                lines.push(format!("[connector] {connector_text}"));
            }
            ContentBlock::AdvisorToolResult { content, .. } => {
                lines.push(format!("[advisor result] {content}"));
            }
            ContentBlock::MediaAnalysis { .. } | ContentBlock::ProviderContent { .. } => {}
        }
    }
    lines.join("\n")
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
            ContentBlock::Text { text } => Some(text.trim()),
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
    use protocol::MessageId;

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_string())
    }

    fn assistant(text: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: Some("end_turn".to_string()),
        }
    }

    #[test]
    fn renders_role_prefixed_transcript() {
        let msgs = vec![user("hello"), assistant("hi there")];
        let s = render_messages_to_plain_text(&msgs);
        assert_eq!(s, "User: hello\n\nAssistant: hi there");
    }

    #[test]
    fn renders_tool_blocks() {
        let msgs = vec![ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "running".to_string(),
                },
                ContentBlock::ToolUse {
                    id: protocol::ToolUseId::new(),
                    name: "Read".to_string(),
                    input: serde_json::json!({"file_path": "/x"}),
                    provider_id: None,
                },
            ],
            stop_reason: None,
        }];
        let s = render_messages_to_plain_text(&msgs);
        assert!(s.contains("Assistant: running"));
        assert!(s.contains("[tool: Read]"));
    }

    #[test]
    fn skips_internal_media_analysis_blocks() {
        let msgs = vec![ConversationMessage::User {
            id: MessageId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "hello".to_string(),
                },
                ContentBlock::MediaAnalysis {
                    analysis: protocol::MediaAnalysis {
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
        }];
        let rendered = render_messages_to_plain_text(&msgs);
        assert_eq!(rendered, "User: hello");
        assert!(!rendered.contains("internal-question-key"));
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
        mock.set_status_snapshot(platform_api::StatusSnapshot {
            cwd: dir.clone(),
            ..platform_api::StatusSnapshot::default()
        });
        let h = ExportHandler::new(mock);
        let args = ParsedSlashCommand {
            name: "export".to_string(),
            raw_args: "out".to_string(),
            positional_args: vec!["out".to_string()],
        };
        let display = match h.handle(&args).await {
            CommandResult::Done { display: Some(s) } => s,
            other => panic!("{other:?}"),
        };
        let expected = dir.join("out.txt");
        assert!(
            display == format!("Conversation exported to: {}", expected.display()),
            "got {display}"
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
