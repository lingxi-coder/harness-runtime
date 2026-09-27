//! Exit-code reinterpretation — 1:1 port of claude-code
//! `src/tools/BashTool/commandSemantics.ts` (`interpretCommandResult`).
//!
//! Many commands use the exit code to convey information other than
//! success/failure: `grep`/`rg` return 1 for "no matches", `find` returns 1
//! for "some directories inaccessible", `diff` returns 1 for "differences
//! found", `test`/`[` return 1 for "condition false". Treating those as errors
//! (the naive `exit_code != 0`) wrongly reports them to the model as failures.
//! This maps the per-command semantics so exit 1 on those commands is a
//! NON-error with a descriptive note, while exit ≥ 2 stays an error.

use permission::shell_command::split_command;

/// Outcome of [`interpret_command_result`].
pub struct CommandInterpretation {
    /// Whether the model should treat the result as an error.
    pub is_error: bool,
    /// Optional descriptive note (claude-code `returnCodeInterpretation`),
    /// `None` when the default semantic produced no message.
    pub message: Option<String>,
}

/// Interpret a finished command's exit code with command-specific semantics
/// (claude-code `interpretCommandResult`). `stdout`/`stderr` are unused by the
/// ported semantics (they are part of the TS signature but never consulted).
///
/// 2.1.196 additions (binary 2.1.198 `cLp`/`uLp`/`lLp` @212637913):
/// `egrep`/`fgrep` join the grep family, and `git diff` / `git grep` get their
/// own exit-1 semantics (the git subcommand is found by skipping flags, with
/// `-C`/`-c` consuming their value argument — `uLp`).
#[must_use]
pub fn interpret_command_result(command: &str, exit_code: i32) -> CommandInterpretation {
    let last = last_subcommand(command);
    // `cLp`: the git special case is checked BEFORE the per-command map; a git
    // subcommand other than diff/grep falls through to the default semantic
    // (base "git" is not in the map).
    if extract_base_command(&last) == "git" {
        match git_subcommand(&last).as_deref() {
            Some("diff") => {
                return CommandInterpretation {
                    is_error: exit_code >= 2,
                    message: (exit_code == 1).then(|| "Files differ".to_string()),
                }
            }
            Some("grep") => {
                return CommandInterpretation {
                    is_error: exit_code >= 2,
                    message: (exit_code == 1).then(|| "No matches found".to_string()),
                }
            }
            _ => {}
        }
    }
    match extract_base_command(&last).as_str() {
        // grep family: 0=matches, 1=no matches, 2+=error.
        "grep" | "rg" | "egrep" | "fgrep" => CommandInterpretation {
            is_error: exit_code >= 2,
            message: (exit_code == 1).then(|| "No matches found".to_string()),
        },
        // find: 0=success, 1=partial (some dirs inaccessible), 2+=error.
        "find" => CommandInterpretation {
            is_error: exit_code >= 2,
            message: (exit_code == 1).then(|| "Some directories were inaccessible".to_string()),
        },
        // diff: 0=no differences, 1=differences found, 2+=error.
        "diff" => CommandInterpretation {
            is_error: exit_code >= 2,
            message: (exit_code == 1).then(|| "Files differ".to_string()),
        },
        // test / [: 0=condition true, 1=condition false, 2+=error.
        "test" | "[" => CommandInterpretation {
            is_error: exit_code >= 2,
            message: (exit_code == 1).then(|| "Condition is false".to_string()),
        },
        // Everything else: default semantic (any non-zero is an error).
        _ => CommandInterpretation {
            is_error: exit_code != 0,
            message: (exit_code != 0).then(|| format!("Command failed with exit code {exit_code}")),
        },
    }
}

/// `heuristicallyExtractBaseCommand`: the exit code is determined by the LAST
/// subcommand, so split into subcommands (the `splitCommand_DEPRECATED`
/// analogue) and take the last segment. "May get it wrong — not used for
/// security" (claude-code comment). `split_command` is quote-aware, so a `|`
/// inside quotes (e.g. `grep "a|b" f`) does NOT start a new segment — the
/// 2.1.196 "quoted | patterns" behavior.
fn last_subcommand(command: &str) -> String {
    split_command(command)
        .last()
        .map_or_else(|| command.to_string(), std::clone::Clone::clone)
}

