//! Bash command-injection safety validator chain — faithful port of
//! claude-code `src/tools/BashTool/bashSecurity.ts`'s LEGACY (non-tree-sitter)
//! path `bashCommandIsSafe_DEPRECATED` (`bashSecurity.ts:2257-2413`).
//!
//! In the external build tree-sitter is OFF, so this legacy regex/scan battery
//! is the fallback that ALWAYS runs (claude-code `bashPermissions.ts:1217-1239`
//! gates it on `!astParseSucceeded`, which is `false` in the external build).
//! It runs ~23 validators in a fixed order and returns the FIRST detection's
//! `ask` message (short-circuit: first detection wins), else `Safe`.
//!
//! Each validator below cites the TS source line(s) it ports. Where TS uses a
//! regex, this port uses the same regex via the `regex` crate (already a
//! `permission` dependency) OR a hand-written char scan (cited as such) when the
//! TS code is itself a char scan or the regex uses lookbehind/lookahead the
//! `regex` crate does not support.
//!
//! ## Faithful deviations (all in the STRICTER direction)
//! - `validateMalformedTokenInjection` (`bashSecurity.ts:1082`) depends on the
//!   `shell-quote` npm tokenizer (`tryParseShellCommand` / `hasMalformedTokens`).
//!   We have no `shell-quote` port in this crate. We port the parts of
//!   `hasMalformedTokens` that are pure char/string scans (unbalanced
//!   quotes/braces/parens/brackets in the raw command) gated on the presence of
//!   a `;`/`&&`/`||` separator (the same gate TS applies). This is a faithful
//!   subset; the token-array brace/paren/bracket-per-token checks that require
//!   the shell-quote token list are documented as deferred. The subset can only
//!   ADD asks, never remove one.
//! - `validateSafeCommandSubstitution` / `isSafeHeredoc` early-ALLOW path
//!   (`bashSecurity.ts:585-610`, `:317-514`) is a passthrough-producing branch
//!   in TS (an `allow` there is mapped to `passthrough` by the orchestrator at
//!   `:2317-2326`). Porting the full line-based heredoc verifier is large; we
//!   port its GATE (`HEREDOC_IN_SUBSTITUTION`) and conservatively treat a
//!   heredoc-in-substitution as NOT-safe (fall through to the substitution
//!   detector), which is STRICTER than TS (TS may early-allow a provably-safe
//!   `$(cat <<'EOF'…)`; we ask). Documented; never a bypass.

// This module is a FAITHFUL char-scan port of a large TS regex/scan battery.
// Several pedantic lints fire inherently on that shape and fighting them would
// either obscure the 1:1 correspondence with `bashSecurity.ts` (the doc
// comments quote the TS regexes verbatim, which embed backticks and look like
// "items missing backticks") or force unnatural refactors of the faithful
// scanners. They are scoped-allowed here, not workspace-wide:
//   - `doc_markdown`: doc comments quote TS regexes containing backticks/`_`.
//   - `items_after_statements`: per-validator `static RE: OnceLock` lives next
//     to its use (the established pattern in `shell_command.rs`).
//   - `too_many_lines`: the orchestrator + the obfuscated-flag scanner mirror
//     long TS functions 1:1.
//   - `cast_possible_wrap` / `cast_sign_loss`: backslash-parity scans walk an
//     index backwards as `isize`; the values are tiny string offsets.
//   - `similar_names` / `many_single_char_names`: the scanners reuse the TS
//     single-letter loop indices (`i`, `j`, `k`) for traceability.
#![allow(
    clippy::doc_markdown,
    clippy::items_after_statements,
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::similar_names,
    clippy::many_single_char_names
)]

use regex::Regex;
use std::sync::OnceLock;

/// Verdict of [`bash_command_is_safe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BashSafetyVerdict {
    /// The command passed every validator.
    Safe,
    /// A validator detected an injection-shaped pattern; the user must approve.
    /// `message` is the byte-faithful TS `ask` message.
    Ask {
        /// Byte-faithful TS ask message.
        message: String,
    },
}

/// Per-validator extracted/derived views of the command, mirroring the TS
/// `ValidationContext` (`bashSecurity.ts:103-117`).
struct Ctx {
    /// The raw command (TS `originalCommand`).
    original: String,
    /// First space-delimited word (TS `command.split(' ')[0]`, `:2295`).
    base_command: String,
    /// Single-quoted content stripped, double-quoted content KEPT
    /// (TS `withDoubleQuotes` → `unquotedContent`, `:2296-2302`).
    unquoted_with_dq: String,
    /// Both quote types stripped, then `stripSafeRedirections`
    /// (TS `fullyUnquotedContent`, `:2303`).
    fully_unquoted: String,
    /// Both quote types stripped, BEFORE `stripSafeRedirections`
    /// (TS `fullyUnquotedPreStrip`, `:2304`).
    fully_unquoted_pre_strip: String,
    /// Quoted content stripped but quote DELIMITERS kept
    /// (TS `unquotedKeepQuoteChars`, `:2305`).
    unquoted_keep_quote_chars: String,
    /// AST verdict: whether real `;`/`&&`/`||`/`list` operator nodes exist.
    /// `Some(false)` lets `validate_backslash_escaped_operators` short-circuit
    /// (claude-code `bashSecurity.ts:1702`). `None` => no AST (legacy behavior:
    /// the `bash-ast` feature is off, or the parser had no tree).
    has_actual_operator_nodes: Option<bool>,
    /// Whether an AST quoteContext was obtained (claude-code `treeSitter != null`,
    /// bashSecurity.ts:1998). `true` => the quote views are AST-derived and
    /// validate_comment_quote_desync is a passthrough. `false` on the legacy path.
    tree_sitter_present: bool,
}

// NOTE: TS `validateSafeCommandSubstitution` / `isSafeHeredoc` (the
// `HEREDOC_IN_SUBSTITUTION` early-ALLOW path) is intentionally NOT ported here
// (see the module docs): we never early-allow a heredoc substitution, so a
// `$(cat <<'EOF'…)` falls through to `validate_dangerous_patterns`'s `$(`
// detector and asks — STRICTER than TS, never a bypass.

/// TS `extractQuotedContent` (`bashSecurity.ts:128-174`). Returns
/// `(withDoubleQuotes, fullyUnquoted, unquotedKeepQuoteChars)`.
fn extract_quoted_content(command: &str, is_jq: bool) -> (String, String, String) {
    let mut with_double_quotes = String::new();
    let mut fully_unquoted = String::new();
    let mut unquoted_keep_quote_chars = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];

        if escaped {
            escaped = false;
            if !in_single {
                with_double_quotes.push(ch);
            }
            if !in_single && !in_double {
                fully_unquoted.push(ch);
                unquoted_keep_quote_chars.push(ch);
            }
            i += 1;
            continue;
        }

        if ch == '\\' && !in_single {
            escaped = true;
            if !in_single {
                with_double_quotes.push(ch);
            }
            if !in_single && !in_double {
                fully_unquoted.push(ch);
                unquoted_keep_quote_chars.push(ch);
            }
            i += 1;
            continue;
        }

        if ch == '\'' && !in_double {
            in_single = !in_single;
            unquoted_keep_quote_chars.push(ch);
            i += 1;
            continue;
        }

        if ch == '"' && !in_single {
            in_double = !in_double;
            unquoted_keep_quote_chars.push(ch);
            // For jq, include quotes in extraction (TS `:164-166`).
            if !is_jq {
                i += 1;
                continue;
            }
        }

        if !in_single {
            with_double_quotes.push(ch);
        }
        if !in_single && !in_double {
            fully_unquoted.push(ch);
            unquoted_keep_quote_chars.push(ch);
        }
        i += 1;
    }

    (
        with_double_quotes,
        fully_unquoted,
        unquoted_keep_quote_chars,
    )
}

/// TS `stripSafeRedirections` (`bashSecurity.ts:176-188`).
fn strip_safe_redirections(content: &str) -> String {
    static RE_2GT1: OnceLock<Regex> = OnceLock::new();
    static RE_DEVNULL: OnceLock<Regex> = OnceLock::new();
    static RE_LT_DEVNULL: OnceLock<Regex> = OnceLock::new();
    // `/\s+2\s*>&\s*1(?=\s|$)/g` — replicate the lookahead with an explicit
    // trailing-boundary match-and-restore (regex crate has no lookahead).
    let re1 = RE_2GT1.get_or_init(|| Regex::new(r"\s+2\s*>&\s*1(\s|$)").unwrap());
    let re2 = RE_DEVNULL.get_or_init(|| Regex::new(r"[012]?\s*>\s*/dev/null(\s|$)").unwrap());
    let re3 = RE_LT_DEVNULL.get_or_init(|| Regex::new(r"\s*<\s*/dev/null(\s|$)").unwrap());
    // Replace match but keep the trailing boundary char (capture group 1).
    let s = re1.replace_all(content, "$1");
    let s = re2.replace_all(&s, "$1");
    let s = re3.replace_all(&s, "$1");
    s.into_owned()
}

