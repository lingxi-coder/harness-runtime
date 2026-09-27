//! Header + footer constants — byte-locked from claude-code prompts.ts /
//! system.ts. See M5-03 plan "Reverse-engineered byte-locks".
#![forbid(unsafe_code)]

/// Opening literal of every assembled system prompt — LingXi's identity.
///
/// Diverged from claude-code's `DEFAULT_PREFIX` (`system.ts:10`) by the rebrand:
/// LingXi is not Anthropic's official CLI, so the descriptor is debranded.
/// (no leading/trailing whitespace; no LF.)
pub const HEADER: &str = "You are LingXi, an agentic command-line coding assistant.";

/// Section separator between assembled prompt sections. Two LFs (one blank
/// line); a model body may itself begin with an additional oracle-locked LF.
pub const SECTION_SEP: &str = "\n\n";

/// Final trailing newline appended once at the end of `assemble_system_prompt`.
pub const TRAILING_NL: &str = "\n";

/// Closing literal of every assembled system prompt.
///
/// Source: `claude-code/src/constants/prompts.ts:766-770` (the
/// `notes:` block inside `enhanceSystemPromptWithEnvDetails`). The
/// em-dash character (U+2014, 3 UTF-8 bytes) appears in bullets 2 and 5.
/// Byte length is asserted in `prompt_assemble_test::footer_byte_length_locked`
/// (re-verify via `wc -c` after each claude-code rebase).
///
/// The 5th bullet (the "do NOT Write report/.md files" instruction) is built by
/// the same binary `H$t` footer formatter on BOTH the main and subagent paths,
/// so it belongs in this FOOTER too (the subagent-side `SUBAGENT_NOTES_TRAILER`
/// in `agent::handle` already carries it). FOOTER keeps its own trailing `\n`
/// (the assembler appends none), matching the binary's final `…create.\n`.
pub const FOOTER: &str = "Notes:\n\
- Agent threads always have their cwd reset between bash calls, as a result please only use absolute file paths.\n\
- In your final response, share file paths (always absolute, never relative) that are relevant to the task. Include code snippets only when the exact text is load-bearing (e.g., a bug you found, a function signature the caller asked for) — do not recap code you merely read.\n\
- For clear communication with the user the assistant MUST avoid using emojis.\n\
- Do not use a colon before tool calls. Text like \"Let me read the file:\" followed by a read tool call should just be \"Let me read the file.\" with a period.\n\
- Do NOT Write report/summary/findings/analysis .md files. Return findings directly as your final assistant message — the parent agent reads your text output, not files you create. (Files written as input to another tool are fine; this note is about report files.)\n";
