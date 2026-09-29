//! Embedded shell-command expansion for markdown command / skill prompts.
//!
//! Faithful port of `claude-code/src/utils/promptShellExecution.ts`. A prompt
//! body may embed shell commands in two syntaxes, both replaced in place with
//! the command's output before the prompt is sent to the model:
//!
//! * **Block** — a fenced code block whose info string is `!` (the TS
//!   `BLOCK_PATTERN`).
//! * **Inline** — a bang immediately followed by a backtick-quoted command, at
//!   start-of-line or after whitespace (the TS `INLINE_PATTERN`, whose
//!   lookbehind requires the preceding char to be start-of-text or whitespace).
//!
//! ## Design (leaf crate — no tool dependency)
//!
//! TS calls `BashTool` / `PowerShellTool` and `hasPermissionsToUseTool`
//! directly. To keep `command-api` a leaf, both are injected as traits:
//! * [`ShellRunner`] — runs one command and yields stdout/stderr/interrupted.
//! * [`ShellPermissionGate`] — the per-command permission check.
//!
//! ## Divergence from TS (documented intentionally)
//!
//! * **No `regex` crate.** `BLOCK_PATTERN` and `INLINE_PATTERN` are reproduced
//!   with hand-written scanners. The inline lookbehind is emulated by checking
//!   the char preceding the bang (start-of-text or whitespace), and — exactly
//!   like TS — the expensive inline scan is gated behind a cheap substring
//!   fast-path on the inline-marker sequence.
//! * The TS `processToolResultBlock` persistence flow is out of scope here; this
//!   port formats output with the same `formatBashOutput` rules
//!   (`promptShellExecution.ts:145-165`).
//! * Replacement uses a manual splice (not `str::replace`), so dollar-laden
//!   shell output is inserted verbatim, mirroring the TS function-replacer note.

use crate::model::FrontmatterShell;
use async_trait::async_trait;
use std::sync::Arc;

/// Output of a single embedded shell command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellOut {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Whether the command was interrupted (mirrors TS `ShellError.interrupted`).
    pub interrupted: bool,
}

/// Why an embedded shell command failed.
///
/// All variants surface as TS `MalformedCommandError` (the only error the
/// caller catches); [`Self::Malformed`] carries the formatted message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShellExpansionError {
    /// Permission denied, execution failure, or interruption — the message is
    /// pre-formatted to match the TS `MalformedCommandError` text.
    #[error("{0}")]
    Malformed(String),
}

/// Result of a per-command permission check. Mirrors the relevant slice of TS
/// `hasPermissionsToUseTool`'s return: `behavior === 'allow'` vs anything else
/// (which carries an optional message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellPermissionDecision {
    /// The command may run.
    Allow,
    /// The command is denied; the optional message is folded into the error.
    Deny {
        /// Human-readable reason (TS `permissionResult.message`).
        message: Option<String>,
    },
}

/// Per-command permission gate. Injected so `command-api` does not depend on the
/// permission subsystem. Mirrors the TS `hasPermissionsToUseTool(shellTool,
/// { command }, …)` call.
pub trait ShellPermissionGate: Send + Sync {
    /// Decide whether `command` (routed through `shell`) may execute.
    fn check(&self, command: &str, shell: Option<FrontmatterShell>) -> ShellPermissionDecision;
}

/// Runs a single shell command. Injected so the crate stays leaf (no `BashTool`
/// dependency). Mirrors the TS `shellTool.call({ command }, context)`.
///
/// `run` is `async` so adapters can drive an `async` process runner (the
/// production `tool-skill` adapter spawns through `lingxi_core::host::process::ProcessRunner`,
/// whose `run` is `async`). The public [`execute_shell_commands_in_prompt`] is
/// already `async`, so this only adds a `.await` at its single call site.
#[async_trait]
pub trait ShellRunner: Send + Sync {
    /// Execute `command` through `shell`, returning captured output. An `Err`
    /// here corresponds to the TS `ShellError`/throw path; `code` is the exit
    /// status when known.
    async fn run(
        &self,
        command: &str,
        shell: Option<FrontmatterShell>,
    ) -> Result<ShellOut, ShellRunError>;
}