/// TS `hasUnescapedChar(content, char)` (`bashSecurity.ts:209-231`).
fn has_unescaped_char(content: &str, target: char) -> bool {
    let chars: Vec<char> = content.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            i += 2;
            continue;
        }
        if chars[i] == target {
            return true;
        }
        i += 1;
    }
    false
}

/// TS `isEscapedAtPosition(content, pos)` (`bashSecurity.ts:1727-1735`).
/// `pos` is a char index into `chars`.
fn is_escaped_at_position(chars: &[char], pos: usize) -> bool {
    let mut backslash_count = 0usize;
    let mut i = pos as isize - 1;
    while i >= 0 && chars[i as usize] == '\\' {
        backslash_count += 1;
        i -= 1;
    }
    backslash_count % 2 == 1
}

// ── Validators ────────────────────────────────────────────────────────────
// Each returns Some(message) on detection (TS `behavior: 'ask'`), None on
// passthrough. The orchestrator returns the first Some.

/// TS `validateIncompleteCommands` (`bashSecurity.ts:244-286`).
fn validate_incomplete_commands(ctx: &Ctx) -> Option<String> {
    let original = &ctx.original;
    let trimmed = original.trim();

    // `/^\s*\t/` — starts with whitespace then a tab.
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r"^\s*\t").unwrap())
            .is_match(original)
        {
            return Some(
                "Command appears to be an incomplete fragment (starts with tab)".to_string(),
            );
        }
    }
    if trimmed.starts_with('-') {
        return Some(
            "Command appears to be an incomplete fragment (starts with flags)".to_string(),
        );
    }
    // `/^\s*(&&|\|\||;|>>?|<)/`
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r"^\s*(&&|\|\||;|>>?|<)").unwrap())
            .is_match(original)
        {
            return Some(
                "Command appears to be a continuation line (starts with operator)".to_string(),
            );
        }
    }
    None
}

/// Hand-parse of the TS git-commit message regex
/// `/^git[ \t]+commit[ \t]+[^;&|`$<>()\n\r]*?-m[ \t]+(["'])([\s\S]*?)\1(.*)$/`
/// (`bashSecurity.ts:644-646`). Returns `(quote, message_content, remainder)`
/// on a match, mirroring the lazy quantifiers and the `\1` backreference (the
/// closing quote must equal the opening quote). `(.*)$` excludes a newline (JS
/// `.` without `s`-flag), so the remainder is the rest of the FIRST line.
fn parse_git_commit_message(s: &str) -> Option<(char, String, String)> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    // `^git[ \t]+commit[ \t]+`
    let mut i = 0;
    let lit = |chars: &[char], i: usize, word: &str| -> Option<usize> {
        let wc: Vec<char> = word.chars().collect();
        if i + wc.len() <= chars.len() && chars[i..i + wc.len()] == wc[..] {
            Some(i + wc.len())
        } else {
            None
        }
    };
    let hws = |chars: &[char], mut i: usize| -> Option<usize> {
        let start = i;
        while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
            i += 1;
        }
        if i > start {
            Some(i)
        } else {
            None
        }
    };
    i = lit(&chars, i, "git")?;
    i = hws(&chars, i)?;
    i = lit(&chars, i, "commit")?;
    i = hws(&chars, i)?;

    // Lazily scan `[^;&|`$<>()\n\r]*?` then `-m[ \t]+`. Try the SHORTEST run
    // first (lazy): at each position, attempt to match `-m<hws>`; if it fails,
    // consume one allowed char and retry. Disallowed chars abort the scan.
    let is_allowed = |c: char| {
        !matches!(
            c,
            ';' | '&' | '|' | '`' | '$' | '<' | '>' | '(' | ')' | '\n' | '\r'
        )
    };
    let mut j = i;
    loop {
        // Try `-m[ \t]+` at position j.
        if j + 1 < n && chars[j] == '-' && chars[j + 1] == 'm' {
            if let Some(after_m) = hws(&chars, j + 2) {
                // Opening quote.
                if after_m < n && (chars[after_m] == '"' || chars[after_m] == '\'') {
                    let quote = chars[after_m];
                    // Lazy content up to the SAME quote (`[\s\S]*?\1`).
                    let mut k = after_m + 1;
                    while k < n && chars[k] != quote {
                        k += 1;
                    }
                    if k < n && chars[k] == quote {
                        let message_content: String = chars[after_m + 1..k].iter().collect();
                        // `(.*)$` — rest of the first line (no newline).
                        let mut rem_end = k + 1;
                        while rem_end < n && chars[rem_end] != '\n' {
                            rem_end += 1;
                        }
                        let remainder: String = chars[k + 1..rem_end].iter().collect();
                        return Some((quote, message_content, remainder));
                    }
                }
            }
        }
        // Consume one allowed char and retry (lazy expansion).
        if j < n && is_allowed(chars[j]) {
            j += 1;
        } else {
            return None;
        }
    }
}

/// TS `validateGitCommit` (`bashSecurity.ts:612-740`). Faithful port of the
/// detection (ask) branches. The TS `allow`/`passthrough` branches map to
/// "no detection" here (the orchestrator's early-allow → passthrough is a
/// no-op for our Safe/Ask verdict, since we never short-circuit to Safe early).
fn validate_git_commit(ctx: &Ctx) -> Option<String> {
    let original = &ctx.original;
    if ctx.base_command != "git" {
        return None;
    }
    // `/^git\s+commit\s+/`
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if !RE
            .get_or_init(|| Regex::new(r"^git\s+commit\s+").unwrap())
            .is_match(original)
        {
            return None;
        }
    }
    // Backslash → bail to full validation (no detection here).
    if original.contains('\\') {
        return None;
    }
    // The message regex (`:644-646`):
    // /^git[ \t]+commit[ \t]+[^;&|`$<>()\n\r]*?-m[ \t]+(["'])([\s\S]*?)\1(.*)$/
    // The `regex` crate has no backreference for `\1`, so this is hand-parsed:
    // matches `git<hws>commit<hws>`, lazily consumes a metachar-free run up to
    // `-m<hws>`, reads the opening quote, lazily reads content up to the SAME
    // quote, and captures the remainder. `(quote, message_content, remainder)`.
    let (quote, message_content_owned, remainder_owned) = parse_git_commit_message(original)?;
    let message_content = message_content_owned.as_str();
    let remainder = remainder_owned.as_str();

    // Double-quoted message with command substitution (`:651`).
    if quote == '"' && !message_content.is_empty() {
        static SUB_RE: OnceLock<Regex> = OnceLock::new();
        if SUB_RE
            .get_or_init(|| Regex::new(r"\$\(|`|\$\{").unwrap())
            .is_match(message_content)
        {
            return Some("Git commit message contains command substitution patterns".to_string());
        }
    }
    // Remainder shell metacharacters (`:679`) → TS returns passthrough (full
    // validation), NOT an ask. No detection here.
    {
        static REM_RE: OnceLock<Regex> = OnceLock::new();
        if !remainder.is_empty()
            && REM_RE
                .get_or_init(|| Regex::new(r"[;|&()`]|\$\(|\$\{").unwrap())
                .is_match(remainder)
        {
            return None;
        }
    }
    // Remainder unquoted redirect (`:685-714`) → passthrough, no detection.
    if !remainder.is_empty() {
        let mut unquoted = String::new();
        let mut in_sq = false;
        let mut in_dq = false;
        for c in remainder.chars() {
            if c == '\'' && !in_dq {
                in_sq = !in_sq;
                continue;
            }
            if c == '"' && !in_sq {
                in_dq = !in_dq;
                continue;
            }
            if !in_sq && !in_dq {
                unquoted.push(c);
            }
        }
        if unquoted.contains('<') || unquoted.contains('>') {
            return None;
        }
    }
    // Message starting with dash (`:718`) — obfuscated flag.
    if message_content.starts_with('-') {
        return Some("Command contains quoted characters in flag names".to_string());
    }
    // Otherwise TS returns allow (→ passthrough); no detection.
    None
}

