//! sed auto-allow guard — faithful port of claude-code
//! `src/tools/BashTool/sedValidation.ts` (`checkSedConstraints` /
//! `sedCommandIsAllowedByAllowlist` and their helpers), extended with the
//! working-directory containment gate the `acceptEdits` bash auto-allow needs.
//!
//! # What the TS does
//! `sedValidation.ts` decides which `sed` invocations are *safe enough* to
//! auto-allow. Two allowlist patterns are recognized:
//!   - **Pattern 1 (line printing):** `sed -n 'p'`, `sed -n '1,5p'`, etc. — a
//!     strict allowlist of print commands, optional `-E`/`-r`/`-z` flags, file
//!     arguments allowed. Read-only; never writes.
//!   - **Pattern 2 (substitution):** `sed 's/a/b/flags'` with a strict flag
//!     allowlist. With `allowFileWrites=false` it must be stdout-only (no file
//!     args, no `-i`); with `allowFileWrites=true` (`acceptEdits` mode) the `-i`
//!     / `--in-place` flag and file arguments are permitted for in-place editing.
//!
//! A defense-in-depth denylist (`containsDangerousOperations`) rejects `w`/`W`
//! (write-to-file), `e`/`E` (execute), block/`{}`, negation, GNU step/offset
//! addresses, non-ASCII homoglyphs, and other tricks regardless of pattern.
//!
//! `checkSedConstraints` runs this per-subcommand: in `acceptEdits` mode it
//! passes `allowFileWrites=true` (so in-place edits are *candidate*-allowable),
//! and asks (`behavior: 'ask'`) for any sed the allowlist rejects.
//!
//! # The Rust extension: working-dir containment for in-place writes
//! `sedValidation.ts` has NO concept of working directories — it only decides
//! pattern/denylist safety. This crate's `acceptEdits`-mode bash auto-allow
//! ([`crate::policy`]) must ALSO ensure an in-place sed does not write OUTSIDE
//! the allowed working dirs (cwd + additional), mirroring the sibling
//! `checkPathConstraints` containment the redirect/`cd` guards enforce. So
//! [`sed_auto_allow_verdict`] composes the byte-faithful TS predicate with the
//! [`crate::filesystem::path_in_allowed_working_path`] containment used by the
//! rest of the crate:
//!   - a **read-only** sed (line-printing, or stdout substitution) is
//!     [`SedVerdict::Safe`] regardless of any file paths (it never writes);
//!   - an **in-place** (`-i`) substitution is [`SedVerdict::Safe`] only when it
//!     matches the allowlist AND every file target lies inside a working dir;
//!     otherwise it is [`SedVerdict::Unsafe`] (→ the bash auto-allow falls
//!     through to ask).
//!
//! # Byte-locked message
//! The ask MESSAGE / reason are taken verbatim from `checkSedConstraints`
//! (`sedValidation.ts:665-675`): see [`SED_ASK_MESSAGE`] / [`SED_ASK_REASON`].
//!
//! # Documented divergences
//! - **Tokenizer.** TS parses sed args with the `shell-quote` library; this port
//!   uses a focused quote-aware splitter (the same approach as
//!   [`crate::dangerous_removal`]'s `split_argv`). Glob tokens (`*.log`) are
//!   recognized by their glob metacharacters rather than `shell-quote`'s glob
//!   objects — the only consumer of that distinction is [`has_file_args`], where
//!   a glob still counts as a file argument.
//! - **Lexical containment**, reusing [`crate::filesystem`]'s
//!   `path_in_allowed_working_path` (the crate's accepted no-`realpath`
//!   divergence).

use crate::filesystem::{path_in_allowed_working_path, FsRoots};
use std::path::{Path, PathBuf};

/// Byte-locked ask MESSAGE from `checkSedConstraints` (`sedValidation.ts:668`).
pub const SED_ASK_MESSAGE: &str =
    "sed command requires approval (contains potentially dangerous operations)";

/// Byte-locked ask REASON from `checkSedConstraints` (`sedValidation.ts:672`).
pub const SED_ASK_REASON: &str =
    "sed command contains operations that require explicit approval (e.g., write commands, execute commands)";

/// Byte-locked ask REASON from the 2.1.211 `mQi` over-length / tokenize-mismatch
/// pre-check (the first `mQi` branch, `command.length > c5e || $4r(command)`).
pub const SED_OVER_LENGTH_REASON: &str =
    "sed command could not be statically validated (command is over-length or contains characters bash and the analyzer tokenize differently)";

/// Byte-locked ask REASON from the 2.1.211 `mQi` redirect-borne branch (`Unu`).
/// Currently emitted only by the (deferred) redirect-borne AST analysis; kept
/// here so downstream wiring can reference the exact bytes.
pub const SED_REDIRECT_BORNE_REASON: &str =
    "sed command carries redirect-borne content that cannot be statically validated (swallowed arguments, unanalyzable heredoc, or expansion in a redirect target)";

/// 2.1.211 `c5e` (`1e4`): the maximum sed command length that is statically
/// validated. Anything longer is over-length and must ask.
pub const SED_MAX_COMMAND_LEN: usize = 10_000;

/// 2.1.211 `$4r`: does the command contain characters that bash and the static
/// analyzer tokenize differently (so it cannot be statically validated)? 1:1 with
///
/// ```js
/// function $4r(e){return M3i.test(e)||N3i.test(e)||F3i.test(e)
///   ||rzn.test(e)||nzn.test(e)||U3i.test(e)}
/// ```
///
/// - `M3i` = `/[\x00-\x08\x0B-\x1F\x7F]/` — control chars (excludes `\t`/`\n`).
/// - `N3i` = a lone UTF-16 surrogate. A Rust `&str` is always valid UTF-8, so a
///   lone surrogate cannot occur — this arm never fires here and is omitted.
/// - `F3i` = `/\\[ \t]|(?:^|[^ \t\\])(?:\\\\)*\\\n|[ \t](?:\\\\)+\\\n/` —
///   backslash-space/tab, or a line-continuation.
/// - `rzn` = `/~\[/`.
/// - `nzn` = `/(?:^|[\s;&|])=[a-zA-Z_]/`.
/// - `U3i` = `/<\d*-\d*>/`.
#[must_use]
pub fn sed_command_untokenizable(command: &str) -> bool {
    use std::sync::LazyLock;
    // M3i control-char class (hand-tested; no regex needed).
    if command
        .bytes()
        .any(|b| matches!(b, 0x00..=0x08 | 0x0B..=0x1F | 0x7F))
    {
        return true;
    }
    // F3i: backslash-space/tab, or a backslash line-continuation.
    static F3I: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\\[ \t]|(?:^|[^ \t\\])(?:\\\\)*\\\n|[ \t](?:\\\\)+\\\n").unwrap()
    });
    // rzn: `~[`.
    static RZN: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"~\[").unwrap());
    // nzn: an unquoted assignment `=` after start / whitespace / `;` / `&` / `|`.
    static NZN: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?:^|[\s;&|])=[a-zA-Z_]").unwrap());
    // U3i: a `<n-m>` brace-range.
    static U3I: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"<\d*-\d*>").unwrap());
    F3I.is_match(command) || RZN.is_match(command) || NZN.is_match(command) || U3I.is_match(command)
}

