//! "Silent" command detection — 1:1 port of claude-code
//! `src/tools/BashTool/BashTool.tsx::isSilentBashCommand` (lines 178-217) plus
//! its `BASH_SILENT_COMMANDS` / `BASH_SEMANTIC_NEUTRAL_COMMANDS` constant sets
//! (lines 77-81).
//!
//! A command is "silent" when it is expected to produce no stdout on success
//! (`mkdir x`, `cd /tmp`, `export FOO=1`, …). The tool surfaces this as
//! `no_output_expected` so the UI/model can show "Done" instead of treating
//! empty output as `(No output)` / an error (BashTool.tsx:809
//! `noOutputExpected`).
//!
//! ## Tokenizer
//! `isSilentBashCommand` needs the command split into parts **including** the
//! control/redirect operators (`||`, `&&`, `|`, `;`, `>`, `>>`, `>&`) so it can
//! skip redirect targets and apply the `||`-fallback-neutral rule. The
//! permission crate's `split_command` DROPS operators, so it cannot be reused
//! here. [`split_command_with_operators`] is a small quote-aware port of
//! claude-code `splitCommandWithOperators` (`src/utils/bash/commands.ts:85`):
//! it tokenizes quote-/escape-aware, emits the operators above as their own
//! parts, and collapses adjacent argument tokens into one space-joined part
//! (mirroring shell-quote's adjacent-string collapse).
//!
//! ## Deferred (documented, out of this subsystem's scope)
//! The TS `splitCommandWithOperators` additionally handles heredoc extraction,
//! `\`-newline continuation joining, brace/glob token objects, and shell-quote's
//! comment handling. Those only matter for the permission/path-constraint path
//! (ported elsewhere) — for the silent-command heuristic the simple quote-aware
//! splitter is sufficient and conservative: anything it can't classify as a
//! known silent command makes `is_silent_bash_command` return `false` (the
//! safe default — show output rather than "Done").

/// Commands that typically produce no stdout on success — verbatim from
/// claude-code `BashTool.tsx:81` `BASH_SILENT_COMMANDS`.
const BASH_SILENT_COMMANDS: &[&str] = &[
    "mv", "cp", "rm", "mkdir", "rmdir", "chmod", "chown", "chgrp", "touch", "ln", "cd", "export",
    "unset", "wait",
];

/// Commands that are semantic-neutral in any position — verbatim from
/// claude-code `BashTool.tsx:77` `BASH_SEMANTIC_NEUTRAL_COMMANDS`. The bash
/// no-op `:` is included.
const BASH_SEMANTIC_NEUTRAL_COMMANDS: &[&str] = &["echo", "printf", "true", "false", ":"];

#[inline]
fn is_silent(base: &str) -> bool {
    BASH_SILENT_COMMANDS.contains(&base)
}

#[inline]
fn is_neutral(base: &str) -> bool {
    BASH_SEMANTIC_NEUTRAL_COMMANDS.contains(&base)
}

