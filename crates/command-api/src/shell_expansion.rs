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
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::fmt;
use std::sync::Arc;

type UJP = Utf16JsonProjection;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellExpansionError {
    /// Permission denied, execution failure, or interruption — the message is
    /// pre-formatted to match the TS `MalformedCommandError` text.
    Malformed(UJP),
}

impl fmt::Display for ShellExpansionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(message) => f.write_str(message.string_at("").unwrap_or_default()),
        }
    }
}

impl std::error::Error for ShellExpansionError {}

impl ShellExpansionError {
    /// Exact source diagnostic; display formatting is reserved for UI/log sinks.
    #[must_use]
    pub fn source_message(&self) -> UJP {
        match self {
            Self::Malformed(message) => message.clone(),
        }
    }
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
        message: Option<UJP>,
    },
}

/// Per-command permission gate. Injected so `command-api` does not depend on the
/// permission subsystem. Mirrors the TS `hasPermissionsToUseTool(shellTool,
/// { command }, …)` call.
pub trait ShellPermissionGate: Send + Sync {
    /// Decide whether `command` (routed through `shell`) may execute.
    fn check(&self, command: &UJP, shell: Option<FrontmatterShell>) -> ShellPermissionDecision;
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
        command: &UJP,
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
    /// UTF-16 code-unit offset of the whole match (`match[0]`) within `text`.
    start: usize,
    /// Exclusive end code-unit offset of the whole match.
    end: usize,
    /// The matched substring (`match[0]`), used as the literal replacement key.
    full: Vec<u16>,
    /// The trimmed exact source command (`match[1]?.trim()`).
    command: UJP,
}

/// Format combined stdout/stderr exactly like TS `formatBashOutput`.
fn format_bash_output(stdout: &str, stderr: &str, inline: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    let out_trimmed = stdout.trim_matches(lingxi_core::host::effort::javascript_whitespace);
    if !out_trimmed.is_empty() {
        parts.push(out_trimmed.to_string());
    }
    let err_trimmed = stderr.trim_matches(lingxi_core::host::effort::javascript_whitespace);
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
fn format_bash_error(err: &ShellRunError, pattern: &[u16]) -> ShellExpansionError {
    if let Some(message) = &err.generic_message {
        return malformed_text(&format!("[Error]\n{message}"));
    }
    if err.interrupted {
        return malformed_with_pattern(
            "Shell command interrupted for pattern \"",
            pattern,
            "\": [Command interrupted]",
        );
    }
    let output = format_bash_output(&err.stdout, &err.stderr, false);
    malformed_with_pattern_and_text(
        "Shell command failed for pattern \"",
        pattern,
        "\": ",
        &output,
    )
}

/// Execute embedded shell commands while keeping the prompt and extracted
/// command text in exact UTF-16 units. Only the injected process runner crosses
/// the host OS command-string boundary.
pub async fn execute_shell_commands_in_prompt(
    text: &UJP,
    ctx: &ShellExpansionCtx,
    slash_command_name: &UJP,
    shell: Option<FrontmatterShell>,
) -> Result<UJP, ShellExpansionError> {
    let source_units = text
        .string_units("")
        .expect("shell expansion input must be a root-string projection");
    let mut result = source_units.to_vec();

    // Same ordering as TS: block matches, then (gated) inline matches.
    let mut matches = extract_block_commands(&source_units);
    if source_units
        .windows(2)
        .any(|window| window == [b'!' as u16, b'`' as u16])
    {
        matches.extend(extract_inline_commands(&source_units));
    }

    for command in &matches {
        if command
            .command
            .string_units("")
            .is_some_and(|units| units.is_empty())
        {
            continue;
        }
        match ctx.permission_gate.check(&command.command, shell) {
            ShellPermissionDecision::Allow => {}
            ShellPermissionDecision::Deny { message } => {
                let _ = slash_command_name; // mirrors TS debug log; no-op here.
                let reason = message.unwrap_or_else(|| UJP::from("Permission denied"));
                return Err(malformed_with_pattern_and_source(
                    "Shell command permission check failed for pattern \"",
                    &command.full,
                    "\": ",
                    &reason,
                ));
            }
        }

        let output = match ctx.runner.run(&command.command, shell).await {
            Ok(data) => UJP::from(format_bash_output(&data.stdout, &data.stderr, false)),
            Err(error) => return Err(format_bash_error(&error, &command.full)),
        };
        result = replace_first_units(
            &result,
            &command.full,
            &output.string_units("").unwrap_or_default(),
        );
    }

    Ok(source_from_units(result))
}

fn replace_first_units(haystack: &[u16], needle: &[u16], replacement: &[u16]) -> Vec<u16> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let Some(index) = haystack
        .windows(needle.len())
        .position(|window| window == needle)
    else {
        return haystack.to_vec();
    };
    let mut output = Vec::with_capacity(haystack.len() - needle.len() + replacement.len());
    output.extend_from_slice(&haystack[..index]);
    output.extend_from_slice(replacement);
    output.extend_from_slice(&haystack[index + needle.len()..]);
    output
}