/// 2.1.211 `mQi` over-length / tokenize-mismatch pre-check (the first `mQi`
/// branch). Returns the byte-locked over-length ASK verdict when `command`
/// exceeds [`SED_MAX_COMMAND_LEN`] or is [`sed_command_untokenizable`];
/// otherwise `None`.
///
/// ## Divergence (documented)
/// 2.1.211's `mQi` runs on the WHOLE bash command (`e.command`, memoized once
/// across subcommands). This port receives a single `sed` subcommand from the
/// caller and applies the check to that subcommand — an over-ask-safe
/// approximation (a subcommand is a substring of the whole command, so this only
/// ever asks on a narrower slice, never allows more). The `mQi` second branch —
/// the `Unu` redirect-borne AST analysis emitting [`SED_REDIRECT_BORNE_REASON`]
/// — needs the bash tree-sitter parser and the whole-command Xwu orchestration
/// (in [`crate::policy`]); it is deferred.
#[must_use]
fn sed_precheck_verdict(command: &str) -> Option<SedVerdict> {
    if command.len() > SED_MAX_COMMAND_LEN || sed_command_untokenizable(command) {
        return Some(SedVerdict::Unsafe {
            message: SED_ASK_MESSAGE.to_string(),
            reason: SED_OVER_LENGTH_REASON.to_string(),
        });
    }
    None
}

/// 2.1.218 `xDs` whole-command sed-validation gate (the `_gd` pre-pass that runs
/// ONCE per whole command, before the per-sed constraint loop). Mirrors `xDs`:
///
/// 1. Over-length / untokenizable whole command
///    ([`sed_precheck_verdict`]) → [`SedVerdict::Unsafe`] with
///    [`SED_OVER_LENGTH_REASON`] (the first `xDs` branch).
/// 2. Under the `bash-ast` feature, a redirect-borne risk
///    ([`crate::sed_redirect_borne::sed_command_has_redirect_borne_risk`], the
///    `bgd` walk) → [`SedVerdict::Unsafe`] with [`SED_REDIRECT_BORNE_REASON`]
///    (the second `xDs` branch).
/// 3. Otherwise `None` (the `xDs` passthrough).
///
/// The caller ([`crate::policy`]) runs this once when the whole command has at
/// least one `sed` subcommand, and returns its ask BEFORE the per-sed
/// constraint loop — mirroring `_gd` consulting `xDs` before `gpr`.
///
/// ## Divergence (documented)
/// Under a NON-`bash-ast` build the tree-sitter parser is unavailable, so the
/// redirect-borne branch is a documented no-op: only the over-length /
/// untokenizable precheck runs. This mirrors the port's existing AST-gating
/// philosophy (the `bash-ast` feature is enabled in the desktop production
/// build) and can only UNDER-detect on the minimal build, never over-allow on
/// the production build.
#[must_use]
pub fn sed_redirect_borne_verdict(whole_command: &str) -> Option<SedVerdict> {
    // xDs branch 1: over-length / untokenizable (SED_OVER_LENGTH_REASON).
    if let Some(v) = sed_precheck_verdict(whole_command) {
        return Some(v);
    }
    // xDs branch 2: redirect-borne risk (SED_REDIRECT_BORNE_REASON). Needs the
    // tree-sitter parser; a documented no-op under non-`bash-ast` builds.
    #[cfg(feature = "bash-ast")]
    {
        if crate::sed_redirect_borne::sed_command_has_redirect_borne_risk(whole_command) {
            return Some(SedVerdict::Unsafe {
                message: SED_ASK_MESSAGE.to_string(),
                reason: SED_REDIRECT_BORNE_REASON.to_string(),
            });
        }
    }
    None
}

/// Verdict for whether a single `sed` subcommand may be auto-allowed in
/// `acceptEdits` mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SedVerdict {
    /// The sed invocation is safe to auto-allow (read-only, or an in-place edit
    /// whose targets all lie inside the allowed working dirs).
    Safe,
    /// The sed invocation must ASK — it is not on the allowlist, contains a
    /// dangerous operation, or writes in-place to a path outside the working
    /// dirs. Carries the byte-locked ask message + reason.
    Unsafe {
        /// Byte-locked ask message ([`SED_ASK_MESSAGE`]).
        message: String,
        /// Byte-locked ask reason ([`SED_ASK_REASON`]).
        reason: String,
    },
}

impl SedVerdict {
    /// Construct the byte-locked [`SedVerdict::Unsafe`] verdict.
    fn unsafe_default() -> Self {
        SedVerdict::Unsafe {
            message: SED_ASK_MESSAGE.to_string(),
            reason: SED_ASK_REASON.to_string(),
        }
    }
}

/// One shell token: a plain word (quotes stripped) or a glob (`*.log`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SedToken {
    /// The token text (surrounding quotes already stripped).
    text: String,
    /// Whether the token carried unquoted glob metacharacters (`*`, `?`, `[`).
    /// TS `shell-quote` would emit a `{ op: 'glob' }` object for these; the only
    /// consumer is [`has_file_args`], where a glob counts as a file argument.
    is_glob: bool,
}

/// Quote-aware tokenizer for the part of a sed command AFTER `sed `. Reproduces
/// the effect of TS `tryParseShellCommand` for the cases the sed validators
/// need: whitespace splits tokens, single/double quotes group (and are stripped
/// from the token), a backslash escapes the next char outside single quotes, and
/// unquoted glob metacharacters mark the token as a glob.
fn tokenize(rest: &str) -> Vec<SedToken> {
    let chars: Vec<char> = rest.chars().collect();
    let n = chars.len();
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut has_token = false;
    let mut is_glob = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                cur.push(c);
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == '\\' && i + 1 < n && matches!(chars[i + 1], '"' | '\\' | '$' | '`') {
                cur.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_double = false;
            } else {
                cur.push(c);
            }
            i += 1;
            continue;
        }
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if has_token {
                    tokens.push(SedToken {
                        text: std::mem::take(&mut cur),
                        is_glob,
                    });
                    has_token = false;
                    is_glob = false;
                }
            }
            '\'' => {
                in_single = true;
                has_token = true;
            }
            '"' => {
                in_double = true;
                has_token = true;
            }
            '\\' => {
                if i + 1 < n {
                    cur.push(chars[i + 1]);
                    i += 2;
                    has_token = true;
                    continue;
                }
            }
            '*' | '?' | '[' => {
                cur.push(c);
                has_token = true;
                is_glob = true;
            }
            _ => {
                cur.push(c);
                has_token = true;
            }
        }
        i += 1;
    }
    if has_token {
        tokens.push(SedToken { text: cur, is_glob });
    }
    tokens
}

/// Strip the leading `sed ` (with optional leading whitespace) from `command`,
/// returning the remainder. Mirrors TS `command.match(/^\s*sed\s+/)` + slice;
/// returns `None` when the command is not a `sed` invocation.
fn strip_sed_prefix(command: &str) -> Option<&str> {
    let trimmed_start = command.trim_start();
    let after = trimmed_start.strip_prefix("sed")?;
    // `sed` must be followed by whitespace (the `\s+` in `/^\s*sed\s+/`).
    if after.is_empty() || !after.starts_with([' ', '\t', '\n', '\r']) {
        return None;
    }
    Some(after.trim_start())
}