/// TS `validateJqCommand` (`bashSecurity.ts:742-781`).
fn validate_jq_command(ctx: &Ctx) -> Option<String> {
    if ctx.base_command != "jq" {
        return None;
    }
    let original = &ctx.original;
    // `/\bsystem\s*\(/`
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r"\bsystem\s*\(").unwrap())
            .is_match(original)
        {
            return Some(
                "jq command contains system() function which executes arbitrary commands"
                    .to_string(),
            );
        }
    }
    // `originalCommand.substring(3).trim()` then dangerous-flag regex (`:763-767`).
    let after_jq: String = original.chars().skip(3).collect();
    let after_jq = after_jq.trim();
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| {
                Regex::new(
                    r"(?:^|\s)(?:-f\b|--from-file|--rawfile|--slurpfile|-L\b|--library-path)",
                )
                .unwrap()
            })
            .is_match(after_jq)
        {
            return Some(
                "jq command contains dangerous flags that could execute code or read arbitrary files"
                    .to_string(),
            );
        }
    }
    None
}

/// TS `validateObfuscatedFlags` (`bashSecurity.ts:1130-1537`).
fn validate_obfuscated_flags(ctx: &Ctx) -> Option<String> {
    let original = &ctx.original;
    let base_command = &ctx.base_command;

    // Simple `echo` (no shell operators) is safe (`:1138-1144`).
    static OPS_RE: OnceLock<Regex> = OnceLock::new();
    let has_shell_operators = OPS_RE
        .get_or_init(|| Regex::new(r"[|&;]").unwrap())
        .is_match(original);
    if base_command == "echo" && !has_shell_operators {
        return None;
    }

    // 1. ANSI-C quoting `$'...'` — `/\$'[^']*'/` (`:1155`).
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r"\$'[^']*'").unwrap())
            .is_match(original)
        {
            return Some("Command contains ANSI-C quoting which can hide characters".to_string());
        }
    }
    // 2. Locale quoting `$"..."` — `/\$"[^"]*"/` (`:1168`).
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r#"\$"[^"]*""#).unwrap())
            .is_match(original)
        {
            return Some("Command contains locale quoting which can hide characters".to_string());
        }
    }
    // 3. Empty ANSI-C/locale quotes before dash — `/\$['"]{2}\s*-/` (`:1181`).
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r#"\$['"]{2}\s*-"#).unwrap())
            .is_match(original)
        {
            return Some(
                "Command contains empty special quotes before dash (potential bypass)".to_string(),
            );
        }
    }
    // 4. Empty quote pairs before dash — `/(?:^|\s)(?:''|"")+\s*-/` (`:1196`).
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r#"(?:^|\s)(?:''|"")+\s*-"#).unwrap())
            .is_match(original)
        {
            return Some(
                "Command contains empty quotes before dash (potential bypass)".to_string(),
            );
        }
    }
    // 4b. Homogeneous empty pair adjacent to quoted dash — `/(?:""|'')+['"]-/` (`:1237`).
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r#"(?:""|'')+['"]-"#).unwrap())
            .is_match(original)
        {
            return Some(
                "Command contains empty quote pair adjacent to quoted dash (potential flag obfuscation)"
                    .to_string(),
            );
        }
    }
    // 4c. 3+ consecutive quotes at word start — `/(?:^|\s)['"]{3,}/` (`:1253`).
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r#"(?:^|\s)['"]{3,}"#).unwrap())
            .is_match(original)
        {
            return Some(
                "Command contains consecutive quote characters at word start (potential obfuscation)"
                    .to_string(),
            );
        }
    }

    // Char-scan for quoted-flag obfuscation (`:1265-1508`). Faithful port of the
    // quote-state tracker and the two flag detectors:
    //   (a) whitespace + quote whose content / continuation forms a flag;
    //   (b) whitespace + dash whose collected flag content contains a quote.
    let chars: Vec<char> = original.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let n = chars.len();
    if n >= 1 {
        for i in 0..n - 1 {
            let current = chars[i];
            let next = chars[i + 1];

            if escaped {
                escaped = false;
                continue;
            }
            if current == '\\' && !in_single {
                escaped = true;
                continue;
            }
            if current == '\'' && !in_double {
                in_single = !in_single;
                continue;
            }
            if current == '"' && !in_single {
                in_double = !in_double;
                continue;
            }
            if in_single || in_double {
                continue;
            }

            // (a) whitespace followed by a quote (`:1317-1450`).
            if current.is_whitespace() && (next == '\'' || next == '"' || next == '`') {
                let quote_char = next;
                let mut j = i + 2;
                let mut inside_quote = String::new();
                while j < n && chars[j] != quote_char {
                    inside_quote.push(chars[j]);
                    j += 1;
                }
                let char_after_quote = chars.get(j + 1).copied();

                // `/^-+[a-zA-Z0-9$`]/` on inside_quote.
                let has_flag_chars_inside = flag_chars_inside(&inside_quote);
                // continuation: inside is all dashes AND next char continues a flag.
                let has_flag_chars_continuing = is_all_dashes(&inside_quote)
                    && !inside_quote.is_empty()
                    && char_after_quote.is_some_and(is_flag_continuation_char);
                let has_flag_chars_in_next_quote = (inside_quote.is_empty()
                    || is_all_dashes(&inside_quote))
                    && char_after_quote.is_some_and(|c| c == '\'' || c == '"' || c == '`')
                    && flag_chain_forms_flag(&chars, j, &inside_quote);

                if j < n
                    && chars[j] == quote_char
                    && (has_flag_chars_inside
                        || has_flag_chars_continuing
                        || has_flag_chars_in_next_quote)
                {
                    return Some("Command contains quoted characters in flag names".to_string());
                }
            }

            // (b) whitespace followed by dash (`:1453-1507`).
            if current.is_whitespace() && next == '-' {
                let mut j = i + 1;
                let mut flag_content = String::new();
                while j < n {
                    let flag_char = chars[j];
                    if flag_char.is_whitespace() || flag_char == '=' {
                        break;
                    }
                    if flag_char == '\'' || flag_char == '"' || flag_char == '`' {
                        // cut -d quoted delimiter exception (`:1475-1482`).
                        if base_command == "cut" && flag_content == "-d" {
                            break;
                        }
                        if j + 1 < n {
                            let nxt = chars[j + 1];
                            if !is_flag_inner_char(nxt) {
                                break;
                            }
                        }
                    }
                    flag_content.push(flag_char);
                    j += 1;
                }
                if flag_content.contains('"') || flag_content.contains('\'') {
                    return Some("Command contains quoted characters in flag names".to_string());
                }
            }
        }
    }

    // `/\s['"`]-/` on fullyUnquotedContent (`:1512`).
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r#"\s['"`]-"#).unwrap())
            .is_match(&ctx.fully_unquoted)
        {
            return Some("Command contains quoted characters in flag names".to_string());
        }
    }
    // `/['"`]{2}-/` on fullyUnquotedContent (`:1525`).
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r#"['"`]{2}-"#).unwrap())
            .is_match(&ctx.fully_unquoted)
        {
            return Some("Command contains quoted characters in flag names".to_string());
        }
    }
    None
}

/// `/^-+[a-zA-Z0-9$`]/` (TS `:1346`, `:1392`).
fn flag_chars_inside(s: &str) -> bool {
    let mut chars = s.chars().peekable();
    let mut saw_dash = false;
    while let Some(&c) = chars.peek() {
        if c == '-' {
            saw_dash = true;
            chars.next();
        } else {
            break;
        }
    }
    if !saw_dash {
        return false;
    }
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric() || c == '$' || c == '`')
}

/// `/^-+$/` — non-empty, all dashes.
fn is_all_dashes(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c == '-')
}

/// TS `FLAG_CONTINUATION_CHARS = /[a-zA-Z0-9\\${`-]/` (`:1357`).
fn is_flag_continuation_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '\\' || c == '$' || c == '{' || c == '`' || c == '-'
}

/// `/[a-zA-Z0-9_'"-]/` — flag-inner char after a quote (TS `:1487`).
fn is_flag_inner_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '\'' || c == '"' || c == '-'
}