/// Failure raised by a [`ShellRunner::run`] call (the TS `ShellError` analogue).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellRunError {
    /// Partial standard output captured before failure.
    pub stdout: String,
    /// Partial standard error captured before failure.
    pub stderr: String,
    /// Whether the command was interrupted.
    pub interrupted: bool,
    /// A non-`ShellError` message (TS `errorMessage(e)` path). When set, the
    /// error is treated as a generic failure rather than a shell failure.
    pub generic_message: Option<String>,
}

/// Injected dependencies for [`execute_shell_commands_in_prompt`]. Replaces the
/// TS `ToolUseContext`; carries only what this port needs.
pub struct ShellExpansionCtx {
    /// Executes individual commands.
    pub runner: Arc<dyn ShellRunner>,
    /// Permission gate consulted before each command runs.
    pub permission_gate: Arc<dyn ShellPermissionGate>,
}

/// Factory that builds a per-command [`ShellExpansionCtx`] with that command's
/// own allowed-tools baked into the permission gate.
///
/// Mirrors claude-code building a FRESH `toolPermissionContext` per
/// `getPromptForCommand` call before running `executeShellCommandsInPrompt`:
/// each command injects its own `allowedTools` into
/// `alwaysAllowRules.command` on top of the base policy (e.g. /commit adds
/// `Bash(git add:*)` / `Bash(git status:*)` / `Bash(git commit:*)`), rather than
/// mutating the shared boot policy.
///
/// Injected as `Arc<dyn ShellExpansionProvider>` so `command-api` stays a leaf:
/// the concrete provider (holding the live `PermissionPolicy` + runner deps)
/// lives in `tool-api` and is constructed at each composition root. The
/// dispatcher / TUI call [`Self::build`] per expansion with the command's
/// declared allow-list, then run [`execute_shell_commands_in_prompt`] over the
/// returned context.
pub trait ShellExpansionProvider: Send + Sync {
    /// Build a per-command expansion context. `allowed_tools` is that command's
    /// declared allow-list (e.g. `["Bash(git add:*)", …]`), injected on top of
    /// the base policy for this expansion only; `shell` is the
    /// frontmatter-selected shell (`None` → bash; builtins always pass `None`).
    fn build(&self, allowed_tools: &[String], shell: Option<FrontmatterShell>)
        -> ShellExpansionCtx;
}

/// A single extracted shell command: its full matched span (for replacement)
/// and the trimmed command body.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtractedCommand {
    /// Byte offset of the whole match (`match[0]`) within `text`.
    start: usize,
    /// Exclusive end byte offset of the whole match.
    end: usize,
    /// The matched substring (`match[0]`), used as the literal replacement key.
    full: String,
    /// The trimmed command (`match[1]?.trim()`); empty when nothing remains.
    command: String,
}

/// Format combined stdout/stderr exactly like TS `formatBashOutput`.
fn format_bash_output(stdout: &str, stderr: &str, inline: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    let out_trimmed = stdout.trim();
    if !out_trimmed.is_empty() {
        parts.push(out_trimmed.to_string());
    }
    let err_trimmed = stderr.trim();
    if !err_trimmed.is_empty() {
        if inline {
            parts.push(format!("[stderr: {err_trimmed}]"));
        } else {
            parts.push(format!("[stderr]\n{err_trimmed}"));
        }
    }
    parts.join(if inline { " " } else { "\n" })
}

/// Format a [`ShellRunError`] into the TS `MalformedCommandError` message for a
/// given match pattern (port of TS `formatBashError`, non-inline path).
fn format_bash_error(err: &ShellRunError, pattern: &str) -> ShellExpansionError {
    if let Some(message) = &err.generic_message {
        // Non-ShellError path: TS `[Error]\n{message}`.
        return ShellExpansionError::Malformed(format!("[Error]\n{message}"));
    }
    if err.interrupted {
        return ShellExpansionError::Malformed(format!(
            "Shell command interrupted for pattern \"{pattern}\": [Command interrupted]"
        ));
    }
    let output = format_bash_output(&err.stdout, &err.stderr, false);
    ShellExpansionError::Malformed(format!(
        "Shell command failed for pattern \"{pattern}\": {output}"
    ))
}