/// `uLp`: the git subcommand of a single (last) segment — the first token
/// after `git` that is not a flag, where `-C` and `-c` consume their value
/// argument. Returns `None` when the segment is not a git command or has no
/// subcommand.
fn git_subcommand(segment: &str) -> Option<String> {
    let toks: Vec<&str> = segment.split_whitespace().collect();
    if toks.first().copied() != Some("git") {
        return None;
    }
    let mut i = 1;
    while i < toks.len() {
        let t = toks[i];
        if t.starts_with('-') {
            if t == "-C" || t == "-c" {
                i += 1;
            }
            i += 1;
            continue;
        }
        return Some(t.to_string());
    }
    None
}

/// First whitespace-delimited token of a single command (`extractBaseCommand`).
fn extract_base_command(command: &str) -> String {
    command.split_whitespace().next().unwrap_or("").to_string()
}

/// Best-effort parse-only scan for files a Bash command likely WRITES, so the
/// caller can invalidate stale read-file-state. Conservative: detects `>`/`>>`
/// redirects, `tee [-a] FILE...`, heredoc `> FILE`, and common write tools
/// (`cp dst`, `mv dst`, `touch`, `install`). Never executes; returns deduped
/// path tokens. An empty result means "no write recognized", NOT "no write".
/// Unknown-but-risky shapes are left to the caller's conservative invalidation.
#[must_use]
pub fn parsed_written_paths(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: &str| {
        let t = p.trim_matches(['"', '\'']).trim();
        if !t.is_empty() && !t.starts_with('-') && !out.iter().any(|e| e == t) {
            out.push(t.to_string());
        }
    };
    let toks: Vec<&str> = command.split_whitespace().collect();
    for (i, tok) in toks.iter().enumerate() {
        // `>file` / `>>file` (glued) and bare `>` / `>>` (next token is target).
        if let Some(rest) = tok.strip_prefix(">>").or_else(|| tok.strip_prefix('>')) {
            if rest.is_empty() {
                if let Some(n) = toks.get(i + 1) {
                    push(n);
                }
            } else {
                push(rest);
            }
        }
    }
    // `tee [-a] FILE...` writes each non-flag arg after `tee`.
    if let Some(p) = toks.iter().position(|t| *t == "tee") {
        for n in &toks[p + 1..] {
            if n.starts_with('-') {
                continue;
            }
            push(n);
        }
    }
    // Single-dest writers: `touch`, `cp`/`mv`/`install` final arg.
    match toks.first().copied() {
        Some("touch") => toks[1..].iter().for_each(|n| push(n)),
        Some("cp" | "mv" | "install") if toks.len() >= 2 => push(toks[toks.len() - 1]),
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_no_match_is_not_an_error() {
        let r = interpret_command_result("grep needle file.txt", 1);
        assert!(!r.is_error);
        assert_eq!(r.message.as_deref(), Some("No matches found"));
    }

    #[test]
    fn grep_real_error_is_error() {
        let r = interpret_command_result("grep -X bad", 2);
        assert!(r.is_error);
        assert_eq!(r.message, None);
    }

    #[test]
    fn rg_find_diff_test_messages() {
        assert_eq!(
            interpret_command_result("rg pat", 1).message.as_deref(),
            Some("No matches found")
        );
        assert_eq!(
            interpret_command_result("find . -name x", 1)
                .message
                .as_deref(),
            Some("Some directories were inaccessible")
        );
        assert_eq!(
            interpret_command_result("diff a b", 1).message.as_deref(),
            Some("Files differ")
        );
        assert_eq!(
            interpret_command_result("test -f x", 1).message.as_deref(),
            Some("Condition is false")
        );
        assert_eq!(
            interpret_command_result("[ -f x ]", 1).message.as_deref(),
            Some("Condition is false")
        );
    }

    /// 2.1.196: `egrep`/`fgrep` share the grep exit-1 semantics (binary
    /// 2.1.198 `lLp` @212637913 lists all four).
    #[test]
    fn egrep_fgrep_no_match_is_not_an_error() {
        for cmd in ["egrep pat file.txt", "fgrep lit file.txt"] {
            let r = interpret_command_result(cmd, 1);
            assert!(!r.is_error, "{cmd}");
            assert_eq!(r.message.as_deref(), Some("No matches found"), "{cmd}");
            assert!(interpret_command_result(cmd, 2).is_error, "{cmd}");
        }
    }

    /// 2.1.196: `git diff` exit 1 = "Files differ", `git grep` exit 1 =
    /// "No matches found" (binary `cLp`/`uLp`); `-C`/`-c` consume their value
    /// so the subcommand is still found; any OTHER git subcommand keeps the
    /// default any-nonzero-is-error semantic.
    #[test]
    fn git_diff_and_grep_exit_one_is_not_an_error() {
        let d = interpret_command_result("git diff", 1);
        assert!(!d.is_error);
        assert_eq!(d.message.as_deref(), Some("Files differ"));

        let g = interpret_command_result("git grep needle", 1);
        assert!(!g.is_error);
        assert_eq!(g.message.as_deref(), Some("No matches found"));

        // `-C <dir>` / `-c <kv>` consume their argument (uLp).
        let c = interpret_command_result("git -C /repo -c core.pager=cat diff HEAD~1", 1);
        assert!(!c.is_error);
        assert_eq!(c.message.as_deref(), Some("Files differ"));

        // Exit >= 2 stays an error even for diff/grep.
        assert!(interpret_command_result("git diff", 2).is_error);

        // Other git subcommands keep the default semantic.
        let s = interpret_command_result("git status", 1);
        assert!(s.is_error);
        assert_eq!(
            s.message.as_deref(),
            Some("Command failed with exit code 1")
        );
    }

    /// 2.1.196 "quoted | patterns": a pipe INSIDE quotes must not split the
    /// command, so `grep "a|b" f` still resolves to grep semantics.
    #[test]
    fn quoted_pipe_pattern_stays_grep() {
        let r = interpret_command_result("grep \"foo|bar\" file.txt", 1);
        assert!(!r.is_error);
        assert_eq!(r.message.as_deref(), Some("No matches found"));

        let r2 = interpret_command_result("git grep 'a|b' -- src", 1);
        assert!(!r2.is_error);
        assert_eq!(r2.message.as_deref(), Some("No matches found"));
    }

    #[test]
    fn last_subcommand_determines_semantic() {
        // The pipeline's exit code is the LAST command's: `... | grep x`.
        let r = interpret_command_result("cat f | grep needle", 1);
        assert!(!r.is_error);
        assert_eq!(r.message.as_deref(), Some("No matches found"));
        // `grep x && echo done` → last is `echo`, default semantic.
        let r2 = interpret_command_result("grep x f && echo done", 1);
        assert!(r2.is_error);
        assert_eq!(
            r2.message.as_deref(),
            Some("Command failed with exit code 1")
        );
    }

    #[test]
    fn default_semantic_for_unmapped_commands() {
        let ok = interpret_command_result("echo hi", 0);
        assert!(!ok.is_error);
        assert_eq!(ok.message, None);
        let bad = interpret_command_result("cargo build", 101);
        assert!(bad.is_error);
        assert_eq!(
            bad.message.as_deref(),
            Some("Command failed with exit code 101")
        );
    }

    #[test]
    fn zero_exit_never_errors() {
        assert!(!interpret_command_result("grep x f", 0).is_error);
        assert_eq!(interpret_command_result("grep x f", 0).message, None);
    }

    #[test]
    fn detects_redirect_tee_and_writers() {
        assert_eq!(parsed_written_paths("echo hi > a.txt"), vec!["a.txt"]);
        assert_eq!(parsed_written_paths("echo hi >>log"), vec!["log"]);
        assert_eq!(parsed_written_paths("cat <<EOF > out.md"), vec!["out.md"]);
        assert_eq!(parsed_written_paths("foo | tee -a x y"), vec!["x", "y"]);
        assert_eq!(parsed_written_paths("touch p q"), vec!["p", "q"]);
        assert_eq!(parsed_written_paths("cp src dst"), vec!["dst"]);
        assert!(parsed_written_paths("ls -la").is_empty());
    }
}