/// Faithful port of the adjacent-quote-chaining IIFE (`:1372-1432`). `chars` is
/// the command, `j` is the index of the first segment's CLOSING quote, and
/// `inside_quote` is that segment's content. Returns whether the chain forms a
/// flag.
fn flag_chain_forms_flag(chars: &[char], j: usize, inside_quote: &str) -> bool {
    let n = chars.len();
    let mut pos = j + 1;
    let mut combined: String = inside_quote.to_string();
    while pos < n && (chars[pos] == '\'' || chars[pos] == '"' || chars[pos] == '`') {
        let seg_quote = chars[pos];
        let mut end = pos + 1;
        while end < n && chars[end] != seg_quote {
            end += 1;
        }
        let segment: String = chars[pos + 1..end.min(n)].iter().collect();
        combined.push_str(&segment);

        // `/^-+[a-zA-Z0-9$`]/` on combined.
        if flag_chars_inside(&combined) {
            return true;
        }
        // prior = combined minus this segment.
        let prior: String = if segment.is_empty() {
            combined.clone()
        } else {
            let cut = combined.len() - segment.len();
            combined[..cut].to_string()
        };
        if is_all_dashes(&prior)
            && segment
                .chars()
                .any(|c| c.is_ascii_alphanumeric() || c == '$' || c == '`')
        {
            return true;
        }
        if end >= n {
            break;
        }
        pos = end + 1;
    }
    // trailing unquoted char (`:1410-1431`).
    if pos < n && is_flag_continuation_char(chars[pos]) {
        let combined_all_dashes_or_empty = combined.is_empty() || is_all_dashes(&combined);
        if combined_all_dashes_or_empty {
            let next_char = chars[pos];
            if next_char == '-' {
                return true;
            }
            if (next_char.is_ascii_alphanumeric()
                || next_char == '\\'
                || next_char == '$'
                || next_char == '{'
                || next_char == '`')
                && !combined.is_empty()
            {
                return true;
            }
        }
        if combined.starts_with('-') {
            return true;
        }
    }
    false
}

/// TS `validateShellMetacharacters` (`bashSecurity.ts:783-821`).
fn validate_shell_metacharacters(ctx: &Ctx) -> Option<String> {
    let content = &ctx.unquoted_with_dq;
    let message = "Command contains shell metacharacters (;, |, or &) in arguments".to_string();
    // `/(?:^|\s)["'][^"']*[;&][^"']*["'](?:\s|$)/`
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r#"(?:^|\s)["'][^"']*[;&][^"']*["'](?:\s|$)"#).unwrap())
            .is_match(content)
        {
            return Some(message);
        }
    }
    // glob patterns -name / -path / -iname with `[;|&]`.
    {
        static RE: OnceLock<[Regex; 3]> = OnceLock::new();
        let res = RE.get_or_init(|| {
            [
                Regex::new(r#"-name\s+["'][^"']*[;|&][^"']*["']"#).unwrap(),
                Regex::new(r#"-path\s+["'][^"']*[;|&][^"']*["']"#).unwrap(),
                Regex::new(r#"-iname\s+["'][^"']*[;|&][^"']*["']"#).unwrap(),
            ]
        });
        if res.iter().any(|r| r.is_match(content)) {
            return Some(message);
        }
    }
    // `-regex` with `[;&]`.
    {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r#"-regex\s+["'][^"']*[;&][^"']*["']"#).unwrap())
            .is_match(content)
        {
            return Some(message);
        }
    }
    None
}

/// TS `validateDangerousVariables` (`bashSecurity.ts:823-844`).
fn validate_dangerous_variables(ctx: &Ctx) -> Option<String> {
    let content = &ctx.fully_unquoted;
    static RE1: OnceLock<Regex> = OnceLock::new();
    static RE2: OnceLock<Regex> = OnceLock::new();
    let re1 = RE1.get_or_init(|| Regex::new(r"[<>|]\s*\$[A-Za-z_]").unwrap());
    let re2 = RE2.get_or_init(|| Regex::new(r"\$[A-Za-z_][A-Za-z0-9_]*\s*[|<>]").unwrap());
    if re1.is_match(content) || re2.is_match(content) {
        return Some(
            "Command contains variables in dangerous contexts (redirections or pipes)".to_string(),
        );
    }
    None
}

/// TS `validateDangerousPatterns` (`bashSecurity.ts:846-873`).
/// COMMAND_SUBSTITUTION_PATTERNS (`:16-41`) ported as ordered (regex, message).
fn validate_dangerous_patterns(ctx: &Ctx) -> Option<String> {
    let content = &ctx.unquoted_with_dq;
    // Unescaped backtick (`:853`).
    if has_unescaped_char(content, '`') {
        return Some("Command contains backticks (`) for command substitution".to_string());
    }
    static PATS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let pats = PATS.get_or_init(|| {
        vec![
            (Regex::new(r"<\(").unwrap(), "process substitution <()"),
            (Regex::new(r">\(").unwrap(), "process substitution >()"),
            (Regex::new(r"=\(").unwrap(), "Zsh process substitution =()"),
            (
                Regex::new(r"(?:^|[\s;&|])=[a-zA-Z_]").unwrap(),
                "Zsh equals expansion (=cmd)",
            ),
            (Regex::new(r"\$\(").unwrap(), "$() command substitution"),
            (Regex::new(r"\$\{").unwrap(), "${} parameter substitution"),
            (
                Regex::new(r"\$\[").unwrap(),
                "$[] legacy arithmetic expansion",
            ),
            (Regex::new(r"~\[").unwrap(), "Zsh-style parameter expansion"),
            (Regex::new(r"\(e:").unwrap(), "Zsh-style glob qualifiers"),
            (
                Regex::new(r"\(\+").unwrap(),
                "Zsh glob qualifier with command execution",
            ),
            (
                Regex::new(r"\}\s*always\s*\{").unwrap(),
                "Zsh always block (try/always construct)",
            ),
            (Regex::new(r"<#").unwrap(), "PowerShell comment syntax"),
        ]
    });
    for (re, msg) in pats {
        if re.is_match(content) {
            return Some(format!("Command contains {msg}"));
        }
    }
    None
}

/// TS `validateRedirections` (`bashSecurity.ts:875-903`). NON-misparsing.
fn validate_redirections(ctx: &Ctx) -> Option<String> {
    let content = &ctx.fully_unquoted;
    if content.contains('<') {
        return Some(
            "Command contains input redirection (<) which could read sensitive files".to_string(),
        );
    }
    if content.contains('>') {
        return Some(
            "Command contains output redirection (>) which could write to arbitrary files"
                .to_string(),
        );
    }
    None
}

/// TS `validateNewlines` (`bashSecurity.ts:905-941`). NON-misparsing. Uses
/// `fullyUnquotedPreStrip`. The lookbehind `/(?<![\s]\\)[\n\r]\s*\S/` is ported
/// as a hand scan (regex crate has no lookbehind).
fn validate_newlines(ctx: &Ctx) -> Option<String> {
    let content = &ctx.fully_unquoted_pre_strip;
    if !content.contains('\n') && !content.contains('\r') {
        return None;
    }
    // Flag any \n/\r followed (after optional whitespace) by non-whitespace,
    // EXCEPT when the newline is preceded by `<space>\` (a line continuation at
    // a word boundary). Equivalent to `/(?<![\s]\\)[\n\r]\s*\S/.test(content)`.
    let chars: Vec<char> = content.chars().collect();
    let n = chars.len();
    for i in 0..n {
        if chars[i] != '\n' && chars[i] != '\r' {
            continue;
        }
        // negative lookbehind `(?<![\s]\\)`: the 2 chars before must NOT be
        // [whitespace][backslash].
        let lb_blocked = i >= 2 && chars[i - 1] == '\\' && chars[i - 2].is_whitespace();
        if lb_blocked {
            continue;
        }
        // `\s*\S` after the newline.
        let mut j = i + 1;
        while j < n && chars[j].is_whitespace() {
            j += 1;
        }
        if j < n {
            // chars[j] is non-whitespace.
            return Some(
                "Command contains newlines that could separate multiple commands".to_string(),
            );
        }
    }
    None
}

/// TS `validateCarriageReturn` (`bashSecurity.ts:971-1015`). Misparsing.
fn validate_carriage_return(ctx: &Ctx) -> Option<String> {
    let original = &ctx.original;
    if !original.contains('\r') {
        return None;
    }
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for c in original.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && !in_single {
            escaped = true;
            continue;
        }
        if c == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if c == '\r' && !in_double {
            return Some(
                "Command contains carriage return (\\r) which shell-quote and bash tokenize differently"
                    .to_string(),
            );
        }
    }
    None
}

/// TS `validateIFSInjection` (`bashSecurity.ts:1017-1036`).
fn validate_ifs_injection(ctx: &Ctx) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `/\$IFS|\$\{[^}]*IFS/`
    if RE
        .get_or_init(|| Regex::new(r"\$IFS|\$\{[^}]*IFS").unwrap())
        .is_match(&ctx.original)
    {
        return Some(
            "Command contains IFS variable usage which could bypass security validation"
                .to_string(),
        );
    }
    None
}

/// TS `validateProcEnvironAccess` (`bashSecurity.ts:1041-1067`).
fn validate_proc_environ_access(ctx: &Ctx) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    if RE
        .get_or_init(|| Regex::new(r"/proc/.*/environ").unwrap())
        .is_match(&ctx.original)
    {
        return Some(
            "Command accesses /proc/*/environ which could expose sensitive environment variables"
                .to_string(),
        );
    }
    None
}

