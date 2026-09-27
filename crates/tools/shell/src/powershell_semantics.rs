//! PowerShell exit-code reinterpretation — 1:1 port of claude-code's
//! PowerShell-tool command semantics (2.1.196 fix, binary 2.1.198 `Wja` +
//! helpers @213326500-213329000):
//!
//! - `Wja(command, code, stdout, stderr)` → [`interpret_powershell_command_result`]
//! - `W6p` (last-segment splitter, PowerShell-aware quoting: `'...'` literal,
//!   `"..."` with backtick escapes, `#` comments, separators `;` `|` newline
//!   `&&` and a space-preceded lone `&`) → [`last_segment`]
//! - `JEo` (base extractor: leading `&`/`.` call-operator strip, quoted first
//!   token, `\`/`/` basename, lowercase, `.exe`/`.cmd`/`.bat` strip) →
//!   [`base_command`]
//! - `G6p` (git subcommand with `-C`/`-c` value skip) → [`git_subcommand`]
//!
//! The per-command map (`j6p`) covers `grep`/`rg`/`egrep`/`fgrep`/`findstr`
//! (exit 1 = "No matches found", NOT an error) and `robocopy` (0-7 = success
//! grades, ≥8 = error); `git grep`/`git diff` route to the grep/diff handlers.
//! Everything else keeps the default any-nonzero-is-error semantic. Unlike the
//! Bash-tool variant (`command_semantics.rs`, `isError: code >= 2`), the
//! PowerShell grep/diff handlers are `isError: code != 0 && code != 1` —
//! byte-faithful to `Nht`/`q6p`.

use crate::command_semantics::CommandInterpretation;

/// `Wja`: interpret a finished PowerShell command's exit code with
/// command-specific semantics. The TS signature also receives
/// `stdout`/`stderr` but no handler consults them.
#[must_use]
pub fn interpret_powershell_command_result(command: &str, exit_code: i32) -> CommandInterpretation {
    let seg = last_segment(command);
    let base = base_command(&seg);
    if base == "git" {
        match git_subcommand(&seg).as_deref() {
            Some("grep") => return grep_semantics(exit_code),
            Some("diff") => return diff_semantics(exit_code),
            _ => {}
        }
    }
    match base.as_str() {
        "grep" | "rg" | "egrep" | "fgrep" | "findstr" => grep_semantics(exit_code),
        "robocopy" => robocopy_semantics(exit_code),
        _ => CommandInterpretation {
            is_error: exit_code != 0,
            message: (exit_code != 0).then(|| format!("Command failed with exit code {exit_code}")),
        },
    }
}

/// `Nht`: grep-family — exit 1 is "no matches", never an error.
fn grep_semantics(exit_code: i32) -> CommandInterpretation {
    CommandInterpretation {
        is_error: exit_code != 0 && exit_code != 1,
        message: (exit_code == 1).then(|| "No matches found".to_string()),
    }
}

/// `q6p`: diff — exit 1 is "differences found", never an error.
fn diff_semantics(exit_code: i32) -> CommandInterpretation {
    CommandInterpretation {
        is_error: exit_code != 0 && exit_code != 1,
        message: (exit_code == 1).then(|| "Files differ".to_string()),
    }
}

/// The `robocopy` handler in `j6p`: 0-7 are success grades (odd = files
/// copied), ≥ 8 (or negative) is an error.
fn robocopy_semantics(exit_code: i32) -> CommandInterpretation {
    let message = if exit_code == 0 {
        Some("No files copied (already in sync)".to_string())
    } else if (1..8).contains(&exit_code) {
        Some(if exit_code & 1 == 1 {
            "Files copied successfully".to_string()
        } else {
            "Robocopy completed (no errors)".to_string()
        })
    } else {
        None
    };
    CommandInterpretation {
        is_error: !(0..8).contains(&exit_code),
        message,
    }
}