fn projection_from_units(units: Vec<u16>) -> UJP {
    let display = String::from_utf16_lossy(&units);
    UJP::root_string(display, units)
        .expect("display projection is derived from the same UTF-16 units")
}

fn source_from_units(units: Vec<u16>) -> UJP {
    projection_from_units(units)
}

fn malformed_text(text: &str) -> ShellExpansionError {
    ShellExpansionError::Malformed(UJP::from(text))
}

fn malformed_with_pattern(prefix: &str, pattern: &[u16], suffix: &str) -> ShellExpansionError {
    let units = [
        prefix.encode_utf16().collect::<Vec<_>>(),
        pattern.to_vec(),
        suffix.encode_utf16().collect::<Vec<_>>(),
    ]
    .concat();
    ShellExpansionError::Malformed(source_from_units(units))
}

fn malformed_with_pattern_and_text(
    prefix: &str,
    pattern: &[u16],
    middle: &str,
    suffix: &str,
) -> ShellExpansionError {
    let units = [
        prefix.encode_utf16().collect::<Vec<_>>(),
        pattern.to_vec(),
        middle.encode_utf16().collect::<Vec<_>>(),
        suffix.encode_utf16().collect::<Vec<_>>(),
    ]
    .concat();
    ShellExpansionError::Malformed(source_from_units(units))
}

fn malformed_with_pattern_and_source(
    prefix: &str,
    pattern: &[u16],
    middle: &str,
    suffix: &UJP,
) -> ShellExpansionError {
    let units = [
        prefix.encode_utf16().collect::<Vec<_>>(),
        pattern.to_vec(),
        middle.encode_utf16().collect::<Vec<_>>(),
        suffix.string_units("").unwrap_or_default().to_vec(),
    ]
    .concat();
    ShellExpansionError::Malformed(source_from_units(units))
}

/// Extract ` ```! … ``` ` block commands using Native regex whitespace and
/// UTF-16 code-unit spans.
fn extract_block_commands(text: &[u16]) -> Vec<ExtractedCommand> {
    const OPEN: &[u16] = &[b'`' as u16, b'`' as u16, b'`' as u16, b'!' as u16];
    const FENCE: &[u16] = &[b'`' as u16, b'`' as u16, b'`' as u16];
    let mut out = Vec::new();
    let mut search_from = 0;

    while let Some(open_start) = find_units(text, OPEN, search_from) {
        let mut content_start = open_start + OPEN.len();
        while content_start < text.len() && is_js_whitespace_unit(text[content_start]) {
            content_start += 1;
        }
        let Some(close_at) = find_units(text, FENCE, content_start) else {
            break;
        };

        let mut capture_end = close_at;
        if capture_end > content_start && text[capture_end - 1] == b'\n' as u16 {
            capture_end -= 1;
        }
        let body = trim_js_whitespace_units(&text[content_start..capture_end]).to_vec();
        let end = close_at + FENCE.len();
        out.push(ExtractedCommand {
            start: open_start,
            end,
            full: text[open_start..end].to_vec(),
            command: source_from_units(body),
        });
        search_from = end;
    }
    out
}

/// Extract inline commands with Native `(?<=^|\\s)!` lookbehind semantics.
fn extract_inline_commands(text: &[u16]) -> Vec<ExtractedCommand> {
    let mut out = Vec::new();
    let mut index = 0;
    while index + 1 < text.len() {
        if text[index] == b'!' as u16 && text[index + 1] == b'`' as u16 {
            let preceded_ok = index == 0 || is_js_whitespace_unit(text[index - 1]);
            if preceded_ok {
                let body_start = index + 2;
                if let Some(body_end) = text[body_start..]
                    .iter()
                    .position(|unit| *unit == b'`' as u16)
                    .map(|offset| body_start + offset)
                {
                    if body_end > body_start {
                        let end = body_end + 1;
                        out.push(ExtractedCommand {
                            start: index,
                            end,
                            full: text[index..end].to_vec(),
                            command: source_from_units(
                                trim_js_whitespace_units(&text[body_start..body_end]).to_vec(),
                            ),
                        });
                        index = end;
                        continue;
                    }
                }
            }
        }
        index += 1;
    }
    out
}