/// Split `command` into parts INCLUDING the operators `||`, `&&`, `|`, `;`,
/// `>>`, `>&`, `>`. Quote-/escape-aware: separators inside `'...'` / `"..."` or
/// after `\` are NOT split points. Adjacent argument tokens collapse into one
/// space-joined part (mirroring claude-code `splitCommandWithOperators`'
/// adjacent-string collapse so a part like `mkdir foo` keeps its base command).
///
/// On an empty/whitespace-only command this returns an empty vector (matches the
/// TS empty-array path at `commands.ts:206`).
#[must_use]
pub fn split_command_with_operators(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    // The current argument word being accumulated (sans surrounding quotes are
    // kept verbatim — we only care about base commands and operators).
    let mut cur = String::new();
    // Whether `cur` holds any token chars (so we can collapse adjacent words).
    let mut cur_has_word = false;

    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    // Flush the in-progress word into `cur` as a completed token, joining with a
    // space if `cur` already has content (adjacent-string collapse).
    let push_operator =
        |parts: &mut Vec<String>, cur: &mut String, has_word: &mut bool, op: &str| {
            if *has_word {
                parts.push(std::mem::take(cur));
                *has_word = false;
            }
            parts.push(op.to_string());
        };

    while i < chars.len() {
        let c = chars[i];
        if in_single {
            cur.push(c);
            cur_has_word = true;
            if c == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            cur.push(c);
            cur_has_word = true;
            if c == '\\' && i + 1 < chars.len() {
                cur.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            '\\' if i + 1 < chars.len() => {
                cur.push(c);
                cur.push(chars[i + 1]);
                cur_has_word = true;
                i += 2;
                continue;
            }
            '\'' => {
                in_single = true;
                cur.push(c);
                cur_has_word = true;
                i += 1;
            }
            '"' => {
                in_double = true;
                cur.push(c);
                cur_has_word = true;
                i += 1;
            }
            c if c.is_whitespace() && c != '\n' => {
                // Whitespace separates argument words *within* a part — but
                // adjacent words collapse with a single space (shell-quote
                // behavior). Add the separator space only if a word is pending
                // and the next char starts another word; defer by setting a
                // boundary marker via a trailing space we trim on flush.
                if cur_has_word && !cur.ends_with(' ') {
                    cur.push(' ');
                }
                i += 1;
            }
            '\n' => {
                // A bare newline is a command separator in bash. Treat it like
                // `;` for tokenization purposes (its own part).
                push_operator(&mut parts, &mut cur, &mut cur_has_word, ";");
                i += 1;
            }
            '&' => {
                // `&&` operator, `>&`/`N>&` handled under `>`. A lone `&`
                // (background) is not one of the operators isSilent cares about;
                // keep it attached to the current word so the base command of
                // the part is unaffected.
                if i + 1 < chars.len() && chars[i + 1] == '&' {
                    push_operator(&mut parts, &mut cur, &mut cur_has_word, "&&");
                    i += 2;
                } else {
                    cur.push('&');
                    cur_has_word = true;
                    i += 1;
                }
            }
            '|' => {
                if i + 1 < chars.len() && chars[i + 1] == '|' {
                    push_operator(&mut parts, &mut cur, &mut cur_has_word, "||");
                    i += 2;
                } else {
                    push_operator(&mut parts, &mut cur, &mut cur_has_word, "|");
                    i += 1;
                }
            }
            ';' => {
                push_operator(&mut parts, &mut cur, &mut cur_has_word, ";");
                // `;;` (case terminator) — emit a second `;` to stay faithful to
                // the per-separator split; the silent loop ignores `;` either
                // way.
                if i + 1 < chars.len() && chars[i + 1] == ';' {
                    parts.push(";".to_string());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            '>' => {
                // Redirect operators: `>>`, `>&`, `>`. A leading file-descriptor
                // digit (e.g. `2>`) was accumulated into `cur` as a trailing
                // token; for the silent heuristic we only need the canonical
                // `>`/`>>`/`>&` token so the *next* part is skipped as a redirect
                // target. Drop a trailing all-digit token (the fd) that is
                // directly adjacent to `>` so it does not masquerade as a base
                // command — but leave the rest of the part (`touch f` in
                // `touch f 2>&1`) intact.
                if cur_has_word {
                    let trimmed = cur.trim_end();
                    let last_tok_start =
                        trimmed.rfind(char::is_whitespace).map_or(0, |idx| idx + 1);
                    let last_tok = &trimmed[last_tok_start..];
                    if !last_tok.is_empty() && last_tok.chars().all(|ch| ch.is_ascii_digit()) {
                        cur.truncate(last_tok_start);
                        let new_trim = cur.trim_end().len();
                        cur.truncate(new_trim);
                        cur_has_word = !cur.is_empty();
                    }
                }
                if i + 1 < chars.len() && chars[i + 1] == '>' {
                    push_operator(&mut parts, &mut cur, &mut cur_has_word, ">>");
                    i += 2;
                } else if i + 1 < chars.len() && chars[i + 1] == '&' {
                    push_operator(&mut parts, &mut cur, &mut cur_has_word, ">&");
                    i += 2;
                } else {
                    push_operator(&mut parts, &mut cur, &mut cur_has_word, ">");
                    i += 1;
                }
            }
            _ => {
                cur.push(c);
                cur_has_word = true;
                i += 1;
            }
        }
    }
    if cur_has_word {
        parts.push(cur);
    }
    parts
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Checks if a bash command is expected to produce no stdout on success.
///
/// 1:1 port of claude-code `BashTool.tsx:178-217` `isSilentBashCommand`. Used to
/// set the `no_output_expected` flag so the UI/model show "Done" instead of
/// "(No output)" on an empty result.
///
/// Logic: tokenize with operators; skip redirect targets (the part following
/// `>`/`>>`/`>&`); skip control operators while remembering the last one; for
/// each command part, a `||`-fallback to a neutral command (`mkdir x || echo
/// failed`) is ignored; every remaining command's base must be in
/// `BASH_SILENT_COMMANDS`, and at least one non-fallback command must exist.
#[must_use]
pub fn is_silent_bash_command(command: &str) -> bool {
    let parts_with_operators = split_command_with_operators(command);
    if parts_with_operators.is_empty() {
        return false;
    }

    let mut has_non_fallback_command = false;
    let mut last_operator: Option<&str> = None;
    let mut skip_next_as_redirect_target = false;

    for part in &parts_with_operators {
        if skip_next_as_redirect_target {
            skip_next_as_redirect_target = false;
            continue;
        }
        if part == ">" || part == ">>" || part == ">&" {
            skip_next_as_redirect_target = true;
            continue;
        }
        if part == "||" || part == "&&" || part == "|" || part == ";" {
            last_operator = Some(part.as_str());
            continue;
        }
        // baseCommand = part.trim().split(/\s+/)[0]
        let base_command = part.split_whitespace().next().unwrap_or("");
        if base_command.is_empty() {
            continue;
        }
        if last_operator == Some("||") && is_neutral(base_command) {
            continue;
        }
        has_non_fallback_command = true;
        if !is_silent(base_command) {
            return false;
        }
    }
    has_non_fallback_command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_command_is_not_silent() {
        assert!(!is_silent_bash_command(""));
        assert!(!is_silent_bash_command("   "));
    }

    #[test]
    fn single_silent_command() {
        assert!(is_silent_bash_command("mkdir x"));
        assert!(is_silent_bash_command("cd /tmp"));
        assert!(is_silent_bash_command("export FOO=1"));
        assert!(is_silent_bash_command("rm -rf build"));
        assert!(is_silent_bash_command("touch file"));
    }

    #[test]
    fn non_silent_command() {
        assert!(!is_silent_bash_command("ls"));
        assert!(!is_silent_bash_command("ls -la"));
        assert!(!is_silent_bash_command("cat file"));
        assert!(!is_silent_bash_command("echo hi"));
    }

    #[test]
    fn neutral_only_is_not_silent() {
        // `echo` is neutral, not silent; with no non-neutral command and no
        // `||` fallback it falls through to has_non_fallback_command=true but
        // echo is not in BASH_SILENT_COMMANDS → false.
        assert!(!is_silent_bash_command("echo foo"));
        assert!(!is_silent_bash_command("true"));
    }

    #[test]
    fn or_fallback_neutral_is_ignored() {
        // `mkdir x || echo failed` → mkdir is silent, the `echo` after `||` is
        // neutral fallback (skipped). Result: silent.
        assert!(is_silent_bash_command("mkdir x || echo failed"));
        assert!(is_silent_bash_command("rm f || true"));
        assert!(is_silent_bash_command("cp a b || printf nope"));
    }

    #[test]
    fn or_fallback_non_neutral_breaks_silence() {
        // After `||`, `ls` is NOT neutral, so it counts as a command and is not
        // in silent set → false.
        assert!(!is_silent_bash_command("mkdir x || ls"));
    }

    #[test]
    fn and_chain_all_silent() {
        assert!(is_silent_bash_command("mkdir x && cd x"));
        assert!(is_silent_bash_command("touch a && touch b && chmod +x a"));
    }

    #[test]
    fn and_chain_with_non_silent_breaks() {
        assert!(!is_silent_bash_command("mkdir x && ls"));
        assert!(!is_silent_bash_command("cd x && cat f"));
    }

    #[test]
    fn and_after_silent_with_neutral_is_not_fallback() {
        // `&&` (not `||`) does NOT trigger the neutral-fallback skip; the neutral
        // command after `&&` is treated as a real command → echo is not silent →
        // false.
        assert!(!is_silent_bash_command("mkdir x && echo done"));
    }

    #[test]
    fn redirect_target_is_skipped() {
        // The target after `>` is skipped, so `out.txt` is not treated as a base
        // command. `cp a b` is silent; the redirect doesn't break it.
        assert!(is_silent_bash_command("cp a b > out.txt"));
        assert!(is_silent_bash_command("rm f >> log"));
        assert!(is_silent_bash_command("touch f 2>&1"));
    }

    #[test]
    fn pipe_makes_non_silent() {
        // `mkdir x | tee` — tee is not silent → false (pipe stage is a real
        // command, not a `||` fallback).
        assert!(!is_silent_bash_command("mkdir x | tee log"));
    }

    #[test]
    fn quoted_separators_not_split() {
        // The `&&` inside quotes is part of the echo argument, not an operator.
        assert!(!is_silent_bash_command("echo 'a && b'"));
        // `mv 'a b' 'c d'` — quoted args keep mv as base; silent.
        assert!(is_silent_bash_command("mv 'a b' 'c d'"));
    }

    #[test]
    fn semicolon_chain() {
        assert!(is_silent_bash_command("cd /tmp; mkdir x"));
        assert!(!is_silent_bash_command("cd /tmp; ls"));
    }

    #[test]
    fn split_operators_basic() {
        assert_eq!(
            split_command_with_operators("mkdir x || echo failed"),
            vec!["mkdir x", "||", "echo failed"]
        );
        assert_eq!(
            split_command_with_operators("cp a b > out.txt"),
            vec!["cp a b", ">", "out.txt"]
        );
        assert_eq!(
            split_command_with_operators("touch f 2>&1"),
            vec!["touch f", ">&", "1"]
        );
        assert_eq!(
            split_command_with_operators("a && b | c"),
            vec!["a", "&&", "b", "|", "c"]
        );
        assert_eq!(split_command_with_operators("a >> b"), vec!["a", ">>", "b"]);
    }

    #[test]
    fn split_operators_empty() {
        assert!(split_command_with_operators("").is_empty());
        assert!(split_command_with_operators("   ").is_empty());
    }

    #[test]
    fn split_operators_quote_aware() {
        assert_eq!(
            split_command_with_operators("echo 'a && b'"),
            vec!["echo 'a && b'"]
        );
        assert_eq!(
            split_command_with_operators("echo \"x | y\""),
            vec!["echo \"x | y\""]
        );
    }
}