/// TS `hasBackslashEscapedWhitespace` (`bashSecurity.ts:1549-1581`).
fn has_backslash_escaped_whitespace(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && !in_single {
            if !in_double {
                if let Some(&nxt) = chars.get(i + 1) {
                    if nxt == ' ' || nxt == '\t' {
                        return true;
                    }
                }
            }
            i += 1;
            i += 1;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        i += 1;
    }
    false
}

/// TS `validateBackslashEscapedWhitespace` (`bashSecurity.ts:1583-1601`).
fn validate_backslash_escaped_whitespace(ctx: &Ctx) -> Option<String> {
    if has_backslash_escaped_whitespace(&ctx.original) {
        return Some(
            "Command contains backslash-escaped whitespace that could alter command parsing"
                .to_string(),
        );
    }
    None
}

/// TS `hasBackslashEscapedOperator` (`bashSecurity.ts:1631-1694`).
/// SHELL_OPERATORS = `;`, `|`, `&`, `<`, `>` (`:1629`).
fn has_backslash_escaped_operator(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && !in_single {
            if !in_double {
                if let Some(&nxt) = chars.get(i + 1) {
                    if matches!(nxt, ';' | '|' | '&' | '<' | '>') {
                        return true;
                    }
                }
            }
            i += 1;
            i += 1;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        i += 1;
    }
    false
}

/// TS `validateBackslashEscapedOperators` (`bashSecurity.ts:1696-1721`),
/// including the tree-sitter short-circuit (`:1702-1704`): when the AST proves
/// there are no real operator nodes, a backslash-escaped operator is a literal
/// argument (e.g. `find -exec \;`), not hidden structure — passthrough.
fn validate_backslash_escaped_operators(ctx: &Ctx) -> Option<String> {
    if ctx.has_actual_operator_nodes == Some(false) {
        return None;
    }
    if has_backslash_escaped_operator(&ctx.original) {
        return Some(
            "Command contains a backslash before a shell operator (;, |, &, <, >) which can hide command structure"
                .to_string(),
        );
    }
    None
}

/// Matches Unicode whitespace (TS `UNICODE_WS_RE`, `bashSecurity.ts:1899-1900`).
fn validate_unicode_whitespace(ctx: &Ctx) -> Option<String> {
    let hit = ctx.original.chars().any(|c| {
        matches!(
            c,
            '\u{00A0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200A}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202F}'
                    | '\u{205F}'
                    | '\u{3000}'
                    | '\u{FEFF}'
        )
    });
    if hit {
        return Some(
            "Command contains Unicode whitespace characters that could cause parsing inconsistencies"
                .to_string(),
        );
    }
    None
}

/// TS `validateMidWordHash` (`bashSecurity.ts:1919-1962`). Uses
/// `unquotedKeepQuoteChars` (and its continuation-joined variant). The lookbehind
/// `/\S(?<!\$\{)#/` is ported as a hand scan.
fn validate_mid_word_hash(ctx: &Ctx) -> Option<String> {
    let base = &ctx.unquoted_keep_quote_chars;
    // joined = base with `\\+\n` runs collapsed per TS `:1942-1945`.
    let joined = collapse_continuations(base);
    if mid_word_hash_hit(base) || mid_word_hash_hit(&joined) {
        return Some(
            "Command contains mid-word # which is parsed differently by shell-quote vs bash"
                .to_string(),
        );
    }
    None
}

/// `/\S(?<!\$\{)#/.test(s)` — a `#` preceded by a non-whitespace char, where the
/// two chars immediately before `#` are NOT `${`.
fn mid_word_hash_hit(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    for i in 0..chars.len() {
        if chars[i] != '#' {
            continue;
        }
        if i == 0 {
            continue;
        }
        let prev = chars[i - 1];
        if prev.is_whitespace() {
            continue;
        }
        // negative lookbehind `(?<!\$\{)`: the 2 chars before `#` are not `${`.
        let is_dollar_brace = i >= 2 && chars[i - 2] == '$' && chars[i - 1] == '{';
        if is_dollar_brace {
            continue;
        }
        return true;
    }
    false
}