fn find_units(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

fn is_js_whitespace_unit(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(lingxi_core::host::effort::javascript_whitespace)
}

fn trim_js_whitespace_units(input: &[u16]) -> &[u16] {
    let mut start = 0;
    let mut end = input.len();
    while start < end && is_js_whitespace_unit(input[start]) {
        start += 1;
    }
    while end > start && is_js_whitespace_unit(input[end - 1]) {
        end -= 1;
    }
    &input[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct ScriptRunner {
        calls: Mutex<Vec<UJP>>,
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
            command: &UJP,
            _shell: Option<FrontmatterShell>,
        ) -> Result<ShellOut, ShellRunError> {
            self.calls.lock().unwrap().push(command.clone());
            Ok(ShellOut {
                stdout: format!("OUT[{}]", command.string_at("").unwrap_or_default()),
                stderr: String::new(),
                interrupted: false,
            })
        }
    }

    struct FixedRunner {
        calls: Mutex<Vec<UJP>>,
    }

    #[async_trait]
    impl ShellRunner for FixedRunner {
        async fn run(
            &self,
            command: &UJP,
            _shell: Option<FrontmatterShell>,
        ) -> Result<ShellOut, ShellRunError> {
            self.calls.lock().unwrap().push(command.clone());
            Ok(ShellOut {
                stdout: "done".into(),
                stderr: String::new(),
                interrupted: false,
            })
        }
    }

    struct AllowAll;
    impl ShellPermissionGate for AllowAll {
        fn check(
            &self,
            _command: &UJP,
            _shell: Option<FrontmatterShell>,
        ) -> ShellPermissionDecision {
            ShellPermissionDecision::Allow
        }
    }

    struct DenyAll(Option<UJP>);
    impl ShellPermissionGate for DenyAll {
        fn check(
            &self,
            _command: &UJP,
            _shell: Option<FrontmatterShell>,
        ) -> ShellPermissionDecision {
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

    async fn expand(text: &str, ctx: &ShellExpansionCtx) -> Result<UJP, ShellExpansionError> {
        execute_shell_commands_in_prompt(&UJP::from(text), ctx, &UJP::from("/x"), None).await
    }

    #[test]
    fn block_extraction_uses_js_whitespace_and_preserves_code_units() {
        let text = "before\n```!\u{00a0}echo hi\n```\nafter";
        let units = text.encode_utf16().collect::<Vec<_>>();
        let commands = extract_block_commands(&units);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command.string_at(""), Some("echo hi"));
        assert_eq!(
            commands[0].full,
            "```!\u{00a0}echo hi\n```"
                .encode_utf16()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn inline_extraction_uses_js_whitespace_lookbehind() {
        let text = "run\u{2003}!`echo hi` and foo!`nope`";
        let units = text.encode_utf16().collect::<Vec<_>>();
        let commands = extract_inline_commands(&units);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command.string_at(""), Some("echo hi"));
        assert_eq!(
            commands[0].full,
            "!`echo hi`".encode_utf16().collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn no_marker_preserves_opaque_source_without_running() {
        let runner = Arc::new(ScriptRunner::echoing());
        let ctx = ctx_with(runner.clone(), Arc::new(AllowAll));
        let units = [
            "prefix".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800],
            " suffix".encode_utf16().collect(),
        ]
        .concat();
        let text = UJP::root_string(String::from_utf16_lossy(&units), units.clone()).unwrap();
        let output = execute_shell_commands_in_prompt(&text, &ctx, &"/x".into(), None)
            .await
            .unwrap();
        assert_eq!(output.string_units(""), Some(units));
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn block_and_inline_replacement_keeps_native_order() {
        let runner = Arc::new(ScriptRunner::echoing());
        let ctx = ctx_with(runner.clone(), Arc::new(AllowAll));
        let output = expand("A ```!\nblockcmd\n``` B !`inlinecmd` C", &ctx)
            .await
            .unwrap();
        assert_eq!(
            output.string_at(""),
            Some("A OUT[blockcmd] B OUT[inlinecmd] C")
        );
        assert_eq!(
            runner
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|value| value.string_at(""))
                .collect::<Vec<_>>(),
            [Some("blockcmd"), Some("inlinecmd")]
        );
    }

    #[tokio::test]
    async fn repeated_matches_replace_first_occurrences_in_native_order() {
        let runner = Arc::new(ScriptRunner::echoing());
        let ctx = ctx_with(runner.clone(), Arc::new(AllowAll));
        let output = expand("!`one` then !`two` then !`three`", &ctx)
            .await
            .unwrap();
        assert_eq!(
            output.string_at(""),
            Some("OUT[one] then OUT[two] then OUT[three]")
        );
        assert_eq!(runner.calls.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn source_extraction_retains_surrogates_around_and_inside_shell_command() {
        let runner = Arc::new(FixedRunner {
            calls: Mutex::new(Vec::new()),
        });
        let ctx = ctx_with(runner.clone(), Arc::new(AllowAll));
        let units = [
            "before".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800],
            " !`echo ".encode_utf16().collect(),
            vec![0xdfff],
            "` after".encode_utf16().collect(),
            vec![0xd800],
        ]
        .concat();
        let text = UJP::root_string(String::from_utf16_lossy(&units), units.clone()).unwrap();
        let output = execute_shell_commands_in_prompt(&text, &ctx, &"/exact�".into(), None)
            .await
            .unwrap();
        let expected = [
            "before".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800],
            " done after".encode_utf16().collect(),
            vec![0xd800],
        ]
        .concat();
        assert_eq!(output.string_units(""), Some(expected));
        assert_eq!(
            runner.calls.lock().unwrap()[0].string_units(""),
            Some(["echo ".encode_utf16().collect::<Vec<_>>(), vec![0xdfff]].concat())
        );
    }

    #[tokio::test]
    async fn permission_deny_keeps_exact_matched_pattern_and_reason() {
        let runner = Arc::new(ScriptRunner::echoing());
        let ctx = ctx_with(
            runner.clone(),
            Arc::new(DenyAll(Some("not allowed".into()))),
        );
        let error = expand("x !`rm -rf /` y", &ctx).await.unwrap_err();
        assert_eq!(
            error.source_message().string_at(""),
            Some("Shell command permission check failed for pattern \"!`rm -rf /`\": not allowed")
        );
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn permission_diagnostic_retains_unpaired_command_and_reason_units() {
        let command_units = [
            "!`run ".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800],
            vec![b'`' as u16],
        ]
        .concat();
        let reason = UJP::root_string("reason�".into(), vec![b'r' as u16, 0xdfff]).unwrap();
        let text = UJP::root_string(
            String::from_utf16_lossy(&command_units),
            command_units.clone(),
        )
        .unwrap();
        let ctx = ctx_with(
            Arc::new(ScriptRunner::echoing()),
            Arc::new(DenyAll(Some(reason))),
        );

        let error = execute_shell_commands_in_prompt(&text, &ctx, &"/x".into(), None)
            .await
            .unwrap_err();
        let expected = [
            "Shell command permission check failed for pattern \""
                .encode_utf16()
                .collect::<Vec<_>>(),
            command_units,
            "\": reason".encode_utf16().collect(),
            vec![0xdfff],
        ]
        .concat();
        assert_eq!(error.source_message().string_units(""), Some(expected));
    }

    #[tokio::test]
    async fn execution_failure_and_stderr_keep_existing_formatting() {
        struct FailRunner;
        #[async_trait]
        impl ShellRunner for FailRunner {
            async fn run(
                &self,
                _command: &UJP,
                _shell: Option<FrontmatterShell>,
            ) -> Result<ShellOut, ShellRunError> {
                Err(ShellRunError {
                    stdout: String::new(),
                    stderr: "boom".into(),
                    interrupted: false,
                    generic_message: None,
                })
            }
        }
        let failed = ctx_with(Arc::new(FailRunner), Arc::new(AllowAll));
        let error = expand("!`bad`", &failed).await.unwrap_err();
        assert_eq!(
            error.source_message().string_at(""),
            Some("Shell command failed for pattern \"!`bad`\": [stderr]\nboom")
        );

        struct StderrRunner;
        #[async_trait]
        impl ShellRunner for StderrRunner {
            async fn run(
                &self,
                _command: &UJP,
                _shell: Option<FrontmatterShell>,
            ) -> Result<ShellOut, ShellRunError> {
                Ok(ShellOut {
                    stdout: " hello ".into(),
                    stderr: " warn ".into(),
                    interrupted: false,
                })
            }
        }
        let output = expand(
            "!`x`",
            &ctx_with(Arc::new(StderrRunner), Arc::new(AllowAll)),
        )
        .await
        .unwrap();
        assert_eq!(output.string_at(""), Some("hello\n[stderr]\nwarn"));
    }

    #[tokio::test]
    async fn empty_command_is_skipped_and_left_verbatim() {
        let runner = Arc::new(ScriptRunner::echoing());
        let ctx = ctx_with(runner.clone(), Arc::new(AllowAll));
        let output = expand("keep !`\u{00a0}` keep", &ctx).await.unwrap();
        assert_eq!(output.string_at(""), Some("keep !`\u{00a0}` keep"));
        assert!(runner.calls.lock().unwrap().is_empty());
    }
}