/// Parse a prompt and execute any embedded shell commands, replacing each match
/// with the command's output. Faithful port of TS
/// `executeShellCommandsInPrompt`.
///
/// `shell` comes from `.md` frontmatter (the author's choice), never from
/// settings; `None` means bash. Matches are processed in source order: all block
/// matches first, then inline matches (mirroring TS
/// `[...blockMatches, ...inlineMatches]`).
///
/// Returns the rewritten text, or [`ShellExpansionError::Malformed`] if any
/// command is denied or fails.
pub async fn execute_shell_commands_in_prompt(
    text: &str,
    ctx: &ShellExpansionCtx,
    slash_command_name: &str,
    shell: Option<FrontmatterShell>,
) -> Result<String, ShellExpansionError> {
    let mut result = text.to_string();

    // Same ordering as TS: block matches, then (gated) inline matches.
    let mut matches = extract_block_commands(text);
    if text.contains("!`") {
        matches.extend(extract_inline_commands(text));
    }

    for m in &matches {
        if m.command.is_empty() {
            continue;
        }
        // Permission check before executing.
        match ctx.permission_gate.check(&m.command, shell) {
            ShellPermissionDecision::Allow => {}
            ShellPermissionDecision::Deny { message } => {
                let _ = slash_command_name; // mirrors TS debug log; no-op here.
                let reason = message.unwrap_or_else(|| "Permission denied".to_string());
                return Err(ShellExpansionError::Malformed(format!(
                    "Shell command permission check failed for pattern \"{}\": {}",
                    m.full, reason
                )));
            }
        }

        let output = match ctx.runner.run(&m.command, shell).await {
            Ok(data) => format_bash_output(&data.stdout, &data.stderr, false),
            Err(e) => return Err(format_bash_error(&e, &m.full)),
        };

        // Manual splice replacement of the FIRST occurrence of `m.full`, so
        // `$`-laden output is inserted verbatim (TS function-replacer note).
        result = replace_first(&result, &m.full, &output);
    }

    Ok(result)
}

/// Replace the first occurrence of `needle` in `haystack` with `replacement`,
/// inserting `replacement` literally (no `$`-interpretation).
fn replace_first(haystack: &str, needle: &str, replacement: &str) -> String {
    match haystack.find(needle) {
        Some(idx) => {
            let mut out = String::with_capacity(haystack.len() + replacement.len());
            out.push_str(&haystack[..idx]);
            out.push_str(replacement);
            out.push_str(&haystack[idx + needle.len()..]);
            out
        }
        None => haystack.to_string(),
    }
}

/// Extract ` ```! … ``` ` block commands. Mirrors `/```!\s*\n?([\s\S]*?)\n?```/g`.
fn extract_block_commands(text: &str) -> Vec<ExtractedCommand> {
    const OPEN: &str = "```!";
    const FENCE: &str = "```";
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut search_from = 0;

    while let Some(rel) = text[search_from..].find(OPEN) {
        let open_start = search_from + rel;
        // `\s*` after `!`: consume whitespace (greedy). The optional `\n?` that
        // follows is subsumed — any leading newline is whitespace too. The
        // capture group `([\s\S]*?)` then starts after that whitespace run.
        let mut content_start = open_start + OPEN.len();
        while content_start < bytes.len() && is_ascii_ws(bytes[content_start]) {
            content_start += 1;
        }

        // Lazily find the closing ``` fence.
        let Some(close_rel) = text[content_start..].find(FENCE) else {
            // No closing fence: nothing more can match.
            break;
        };
        let close_at = content_start + close_rel;

        // The regex `\n?` before the fence trims one trailing newline from the
        // capture. `[\s\S]*?` is lazy, so the capture stops just before any
        // optional `\n` immediately preceding the fence.
        let mut capture_end = close_at;
        if capture_end > content_start && bytes[capture_end - 1] == b'\n' {
            capture_end -= 1;
            // A preceding `\r` is part of the line break in CRLF text; `\n?`
            // only trims `\n`, so leave `\r` (faithful to the JS regex).
        }

        let captured = &text[content_start..capture_end];
        let full = &text[open_start..close_at + FENCE.len()];
        out.push(ExtractedCommand {
            start: open_start,
            end: close_at + FENCE.len(),
            full: full.to_string(),
            command: captured.trim().to_string(),
        });
        search_from = close_at + FENCE.len();
    }
    out
}