/// TS `joined = unquotedKeepQuoteChars.replace(/\\+\n/g, …)` (`:1942-1945`):
/// for a run of `k` backslashes before a `\n`, if `k` is odd keep `k-1`
/// backslashes (drop the continuation pair `\<NL>`), else keep the run + `\n`.
fn collapse_continuations(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let n = chars.len();
    while i < n {
        if chars[i] == '\\' {
            // count the backslash run.
            let start = i;
            while i < n && chars[i] == '\\' {
                i += 1;
            }
            let run = i - start;
            if i < n && chars[i] == '\n' {
                if run % 2 == 1 {
                    // keep run-1 backslashes, drop the `\` + `\n` pair.
                    for _ in 0..run - 1 {
                        out.push('\\');
                    }
                    i += 1; // consume the '\n'
                } else {
                    for _ in 0..run {
                        out.push('\\');
                    }
                    out.push('\n');
                    i += 1;
                }
            } else {
                for _ in 0..run {
                    out.push('\\');
                }
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// TS `validateCommentQuoteDesync` (`bashSecurity.ts:1990-2089`), including the
/// tree-sitter passthrough (`:1998-2003`): when an AST quoteContext is present,
/// the other validators already read AST-accurate quote stripping, so a quote
/// inside a `#` comment cannot desync them — passthrough.
fn validate_comment_quote_desync(ctx: &Ctx) -> Option<String> {
    if ctx.tree_sitter_present {
        return None;
    }
    let chars: Vec<char> = ctx.original.chars().collect();
    let n = chars.len();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            i += 1;
            continue;
        }
        if in_double {
            if ch == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        if ch == '\'' {
            in_single = true;
            i += 1;
            continue;
        }
        if ch == '"' {
            in_double = true;
            i += 1;
            continue;
        }
        if ch == '#' {
            // Rest of line until '\n'.
            let mut k = i + 1;
            let mut has_quote = false;
            while k < n && chars[k] != '\n' {
                if chars[k] == '\'' || chars[k] == '"' {
                    has_quote = true;
                    break;
                }
                k += 1;
            }
            if has_quote {
                return Some(
                    "Command contains quote characters inside a # comment which can desync quote tracking"
                        .to_string(),
                );
            }
            // Skip to end of line.
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    None
}

/// TS `validateQuotedNewline` (`bashSecurity.ts:2109-2174`). Misparsing.
fn validate_quoted_newline(ctx: &Ctx) -> Option<String> {
    let original = &ctx.original;
    if !original.contains('\n') || !original.contains('#') {
        return None;
    }
    let chars: Vec<char> = original.chars().collect();
    let n = chars.len();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for i in 0..n {
        let ch = chars[i];
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch == '\n' && (in_single || in_double) {
            let line_start = i + 1;
            // next line = up to next '\n'.
            let mut k = line_start;
            while k < n && chars[k] != '\n' {
                k += 1;
            }
            let next_line: String = chars[line_start..k].iter().collect();
            if next_line.trim_start().starts_with('#') {
                return Some(
                    "Command contains a quoted newline followed by a #-prefixed line, which can hide arguments from line-based permission checks"
                        .to_string(),
                );
            }
        }
    }
    None
}

/// Zsh-specific dangerous commands (TS `ZSH_DANGEROUS_COMMANDS`, `:45-74`).
const ZSH_DANGEROUS_COMMANDS: &[&str] = &[
    "zmodload", "emulate", "sysopen", "sysread", "syswrite", "sysseek", "zpty", "ztcp", "zsocket",
    "mapfile", "zf_rm", "zf_mv", "zf_ln", "zf_chmod", "zf_chown", "zf_mkdir", "zf_rmdir",
    "zf_chgrp",
];

/// TS `validateZshDangerousCommands` (`bashSecurity.ts:2186-2242`).
fn validate_zsh_dangerous_commands(ctx: &Ctx) -> Option<String> {
    let trimmed = ctx.original.trim();
    // base command after skipping env-var assignments + precommand modifiers.
    const PRECOMMAND: &[&str] = &["command", "builtin", "noglob", "nocorrect"];
    static ENV_RE: OnceLock<Regex> = OnceLock::new();
    let env_re = ENV_RE.get_or_init(|| Regex::new(r"^[A-Za-z_]\w*=").unwrap());
    let mut base_cmd = "";
    for token in trimmed.split_whitespace() {
        if env_re.is_match(token) {
            continue;
        }
        if PRECOMMAND.contains(&token) {
            continue;
        }
        base_cmd = token;
        break;
    }
    if ZSH_DANGEROUS_COMMANDS.contains(&base_cmd) {
        return Some(format!(
            "Command uses Zsh-specific '{base_cmd}' which can bypass security checks"
        ));
    }
    // `fc -e` (`:2226`): `/\s-\S*e/`.
    if base_cmd == "fc" {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r"\s-\S*e").unwrap())
            .is_match(trimmed)
        {
            return Some(
                "Command uses 'fc -e' which can execute arbitrary commands via editor".to_string(),
            );
        }
    }
    None
}

/// TS `validateBraceExpansion` (`bashSecurity.ts:1751-1892`). Uses
/// `fullyUnquotedPreStrip`.
fn validate_brace_expansion(ctx: &Ctx) -> Option<String> {
    let content_str = &ctx.fully_unquoted_pre_strip;
    let content: Vec<char> = content_str.chars().collect();

    // Count unescaped braces (`:1776-1784`).
    let mut open = 0usize;
    let mut close = 0usize;
    for i in 0..content.len() {
        if content[i] == '{' && !is_escaped_at_position(&content, i) {
            open += 1;
        } else if content[i] == '}' && !is_escaped_at_position(&content, i) {
            close += 1;
        }
    }
    // Excess closing braces (`:1790`).
    if open > 0 && close > open {
        return Some(
            "Command has excess closing braces after quote stripping, indicating possible brace expansion obfuscation"
                .to_string(),
        );
    }
    // Quoted brace inside brace context (`:1813-1828`): `/['"][{}]['"]/`.
    if open > 0 {
        static RE: OnceLock<Regex> = OnceLock::new();
        if RE
            .get_or_init(|| Regex::new(r#"['"][{}]['"]"#).unwrap())
            .is_match(&ctx.original)
        {
            return Some(
                "Command contains quoted brace character inside brace context (potential brace expansion obfuscation)"
                    .to_string(),
            );
        }
    }
    // Depth-matched brace expansion with outer-level `,` or `..` (`:1833-1886`).
    for i in 0..content.len() {
        if content[i] != '{' || is_escaped_at_position(&content, i) {
            continue;
        }
        let mut depth = 1i32;
        let mut matching_close: isize = -1;
        let mut j = i + 1;
        while j < content.len() {
            let c = content[j];
            if c == '{' && !is_escaped_at_position(&content, j) {
                depth += 1;
            } else if c == '}' && !is_escaped_at_position(&content, j) {
                depth -= 1;
                if depth == 0 {
                    matching_close = j as isize;
                    break;
                }
            }
            j += 1;
        }
        if matching_close == -1 {
            continue;
        }
        let mc = matching_close as usize;
        let mut inner_depth = 0i32;
        let mut k = i + 1;
        while k < mc {
            let c = content[k];
            if c == '{' && !is_escaped_at_position(&content, k) {
                inner_depth += 1;
            } else if c == '}' && !is_escaped_at_position(&content, k) {
                inner_depth -= 1;
            } else if inner_depth == 0
                && (c == ',' || (c == '.' && k + 1 < mc && content[k + 1] == '.'))
            {
                return Some(
                    "Command contains brace expansion that could alter command parsing".to_string(),
                );
            }
            k += 1;
        }
    }
    None
}

/// TS `validateMalformedTokenInjection` (`bashSecurity.ts:1082-1128`) — FAITHFUL
/// SUBSET. The full check relies on the `shell-quote` token list
/// (`tryParseShellCommand` / `hasMalformedTokens`, `shellQuote.ts:117-176`),
/// which has no port in this crate. We port (1) the command-separator gate
/// (`;`, `&&`, `||`) using a quote-aware scan — TS checks the token op array;
/// ours scans the raw command honoring quotes/escapes, equivalent for these
/// operators — and (2) the raw-command unbalanced-quote check
/// (`hasMalformedTokens` `:121-143`).
///
/// The per-token brace/paren/bracket checks (which need the shell-quote tokens)
/// are deferred — documented STRICTER-or-equal: we may miss some token-level
/// malformations but never produce a false Safe relative to TS detection here.
fn validate_malformed_token_injection(ctx: &Ctx) -> Option<String> {
    let command = &ctx.original;
    if !has_command_separator_raw(command) {
        return None;
    }
    if has_unbalanced_quotes_raw(command) {
        return Some(
            "Command contains ambiguous syntax with command separators that could be misinterpreted"
                .to_string(),
        );
    }
    None
}

/// Quote/escape-aware scan for an UNQUOTED `;`, `&&`, or `||` (the TS separator
/// ops). Mirrors the bash quote semantics used throughout this file.
fn has_command_separator_raw(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    let n = chars.len();
    while i < n {
        let c = chars[i];
        if c == '\\' && !in_single {
            i += 2;
            continue;
        }
        if c == '\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double {
            if c == ';' {
                return true;
            }
            if (c == '&' && i + 1 < n && chars[i + 1] == '&')
                || (c == '|' && i + 1 < n && chars[i + 1] == '|')
            {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// TS `hasMalformedTokens` raw-command part (`shellQuote.ts:121-143`): walk with
/// bash semantics counting unescaped `"` and `'`; odd parity = malformed.
fn has_unbalanced_quotes_raw(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    let mut double_count = 0usize;
    let mut single_count = 0usize;
    let mut i = 0;
    let n = chars.len();
    while i < n {
        let c = chars[i];
        if c == '\\' && !in_single {
            i += 2;
            continue;
        }
        if c == '"' && !in_single {
            double_count += 1;
            in_double = !in_double;
        } else if c == '\'' && !in_double {
            single_count += 1;
            in_single = !in_single;
        }
        i += 1;
    }
    !double_count.is_multiple_of(2) || !single_count.is_multiple_of(2)
}

/// Control-character pre-check (TS `CONTROL_CHAR_RE`, `bashSecurity.ts:2251`).
/// Misparsing; runs FIRST.
fn has_control_chars(command: &str) -> bool {
    command.chars().any(|c| {
        let u = c as u32;
        (u <= 0x08) || u == 0x0B || u == 0x0C || (0x0E..=0x1F).contains(&u) || u == 0x7F
    })
}

/// TS `hasShellQuoteSingleQuoteBug` (`shellQuote.ts:190-265`). Misparsing;
/// runs SECOND (before any validator).
fn has_shell_quote_single_quote_bug(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let n = chars.len();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        if ch == '\\' && !in_single {
            i += 2;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            // Just closed a single quote: check trailing backslash parity.
            if !in_single {
                let mut backslash_count = 0usize;
                let mut j = i as isize - 1;
                while j >= 0 && chars[j as usize] == '\\' {
                    backslash_count += 1;
                    j -= 1;
                }
                if backslash_count > 0 && backslash_count % 2 == 1 {
                    return true;
                }
                if backslash_count > 0
                    && backslash_count.is_multiple_of(2)
                    && chars[i + 1..].contains(&'\'')
                {
                    return true;
                }
            }
            i += 1;
            continue;
        }
        i += 1;
    }
    false
}

/// Entry point — faithful port of `bashCommandIsSafe_DEPRECATED`
/// (`bashSecurity.ts:2257-2413`).
///
/// Runs the validator battery in TS order and returns the first detection's
/// ask message, else [`BashSafetyVerdict::Safe`]. The TS deferred-non-misparsing
/// logic (`:2392-2407`) is replicated: `validate_newlines` and
/// `validate_redirections` ask-results are deferred — if a later misparsing
/// validator fires it wins; only if none fires does the deferred ask return.
///
/// HEREDOC NOTE: TS strips quoted heredoc bodies via `extractHeredocs` before
/// extracting quoted content (`:2293`). We do NOT have a heredoc-body extractor
/// here; we run the validators on the raw command. This is STRICTER (a quoted
/// heredoc body's chars are still scanned), never a bypass.
#[must_use]
pub fn bash_command_is_safe(command: &str) -> BashSafetyVerdict {
    // 1. Control characters (`:2263-2273`).
    if has_control_chars(command) {
        return BashSafetyVerdict::Ask {
            message: "Command contains non-printable control characters that could be used to bypass security checks".to_string(),
        };
    }
    // 2. shell-quote single-quote bug (`:2277-2284`).
    if has_shell_quote_single_quote_bug(command) {
        return BashSafetyVerdict::Ask {
            message:
                "Command contains single-quoted backslash pattern that could bypass security checks"
                    .to_string(),
        };
    }

    // Build the validation context (`:2295-2306`).
    let base_command = command.split(' ').next().unwrap_or("").to_string();
    let is_jq = base_command == "jq";
    // claude-code's tree-sitter path sources the quote views from the AST
    // quoteContext (bashSecurity.ts:2468-2488), which is structural — NO is_jq
    // special-case (that quirk is legacy-regex-only). None (empty/over-cap/
    // unparseable) falls back to the legacy regex stripper.
    #[cfg(feature = "bash-ast")]
    let (with_double_quotes, fully_unquoted, unquoted_keep_quote_chars, tree_sitter_present) =
        if let Some(qc) = crate::bash_tree_sitter::extract_quote_context(command) {
            (
                qc.with_double_quotes,
                qc.fully_unquoted,
                qc.unquoted_keep_quote_chars,
                true,
            )
        } else {
            let (a, b, c) = extract_quoted_content(command, is_jq);
            (a, b, c, false)
        };
    #[cfg(not(feature = "bash-ast"))]
    let (with_double_quotes, fully_unquoted, unquoted_keep_quote_chars, tree_sitter_present) = {
        let (a, b, c) = extract_quoted_content(command, is_jq);
        (a, b, c, false)
    };
    // AST operator-node detection (claude-code's tree-sitter precision layer).
    // Feature off => None => every validator keeps the legacy regex path.
    #[cfg(feature = "bash-ast")]
    let has_actual_operator_nodes = crate::bash_tree_sitter::has_actual_operator_nodes(command);
    #[cfg(not(feature = "bash-ast"))]
    let has_actual_operator_nodes: Option<bool> = None;
    let ctx = Ctx {
        original: command.to_string(),
        base_command,
        unquoted_with_dq: with_double_quotes,
        fully_unquoted: strip_safe_redirections(&fully_unquoted),
        fully_unquoted_pre_strip: fully_unquoted,
        unquoted_keep_quote_chars,
        has_actual_operator_nodes,
        tree_sitter_present,
    };

    // Empty command (`validateEmpty`, `:233-242`): an early-allow → passthrough
    // in TS. An empty command produces Safe here (no further validation).
    if ctx.original.trim().is_empty() {
        return BashSafetyVerdict::Safe;
    }

    // Early validators (`:2308-2332`). In TS an `allow` here maps to
    // passthrough (Safe); an `ask` short-circuits. None of these are deferred.
    if let Some(message) = validate_incomplete_commands(&ctx) {
        return BashSafetyVerdict::Ask { message };
    }
    // validateSafeCommandSubstitution: TS early-allows a PROVABLY-safe heredoc
    // substitution. We do not port the full verifier — we never early-allow
    // here, so a heredoc-in-substitution falls through to validateDangerousPatterns
    // (the `$(` detector) and asks. STRICTER, documented.
    if let Some(message) = validate_git_commit(&ctx) {
        return BashSafetyVerdict::Ask { message };
    }

    // Main validators (`:2348-2378`), with deferred-non-misparsing handling.
    // The order is EXACTLY the TS `validators` array.
    type V = fn(&Ctx) -> Option<String>;
    let validators: [(V, bool); 18] = [
        (validate_jq_command, false),
        (validate_obfuscated_flags, false),
        (validate_shell_metacharacters, false),
        (validate_dangerous_variables, false),
        (validate_comment_quote_desync, false),
        (validate_quoted_newline, false),
        (validate_carriage_return, false),
        (validate_newlines, true), // non-misparsing → deferred
        (validate_ifs_injection, false),
        (validate_proc_environ_access, false),
        (validate_dangerous_patterns, false),
        (validate_redirections, true), // non-misparsing → deferred
        (validate_backslash_escaped_whitespace, false),
        (validate_backslash_escaped_operators, false),
        (validate_unicode_whitespace, false),
        (validate_mid_word_hash, false),
        (validate_brace_expansion, false),
        (validate_zsh_dangerous_commands, false),
    ];
    // validateMalformedTokenInjection runs LAST (`:2377`).
    let mut deferred: Option<String> = None;
    for (validator, is_non_misparsing) in validators {
        if let Some(message) = validator(&ctx) {
            if is_non_misparsing {
                if deferred.is_none() {
                    deferred = Some(message);
                }
                continue;
            }
            return BashSafetyVerdict::Ask { message };
        }
    }
    if let Some(message) = validate_malformed_token_injection(&ctx) {
        return BashSafetyVerdict::Ask { message };
    }
    if let Some(message) = deferred {
        return BashSafetyVerdict::Ask { message };
    }
    BashSafetyVerdict::Safe
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asks(command: &str) -> bool {
        matches!(bash_command_is_safe(command), BashSafetyVerdict::Ask { .. })
    }

    fn message(command: &str) -> Option<String> {
        match bash_command_is_safe(command) {
            BashSafetyVerdict::Ask { message } => Some(message),
            BashSafetyVerdict::Safe => None,
        }
    }

    // ── benign negatives ────────────────────────────────────────────────
    #[test]
    fn benign_commands_are_safe() {
        assert!(!asks("ls -la"));
        assert!(!asks("git status"));
        assert!(!asks("npm install"));
        assert!(!asks("cargo build --release"));
        assert!(!asks("grep foo bar.txt"));
        assert!(!asks(""));
        assert!(!asks("   "));
    }

    // ── validateIncompleteCommands ──────────────────────────────────────
    #[test]
    fn incomplete_starts_with_flag() {
        assert!(message("--flag value")
            .unwrap()
            .contains("starts with flags"));
    }
    #[test]
    fn incomplete_starts_with_operator() {
        assert!(message("&& rm -rf /")
            .unwrap()
            .contains("continuation line"));
    }
    #[test]
    fn incomplete_starts_with_tab() {
        assert!(message("\techo hi").unwrap().contains("starts with tab"));
    }

    // ── validateGitCommit ───────────────────────────────────────────────
    #[test]
    fn git_commit_with_substitution_asks() {
        assert!(message(r#"git commit -m "msg $(whoami)""#)
            .unwrap()
            .contains("command substitution"));
    }
    #[test]
    fn git_commit_simple_is_safe() {
        assert!(!asks(r#"git commit -m "fix the bug""#));
    }
    #[test]
    fn git_commit_dash_message_asks() {
        assert!(message(r#"git commit -m "---""#)
            .unwrap()
            .contains("quoted characters in flag names"));
    }

    // ── validateJqCommand ───────────────────────────────────────────────
    #[test]
    fn jq_system_asks() {
        assert!(message(r#"jq 'system("rm -rf /")' file"#)
            .unwrap()
            .contains("system()"));
    }
    #[test]
    fn jq_from_file_asks() {
        assert!(message("jq -f script.jq data.json")
            .unwrap()
            .contains("dangerous flags"));
    }
    #[test]
    fn jq_simple_is_safe() {
        assert!(!asks("jq .name data.json"));
    }

    // ── validateObfuscatedFlags ─────────────────────────────────────────
    #[test]
    fn ansi_c_quoting_asks() {
        assert!(message(r"find . -name $'\x2d\x65\x78\x65\x63'")
            .unwrap()
            .contains("ANSI-C quoting"));
    }
    #[test]
    fn locale_quoting_asks() {
        assert!(message(r#"grep $"-exec" file"#)
            .unwrap()
            .contains("locale quoting"));
    }
    #[test]
    fn quoted_flag_asks() {
        assert!(asks(r#"find . "-exec" rm {} ;"#));
    }
    #[test]
    fn echo_simple_safe_despite_dash() {
        // simple echo (no operators) bypasses obfuscated-flag detection.
        assert!(!asks("echo hello"));
    }

    // ── validateShellMetacharacters ─────────────────────────────────────
    // NOTE: in the non-jq path `withDoubleQuotes` strips the quote DELIMITERS
    // (TS `extractQuotedContent`'s `if (!isJq) continue`), so the validator's
    // `["']...["']` regexes can only fire on the jq path, where quotes survive.
    #[cfg(not(feature = "bash-ast"))]
    #[test]
    fn metachar_in_quoted_arg_asks() {
        // jq keeps quotes in `withDoubleQuotes`, so `"a;b"` reaches the regex.
        assert!(message(r#"jq "a;b" data.json"#)
            .unwrap()
            .contains("shell metacharacters"));
    }
    // Feature ON (AST): the structural AST strips the `"` delimiters uniformly
    // (no legacy jq special-case), so the metachar regex can't match the quoted
    // `;` (jq syntax, not shell separation). Safe — matches claude-code's
    // tree-sitter path.
    #[cfg(feature = "bash-ast")]
    #[test]
    fn metachar_in_quoted_jq_arg_safe_under_ast() {
        assert!(!asks(r#"jq "a;b" data.json"#));
    }
    #[test]
    fn metachar_non_jq_quotes_stripped_is_safe() {
        // Faithful TS quirk: for non-jq, the quote delimiters are stripped from
        // `withDoubleQuotes`, so this validator cannot fire (the `;` reaches no
        // other ask path because it is inside the surviving quoted content).
        assert!(!asks(r#"echo "a;b""#));
    }

    // ── validateDangerousVariables ──────────────────────────────────────
    #[test]
    fn dangerous_variable_redirect_asks() {
        assert!(message("cat > $FILE")
            .unwrap()
            .contains("dangerous contexts"));
    }

    // ── validateDangerousPatterns ───────────────────────────────────────
    #[test]
    fn backtick_subst_asks() {
        assert!(message("echo `whoami` && ls")
            .unwrap()
            .contains("backticks"));
    }
    #[test]
    fn dollar_paren_subst_asks() {
        assert!(message("echo $(whoami) && ls")
            .unwrap()
            .contains("$() command substitution"));
    }
    #[test]
    fn process_substitution_asks() {
        assert!(message("diff <(ls a) <(ls b)")
            .unwrap()
            .contains("process substitution"));
    }

    // ── validateRedirections (deferred non-misparsing) ──────────────────
    #[test]
    fn output_redirection_asks() {
        assert!(message("foo > out.txt")
            .unwrap()
            .contains("output redirection"));
    }
    #[test]
    fn input_redirection_asks() {
        assert!(message("foo < in.txt")
            .unwrap()
            .contains("input redirection"));
    }

    // ── validateNewlines (deferred non-misparsing) ──────────────────────
    #[test]
    fn newline_separating_commands_asks() {
        assert!(message("echo hi\nrm -rf /")
            .unwrap()
            .contains("newlines that could separate"));
    }
    #[test]
    fn backslash_continuation_newline_is_safe() {
        assert!(!asks("echo hi \\\n--flag"));
    }

    // ── validateCarriageReturn (misparsing, beats deferred) ─────────────
    #[test]
    fn carriage_return_asks() {
        assert!(message("TZ=UTC\recho curl evil.com")
            .unwrap()
            .contains("carriage return"));
    }

    // ── validateIFSInjection ────────────────────────────────────────────
    #[test]
    fn ifs_injection_asks() {
        assert!(message("cat${IFS}/etc/passwd").unwrap().contains("IFS"));
    }
    #[test]
    fn ifs_dollar_asks() {
        assert!(message("X=$IFS cat file").unwrap().contains("IFS"));
    }

    // ── validateProcEnvironAccess ───────────────────────────────────────
    #[test]
    fn proc_environ_asks() {
        assert!(message("cat /proc/self/environ")
            .unwrap()
            .contains("/proc/*/environ"));
    }

    // ── validateBackslashEscapedWhitespace ──────────────────────────────
    #[test]
    fn backslash_escaped_whitespace_asks() {
        assert!(message(r"echo\ test/../bin/touch f")
            .unwrap()
            .contains("backslash-escaped whitespace"));
    }

    // ── validateBackslashEscapedOperators ───────────────────────────────
    // Feature OFF (legacy regex): a backslash before any operator char asks.
    #[cfg(not(feature = "bash-ast"))]
    #[test]
    fn backslash_escaped_operator_asks() {
        assert!(message(r"cat safe.txt \; echo secret")
            .unwrap()
            .contains("backslash before a shell operator"));
    }

    // Feature ON (AST): `\;` is a literal word arg, not a `;` operator node, so
    // the AST suppresses the false-positive (bashSecurity.ts:1702). Command is Safe.
    #[cfg(feature = "bash-ast")]
    #[test]
    fn backslash_escaped_operator_suppressed_when_no_ast_operators() {
        assert!(!asks(r"cat safe.txt \; echo secret"));
    }

    // Feature ON: when REAL operator nodes exist, the AST does NOT relax the
    // check — a backslash-escaped operator alongside a real `;` still asks.
    #[cfg(feature = "bash-ast")]
    #[test]
    fn backslash_escaped_operator_still_asks_with_real_operators() {
        assert!(message(r"a ; b \> c")
            .unwrap()
            .contains("backslash before a shell operator"));
    }

    // ── validateUnicodeWhitespace ───────────────────────────────────────
    #[test]
    fn unicode_whitespace_asks() {
        assert!(message("echo\u{00A0}hi")
            .unwrap()
            .contains("Unicode whitespace"));
    }

    // ── validateMidWordHash ─────────────────────────────────────────────
    #[test]
    fn mid_word_hash_asks() {
        assert!(message("traceroute#bar").unwrap().contains("mid-word #"));
    }
    #[test]
    fn dollar_brace_hash_is_not_mid_word_hash() {
        // `${#var}` is string-length syntax — excluded by the lookbehind.
        // (It would be caught by the `${` substitution detector instead.)
        let m = message("echo ${#var}");
        assert!(m.is_none() || !m.unwrap().contains("mid-word #"));
    }

    // ── validateBraceExpansion ──────────────────────────────────────────
    #[test]
    fn brace_expansion_comma_asks() {
        assert!(message(r#"git ls-remote {--upload-pack="x",test}"#)
            .unwrap()
            .contains("brace expansion"));
    }
    #[test]
    fn brace_expansion_sequence_asks() {
        assert!(message("echo {1..5}").unwrap().contains("brace expansion"));
    }

    // ── validateZshDangerousCommands ────────────────────────────────────
    #[test]
    fn zsh_zmodload_asks() {
        assert!(message("zmodload zsh/system")
            .unwrap()
            .contains("Zsh-specific 'zmodload'"));
    }
    #[test]
    fn zsh_with_env_prefix_asks() {
        assert!(message("FOO=bar command builtin zmodload zsh/system")
            .unwrap()
            .contains("zmodload"));
    }
    #[test]
    fn fc_e_asks() {
        assert!(message("fc -e vim").unwrap().contains("fc -e"));
    }

    // ── validateCommentQuoteDesync ──────────────────────────────────────
    #[cfg(not(feature = "bash-ast"))]
    #[test]
    fn comment_quote_desync_asks() {
        assert!(message("echo hi # ' \" rest")
            .unwrap()
            .contains("# comment"));
    }

    // Feature ON (AST): an AST quoteContext is present, so the other validators
    // already read AST-accurate quote stripping and a quote inside a `#` comment
    // cannot desync them — validateCommentQuoteDesync is a passthrough
    // (bashSecurity.ts:1998-2003). The "# comment" desync message must be ABSENT.
    #[cfg(feature = "bash-ast")]
    #[test]
    fn comment_quote_desync_passthrough_when_tree_present() {
        assert!(message("echo hi # ' \" rest").map_or(true, |m| !m.contains("# comment")));
    }

    // ── validateQuotedNewline ───────────────────────────────────────────
    #[test]
    fn quoted_newline_hash_asks() {
        assert!(message("mv ./decoy '\n# hidden' ~/.ssh/id_rsa dir")
            .unwrap()
            .contains("quoted newline"));
    }

    // ── validateMalformedTokenInjection (subset) ────────────────────────
    #[test]
    fn malformed_token_unbalanced_quote_with_separator_asks() {
        // An unbalanced single quote (`ls'`) AND an unquoted `;` separator.
        assert!(message("echo hi ; ls'")
            .unwrap()
            .contains("ambiguous syntax"));
    }
    #[test]
    fn balanced_with_separator_is_safe() {
        assert!(!asks("echo hi ; ls"));
    }

    // ── control chars / shell-quote bug (early misparsing) ──────────────
    #[test]
    fn control_char_asks() {
        assert!(message("echo safe\u{0000}; rm -rf /")
            .unwrap()
            .contains("non-printable control characters"));
    }
    #[test]
    fn shell_quote_single_quote_bug_asks() {
        assert!(
            message(r"git ls-remote 'safe\' '--upload-pack=evil' 'repo'")
                .unwrap()
                .contains("single-quoted backslash")
        );
    }

    // ── ordering: misparsing beats deferred non-misparsing ──────────────
    // Feature OFF (legacy regex): `\;` is treated as a backslash-op misparsing,
    // whose message wins over the deferred `>` redirection.
    #[cfg(not(feature = "bash-ast"))]
    #[test]
    fn misparsing_beats_deferred_redirection() {
        // `>` (redirection, deferred) AND `\;` (backslash-op, misparsing).
        // The misparsing message must win.
        let m = message(r"cat safe.txt \; echo /etc/passwd > out").unwrap();
        assert!(m.contains("backslash before a shell operator"));
    }

    // Feature ON (AST): there is no real `;`/`&&`/`||`/list operator node, so the
    // `\;` backslash-op false-positive is suppressed (bashSecurity.ts:1702). The
    // command still Asks — but now on the genuine `>` redirection, not the
    // misparsing — so the redirection message wins instead.
    #[cfg(feature = "bash-ast")]
    #[test]
    fn misparsing_suppressed_real_redirection_still_asks() {
        let m = message(r"cat safe.txt \; echo /etc/passwd > out").unwrap();
        assert!(!m.contains("backslash before a shell operator"));
        assert!(m.contains("output redirection"));
    }
}