/// Collect the flag tokens (`-…`, excluding a bare `--`) from a tokenized sed
/// command. Mirrors the `flags` accumulation in the TS pattern checkers.
fn collect_flags(tokens: &[SedToken]) -> Vec<String> {
    tokens
        .iter()
        .filter(|t| !t.is_glob && t.text.starts_with('-') && t.text != "--")
        .map(|t| t.text.clone())
        .collect()
}

/// TS `validateFlagsAgainstAllowlist` (`sedValidation.ts:13-35`): every flag
/// (including each char of a combined short flag like `-nE`) must be in
/// `allowed_flags`.
fn validate_flags_against_allowlist(flags: &[String], allowed_flags: &[&str]) -> bool {
    for flag in flags {
        if flag.starts_with('-') && !flag.starts_with("--") && flag.chars().count() > 2 {
            // Combined short flags like `-nE`: check each char.
            for ch in flag.chars().skip(1) {
                let single = format!("-{ch}");
                if !allowed_flags.contains(&single.as_str()) {
                    return false;
                }
            }
        } else if !allowed_flags.contains(&flag.as_str()) {
            return false;
        }
    }
    true
}

/// TS `isPrintCommand` (`sedValidation.ts:128-133`): a single command is a valid
/// print command iff it matches `^(?:\d+|\d+,\d+)?p$` — `p`, `1p`, `1,5p`.
fn is_print_command(cmd: &str) -> bool {
    if cmd.is_empty() {
        return false;
    }
    let Some(body) = cmd.strip_suffix('p') else {
        return false;
    };
    if body.is_empty() {
        return true; // `p`
    }
    match body.split_once(',') {
        Some((a, b)) => is_all_digits(a) && is_all_digits(b), // `N,Mp`
        None => is_all_digits(body),                          // `Np`
    }
}

fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// TS `isLinePrintingCommand` (`sedValidation.ts:44-117`): Pattern 1 — `-n`
/// line-printing with optional `-E`/`-r`/`-z` flags and an allowlist of print
/// expressions (semicolon-separated allowed). File arguments are allowed.
fn is_line_printing_command(rest: &str, tokens: &[SedToken], expressions: &[String]) -> bool {
    let _ = rest;
    let flags = collect_flags(tokens);
    let allowed_flags = [
        "-n",
        "--quiet",
        "--silent",
        "-E",
        "--regexp-extended",
        "-r",
        "-z",
        "--zero-terminated",
        "--posix",
    ];
    if !validate_flags_against_allowlist(&flags, &allowed_flags) {
        return false;
    }
    // Must have an `-n`/`--quiet`/`--silent` flag (also detected inside a
    // combined short flag containing `n`).
    let has_n_flag = flags.iter().any(|flag| {
        flag == "-n"
            || flag == "--quiet"
            || flag == "--silent"
            || (flag.starts_with('-') && !flag.starts_with("--") && flag.contains('n'))
    });
    if !has_n_flag {
        return false;
    }
    if expressions.is_empty() {
        return false;
    }
    // All expressions must be print commands (semicolon-separated allowed).
    for expr in expressions {
        for cmd in expr.split(';') {
            if !is_print_command(cmd.trim()) {
                return false;
            }
        }
    }
    true
}

/// TS `isSubstitutionCommand` (`sedValidation.ts:142-238`): Pattern 2 — a single
/// `s/pattern/replacement/flags` expression with a strict flag allowlist. With
/// `allow_file_writes=true`, `-i`/`--in-place` and file args are permitted.
fn is_substitution_command(
    tokens: &[SedToken],
    expressions: &[String],
    has_file_arguments: bool,
    allow_file_writes: bool,
) -> bool {
    if !allow_file_writes && has_file_arguments {
        return false;
    }
    let flags = collect_flags(tokens);
    let mut allowed_flags: Vec<&str> = vec!["-E", "--regexp-extended", "-r", "--posix"];
    if allow_file_writes {
        allowed_flags.push("-i");
        allowed_flags.push("--in-place");
    }
    if !validate_flags_against_allowlist(&flags, &allowed_flags) {
        return false;
    }
    if expressions.len() != 1 {
        return false;
    }
    let expr = expressions[0].trim();
    if !expr.starts_with('s') {
        return false;
    }
    // Must start with `s/` (only `/` delimiter, strict).
    let Some(rest) = expr.strip_prefix("s/") else {
        return false;
    };
    // Find the `/` delimiters, skipping escaped chars.
    let bytes: Vec<char> = rest.chars().collect();
    let mut delimiter_count = 0;
    let mut last_delimiter_pos: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '\\' {
            i += 2;
            continue;
        }
        if bytes[i] == '/' {
            delimiter_count += 1;
            last_delimiter_pos = Some(i);
        }
        i += 1;
    }
    if delimiter_count != 2 {
        return false;
    }
    // Flags = everything after the last `/` delimiter.
    let Some(last) = last_delimiter_pos else {
        return false;
    };
    let expr_flags: String = bytes[(last + 1)..].iter().collect();
    // `^[gpimIM]*[1-9]?[gpimIM]*$`.
    sub_flags_valid(&expr_flags)
}

/// TS `/^[gpimIM]*[1-9]?[gpimIM]*$/.test(exprFlags)`.
fn sub_flags_valid(flags: &str) -> bool {
    let is_letter = |c: char| matches!(c, 'g' | 'p' | 'i' | 'm' | 'I' | 'M');
    let mut seen_digit = false;
    for c in flags.chars() {
        if is_letter(c) {
            continue;
        }
        if ('1'..='9').contains(&c) {
            if seen_digit {
                return false; // at most ONE digit 1-9
            }
            seen_digit = true;
            continue;
        }
        return false;
    }
    true
}

/// TS `hasFileArgs` (`sedValidation.ts:307-379`): does the sed command have file
/// arguments (beyond stdin / the inline expression)?
fn has_file_args(tokens: &[SedToken]) -> bool {
    let mut arg_count = 0;
    let mut has_e_flag = false;
    let mut idx = 0;
    while idx < tokens.len() {
        let tok = &tokens[idx];
        // A glob pattern counts as a file argument.
        if tok.is_glob {
            return true;
        }
        let arg = &tok.text;
        // `-e EXPR` / `--expression EXPR`: the next token is the expression.
        if (arg == "-e" || arg == "--expression") && idx + 1 < tokens.len() {
            has_e_flag = true;
            idx += 2;
            continue;
        }
        if arg.starts_with("--expression=") {
            has_e_flag = true;
            idx += 1;
            continue;
        }
        if arg.starts_with("-e=") {
            has_e_flag = true;
            idx += 1;
            continue;
        }
        // Skip other flags.
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        arg_count += 1;
        // With `-e` flags, ALL non-flag args are file arguments.
        if has_e_flag {
            return true;
        }
        // Without `-e`, the first non-flag arg is the expression; >1 ⇒ files.
        if arg_count > 1 {
            return true;
        }
        idx += 1;
    }
    false
}