/// Extract inline commands written as a bang immediately followed by a
/// backtick-quoted command. Mirrors the TS `INLINE_PATTERN` (a start-of-line or
/// whitespace lookbehind, then the bang, then one-or-more non-backtick chars
/// between backticks); the lookbehind is emulated by inspecting the preceding
/// char.
fn extract_inline_commands(text: &str) -> Vec<ExtractedCommand> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        // Find next `!` followed by a backtick.
        if bytes[i] == b'!' && bytes[i + 1] == b'`' {
            // Lookbehind (?<=^|\s): preceding char must be start-of-text or a
            // whitespace char.
            let preceded_ok = i == 0 || is_inline_ws(bytes[i - 1]);
            if preceded_ok {
                // One-or-more non-backtick chars, then a backtick.
                let body_start = i + 2;
                if let Some(rel) = text[body_start..].find('`') {
                    let body_end = body_start + rel;
                    if body_end > body_start {
                        let captured = &text[body_start..body_end];
                        let full = &text[i..=body_end];
                        out.push(ExtractedCommand {
                            start: i,
                            end: body_end + 1,
                            full: full.to_string(),
                            command: captured.trim().to_string(),
                        });
                        i = body_end + 1;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    out
}

/// `\s` for the block scanner — ASCII whitespace as JS regex `\s` recognises it
/// (space, tab, CR, LF, vertical tab, form feed).
fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c)
}

/// Whitespace for the inline lookbehind. JS `\s` also matches CR/LF, but the `m`
/// flag makes `^` match after a line break; either way a `!` at line start (or
/// after whitespace) qualifies, so the same ASCII-whitespace set is used.
fn is_inline_ws(b: u8) -> bool {
    is_ascii_ws(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records each command it runs and echoes `OUT[<command>]` as stdout.
    struct ScriptRunner {
        calls: Mutex<Vec<String>>,
    }
    impl ScriptRunner {
        fn echoing() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait]
    impl ShellRunner for ScriptRunner {
        async fn run(
            &self,
            command: &str,
            _shell: Option<FrontmatterShell>,
        ) -> Result<ShellOut, ShellRunError> {
            self.calls.lock().unwrap().push(command.to_string());
            Ok(ShellOut {
                stdout: format!("OUT[{command}]"),
                stderr: String::new(),
                interrupted: false,
            })
        }
    }

    struct AllowAll;
    impl ShellPermissionGate for AllowAll {
        fn check(&self, _c: &str, _s: Option<FrontmatterShell>) -> ShellPermissionDecision {
            ShellPermissionDecision::Allow
        }
    }

    struct DenyAll(Option<String>);
    impl ShellPermissionGate for DenyAll {
        fn check(&self, _c: &str, _s: Option<FrontmatterShell>) -> ShellPermissionDecision {
            ShellPermissionDecision::Deny {
                message: self.0.clone(),
            }
        }
    }

    fn ctx_with(
        runner: Arc<dyn ShellRunner>,
        gate: Arc<dyn ShellPermissionGate>,
    ) -> ShellExpansionCtx {
        ShellExpansionCtx {
            runner,
            permission_gate: gate,
        }
    }

    #[test]
    fn block_extraction_basic() {
        let cmds = extract_block_commands("before\n```!\necho hi\n```\nafter");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].command, "echo hi");
        assert_eq!(cmds[0].full, "```!\necho hi\n```");
    }

    #[test]
    fn block_extraction_inline_spaces() {
        // ```! echo hi ``` on one line.
        let cmds = extract_block_commands("```! echo hi ```");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].command, "echo hi");
    }

    #[test]
    fn inline_extraction_lookbehind() {
        // Leading space => match; mid-token `foo!`bar`` => no match.
        let cmds = extract_inline_commands("run !`echo hi` and foo!`nope`");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].command, "echo hi");
        assert_eq!(cmds[0].full, "!`echo hi`");
    }

    #[test]
    fn inline_extraction_start_of_text() {
        let cmds = extract_inline_commands("!`echo hi`");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].command, "echo hi");
    }

    #[tokio::test]
    async fn gated_fast_path_skips_inline_scan() {
        // No "!`" substring => inline pattern not scanned, only block matches.
        let runner = Arc::new(ScriptRunner::echoing());
        let runner_dyn: Arc<dyn ShellRunner> = runner.clone();
        let ctx = ctx_with(runner_dyn, Arc::new(AllowAll));
        let out = execute_shell_commands_in_prompt("no shell here", &ctx, "/x", None)
            .await
            .unwrap();
        assert_eq!(out, "no shell here");
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn inline_and_block_replacement() {
        let runner = Arc::new(ScriptRunner::echoing());
        let runner_dyn: Arc<dyn ShellRunner> = runner.clone();
        let ctx = ctx_with(runner_dyn, Arc::new(AllowAll));
        let text = "A ```!\nblockcmd\n``` B !`inlinecmd` C";
        let out = execute_shell_commands_in_prompt(text, &ctx, "/x", None)
            .await
            .unwrap();
        assert_eq!(out, "A OUT[blockcmd] B OUT[inlinecmd] C");
        // Block first, then inline (source order within each group).
        assert_eq!(
            *runner.calls.lock().unwrap(),
            vec!["blockcmd".to_string(), "inlinecmd".to_string()]
        );
    }

    #[tokio::test]
    async fn multi_match_replacement_order() {
        let runner = Arc::new(ScriptRunner::echoing());
        let runner_dyn: Arc<dyn ShellRunner> = runner.clone();
        let ctx = ctx_with(runner_dyn, Arc::new(AllowAll));
        let text = "!`one` then !`two` then !`three`";
        let out = execute_shell_commands_in_prompt(text, &ctx, "/x", None)
            .await
            .unwrap();
        assert_eq!(out, "OUT[one] then OUT[two] then OUT[three]");
        assert_eq!(
            *runner.calls.lock().unwrap(),
            vec!["one".to_string(), "two".to_string(), "three".to_string()]
        );
    }

    #[tokio::test]
    async fn permission_deny_errors() {
        let runner = Arc::new(ScriptRunner::echoing());
        let runner_dyn: Arc<dyn ShellRunner> = runner.clone();
        let ctx = ctx_with(
            runner_dyn,
            Arc::new(DenyAll(Some("not allowed".to_string()))),
        );
        let err = execute_shell_commands_in_prompt("x !`rm -rf /` y", &ctx, "/danger", None)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ShellExpansionError::Malformed(
                "Shell command permission check failed for pattern \"!`rm -rf /`\": not allowed"
                    .to_string()
            )
        );
        // Denied before run: command never executed.
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn permission_deny_default_message() {
        let runner: Arc<dyn ShellRunner> = Arc::new(ScriptRunner::echoing());
        let ctx = ctx_with(runner, Arc::new(DenyAll(None)));
        let err = execute_shell_commands_in_prompt("!`x`", &ctx, "/c", None)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ShellExpansionError::Malformed(
                "Shell command permission check failed for pattern \"!`x`\": Permission denied"
                    .to_string()
            )
        );
    }

    #[tokio::test]
    async fn run_failure_formats_error() {
        struct FailRunner;
        #[async_trait]
        impl ShellRunner for FailRunner {
            async fn run(
                &self,
                _c: &str,
                _s: Option<FrontmatterShell>,
            ) -> Result<ShellOut, ShellRunError> {
                Err(ShellRunError {
                    stdout: String::new(),
                    stderr: "boom".to_string(),
                    interrupted: false,
                    generic_message: None,
                })
            }
        }
        let ctx = ctx_with(Arc::new(FailRunner), Arc::new(AllowAll));
        let err = execute_shell_commands_in_prompt("!`bad`", &ctx, "/c", None)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ShellExpansionError::Malformed(
                "Shell command failed for pattern \"!`bad`\": [stderr]\nboom".to_string()
            )
        );
    }

    #[tokio::test]
    async fn stderr_included_in_output() {
        struct StderrRunner;
        #[async_trait]
        impl ShellRunner for StderrRunner {
            async fn run(
                &self,
                _c: &str,
                _s: Option<FrontmatterShell>,
            ) -> Result<ShellOut, ShellRunError> {
                Ok(ShellOut {
                    stdout: "hello".to_string(),
                    stderr: "warn".to_string(),
                    interrupted: false,
                })
            }
        }
        let ctx = ctx_with(Arc::new(StderrRunner), Arc::new(AllowAll));
        let out = execute_shell_commands_in_prompt("!`x`", &ctx, "/c", None)
            .await
            .unwrap();
        assert_eq!(out, "hello\n[stderr]\nwarn");
    }

    #[tokio::test]
    async fn empty_inline_command_skipped() {
        // !`   ` trims to empty -> not run, full span left untouched.
        let runner = Arc::new(ScriptRunner::echoing());
        let runner_dyn: Arc<dyn ShellRunner> = runner.clone();
        let ctx = ctx_with(runner_dyn, Arc::new(AllowAll));
        let out = execute_shell_commands_in_prompt("keep !`   ` keep", &ctx, "/c", None)
            .await
            .unwrap();
        assert_eq!(out, "keep !`   ` keep");
        assert!(runner.calls.lock().unwrap().is_empty());
    }
}
