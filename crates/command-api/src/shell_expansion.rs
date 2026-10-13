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

type Ujp = Utf16JsonProjection;

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
    Malformed(Ujp),
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
    pub fn source_message(&self) -> Ujp {
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
        message: Option<Ujp>,
    },
}

/// Per-command permission gate. Injected so `command-api` does not depend on the
/// permission subsystem. Mirrors the TS `hasPermissionsToUseTool(shellTool,
/// { command }, …)` call.
pub trait ShellPermissionGate: Send + Sync {
    /// Decide whether `command` (routed through `shell`) may execute.
    fn check(&self, command: &Ujp, shell: Option<FrontmatterShell>) -> ShellPermissionDecision;
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
        command: &Ujp,
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

/// One live cwd snapshot and its Git-worktree status.
pub struct GitWorktreeContext {
    /// Exact source text of the cwd queried by the host.
    pub cwd: Ujp,
    /// Whether Git identifies this cwd as inside a worktree.
    pub inside: bool,
}

/// Builds the per-command [`ShellExpansionCtx`] and supplies host context while
/// keeping `command-api` independent of the concrete tool and policy crates.
/// The context receives that command's declared allowed-tools list in its
/// permission gate.
///
/// This mirrors claude-code building a fresh `toolPermissionContext` per
/// `getPromptForCommand`: the command's allowed-tools are injected into its
/// permission context instead of mutating shared boot policy. The concrete
/// provider (holding the live `PermissionPolicy` and runner dependencies) lives
/// in `tool-api` and is constructed at each composition root. Dispatchers query
/// [`Self::git_worktree_context`] when needed, then call [`Self::build`] before
/// running [`execute_shell_commands_in_prompt`].
#[async_trait]
pub trait ShellExpansionProvider: Send + Sync {
    /// Query the current host cwd, matching Native's Git-worktree precondition.
    /// A failed Git query is outside a worktree; this executes no prompt text.
    async fn git_worktree_context(&self) -> GitWorktreeContext;

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
    command: Ujp,
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
/// command text in exact UTF-16 units. Matches are admitted and run concurrently
/// like Native's `Promise.all`; successful completions replace the first
/// remaining occurrence of their raw pattern in completion order. Only the
/// injected process runner crosses the host OS command-string boundary.
pub async fn execute_shell_commands_in_prompt(
    text: &Ujp,
    ctx: &ShellExpansionCtx,
    slash_command_name: &Ujp,
    shell: Option<FrontmatterShell>,
) -> Result<Ujp, ShellExpansionError> {
    let source_units = text
        .string_units("")
        .expect("shell expansion input must be a root-string projection");
    // Same ordering as TS: block matches, then (gated) inline matches.
    let mut matches = extract_block_commands(&source_units);
    if source_units
        .windows(2)
        .any(|window| window == [b'!' as u16, b'`' as u16])
    {
        matches.extend(extract_inline_commands(&source_units));
    }

    // Native invokes `Fu` synchronously while mapping matches, in match order,
    // before awaiting the resulting promises. This gate is synchronous in the
    // current host contract, so capture every decision in that same order
    // before starting any runner tasks (including decisions following a deny).
    let matches = matches
        .into_iter()
        .filter(|command| {
            !command
                .command
                .string_units("")
                .is_some_and(|units| units.is_empty())
        })
        .map(|command| {
            let decision = ctx.permission_gate.check(&command.command, shell);
            (command, decision)
        })
        .collect::<Vec<_>>();

    let result = Arc::new(std::sync::Mutex::new(source_units.to_vec()));
    let mut tasks = tokio::task::JoinSet::new();
    let mut previous_launch = None;
    for (command, decision) in matches {
        let (launch_next, launch_next_rx) = tokio::sync::oneshot::channel();
        let launch_previous = previous_launch.take();
        previous_launch = Some(launch_next_rx);
        let command_name = slash_command_name.clone();
        let runner = ctx.runner.clone();
        let result = result.clone();
        tasks.spawn(async move {
            if let Some(launch_previous) = launch_previous {
                let _ = launch_previous.await;
            }

            match decision {
                ShellPermissionDecision::Allow => {}
                ShellPermissionDecision::Deny { message } => {
                    let _ = command_name; // mirrors TS debug log; no-op here.
                    let _ = launch_next.send(());
                    let reason = message.unwrap_or_else(|| Ujp::from("Permission denied"));
                    return Err(malformed_with_pattern_and_source(
                        "Shell command permission check failed for pattern \"",
                        &command.full,
                        "\": ",
                        &reason,
                    ));
                }
            }

            // Polling once starts each runner in source match order, then the
            // launch baton is passed while any asynchronous process work stays
            // concurrent. Native's ready permission continuations are queued
            // FIFO; only the subsequent I/O completion order is nondeterministic.
            let mut run = runner.run(&command.command, shell);
            let first_poll = std::future::poll_fn(|cx| {
                use std::future::Future as _;
                match run.as_mut().poll(cx) {
                    std::task::Poll::Ready(result) => std::task::Poll::Ready(Some(result)),
                    std::task::Poll::Pending => std::task::Poll::Ready(None),
                }
            })
            .await;
            let _ = launch_next.send(());
            let data = match first_poll {
                Some(result) => result,
                None => run.await,
            }
            .map_err(|error| format_bash_error(&error, &command.full))?;
            let output = format_bash_output(&data.stdout, &data.stderr, false);
            let mut current = result
                .lock()
                .expect("shell expansion result lock is not poisoned");
            *current = replace_first_units(
                &current,
                &command.full,
                &output.encode_utf16().collect::<Vec<_>>(),
            );
            Ok::<(), ShellExpansionError>(())
        });
    }

    while let Some(completion) = tasks.join_next().await {
        match completion {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                // JavaScript Promise.all rejects on the first failed promise,
                // but does not cancel the other already-started promises.
                tasks.detach_all();
                return Err(error);
            }
            Err(error) => {
                tasks.detach_all();
                if error.is_panic() {
                    std::panic::resume_unwind(error.into_panic());
                }
                panic!("embedded shell expansion task was cancelled unexpectedly");
            }
        }
    }