/// TS `extractSedExpressions` (`sedValidation.ts:388-466`): the sed expressions
/// (the parts a denylist must inspect), ignoring flags and filenames. Returns
/// `Err` for the dangerous flag combinations TS throws on.
fn extract_sed_expressions(rest: &str, tokens: &[SedToken]) -> Result<Vec<String>, ()> {
    // TS: reject `-e[wWe]` / `-w[eE]` combined flag tricks (`sedValidation.ts:398`).
    if rest.contains("-ew")
        || rest.contains("-eW")
        || rest.contains("-ee")
        || rest.contains("-we")
        || rest.contains("-wE")
    {
        return Err(());
    }
    let mut expressions = Vec::new();
    let mut found_e_flag = false;
    let mut found_expression = false;
    let mut idx = 0;
    while idx < tokens.len() {
        let tok = &tokens[idx];
        if tok.is_glob {
            // A glob is a filename, not an expression; TS skips non-string args.
            idx += 1;
            continue;
        }
        let arg = &tok.text;
        if (arg == "-e" || arg == "--expression") && idx + 1 < tokens.len() {
            found_e_flag = true;
            expressions.push(tokens[idx + 1].text.clone());
            idx += 2;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--expression=") {
            found_e_flag = true;
            expressions.push(v.to_string());
            idx += 1;
            continue;
        }
        if let Some(v) = arg.strip_prefix("-e=") {
            found_e_flag = true;
            expressions.push(v.to_string());
            idx += 1;
            continue;
        }
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        // First non-flag arg (no `-e` seen yet) is the sed expression.
        if !found_e_flag && !found_expression {
            expressions.push(arg.clone());
            found_expression = true;
            idx += 1;
            continue;
        }
        // Remaining non-flag args are filenames.
        break;
    }
    Ok(expressions)
}

/// TS `containsDangerousOperations` (`sedValidation.ts:473-629`): denylist for a
/// single sed expression. Returns `true` if dangerous.
fn contains_dangerous_operations(expression: &str) -> bool {
    let cmd = expression.trim();
    if cmd.is_empty() {
        return false;
    }
    // Reject non-ASCII (homoglyphs, combining chars) — outside 0x01..=0x7F.
    if cmd.chars().any(|c| {
        let u = c as u32;
        u == 0 || u > 0x7F
    }) {
        return true;
    }
    // Reject curly braces (blocks).
    if cmd.contains('{') || cmd.contains('}') {
        return true;
    }
    // Reject newlines and carriage returns — binary `containsDangerousOperations`
    // checks `t.includes("\n")||t.includes("\r")` (od -c @208745623). An embedded
    // CR survives single-quote tokenization and must be treated as dangerous.
    if cmd.contains('\n') || cmd.contains('\r') {
        return true;
    }
    // Reject comments (`#` not immediately after `s`).
    if let Some(hash_index) = cmd.find('#') {
        let prev_is_s = hash_index > 0 && cmd.as_bytes()[hash_index - 1] == b's';
        if !prev_is_s {
            return true;
        }
    }
    // Reject negation: `^!` or `[/\d$]!`.
    if cmd.starts_with('!') {
        return true;
    }
    if regex_negation_after(cmd) {
        return true;
    }
    // Reject GNU step address `\d~\d`, `,~\d`, `$~\d` (whitespace allowed).
    if regex_step_address(cmd) {
        return true;
    }
    // Reject bare leading comma.
    if cmd.starts_with(',') {
        return true;
    }
    // Reject `,` followed by `+`/`-` (GNU offset addresses).
    if regex_comma_offset(cmd) {
        return true;
    }
    // Reject backslash tricks: `s\` or `\[|#%@]`.
    if cmd.contains("s\\") || regex_backslash_alt_delim(cmd) {
        return true;
    }
    // Reject escaped slashes followed by w/W: `\\\/.*[wW]`.
    if regex_escaped_slash_w(cmd) {
        return true;
    }
    // Reject `/[^/]*\s+[wWeE]`.
    if regex_slash_ws_dangerous(cmd) {
        return true;
    }
    // Reject malformed `s/` not matching `^s/[^/]*/[^/]*/[^/]*$`.
    if cmd.starts_with("s/") && !proper_simple_subst(cmd) {
        return true;
    }
    // PARANOID: `s.` ending in w/W/e/E that isn't a proper substitution.
    if starts_with_s_dot(cmd) && ends_with_dangerous(cmd) && !proper_subst_any_delim(cmd) {
        return true;
    }
    // Dangerous write commands (`w`/`W` after various address forms).
    if dangerous_write_command(cmd) {
        return true;
    }
    // Dangerous execute commands (`e` after various address forms).
    if dangerous_execute_command(cmd) {
        return true;
    }
    // Substitution with dangerous flags (`w`/`W`/`e`/`E` in the flags slot).
    if substitution_has_dangerous_flag(cmd) {
        return true;
    }
    // `y` transliterate command with any w/W/e/E.
    if has_y_command(cmd) && cmd.chars().any(|c| matches!(c, 'w' | 'W' | 'e' | 'E')) {
        return true;
    }
    false
}

// ── denylist regex helpers (hand-written to avoid pulling regex into hot path
//    needlessly; each mirrors the corresponding TS regex) ───────────────────

/// `/[/\d$]!/` — a `/`, digit, or `$` immediately followed by `!`.
fn regex_negation_after(cmd: &str) -> bool {
    let b = cmd.as_bytes();
    for i in 1..b.len() {
        if b[i] == b'!' && (b[i - 1] == b'/' || b[i - 1].is_ascii_digit() || b[i - 1] == b'$') {
            return true;
        }
    }
    false
}

/// `/\d\s*~\s*\d|,\s*~\s*\d|\$\s*~\s*\d/` — a `~` step address with a digit/`,`/`$`
/// before (whitespace allowed) and a digit after (whitespace allowed).
fn regex_step_address(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c != '~' {
            continue;
        }
        // Left: skip back over whitespace, require digit / `,` / `$`.
        let mut l = i;
        while l > 0 && chars[l - 1].is_whitespace() {
            l -= 1;
        }
        let left_ok =
            l > 0 && (chars[l - 1].is_ascii_digit() || chars[l - 1] == ',' || chars[l - 1] == '$');
        if !left_ok {
            continue;
        }
        // Right: skip whitespace, require digit.
        let mut r = i + 1;
        while r < chars.len() && chars[r].is_whitespace() {
            r += 1;
        }
        if r < chars.len() && chars[r].is_ascii_digit() {
            return true;
        }
    }
    false
}

/// `/,\s*[+-]/` — a `,` then optional whitespace then `+`/`-`.
fn regex_comma_offset(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c != ',' {
            continue;
        }
        let mut r = i + 1;
        while r < chars.len() && chars[r].is_whitespace() {
            r += 1;
        }
        if r < chars.len() && (chars[r] == '+' || chars[r] == '-') {
            return true;
        }
    }
    false
}

