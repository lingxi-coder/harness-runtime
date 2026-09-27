//! Argument substitution for markdown-defined slash commands.
//!
//! Faithful port of `claude-code/src/utils/argumentSubstitution.ts`. Supports:
//! - `$ARGUMENTS` — replaced with the full (raw) arguments string.
//! - `$ARGUMENTS[0]`, `$ARGUMENTS[1]`, … — individual indexed arguments.
//! - `$0`, `$1`, … — shorthand for `$ARGUMENTS[N]`.
//! - Named arguments (`$foo`, `$bar`) when argument names are declared in
//!   frontmatter.
//!
//! ## Divergence from TS (documented intentionally)
//!
//! * **No `regex` crate.** The crate may not add new dependencies, so the four
//!   TS regexes (`\$<name>(?![\[\w])`, `\$ARGUMENTS\[(\d+)\]`, `\$(\d+)(?!\w)`,
//!   and `replaceAll("$ARGUMENTS", …)`) are reproduced with hand-written
//!   scanners. Each scanner is byte-faithful to the regex it mirrors, including
//!   `\d+`'s greedy-with-backtracking interaction with the `(?!\w)` lookahead
//!   (so `$12` consumes two digits → index 12, while `$12a` matches nothing).
//! * **`parse_arguments` is `shell-quote`-faithful** (ARGS.1). It delegates to
//!   [`crate::parser::tokenize_args`], which reproduces `shell-quote@1.8.1`'s
//!   `parse(args, key => "$" + key)` and keeps only the string tokens — glob,
//!   operator, and comment entries are dropped, adjacent quoted/unquoted runs
//!   concatenate, and a `${…}` "Bad substitution" falls back to a whitespace
//!   split (see that function for the precise behaviour and fidelity notes).
//! * **Named-argument regex semantics** (ARGS.2). TS builds
//!   `new RegExp("\\$" + name + "(?![\\[\\w])", "g")` from the *unescaped*
//!   frontmatter name, so the name is a regex *pattern*. Without the `regex`
//!   crate (and because `(?!…)` lookaheads are unsupported by it anyway) we:
//!   - apply the `(?![\[\w])` boundary by hand (next char is neither `[`, a
//!     word char, nor `_`), and
//!   - validate the name as a regex and **surface an expansion error**
//!     ([`SubstitutionError`]) when it would make `new RegExp(...)` throw —
//!     matching TS's `SyntaxError` throw rather than silently literal-replacing.
//!
//!   Fidelity boundary: full JS-`RegExp` pattern semantics are *not* emulated.
//!   A name that is a *valid* regex with metacharacters (e.g. `a.b`) is matched
//!   **literally** (`$a.b`), not as a pattern (TS would also match `$aXb`). The
//!   validity check detects the cases that make JS `RegExp` throw — an
//!   unterminated character class (`a[b`, the spec's named example), an
//!   unbalanced group (`a(b`, `a)b`), and a name-trailing backslash — and
//!   surfaces [`SubstitutionError::InvalidArgumentName`]; the diagnostic text
//!   differs from V8's `SyntaxError` message.

use crate::parser::{tokenize_args, ParsedSlashCommand};

/// An argument name from frontmatter formed an invalid regular expression.
///
/// TS builds the named-argument matcher with `new RegExp(...)` and lets the
/// resulting `SyntaxError` propagate, aborting expansion with a user-facing
/// error. This is the Rust equivalent that callers surface to the user.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubstitutionError {
    /// The frontmatter argument `name` would make `new RegExp("\\$" + name +
    /// "(?![\\[\\w])")` throw a `SyntaxError`. `reason` is a human-readable
    /// description of why (it does not reproduce V8's exact wording).
    #[error("invalid argument name `{name}`: forms an invalid regular expression ({reason})")]
    InvalidArgumentName {
        /// The offending frontmatter argument name.
        name: String,
        /// Why the name is not a valid regular expression.
        reason: String,
    },
}