    let result = Arc::try_unwrap(result)
        .expect("all shell expansion tasks completed before reading the result")
        .into_inner()
        .expect("shell expansion result lock is not poisoned");
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

fn projection_from_units(units: Vec<u16>) -> Ujp {
    let display = String::from_utf16_lossy(&units);
    Ujp::root_string(display, units)
        .expect("display projection is derived from the same UTF-16 units")
}

fn source_from_units(units: Vec<u16>) -> Ujp {
    projection_from_units(units)
}

fn malformed_text(text: &str) -> ShellExpansionError {
    ShellExpansionError::Malformed(Ujp::from(text))
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
    suffix: &Ujp,
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
    use tokio::sync::{Semaphore, watch};

    struct ScriptRunner {
        calls: Mutex<Vec<Ujp>>,
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
            command: &Ujp,
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
        calls: Mutex<Vec<Ujp>>,
    }

    #[async_trait]
    impl ShellRunner for FixedRunner {
        async fn run(
            &self,
            command: &Ujp,
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
            _command: &Ujp,
            _shell: Option<FrontmatterShell>,
        ) -> ShellPermissionDecision {
            ShellPermissionDecision::Allow
        }
    }

    struct DenyAll(Option<Ujp>);
    impl ShellPermissionGate for DenyAll {
        fn check(
            &self,
            _command: &Ujp,
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

    async fn expand(text: &str, ctx: &ShellExpansionCtx) -> Result<Ujp, ShellExpansionError> {
        execute_shell_commands_in_prompt(&Ujp::from(text), ctx, &Ujp::from("/x"), None).await
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
        let text = Ujp::root_string(String::from_utf16_lossy(&units), units.clone()).unwrap();
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

    struct ControlledRunner {
        started: watch::Sender<Vec<String>>,
        finished: watch::Sender<Vec<usize>>,
        next_call: std::sync::atomic::AtomicUsize,
        release: [Semaphore; 2],
    }

    impl ControlledRunner {
        fn new() -> (
            Arc<Self>,
            watch::Receiver<Vec<String>>,
            watch::Receiver<Vec<usize>>,
        ) {
            let (started, started_rx) = watch::channel(Vec::<String>::new());
            let (finished, finished_rx) = watch::channel(Vec::new());
            (
                Arc::new(Self {
                    started,
                    finished,
                    next_call: std::sync::atomic::AtomicUsize::new(0),
                    release: [Semaphore::new(0), Semaphore::new(0)],
                }),
                started_rx,
                finished_rx,
            )
        }
    }

    #[async_trait]
    impl ShellRunner for ControlledRunner {
        async fn run(
            &self,
            command: &Ujp,
            _shell: Option<FrontmatterShell>,
        ) -> Result<ShellOut, ShellRunError> {
            let call = self
                .next_call
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.started.send_modify(|calls| {
                calls.push(command.string_at("").unwrap_or_default().to_owned())
            });
            self.release[call]
                .acquire()
                .await
                .expect("test runner semaphore remains open")
                .forget();
            self.finished.send_modify(|calls| calls.push(call));
            Ok(ShellOut {
                stdout: format!("OUT[{call}]"),
                stderr: String::new(),
                interrupted: false,
            })
        }
    }

    async fn wait_for_len<T>(receiver: &mut watch::Receiver<Vec<T>>, expected: usize) {
        loop {
            if receiver.borrow_and_update().len() >= expected {
                return;
            }
            receiver
                .changed()
                .await
                .expect("test watch sender remains open");
        }
    }

    #[tokio::test]
    async fn duplicate_raw_patterns_replace_first_remaining_match_in_completion_order() {
        let (runner, mut started, mut finished) = ControlledRunner::new();
        let ctx = Arc::new(ctx_with(runner.clone(), Arc::new(AllowAll)));
        let text = Ujp::from("!`same` and !`same`");
        let execution = tokio::spawn(async move {
            execute_shell_commands_in_prompt(&text, &ctx, &Ujp::from("/x"), None).await
        });

        // Both calls must have reached the runner while neither is released.
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_len(&mut started, 2),
        )
        .await
        .expect("shell command calls start concurrently");

        // Resolve the second promise first. Its output replaces the first raw
        // occurrence; the first promise then replaces the remaining one.
        runner.release[1].add_permits(1);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_len(&mut finished, 1),
        )
        .await
        .expect("second shell call completes before the first is released");
        runner.release[0].add_permits(1);

        let output = execution.await.unwrap().unwrap();
        assert_eq!(output.string_at(""), Some("OUT[1] and OUT[0]"));
    }

    struct RecordingAllowGate(Arc<Mutex<Vec<String>>>);

    impl ShellPermissionGate for RecordingAllowGate {
        fn check(
            &self,
            command: &Ujp,
            _shell: Option<FrontmatterShell>,
        ) -> ShellPermissionDecision {
            self.0
                .lock()
                .unwrap()
                .push(command.string_at("").unwrap_or_default().to_owned());
            ShellPermissionDecision::Allow
        }
    }

    #[tokio::test]
    async fn permission_and_runner_start_order_matches_native_block_then_inline_order() {
        let runner = Arc::new(ScriptRunner::echoing());
        let permission_checks = Arc::new(Mutex::new(Vec::new()));
        let ctx = ctx_with(
            runner.clone(),
            Arc::new(RecordingAllowGate(permission_checks.clone())),
        );
        let output = expand(
            "!`inline-first`\n```!\nblock-command\n```\n!`inline-last`",
            &ctx,
        )
        .await
        .unwrap();

        assert_eq!(
            output.string_at(""),
            Some("OUT[inline-first]\nOUT[block-command]\nOUT[inline-last]")
        );
        let expected = vec![
            "block-command".to_owned(),
            "inline-first".to_owned(),
            "inline-last".to_owned(),
        ];
        let checks = permission_checks.lock().unwrap();
        assert_eq!(*checks, expected);
        let starts = runner.calls.lock().unwrap();
        let starts = starts
            .iter()
            .map(|command| command.string_at("").unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(starts, expected);
    }

    struct DenyOneAndRecord {
        checked: watch::Sender<Vec<String>>,
    }

    impl ShellPermissionGate for DenyOneAndRecord {
        fn check(
            &self,
            command: &Ujp,
            _shell: Option<FrontmatterShell>,
        ) -> ShellPermissionDecision {
            let command = command.string_at("").unwrap_or_default().to_owned();
            self.checked
                .send_modify(|commands| commands.push(command.clone()));
            if command == "deny" {
                ShellPermissionDecision::Deny {
                    message: Some(Ujp::from("denied by test")),
                }
            } else {
                ShellPermissionDecision::Allow
            }
        }
    }

    #[tokio::test]
    async fn permission_rejection_returns_early_without_cancelling_started_sibling() {
        let (runner, mut started, mut finished) = ControlledRunner::new();
        let (checked_tx, mut checked) = watch::channel(Vec::new());
        let gate = Arc::new(DenyOneAndRecord {
            checked: checked_tx,
        });
        let ctx = Arc::new(ctx_with(runner.clone(), gate));
        let text = Ujp::from("!`slow` then !`other` then !`deny`");
        let execution = tokio::spawn(async move {
            execute_shell_commands_in_prompt(&text, &ctx, &Ujp::from("/x"), None).await
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_len(&mut checked, 3),
        )
        .await
        .expect("each command receives its own permission check");
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_len(&mut started, 2),
        )
        .await
        .expect("permitted sibling started before permission rejection is observed");

        let error = tokio::time::timeout(std::time::Duration::from_secs(1), execution)
            .await
            .expect("permission failure rejects without waiting for the sibling")
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.source_message().string_at(""),
            Some("Shell command permission check failed for pattern \"!`deny`\": denied by test")
        );
        assert_eq!(
            checked.borrow_and_update().clone(),
            vec!["slow".to_owned(), "other".to_owned(), "deny".to_owned()]
        );
        assert_eq!(
            started.borrow_and_update().clone(),
            vec!["slow".to_owned(), "other".to_owned()]
        );

        // Promise.all rejection does not cancel already-started promises.
        runner.release[0].add_permits(1);
        runner.release[1].add_permits(1);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_len(&mut finished, 2),
        )
        .await
        .expect("the detached siblings finish after the outer call rejects");
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
        let text = Ujp::root_string(String::from_utf16_lossy(&units), units.clone()).unwrap();
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
        let reason_units = ["reason".encode_utf16().collect::<Vec<_>>(), vec![0xdfff]].concat();
        let reason =
            Ujp::root_string(String::from_utf16_lossy(&reason_units), reason_units).unwrap();
        let text = Ujp::root_string(
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
                _command: &Ujp,
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
                _command: &Ujp,
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