/// `/\\[|#%@]/` — a backslash followed by an alternate delimiter char.
fn regex_backslash_alt_delim(cmd: &str) -> bool {
    let b = cmd.as_bytes();
    for i in 0..b.len().saturating_sub(1) {
        if b[i] == b'\\' && matches!(b[i + 1], b'|' | b'#' | b'%' | b'@') {
            return true;
        }
    }
    false
}

/// `/\\\/.*[wW]/` — an escaped slash (`\/`) somewhere before a `w`/`W`.
fn regex_escaped_slash_w(cmd: &str) -> bool {
    if let Some(pos) = cmd.find("\\/") {
        return cmd[pos + 2..].chars().any(|c| c == 'w' || c == 'W');
    }
    false
}

/// `/\/[^/]*\s+[wWeE]/` — `/`, then non-slash chars, whitespace, then w/W/e/E.
fn regex_slash_ws_dangerous(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if chars[i] == '/' {
            // Consume `[^/]*`.
            let mut j = i + 1;
            while j < n && chars[j] != '/' {
                j += 1;
            }
            // From i+1..j, look for `\s+[wWeE]` within the non-slash run. The TS
            // regex matches `/` then `[^/]*` then `\s+` then a dangerous char —
            // so scan within the run for whitespace followed (after ≥1 ws) by a
            // dangerous char.
            let run = &chars[i + 1..j];
            if run_has_ws_then_dangerous(run) {
                return true;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    false
}

/// Within a char run, is there `\s+[wWeE]` (≥1 whitespace then a dangerous char)?
fn run_has_ws_then_dangerous(run: &[char]) -> bool {
    let mut k = 0;
    while k < run.len() {
        if run[k].is_whitespace() {
            let mut m = k;
            while m < run.len() && run[m].is_whitespace() {
                m += 1;
            }
            if m < run.len() && matches!(run[m], 'w' | 'W' | 'e' | 'E') {
                return true;
            }
            k = m;
            continue;
        }
        k += 1;
    }
    false
}

/// `/^s\/[^/]*\/[^/]*\/[^/]*$/` — a proper simple `s/…/…/…` substitution
/// (slash delimiter, no extra slashes in any field). Used to test malformed
/// `s/` commands.
fn proper_simple_subst(cmd: &str) -> bool {
    let Some(rest) = cmd.strip_prefix("s/") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    // Exactly 3 fields, none containing a `/` (guaranteed by split), and no
    // trailing slash creating a 4th field.
    parts.len() == 3
}

/// `/^s./.test(cmd)` — starts with `s` and has at least a second char.
fn starts_with_s_dot(cmd: &str) -> bool {
    let mut chars = cmd.chars();
    chars.next() == Some('s') && chars.next().is_some()
}

/// `/[wWeE]$/`.
fn ends_with_dangerous(cmd: &str) -> bool {
    matches!(cmd.chars().last(), Some('w' | 'W' | 'e' | 'E'))
}

/// `/^s([^\\\n]).*?\1.*?\1[^wWeE]*$/` — a properly formed substitution with ANY
/// (non-backslash, non-newline) delimiter, whose trailing flags contain no
/// w/W/e/E.
fn proper_subst_any_delim(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    if chars.first() != Some(&'s') || chars.len() < 2 {
        return false;
    }
    let delim = chars[1];
    if delim == '\\' || delim == '\n' {
        return false;
    }
    // Need three occurrences of `delim` at positions 1, then two more.
    let mut positions = Vec::new();
    for (i, &c) in chars.iter().enumerate().skip(1) {
        if c == delim {
            positions.push(i);
        }
    }
    if positions.len() < 3 {
        return false;
    }
    // Trailing flags = chars after the third delimiter must have no w/W/e/E.
    let third = positions[2];
    chars[third + 1..]
        .iter()
        .all(|&c| !matches!(c, 'w' | 'W' | 'e' | 'E'))
}

/// Dangerous write command forms (`w`/`W`) — TS `sedValidation.ts:569-579`.
fn dangerous_write_command(cmd: &str) -> bool {
    re_match_w_at_start(cmd)
        || re_match_after_number_w(cmd)
        || re_match_after_dollar_w(cmd)
        || re_match_after_pattern_w(cmd)
        || re_match_after_range_w(cmd)
        || re_match_after_range_dollar_w(cmd)
        || re_match_after_pattern_range_w(cmd)
}

/// `/^[wW]\s*\S+/`.
fn re_match_w_at_start(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    if chars.is_empty() || !matches!(chars[0], 'w' | 'W') {
        return false;
    }
    ws_then_nonspace(&chars[1..])
}

/// `/^\d+\s*[wW]\s*\S+/`.
fn re_match_after_number_w(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return false;
    }
    w_then_nonspace(&chars[i..])
}

/// `/^\$\s*[wW]\s*\S+/`.
fn re_match_after_dollar_w(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    if chars.first() != Some(&'$') {
        return false;
    }
    w_then_nonspace_ws(&chars[1..])
}

/// `/^\/[^/]*\/[IMim]*\s*[wW]\s*\S+/`.
fn re_match_after_pattern_w(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let Some(after) = consume_pattern_with_flags(&chars) else {
        return false;
    };
    w_then_nonspace_ws(after)
}

/// `/^\d+,\d+\s*[wW]\s*\S+/`.
fn re_match_after_range_w(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || i >= chars.len() || chars[i] != ',' {
        return false;
    }
    i += 1;
    let start = i;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return false;
    }
    w_then_nonspace_ws(&chars[i..])
}

/// `/^\d+,\$\s*[wW]\s*\S+/`.
fn re_match_after_range_dollar_w(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || i + 1 >= chars.len() || chars[i] != ',' || chars[i + 1] != '$' {
        return false;
    }
    i += 2;
    w_then_nonspace_ws(&chars[i..])
}

/// `/^\/[^/]*\/[IMim]*,\/[^/]*\/[IMim]*\s*[wW]\s*\S+/`.
fn re_match_after_pattern_range_w(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let Some(after1) = consume_pattern_with_flags(&chars) else {
        return false;
    };
    if after1.first() != Some(&',') {
        return false;
    }
    let Some(after2) = consume_pattern_with_flags(&after1[1..]) else {
        return false;
    };
    w_then_nonspace_ws(after2)
}

/// Dangerous execute command forms (`e`) — TS `sedValidation.ts:585-594`.
fn dangerous_execute_command(cmd: &str) -> bool {
    cmd.starts_with('e')
        || re_match_after_number_e(cmd)
        || re_match_after_dollar_e(cmd)
        || re_match_after_pattern_e(cmd)
        || re_match_after_range_e(cmd)
        || re_match_after_range_dollar_e(cmd)
        || re_match_after_pattern_range_e(cmd)
}

/// `/^\d+\s*e/`.
fn re_match_after_number_e(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return false;
    }
    skip_ws_then_char(&chars[i..], 'e')
}

/// `/^\$\s*e/`.
fn re_match_after_dollar_e(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    if chars.first() != Some(&'$') {
        return false;
    }
    skip_ws_then_char(&chars[1..], 'e')
}