/// Frontmatter `arguments` field: either a space-separated string or an array
/// of names. Mirrors the TS `string | string[] | undefined` union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterArgs {
    /// Space-separated names in a single string (e.g. `"foo bar baz"`).
    Str(String),
    /// Pre-split list of names (e.g. `["foo", "bar", "baz"]`).
    List(Vec<String>),
}

/// Parse an arguments string into individual arguments.
///
/// Faithful to TS `parseArguments`: empty/whitespace-only input yields an empty
/// vector; otherwise [`crate::parser::tokenize_args`] runs the
/// `shell-quote`-faithful tokenizer and keeps only the string tokens.
#[must_use]
pub fn parse_arguments(args: &str) -> Vec<String> {
    if args.trim().is_empty() {
        return Vec::new();
    }
    tokenize_args(args)
}

/// Parse argument names from the frontmatter `arguments` field.
///
/// Faithful to TS `parseArgumentNames`: filters out empty/whitespace-only names
/// and numeric-only names (which would collide with the `$0`/`$1` shorthand).
#[must_use]
pub fn parse_argument_names(argument_names: Option<&FrontmatterArgs>) -> Vec<String> {
    fn is_valid_name(name: &str) -> bool {
        let trimmed = name.trim();
        !trimmed.is_empty() && !is_all_ascii_digits(name)
    }

    match argument_names {
        None => Vec::new(),
        Some(FrontmatterArgs::List(list)) => {
            list.iter().filter(|n| is_valid_name(n)).cloned().collect()
        }
        Some(FrontmatterArgs::Str(s)) => s
            .split_whitespace()
            .filter(|n| is_valid_name(n))
            .map(str::to_string)
            .collect(),
    }
}