/// `W6p`: split into statement segments and return the LAST non-blank one
/// (the whole command when every segment is blank).
///
/// PowerShell-aware: `'...'` is literal (no escapes); inside `"..."` a
/// backtick escapes the next character; a `#` at the start or after
/// whitespace/newline drops the rest of the line; separators are `;`, `|`,
/// `\n`, `\r`, `&&`, and a lone `&` preceded by a space/tab when the current
/// segment is non-blank. Pipes/semicolons INSIDE quotes do not split — the
/// 2.1.196 "quoted | patterns" fix.
#[must_use]
pub fn last_segment(command: &str) -> String {
    let bytes = command.as_bytes();
    let len = bytes.len();
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0usize;
    while i < len {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'`' {
                // Backtick escapes the next char (`s++` then the loop's own
                // increment). Advance past it, honoring UTF-8 boundaries.
                i += 1;
                if i < len {
                    i += utf8_len(bytes[i]);
                }
                continue;
            }
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        if c == b'#' && (i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r')) {
            segments.push((start, i));
            // Skip to the char BEFORE the next newline (JS `while` on s+1).
            while i + 1 < len && bytes[i + 1] != b'\n' && bytes[i + 1] != b'\r' {
                i += 1;
            }
            start = i + 1;
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b';' | b'|' | b'\n' | b'\r' => {
                segments.push((start, i));
                start = i + 1;
            }
            b'&' => {
                if i + 1 < len && bytes[i + 1] == b'&' {
                    segments.push((start, i));
                    i += 2;
                    start = i;
                    continue;
                }
                let prev_ws = i > 0 && matches!(bytes[i - 1], b' ' | b'\t');
                if prev_ws && !command[start..i].trim().is_empty() {
                    segments.push((start, i));
                    start = i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    segments.push((start.min(len), len));
    segments
        .iter()
        .rev()
        .map(|&(s, e)| &command[s..e])
        .find(|s| !s.trim().is_empty())
        .unwrap_or(command)
        .to_string()
}

/// `JEo` (`.base` only): the lowercased executable name of a segment —
/// leading `&`/`.` call operator stripped, a quoted first token unwrapped,
/// `\`/`/` path components dropped, and a `.exe`/`.cmd`/`.bat` extension
/// removed.
#[must_use]
pub fn base_command(segment: &str) -> String {
    let t = strip_call_operator(segment.trim());
    let r = match leading_quoted(t) {
        Some((inner, _)) => inner.to_string(),
        None => {
            let first = t.split_whitespace().next().unwrap_or("");
            // `.replace(/^["']|["']$/g, "")` — strip ONE leading and ONE
            // trailing quote char.
            let first = first
                .strip_prefix('"')
                .or_else(|| first.strip_prefix('\''))
                .unwrap_or(first);
            let first = first
                .strip_suffix('"')
                .or_else(|| first.strip_suffix('\''))
                .unwrap_or(first);
            first.to_string()
        }
    };
    let s = r
        .rsplit(['\\', '/'])
        .next()
        .filter(|p| !p.is_empty())
        .unwrap_or(&r)
        .to_lowercase();
    for ext in [".exe", ".cmd", ".bat"] {
        if let Some(stripped) = s.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    s
}

/// `G6p`: the git subcommand of a segment — `None` unless the segment's base
/// is `git`; flags are skipped and `-C`/`-c` consume their value argument.
#[must_use]
pub fn git_subcommand(segment: &str) -> Option<String> {
    let t = strip_call_operator(segment.trim());
    // First token: a full quoted match (WITH quotes) or the first
    // whitespace-delimited token.
    let r = match leading_quoted(t) {
        Some((_, full_len)) => &t[..full_len],
        None => t.split_whitespace().next().unwrap_or(""),
    };
    if base_command(r) != "git" {
        return None;
    }
    let rest = &t[r.len()..];
    let toks: Vec<&str> = rest.split_whitespace().collect();
    let mut i = 0;
    while i < toks.len() {
        let tok = toks[i];
        if tok.starts_with('-') {
            if tok == "-C" || tok == "-c" {
                i += 1;
            }
            i += 1;
            continue;
        }
        return Some(tok.to_string());
    }
    None
}

/// `/^[&.]\s+/` — strip a leading `&` or `.` call operator followed by
/// whitespace.
fn strip_call_operator(t: &str) -> &str {
    let mut chars = t.char_indices();
    match chars.next() {
        Some((_, '&' | '.')) => {}
        _ => return t,
    }
    let rest = &t[1..];
    let trimmed = rest.trim_start();
    if trimmed.len() == rest.len() {
        // No whitespace after the operator — the regex requires `\s+`.
        return t;
    }
    trimmed
}

/// `/^"([^"]*)"|^'([^']*)'/` — a leading quoted token. Returns the inner
/// content and the byte length of the FULL match (quotes included).
fn leading_quoted(t: &str) -> Option<(&str, usize)> {
    let bytes = t.as_bytes();
    let quote = *bytes.first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let end = t[1..].find(quote as char)?;
    Some((&t[1..=end], end + 2))
}

/// Number of bytes in the UTF-8 sequence starting with `b` (1 for
/// continuation/ASCII fallback safety).
fn utf8_len(b: u8) -> usize {
    match b {
        0xF0..=0xF7 => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_family_exit_one_is_no_matches_not_error() {
        for cmd in [
            "grep pattern file.txt",
            "rg pattern",
            "egrep pattern file.txt",
            "fgrep literal file.txt",
            "findstr /s pattern *.cs",
        ] {
            let r = interpret_powershell_command_result(cmd, 1);
            assert!(!r.is_error, "{cmd}");
            assert_eq!(r.message.as_deref(), Some("No matches found"), "{cmd}");
            let ok = interpret_powershell_command_result(cmd, 0);
            assert!(!ok.is_error);
            assert_eq!(ok.message, None);
            assert!(
                interpret_powershell_command_result(cmd, 2).is_error,
                "{cmd}"
            );
        }
    }

    #[test]
    fn git_diff_and_git_grep_exit_one_not_error() {
        let d = interpret_powershell_command_result("git diff HEAD~1", 1);
        assert!(!d.is_error);
        assert_eq!(d.message.as_deref(), Some("Files differ"));

        let g = interpret_powershell_command_result("git grep needle", 1);
        assert!(!g.is_error);
        assert_eq!(g.message.as_deref(), Some("No matches found"));

        // `-C`/`-c` consume their value argument (G6p).
        let c = interpret_powershell_command_result("git -C C:\\repo -c core.pager=cat diff", 1);
        assert!(!c.is_error);
        assert_eq!(c.message.as_deref(), Some("Files differ"));

        // Other git subcommands keep the default semantic.
        let s = interpret_powershell_command_result("git status", 1);
        assert!(s.is_error);
        assert_eq!(
            s.message.as_deref(),
            Some("Command failed with exit code 1")
        );
    }

    /// The 2.1.196 headline case: a `|` INSIDE a quoted pattern must not be
    /// treated as a pipeline separator (W6p is quote-aware).
    #[test]
    fn quoted_pipe_pattern_is_not_a_separator() {
        let r = interpret_powershell_command_result("grep \"foo|bar\" file.txt", 1);
        assert!(!r.is_error);
        assert_eq!(r.message.as_deref(), Some("No matches found"));

        let r2 = interpret_powershell_command_result("git grep 'a|b' -- src", 1);
        assert!(!r2.is_error);
        assert_eq!(r2.message.as_deref(), Some("No matches found"));

        // An UNQUOTED pipe still selects the last pipeline stage.
        let r3 = interpret_powershell_command_result("Get-Content f.txt | grep pat", 1);
        assert!(!r3.is_error);
        assert_eq!(r3.message.as_deref(), Some("No matches found"));
    }

    #[test]
    fn last_statement_determines_semantics() {
        // `;`, `&&`, and newlines pick the LAST statement.
        let r = interpret_powershell_command_result("cd src; git diff", 1);
        assert!(!r.is_error);
        assert_eq!(r.message.as_deref(), Some("Files differ"));

        let r2 = interpret_powershell_command_result("git diff && Write-Output done", 1);
        assert!(
            r2.is_error,
            "last stage is Write-Output → default semantics"
        );

        // A trailing comment does not hide the real last statement.
        let r3 = interpret_powershell_command_result("git grep x # searching", 1);
        assert!(!r3.is_error);
    }

    #[test]
    fn call_operator_and_quoted_exe_paths_resolve_base() {
        let r = interpret_powershell_command_result("& git diff", 1);
        assert!(!r.is_error);
        assert_eq!(r.message.as_deref(), Some("Files differ"));

        let q = interpret_powershell_command_result(
            "\"C:\\Program Files\\Git\\bin\\git.exe\" grep needle",
            1,
        );
        assert!(!q.is_error);
        assert_eq!(q.message.as_deref(), Some("No matches found"));

        let e = interpret_powershell_command_result("grep.exe pat file", 1);
        assert!(!e.is_error);
    }

    #[test]
    fn robocopy_grades() {
        let zero = interpret_powershell_command_result("robocopy src dst", 0);
        assert!(!zero.is_error);
        assert_eq!(
            zero.message.as_deref(),
            Some("No files copied (already in sync)")
        );

        let one = interpret_powershell_command_result("robocopy src dst", 1);
        assert!(!one.is_error);
        assert_eq!(one.message.as_deref(), Some("Files copied successfully"));

        let two = interpret_powershell_command_result("robocopy src dst", 2);
        assert!(!two.is_error);
        assert_eq!(
            two.message.as_deref(),
            Some("Robocopy completed (no errors)")
        );

        let three = interpret_powershell_command_result("robocopy src dst", 3);
        assert!(!three.is_error);
        assert_eq!(three.message.as_deref(), Some("Files copied successfully"));

        let eight = interpret_powershell_command_result("robocopy src dst", 8);
        assert!(eight.is_error);
        assert_eq!(eight.message, None);

        let neg = interpret_powershell_command_result("robocopy src dst", -1);
        assert!(neg.is_error);
    }

    #[test]
    fn default_semantics_for_everything_else() {
        let ok = interpret_powershell_command_result("Get-ChildItem", 0);
        assert!(!ok.is_error);
        assert_eq!(ok.message, None);

        let bad = interpret_powershell_command_result("Get-ChildItem", 1);
        assert!(bad.is_error);
        assert_eq!(
            bad.message.as_deref(),
            Some("Command failed with exit code 1")
        );

        let worse = interpret_powershell_command_result("cargo build", 101);
        assert!(worse.is_error);
        assert_eq!(
            worse.message.as_deref(),
            Some("Command failed with exit code 101")
        );
    }

    #[test]
    fn backtick_escape_inside_double_quotes() {
        // The backtick escapes the closing quote; the `| grep` stays inside
        // the string, so the last segment is the whole (single) statement.
        let seg = last_segment("Write-Output \"a`\"b|c\"");
        assert_eq!(seg, "Write-Output \"a`\"b|c\"");
    }

    #[test]
    fn segment_splitter_edges() {
        assert_eq!(last_segment("a; b"), " b");
        assert_eq!(last_segment("a && b"), " b");
        assert_eq!(last_segment("a | b"), " b");
        assert_eq!(last_segment("a\nb"), "b");
        // Space-preceded lone `&` (background/statement separator).
        assert_eq!(last_segment("a & b"), " b");
        // `&` glued to the previous token is NOT a separator.
        assert_eq!(last_segment("a&b"), "a&b");
        // All-blank segments fall back to the whole command.
        assert_eq!(last_segment("  "), "  ");
        // Comment-only tail falls back to the preceding statement.
        assert_eq!(last_segment("git diff # words"), "git diff ");
    }

    #[test]
    fn base_command_edges() {
        assert_eq!(base_command("git diff"), "git");
        assert_eq!(base_command("& git diff"), "git");
        assert_eq!(base_command(". .\\script.bat arg"), "script");
        assert_eq!(base_command("GREP.EXE pat"), "grep");
        assert_eq!(base_command("\"C:\\tools\\rg.exe\" pat"), "rg");
        assert_eq!(base_command("'/usr/bin/fgrep' pat"), "fgrep");
        assert_eq!(base_command(""), "");
    }
}