/// `/^\/[^/]*\/[IMim]*\s*e/`.
fn re_match_after_pattern_e(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let Some(after) = consume_pattern_with_flags(&chars) else {
        return false;
    };
    skip_ws_then_char(after, 'e')
}

/// `/^\d+,\d+\s*e/`.
fn re_match_after_range_e(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || i >= chars.len() || chars[i] != ',' {
        return false;
    }
    i += 1;
    let start = i;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return false;
    }
    skip_ws_then_char(&chars[i..], 'e')
}

/// `/^\d+,\$\s*e/`.
fn re_match_after_range_dollar_e(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || i + 1 >= chars.len() || chars[i] != ',' || chars[i + 1] != '$' {
        return false;
    }
    i += 2;
    skip_ws_then_char(&chars[i..], 'e')
}

/// `/^\/[^/]*\/[IMim]*,\/[^/]*\/[IMim]*\s*e/`.
fn re_match_after_pattern_range_e(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let Some(after1) = consume_pattern_with_flags(&chars) else {
        return false;
    };
    if after1.first() != Some(&',') {
        return false;
    }
    let Some(after2) = consume_pattern_with_flags(&after1[1..]) else {
        return false;
    };
    skip_ws_then_char(after2, 'e')
}

/// Consume `/[^/]*/[IMim]*` from the front of `chars`, returning the remainder.
fn consume_pattern_with_flags(chars: &[char]) -> Option<&[char]> {
    if chars.first() != Some(&'/') {
        return None;
    }
    let mut i = 1;
    while i < chars.len() && chars[i] != '/' {
        i += 1;
    }
    if i >= chars.len() {
        return None; // no closing slash
    }
    i += 1; // consume closing `/`
    while i < chars.len() && matches!(chars[i], 'I' | 'M' | 'i' | 'm') {
        i += 1;
    }
    Some(&chars[i..])
}

/// `\s*[wW]\s*\S+` against the front of `chars`.
fn w_then_nonspace_ws(chars: &[char]) -> bool {
    let mut i = 0;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if i >= chars.len() || !matches!(chars[i], 'w' | 'W') {
        return false;
    }
    ws_then_nonspace(&chars[i + 1..])
}

/// `[wW]\s*\S+` against the front of `chars` (no leading whitespace skip).
fn w_then_nonspace(chars: &[char]) -> bool {
    if chars.first().is_none_or(|c| !matches!(c, 'w' | 'W')) {
        return false;
    }
    ws_then_nonspace(&chars[1..])
}

/// `\s*\S+` — optional whitespace then at least one non-space char.
fn ws_then_nonspace(chars: &[char]) -> bool {
    let mut i = 0;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    i < chars.len() && !chars[i].is_whitespace()
}

/// `\s*<ch>` — optional whitespace then the literal `ch`.
fn skip_ws_then_char(chars: &[char], ch: char) -> bool {
    let mut i = 0;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    i < chars.len() && chars[i] == ch
}

/// Substitution command whose flags slot (after `s<delim>pat<delim>rep<delim>`)
/// contains `w`/`W`/`e`/`E`. TS `cmd.match(/s([^\\\n]).*?\1.*?\1(.*?)$/)`.
fn substitution_has_dangerous_flag(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    // Find the first `s` followed by a valid delimiter, then three delimiters.
    for start in 0..chars.len() {
        if chars[start] != 's' || start + 1 >= chars.len() {
            continue;
        }
        let delim = chars[start + 1];
        if delim == '\\' || delim == '\n' {
            continue;
        }
        // Collect delimiter positions after start+1.
        let mut positions = Vec::new();
        for (i, &c) in chars.iter().enumerate().skip(start + 2) {
            if c == delim {
                positions.push(i);
            }
        }
        if positions.len() < 2 {
            continue;
        }
        // Lazy `.*?\1.*?\1` → first two delimiters bound pattern/replacement.
        let third = positions[1];
        let flags: String = chars[third + 1..].iter().collect();
        if flags.contains('w') || flags.contains('W') || flags.contains('e') || flags.contains('E')
        {
            return true;
        }
        // Only the first matching `s<delim>` is considered (TS `.match` finds
        // the first occurrence).
        return false;
    }
    false
}

/// `cmd.match(/y([^\\\n])/)` — a `y` transliterate command with a valid
/// delimiter.
fn has_y_command(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    for i in 0..chars.len() {
        if chars[i] == 'y' && i + 1 < chars.len() {
            let d = chars[i + 1];
            if d != '\\' && d != '\n' {
                return true;
            }
        }
    }
    false
}

/// TS `sedCommandIsAllowedByAllowlist` (`sedValidation.ts:247-301`): is the sed
/// `command` on the allowlist? `allow_file_writes=true` permits `-i` + file args
/// for substitution (Pattern 2 only).
fn sed_command_is_allowed_by_allowlist(command: &str, allow_file_writes: bool) -> bool {
    let Some(rest) = strip_sed_prefix(command) else {
        return false;
    };
    let tokens = tokenize(rest);
    let Ok(expressions) = extract_sed_expressions(rest, &tokens) else {
        return false;
    };
    let has_file_arguments = has_file_args(&tokens);

    let is_pattern1;
    let is_pattern2;
    if allow_file_writes {
        // When allowing file writes, only check substitution (Pattern 2 variant);
        // Pattern 1 (line printing) doesn't need file writes.
        is_pattern1 = false;
        is_pattern2 = is_substitution_command(&tokens, &expressions, has_file_arguments, true);
    } else {
        is_pattern1 = is_line_printing_command(rest, &tokens, &expressions);
        is_pattern2 = is_substitution_command(&tokens, &expressions, has_file_arguments, false);
    }
    if !is_pattern1 && !is_pattern2 {
        return false;
    }
    // Pattern 2 does not allow semicolons.
    for expr in &expressions {
        if is_pattern2 && expr.contains(';') {
            return false;
        }
    }
    // Defense-in-depth denylist.
    for expr in &expressions {
        if contains_dangerous_operations(expr) {
            return false;
        }
    }
    true
}

/// Does this sed command request an in-place edit (`-i` / `--in-place`)? Looks at
/// the flag tokens. A combined short flag containing `i` (e.g. `-ni`) counts.
fn requests_in_place(tokens: &[SedToken]) -> bool {
    collect_flags(tokens).iter().any(|flag| {
        flag == "--in-place"
            // A short flag (`-i`, `-ni`, …) containing `i` requests in-place.
            || (flag.starts_with('-') && !flag.starts_with("--") && flag.contains('i'))
    })
}

/// Collect the FILE-argument tokens of a sed command (the args that are neither
/// flags nor the inline expression). Mirrors the filename-collection arm of
/// [`extract_sed_expressions`] / [`has_file_args`].
fn file_arguments(tokens: &[SedToken]) -> Vec<String> {
    let mut files = Vec::new();
    let mut found_e_flag = false;
    let mut found_expression = false;
    let mut idx = 0;
    while idx < tokens.len() {
        let tok = &tokens[idx];
        if tok.is_glob {
            // A glob filename target — count it (it will fail containment unless
            // expansion lands inside a working dir, which it never will lexically).
            files.push(tok.text.clone());
            idx += 1;
            continue;
        }
        let arg = &tok.text;
        if (arg == "-e" || arg == "--expression") && idx + 1 < tokens.len() {
            found_e_flag = true;
            idx += 2;
            continue;
        }
        if arg.starts_with("--expression=") || arg.starts_with("-e=") {
            found_e_flag = true;
            idx += 1;
            continue;
        }
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        // First non-flag arg with no `-e` seen is the expression, not a file.
        if !found_e_flag && !found_expression {
            found_expression = true;
            idx += 1;
            continue;
        }
        files.push(arg.clone());
        idx += 1;
    }
    files
}