/// Generate the progressive argument hint showing the remaining unfilled
/// argument names (e.g. `"[arg2] [arg3]"`), or `None` once all are filled.
///
/// Faithful to TS `generateProgressiveArgumentHint`
/// (`utils/argumentSubstitution.ts:76-83`): returns the names after the ones
/// already typed, each wrapped in `[...]` and space-joined. TS `Array.slice`
/// clamps when `typed_args.len() > arg_names.len()`, so the `.min()` here
/// reproduces that clamp without panicking on the slice index.
#[must_use]
pub fn generate_progressive_argument_hint(
    arg_names: &[String],
    typed_args: &[String],
) -> Option<String> {
    let consumed = typed_args.len().min(arg_names.len());
    let remaining = &arg_names[consumed..];
    if remaining.is_empty() {
        return None;
    }
    Some(
        remaining
            .iter()
            .map(|name| format!("[{name}]"))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// `true` when `s` is non-empty and every byte is an ASCII digit — the Rust
/// equivalent of the TS `/^\d+$/` test used for the numeric-only filter and the
/// `$ARGUMENTS[N]` / `$N` index extraction.
fn is_all_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `true` for `[A-Za-z0-9_]` — the chars matched by the regex `\w` token.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Strip the U+FFFF sentinel from a substituted argument value before it is
/// inserted, mirroring TS `i=(f)=>(f??"").replaceAll(QQn,"")` (`QQn="￿"`).
/// Every substituted value (named, indexed, shorthand, and full `$ARGUMENTS`)
/// is routed through this, so the later sentinel-restore
/// (`replaceAll(QQn, "$")`) only ever converts the genuine out-of-range
/// sentinels back to `$` — never a literal U+FFFF that a user supplied inside
/// an argument value (which TS drops rather than corrupting into `$`).
fn strip_sentinel(value: &str) -> std::borrow::Cow<'_, str> {
    if value.contains('\u{FFFF}') {
        std::borrow::Cow::Owned(value.replace('\u{FFFF}', ""))
    } else {
        std::borrow::Cow::Borrowed(value)
    }
}

/// Faithful port of TS `substituteArguments`.
///
/// `args == None` (TS `undefined`/`null`) returns `content` unchanged. An empty
/// string is a valid value that replaces placeholders with empty text. Steps run
/// in the exact TS order:
/// 1. named args via `\$<name>(?![\[\w])`,
/// 2. `\$ARGUMENTS\[(\d+)\]`,
/// 3. `\$(\d+)(?!\w)` shorthand,
/// 4. `replaceAll("$ARGUMENTS", args)`,
/// 5. append `\n\nARGUMENTS: {args}` iff nothing changed, `append_if_no_placeholder`,
///    and `args` is non-empty.
///
/// # Errors
///
/// Returns [`SubstitutionError::InvalidArgumentName`] when a frontmatter
/// argument name forms an invalid regular expression (step 1), mirroring the
/// `SyntaxError` TS throws from `new RegExp(...)`.
pub fn substitute_arguments_faithful(
    content: &str,
    args: Option<&str>,
    append_if_no_placeholder: bool,
    argument_names: &[String],
) -> Result<String, SubstitutionError> {
    let Some(args) = args else {
        return Ok(content.to_string());
    };

    let parsed_args = parse_arguments(args);
    let original_content = content;
    let mut content = content.to_string();

    // (1) Named arguments: $name not followed by `[` or a word char. TS builds
    // the matcher per name and throws on an invalid pattern — we propagate.
    for (i, name) in argument_names.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        let replacement = strip_sentinel(parsed_args.get(i).map_or("", String::as_str));
        content = replace_named_arg(&content, name, &replacement)?;
    }

    // (2) Indexed: $ARGUMENTS[<digits>]. An out-of-range index is preserved
    // verbatim (not stripped): the match is re-emitted with its leading `$`
    // swapped for the U+FFFF sentinel so step 4's `$ARGUMENTS` replaceAll cannot
    // see it; the sentinel is restored to `$` after step 4 (parity 2.1.212).
    let (next, had_unmatched_indexed) = replace_arguments_indexed(&content, &parsed_args);
    content = next;

    // (3) Shorthand: $<digits> not followed by a word char (greedy digits). An
    // out-of-range index is left verbatim (parity 2.1.212).
    content = replace_shorthand_indexed(&content, &parsed_args);

    // (4) Full arguments string. The value is sentinel-stripped before
    // insertion (TS `i(t)`) so a literal U+FFFF in `args` is dropped, not
    // corrupted into `$` by the restore below.
    content = content.replace("$ARGUMENTS", &strip_sentinel(args));

    // Restore the sentinel emitted for out-of-range `$ARGUMENTS[N]` back to `$`
    // (TS `if (u || p) e = e.replaceAll(QQn, "$")`). Only runs when such a
    // placeholder was preserved, matching the `p` guard.
    if had_unmatched_indexed {
        content = content.replace('\u{FFFF}', "$");
    }

    // (5) Tail append when nothing was substituted. A preserved (out-of-range)
    // placeholder does not count as a substitution: after the sentinel restore
    // the content is byte-identical to the original, so this fires exactly as
    // TS's `!d` guard would (parity 2.1.212).
    if content == original_content && append_if_no_placeholder && !args.is_empty() {
        content = format!("{content}\n\nARGUMENTS: {args}");
    }

    Ok(content)
}

/// Replace `$<name>` where the frontmatter argument `name` is interpreted as a
/// regex (per TS `new RegExp("\\$" + name + "(?![\\[\\w])", "g")`) and the
/// following char is neither `[` nor a word char.
///
/// Fidelity boundary: a *valid* metacharacter name matches **literally** (no
/// pattern semantics); an *invalid* name is rejected up front so the caller can
/// surface the error TS would throw. See the module docs.
fn replace_named_arg(
    content: &str,
    name: &str,
    replacement: &str,
) -> Result<String, SubstitutionError> {
    validate_argument_name_regex(name).map_err(|reason| {
        SubstitutionError::InvalidArgumentName {
            name: name.to_string(),
            reason,
        }
    })?;

    let bytes = content.as_bytes();
    let name_bytes = name.as_bytes();
    let pat_len = 1 + name_bytes.len(); // '$' + name
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$'
            && i + pat_len <= bytes.len()
            && &bytes[i + 1..i + pat_len] == name_bytes
        {
            // Negative lookahead `(?![\[\w])` on the char *after* the name.
            let next = bytes.get(i + pat_len).copied();
            let blocked = matches!(next, Some(b'[')) || matches!(next, Some(c) if is_word_byte(c));
            if !blocked {
                out.push_str(replacement);
                i += pat_len;
                continue;
            }
        }
        // `i` always sits on a char boundary — we only ever advance past whole
        // matched ASCII spans or copy one UTF-8 char at a time below.
        let ch_len = utf8_char_len(bytes[i]);
        out.push_str(&content[i..i + ch_len]);
        i += ch_len;
    }
    Ok(out)
}