/// Verdict for whether a single `sed` subcommand may be auto-allowed in
/// `acceptEdits` mode. Composes the byte-faithful [`sed_command_is_allowed_by_allowlist`]
/// predicate with working-dir containment for in-place writes:
///
/// - A **read-only** sed (line-printing or stdout substitution — the
///   `allow_file_writes=false` allowlist) is [`SedVerdict::Safe`] regardless of
///   its file paths: it never writes.
/// - An **in-place** (`-i`/`--in-place`) substitution is [`SedVerdict::Safe`]
///   only when it matches the `allow_file_writes=true` allowlist AND every file
///   target lies inside an allowed working dir; otherwise [`SedVerdict::Unsafe`].
/// - Anything the allowlist rejects entirely (dangerous ops, unknown flags,
///   non-allowlisted expressions) is [`SedVerdict::Unsafe`].
///
/// `command` is a single subcommand whose base command the caller has already
/// verified is `sed`. `roots`/`additional` supply the working-dir set.
#[must_use]
pub fn sed_auto_allow_verdict(
    command: &str,
    roots: &FsRoots,
    additional: &[PathBuf],
) -> SedVerdict {
    // 2.1.211 `mQi` over-length / tokenize-mismatch pre-check (runs before the
    // allowlist `jQt`, exactly as `Xwu` consults `mQi` before `jQt`).
    if let Some(v) = sed_precheck_verdict(command) {
        return v;
    }
    // Read-only allowlist (no file writes): line-printing or stdout substitution.
    if sed_command_is_allowed_by_allowlist(command, false) {
        return SedVerdict::Safe;
    }
    // In-place allowlist (file writes permitted): substitution with `-i`.
    if !sed_command_is_allowed_by_allowlist(command, true) {
        return SedVerdict::unsafe_default();
    }
    // It IS an allowlisted in-place substitution. If it actually writes a file
    // (in-place), every file target must lie inside a working dir.
    let Some(rest) = strip_sed_prefix(command) else {
        return SedVerdict::unsafe_default();
    };
    let tokens = tokenize(rest);
    let files = file_arguments(&tokens);
    // An in-place edit with no file target writes nothing useful (stdin) — but a
    // substitution that passed the `allow_file_writes=true` allowlist with no
    // file target and no `-i` is just a stdout edit → safe.
    if !requests_in_place(&tokens) {
        return SedVerdict::Safe;
    }
    // In-place: containment-check every file target.
    let work_dirs = working_dir_paths(roots, additional);
    for f in &files {
        let resolved = crate::filesystem::expand_path(f, roots);
        if !path_in_allowed_working_path(Path::new(&resolved), &work_dirs, roots) {
            return SedVerdict::unsafe_default();
        }
    }
    SedVerdict::Safe
}

/// Mode-aware sed-constraint verdict — 1:1 with claude-code `checkSedConstraints`
/// (`sedValidation.ts:644-684`), which runs in EVERY permission mode (not only
/// `acceptEdits`). For a single `sed` subcommand:
///
/// - `allow_file_writes = false` (every non-`acceptEdits` mode): only the
///   read-only allowlist (line-printing / stdout substitution) is `Safe`; a sed
///   that writes in-place (`-i`) or carries file args under a substitution is
///   `Unsafe` → ask (TS passes `allowFileWrites: false` outside `acceptEdits`).
/// - `allow_file_writes = true` (`acceptEdits` mode): the in-place allowlist is
///   permitted, but every in-place file target must lie inside an allowed
///   working dir — exactly [`sed_auto_allow_verdict`]'s containment behavior.
///
/// This is the verdict [`crate::policy`] consults for its general (all-modes)
/// sed-constraints layer; the `acceptEdits` bash auto-allow reuses
/// [`sed_auto_allow_verdict`] (the `allow_file_writes = true` case) directly.
#[must_use]
pub fn sed_constraint_verdict(
    command: &str,
    allow_file_writes: bool,
    roots: &FsRoots,
    additional: &[PathBuf],
) -> SedVerdict {
    if allow_file_writes {
        // `acceptEdits`: the in-place allowlist + working-dir containment.
        return sed_auto_allow_verdict(command, roots, additional);
    }
    // 2.1.211 `mQi` over-length / tokenize-mismatch pre-check (runs before the
    // allowlist `jQt`, in every mode — `Xwu` consults `mQi` before `jQt`).
    if let Some(v) = sed_precheck_verdict(command) {
        return v;
    }
    // Non-`acceptEdits`: read-only allowlist only. An in-place / file-writing sed
    // is NOT allowed here (TS `allowFileWrites: false`), so it asks.
    if sed_command_is_allowed_by_allowlist(command, false) {
        SedVerdict::Safe
    } else {
        SedVerdict::unsafe_default()
    }
}

/// Working-dir set (cwd + additional) as `PathBuf`s for the containment check.
fn working_dir_paths(roots: &FsRoots, additional: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(1 + additional.len());
    dirs.push(roots.cwd.clone());
    dirs.extend(additional.iter().cloned());
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> FsRoots {
        FsRoots {
            cwd: PathBuf::from("/proj"),
            home: Some(PathBuf::from("/home/u")),
            lingxi_home: PathBuf::from("/home/u/.lingxi"),
        }
    }

    fn verdict(cmd: &str) -> SedVerdict {
        sed_auto_allow_verdict(cmd, &roots(), &[])
    }

    // ── read-only / line-printing is Safe ──────────────────────────────────

    #[test]
    fn line_printing_is_safe() {
        assert_eq!(verdict("sed -n p file"), SedVerdict::Safe);
        assert_eq!(verdict("sed -n '1,5p' file"), SedVerdict::Safe);
        assert_eq!(verdict("sed -n '1p;2p;3p' file.txt"), SedVerdict::Safe);
    }

    #[test]
    fn line_printing_outside_cwd_still_safe() {
        // Read-only: never writes, so a file path outside cwd is fine.
        assert_eq!(verdict("sed -n p /etc/passwd"), SedVerdict::Safe);
    }

    #[test]
    fn stdout_substitution_is_safe() {
        // No file args, no `-i` → stdout-only substitution.
        assert_eq!(verdict("sed 's/a/b/'"), SedVerdict::Safe);
        assert_eq!(verdict("sed 's/a/b/g'"), SedVerdict::Safe);
    }

    // ── in-place write inside cwd is Safe ──────────────────────────────────

    #[test]
    fn in_place_inside_cwd_is_safe() {
        assert_eq!(verdict("sed -i 's/a/b/' ./local.txt"), SedVerdict::Safe);
        assert_eq!(verdict("sed -i 's/a/b/' sub/dir/x.txt"), SedVerdict::Safe);
        assert_eq!(
            verdict("sed -i 's/a/b/' /proj/inside.txt"),
            SedVerdict::Safe
        );
    }

    // ── in-place write outside cwd asks ────────────────────────────────────

    #[test]
    fn in_place_outside_cwd_is_unsafe() {
        let v = verdict("sed -i 's/a/b/' /etc/passwd");
        match v {
            SedVerdict::Unsafe { message, reason } => {
                assert_eq!(message, SED_ASK_MESSAGE);
                assert_eq!(reason, SED_ASK_REASON);
            }
            SedVerdict::Safe => panic!("in-place write to /etc/passwd must be Unsafe"),
        }
    }

    #[test]
    fn in_place_long_flag_outside_cwd_is_unsafe() {
        assert!(matches!(
            verdict("sed --in-place 's/a/b/' /etc/hosts"),
            SedVerdict::Unsafe { .. }
        ));
    }

    // ── dangerous ops are Unsafe regardless of path ────────────────────────

    #[test]
    fn write_command_is_unsafe() {
        // `w file` write command — denylisted.
        assert!(matches!(
            verdict("sed -n 'w /tmp/out' file"),
            SedVerdict::Unsafe { .. }
        ));
    }

    #[test]
    fn execute_command_is_unsafe() {
        assert!(matches!(
            verdict("sed -n '1e cat /etc/passwd' f"),
            SedVerdict::Unsafe { .. }
        ));
    }

    #[test]
    fn substitution_write_flag_is_unsafe() {
        // `s/a/b/w file` — write flag.
        assert!(matches!(
            verdict("sed -i 's/a/b/w evil' ./local"),
            SedVerdict::Unsafe { .. }
        ));
    }

    #[test]
    fn substitution_execute_flag_is_unsafe() {
        assert!(matches!(
            verdict("sed 's/a/b/e'"),
            SedVerdict::Unsafe { .. }
        ));
    }

    // ── non-allowlisted commands are Unsafe ────────────────────────────────

    #[test]
    fn unknown_flag_is_unsafe() {
        assert!(matches!(
            verdict("sed -X 's/a/b/'"),
            SedVerdict::Unsafe { .. }
        ));
    }

    #[test]
    fn non_sed_command_is_unsafe() {
        assert!(matches!(verdict("cat file"), SedVerdict::Unsafe { .. }));
    }

    // ── predicate parity unit checks ───────────────────────────────────────

    #[test]
    fn print_command_allowlist() {
        assert!(is_print_command("p"));
        assert!(is_print_command("1p"));
        assert!(is_print_command("1,5p"));
        assert!(!is_print_command("w"));
        assert!(!is_print_command("1,2,3p"));
        assert!(!is_print_command(""));
        assert!(!is_print_command("ap"));
    }

    #[test]
    fn dangerous_operations_denylist() {
        assert!(contains_dangerous_operations("w /etc/passwd"));
        assert!(contains_dangerous_operations("1w out"));
        assert!(contains_dangerous_operations("/p/w f"));
        assert!(contains_dangerous_operations("e cat"));
        assert!(contains_dangerous_operations("s/a/b/w f"));
        assert!(contains_dangerous_operations("s/a/b/e"));
        assert!(contains_dangerous_operations("y/abc/def/w"));
        assert!(contains_dangerous_operations("p{q}"));
        assert!(contains_dangerous_operations("!p"));
        // Safe ones:
        assert!(!contains_dangerous_operations("p"));
        assert!(!contains_dangerous_operations("s/a/b/g"));
        assert!(!contains_dangerous_operations("1,5p"));
    }

    #[test]
    fn has_file_args_detection() {
        assert!(has_file_args(&tokenize("-n p file.txt")));
        assert!(!has_file_args(&tokenize("-n p")));
        assert!(!has_file_args(&tokenize("'s/a/b/'")));
        assert!(has_file_args(&tokenize("'s/a/b/' file.txt")));
        // glob counts as a file arg
        assert!(has_file_args(&tokenize("-n p *.log")));
    }

    // ── mQi over-length / tokenize-mismatch pre-check (SED-XWU-RESTRUCTURE-01) ──

    fn over_length_reason(v: &SedVerdict) -> &str {
        match v {
            SedVerdict::Unsafe { reason, .. } => reason.as_str(),
            SedVerdict::Safe => panic!("expected Unsafe"),
        }
    }

    #[test]
    fn over_length_sed_asks_with_over_length_reason() {
        // A command longer than SED_MAX_COMMAND_LEN is over-length → ask with the
        // byte-locked over-length reason, even though it is otherwise a plain
        // stdout substitution the allowlist would accept.
        let long = format!("sed 's/a/{}/'", "b".repeat(SED_MAX_COMMAND_LEN));
        let v = verdict(&long);
        assert_eq!(over_length_reason(&v), SED_OVER_LENGTH_REASON);
        // Same in the general (non-acceptEdits) verdict.
        let v2 = sed_constraint_verdict(&long, false, &roots(), &[]);
        assert_eq!(over_length_reason(&v2), SED_OVER_LENGTH_REASON);
    }

    #[test]
    fn untokenizable_sed_asks_with_over_length_reason() {
        // $4r cases: control char, backslash-space, `~[`, unquoted `=x`, `<n-m>`.
        for cmd in [
            "sed 's/a/b/'\u{0007}", // M3i: BEL control char
            "sed \\ 's/a/b/'",      // F3i: backslash-space
            "sed 's/~[x]/y/' f",    // rzn: ~[
            "sed =foo 's/a/b/'",    // nzn: `=` after whitespace then letter
            "sed 's/a/b/' <1-5>",   // U3i: <n-m> brace-range
        ] {
            let v = verdict(cmd);
            assert_eq!(
                over_length_reason(&v),
                SED_OVER_LENGTH_REASON,
                "{cmd:?} must ask via the over-length/tokenize-mismatch pre-check"
            );
        }
    }

    #[test]
    fn untokenizable_predicate_negative_cases() {
        // Ordinary seds are tokenizable → not flagged by the pre-check.
        assert!(!sed_command_untokenizable("sed -n p file"));
        assert!(!sed_command_untokenizable("sed 's/a/b/g' file.txt"));
        assert!(!sed_command_untokenizable("sed -i 's/foo/bar/' ./x"));
        // …and they remain Safe through the full verdict.
        assert_eq!(verdict("sed -n p file"), SedVerdict::Safe);
        assert_eq!(verdict("sed 's/a/b/'"), SedVerdict::Safe);
    }

    #[test]
    fn additional_working_dir_widens_allowance() {
        let extra = vec![PathBuf::from("/tmp/scratch")];
        assert_eq!(
            sed_auto_allow_verdict("sed -i 's/a/b/' /tmp/scratch/x", &roots(), &extra),
            SedVerdict::Safe
        );
        assert!(matches!(
            sed_auto_allow_verdict("sed -i 's/a/b/' /etc/x", &roots(), &extra),
            SedVerdict::Unsafe { .. }
        ));
    }
}