/// Detect frontmatter argument names that would make
/// `new RegExp("\\$" + name + "(?![\\[\\w])")` throw a `SyntaxError`.
///
/// This is a focused validity check, not a full JS-`RegExp` validator: it flags
/// the constructs that throw and matter in practice — an unterminated character
/// class (`[` with no closing `]`, which swallows the `(?![\[\w])` suffix and
/// leaves an unmatched `)`), an unbalanced group (`(` / `)`), and a
/// name-trailing backslash (which escapes the suffix's `(`). Other valid
/// metacharacters are accepted (and then matched literally — see
/// [`replace_named_arg`]). `\X` escapes are skipped, and `(`/`)` inside a `[…]`
/// class are literal, matching JS.
fn validate_argument_name_regex(name: &str) -> Result<(), String> {
    let chars: Vec<char> = name.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut in_class = false;
    let mut paren_depth: i32 = 0;
    while i < n {
        let c = chars[i];
        if c == '\\' {
            // Escape: consumes the next char. A trailing `\` would escape the
            // suffix's `(`, breaking the boundary assertion -> JS throws.
            if i + 1 >= n {
                return Err("name ends with a backslash".to_string());
            }
            i += 2;
            continue;
        }
        if in_class {
            if c == ']' {
                in_class = false;
            }
            i += 1;
            continue;
        }
        match c {
            '[' => in_class = true,
            '(' => paren_depth += 1,
            ')' => {
                if paren_depth == 0 {
                    return Err("unmatched ')'".to_string());
                }
                paren_depth -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    if in_class {
        return Err("unterminated character class `[`".to_string());
    }
    if paren_depth > 0 {
        return Err("unterminated group `(`".to_string());
    }
    Ok(())
}

/// Replace `$ARGUMENTS[<digits>]` (regex `\$ARGUMENTS\[(\d+)\]`).
///
/// A matched index substitutes the argument. An out-of-range index is preserved
/// verbatim instead of being stripped (TS 2.1.212 `p=!0, QQn + f.slice(1)`): the
/// match is re-emitted as `<U+FFFF>ARGUMENTS[N]` — the leading `$` replaced by
/// the sentinel so the later `replaceAll("$ARGUMENTS", …)` cannot match inside
/// it. The caller restores the sentinel to `$` afterwards. Returns whether any
/// such placeholder was preserved (TS's `p` flag), which gates that restore.
fn replace_arguments_indexed(content: &str, parsed_args: &[String]) -> (String, bool) {
    const PREFIX: &[u8] = b"$ARGUMENTS[";
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut had_unmatched = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$'
            && i + PREFIX.len() <= bytes.len()
            && &bytes[i..i + PREFIX.len()] == PREFIX
        {
            // Consume one-or-more digits, then require a closing ']'.
            let digits_start = i + PREFIX.len();
            let mut j = digits_start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits_start && j < bytes.len() && bytes[j] == b']' {
                let index: usize = content[digits_start..j].parse().unwrap_or(usize::MAX);
                match parsed_args.get(index) {
                    Some(replacement) => out.push_str(&strip_sentinel(replacement)),
                    None => {
                        // Preserve verbatim, shielded by the sentinel (TS
                        // `QQn + f.slice(1)`, where `f` is the whole `$…]` match
                        // and `slice(1)` drops the leading `$`).
                        had_unmatched = true;
                        out.push('\u{FFFF}');
                        out.push_str(&content[i + 1..=j]);
                    }
                }
                i = j + 1; // skip past ']'
                continue;
            }
        }
        let ch_len = utf8_char_len(bytes[i]);
        out.push_str(&content[i..i + ch_len]);
        i += ch_len;
    }
    (out, had_unmatched)
}

/// Replace `$<digits>` shorthand (regex `\$(\d+)(?!\w)`). `\d+` is greedy; the
/// negative lookahead `(?!\w)` then rejects a trailing word char, with the same
/// backtracking the regex engine performs. Concretely:
/// * `$12` → digits `12`, next char is non-word → index 12.
/// * `$12a` → greedy `12` then `a` (word) fails; backtrack to `1`, next is `2`
///   (word) fails; no shorter match → no substitution.
fn replace_shorthand_indexed(content: &str, parsed_args: &[String]) -> String {
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            // Greedily consume all following digits.
            let digits_start = i + 1;
            let mut end = digits_start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > digits_start {
                // Backtrack `end` down to the longest prefix whose following
                // char is not a word char (mirrors regex `(?!\w)` backtracking).
                let mut k = end;
                let mut matched = None;
                while k > digits_start {
                    let next = bytes.get(k).copied();
                    let next_is_word = matches!(next, Some(c) if is_word_byte(c));
                    if !next_is_word {
                        matched = Some(k);
                        break;
                    }
                    k -= 1;
                }
                if let Some(match_end) = matched {
                    let index: usize = content[digits_start..match_end]
                        .parse()
                        .unwrap_or(usize::MAX);
                    match parsed_args.get(index) {
                        Some(replacement) => out.push_str(&strip_sentinel(replacement)),
                        // Out-of-range index is preserved verbatim, not stripped
                        // (TS 2.1.212 `if (s[g] === void 0) return f`): re-emit
                        // the whole `$<digits>` match unchanged.
                        None => out.push_str(&content[i..match_end]),
                    }
                    i = match_end;
                    continue;
                }
                // No valid (?!\w) position: emit '$' and rescan from the digits.
                out.push('$');
                i = digits_start;
                continue;
            }
        }
        let ch_len = utf8_char_len(bytes[i]);
        out.push_str(&content[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// UTF-8 length (in bytes) of the char starting with leading byte `b`.
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Backwards-compatible 2-arg shim retained so downstream crates (e.g.
/// `command-core`) keep compiling. Delegates to [`substitute_arguments_faithful`]
/// with `append_if_no_placeholder = true` and no named arguments, passing the
/// raw argument string. Because it passes **no** argument names, the named-arg
/// regex path is never exercised and the call is infallible.
#[must_use]
pub fn substitute_arguments(template: &str, args: &ParsedSlashCommand) -> String {
    substitute_arguments_faithful(template, Some(&args.raw_args), true, &[])
        .expect("substitute_arguments passes no argument names, so it cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    /// Unwrapping helper for the common (no invalid names) test paths.
    fn sub(content: &str, args: Option<&str>, append: bool, names: &[String]) -> String {
        substitute_arguments_faithful(content, args, append, names).expect("valid substitution")
    }

    // ----- parse_arguments (ARGS.1) -----

    #[test]
    fn parse_arguments_basic_and_quoted() {
        assert_eq!(parse_arguments("foo bar baz"), vec!["foo", "bar", "baz"]);
        assert_eq!(
            parse_arguments("foo \"hello world\" baz"),
            vec!["foo", "hello world", "baz"]
        );
        assert_eq!(
            parse_arguments("foo 'hello world' baz"),
            vec!["foo", "hello world", "baz"]
        );
    }

    #[test]
    fn parse_arguments_empty_and_whitespace() {
        assert!(parse_arguments("").is_empty());
        assert!(parse_arguments("   ").is_empty());
    }

    #[test]
    fn parse_arguments_preserves_dollar_key() {
        // No variable expansion: $KEY stays literal.
        assert_eq!(parse_arguments("$HOME path"), vec!["$HOME", "path"]);
    }

    #[test]
    fn parse_arguments_drops_globs_operators_and_comments() {
        // Glob word dropped, plain word kept.
        assert_eq!(parse_arguments("*.ts foo"), vec!["foo"]);
        // Redirect/pipe operators dropped.
        assert_eq!(
            parse_arguments("report > out.txt"),
            vec!["report", "out.txt"]
        );
        assert_eq!(parse_arguments("a | b"), vec!["a", "b"]);
        // Comment truncates the remainder.
        assert_eq!(parse_arguments("a # b"), vec!["a"]);
        // Adjacent quoted/unquoted runs concatenate.
        assert_eq!(parse_arguments("foo\"bar\"baz"), vec!["foobarbaz"]);
    }

    // ----- parse_argument_names -----

    #[test]
    fn parse_argument_names_string_and_list() {
        assert_eq!(
            parse_argument_names(Some(&FrontmatterArgs::Str("foo bar baz".into()))),
            vec!["foo", "bar", "baz"]
        );
        assert_eq!(
            parse_argument_names(Some(&FrontmatterArgs::List(names(&["foo", "bar"])))),
            vec!["foo", "bar"]
        );
        assert!(parse_argument_names(None).is_empty());
    }

    #[test]
    fn parse_argument_names_filters_empty_and_numeric() {
        assert_eq!(
            parse_argument_names(Some(&FrontmatterArgs::Str("foo  123 bar 7".into()))),
            vec!["foo", "bar"]
        );
        assert_eq!(
            parse_argument_names(Some(&FrontmatterArgs::List(names(&["", "  ", "42", "ok"])))),
            vec!["ok"]
        );
    }

    // ----- progressive argument hint -----

    #[test]
    fn progressive_hint_matches_ts_semantics() {
        let arg_names = names(&["arg1", "arg2", "arg3"]);
        // zero typed → all remaining names, each bracketed, space-joined.
        assert_eq!(
            generate_progressive_argument_hint(&arg_names, &[]),
            Some("[arg1] [arg2] [arg3]".to_string())
        );
        // partial typed → only the names after the typed count.
        assert_eq!(
            generate_progressive_argument_hint(&arg_names, &names(&["x"])),
            Some("[arg2] [arg3]".to_string())
        );
        // all filled → None.
        assert_eq!(
            generate_progressive_argument_hint(&arg_names, &names(&["x", "y", "z"])),
            None
        );
        // typed beyond the declared names → None (TS Array.slice clamps; no panic).
        assert_eq!(
            generate_progressive_argument_hint(&arg_names, &names(&["x", "y", "z", "w"])),
            None
        );
        // no declared names → None regardless of typed args.
        assert_eq!(
            generate_progressive_argument_hint(&[], &names(&["x"])),
            None
        );
    }

    // ----- indexed / shorthand substitution -----

    #[test]
    fn arguments_indexed_and_shorthand() {
        // $ARGUMENTS[0] and $1 select the same token classes.
        assert_eq!(sub("[$ARGUMENTS[0]]", Some("a b c"), true, &[]), "[a]");
        assert_eq!(sub("[$1]", Some("a b c"), true, &[]), "[b]");
        // Out of range -> preserved verbatim (parity 2.1.212); with nothing
        // substituted, the appendIfNoPlaceholder tail is added.
        assert_eq!(
            sub("[$ARGUMENTS[9]]", Some("a b"), true, &[]),
            "[$ARGUMENTS[9]]\n\nARGUMENTS: a b"
        );
    }

    #[test]
    fn out_of_range_placeholders_preserved_verbatim() {
        // parity 2.1.212: unmatched $ARGUMENTS[N] / $N are kept verbatim, not
        // stripped to empty. append off isolates the substitution result.
        assert_eq!(
            sub("[$ARGUMENTS[9]]", Some("a b"), false, &[]),
            "[$ARGUMENTS[9]]"
        );
        assert_eq!(sub("[$9]", Some("a b"), false, &[]), "[$9]");
        // Mixed: matched index expands, out-of-range index preserved.
        assert_eq!(
            sub("$ARGUMENTS[0] $ARGUMENTS[9]", Some("a"), false, &[]),
            "a $ARGUMENTS[9]"
        );
        assert_eq!(sub("$0 $9", Some("a"), false, &[]), "a $9");
    }

    #[test]
    fn preserved_indexed_shielded_from_full_arguments_replace() {
        // The sentinel shields the preserved `$ARGUMENTS[9]` from step 4's
        // `$ARGUMENTS` replaceAll, while a bare `$ARGUMENTS` still expands
        // (parity 2.1.212 `p` sentinel).
        assert_eq!(
            sub("$ARGUMENTS[9] $ARGUMENTS", Some("a"), false, &[]),
            "$ARGUMENTS[9] a"
        );
    }

    #[test]
    fn literal_sentinel_in_args_is_stripped_not_restored() {
        // A literal U+FFFF in the argument value must be STRIPPED before
        // insertion (TS `i=(f)=>f.replaceAll(QQn,"")`), so the out-of-range
        // sentinel-restore only ever converts genuine sentinels back to `$`.
        // `$ARGUMENTS[9]` is out of range -> preserves the sentinel and gates
        // the restore; bare `$ARGUMENTS` expands to the (stripped) arg value.
        // TS `yot("$ARGUMENTS[9] $ARGUMENTS", "a￿b", false, [])` -> "$ARGUMENTS[9] ab".
        assert_eq!(
            sub("$ARGUMENTS[9] $ARGUMENTS", Some("a\u{FFFF}b"), false, &[]),
            "$ARGUMENTS[9] ab"
        );
        // Indexed and shorthand insertions strip the sentinel too.
        assert_eq!(sub("$ARGUMENTS[0]", Some("a\u{FFFF}b"), false, &[]), "ab");
        assert_eq!(sub("$0", Some("a\u{FFFF}b"), false, &[]), "ab");
    }

    #[test]
    fn shorthand_two_digit_consumes_both_digits() {
        // 13 args so index 12 exists; $12 must mean index 12, not $1 + "2".
        let args = "a0 a1 a2 a3 a4 a5 a6 a7 a8 a9 a10 a11 a12";
        assert_eq!(sub("[$12]", Some(args), true, &[]), "[a12]");
        // $1 alone still works and is not greedily merged with a following space.
        assert_eq!(sub("[$1]", Some(args), true, &[]), "[a1]");
    }

    #[test]
    fn shorthand_followed_by_word_char_does_not_match() {
        // `$12a`: greedy `12` rejected by `a`; backtrack to `1` rejected by `2`.
        // No (?!\w) position -> no substitution at all.
        assert_eq!(sub("x$12a", Some("a b c d"), false, &[]), "x$12a");
        // `$1a` likewise leaves the text untouched.
        assert_eq!(sub("$1a", Some("a b"), false, &[]), "$1a");
    }

    #[test]
    fn full_arguments_replacement_uses_raw_string() {
        assert_eq!(sub("all=$ARGUMENTS", Some("a b c"), true, &[]), "all=a b c");
        // $ARGUMENTS[0] is consumed before bare $ARGUMENTS, so the bracket form wins.
        assert_eq!(
            sub("$ARGUMENTS[0]-$ARGUMENTS", Some("a b"), true, &[]),
            "a-a b"
        );
    }

    // ----- named args & (?!\w) boundary -----

    #[test]
    fn named_args_boundary_negative_lookahead() {
        let argnames = names(&["foo"]);
        // $foo replaced; $foobar (word char after) NOT replaced; $foo[0] (next
        // char `[`) NOT replaced — the `(?![\[\w])` lookahead rejects both.
        assert_eq!(
            sub("$foo $foobar $foo[0]", Some("X"), true, &argnames),
            "X $foobar $foo[0]"
        );
    }

    #[test]
    fn named_args_map_by_position() {
        let argnames = names(&["first", "second"]);
        assert_eq!(
            sub("$first then $second", Some("alpha beta"), true, &argnames),
            "alpha then beta"
        );
        // Missing positional -> empty.
        assert_eq!(sub("$second", Some("alpha"), false, &argnames), "");
    }

    // ----- ARGS.2: named-name regex validity & metachar boundary -----

    #[test]
    fn named_arg_unterminated_class_surfaces_error() {
        // arguments: ["a[b"] -> `new RegExp("\\$a[b(?![\\[\\w])")` throws in JS;
        // we surface an InvalidArgumentName error instead of literal-replacing.
        let argnames = names(&["a[b"]);
        let err =
            substitute_arguments_faithful("see $a[b here", Some("V"), true, &argnames).unwrap_err();
        match err {
            SubstitutionError::InvalidArgumentName { name, .. } => assert_eq!(name, "a[b"),
        }
    }

    #[test]
    fn named_arg_unbalanced_parens_surface_error() {
        for bad in ["a)b", "a(b"] {
            let argnames = names(&[bad]);
            assert!(
                substitute_arguments_faithful("$x", Some("V"), true, &argnames).is_err(),
                "name `{bad}` should be rejected as an invalid regex"
            );
        }
    }

    #[test]
    fn named_arg_trailing_backslash_surfaces_error() {
        let argnames = names(&["a\\"]);
        assert!(substitute_arguments_faithful("$x", Some("V"), true, &argnames).is_err());
    }

    #[test]
    fn named_arg_valid_metachar_matches_literally() {
        // `a.b` is a VALID regex (dot = any char). Fidelity boundary: we match
        // it LITERALLY (`$a.b`), so `$aXb` is left untouched (TS would replace
        // it too). The literal occurrence IS replaced.
        let argnames = names(&["a.b"]);
        assert_eq!(
            sub("$a.b and $aXb", Some("V"), true, &argnames),
            "V and $aXb"
        );
    }

    #[test]
    fn named_arg_balanced_class_is_valid_and_literal() {
        // Balanced `[bc]` -> a valid regex -> accepted; matched literally with
        // the (?![\[\w]) boundary (here followed by `!`, which is allowed).
        let argnames = names(&["a[bc]d"]);
        assert_eq!(sub("$a[bc]d!", Some("V"), true, &argnames), "V!");
    }

    // ----- appendIfNoPlaceholder -----

    #[test]
    fn append_if_no_placeholder_on_appends_when_no_change() {
        assert_eq!(
            sub("no placeholders here", Some("a b"), true, &[]),
            "no placeholders here\n\nARGUMENTS: a b"
        );
    }

    #[test]
    fn append_if_no_placeholder_off_does_not_append() {
        assert_eq!(
            sub("no placeholders here", Some("a b"), false, &[]),
            "no placeholders here"
        );
    }

    #[test]
    fn append_if_no_placeholder_empty_vs_missing_args() {
        // Empty args: no append even though append flag on (TS `&& args` guard).
        assert_eq!(sub("plain", Some(""), true, &[]), "plain");
        // Missing args (None): content returned unchanged.
        assert_eq!(sub("plain", None, true, &[]), "plain");
        // A placeholder present -> content changes -> no append even with text.
        // $1 is index 1 -> the SECOND token ("b"), matching $ARGUMENTS[1].
        assert_eq!(sub("got $1", Some("a b"), true, &[]), "got b");
    }

    #[test]
    fn session_id_placeholder_left_untouched() {
        // ${LINGXI_SESSION_ID} is not an arg placeholder; substitution ignores it.
        // $1 -> index 1 -> "b".
        let out = sub("id=${LINGXI_SESSION_ID} $1", Some("a b"), true, &[]);
        assert_eq!(out, "id=${LINGXI_SESSION_ID} b");
    }

    // ----- shim -----

    #[test]
    fn shim_substitutes_positional_and_arguments() {
        let p = crate::parser::parse_slash_command("/foo a b c").unwrap();
        // $1 -> index 1 -> "b" (faithful), $ARGUMENTS -> raw "a b c".
        assert_eq!(
            substitute_arguments("first=$1 all=$ARGUMENTS", &p),
            "first=b all=a b c"
        );
    }
}
