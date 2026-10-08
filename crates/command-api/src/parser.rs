//! Slash-command parser over exact JavaScript UTF-16 source text.

use lingxi_core::types::utf16_json::Utf16JsonProjection;

/// A parsed slash-command invocation with exact source strings.
#[derive(Debug, Clone)]
pub struct ParsedSlashCommand {
    /// Exact command name (without the leading `/`).
    pub name: Utf16JsonProjection,
    /// Exact raw argument string after leading command whitespace.
    pub raw_args: Utf16JsonProjection,
    /// Exact argument tokens after quote-aware splitting.
    pub positional_args: Vec<Utf16JsonProjection>,
}

impl ParsedSlashCommand {
    /// Borrow text for command semantics that require a Unicode Rust string.
    /// This returns `None` for an exact source string containing an unpaired
    /// surrogate instead of silently replacing its code unit.
    #[must_use]
    pub fn raw_args_utf8(&self) -> Option<&str> {
        let text = self.raw_args.string_at("")?;
        self.raw_args
            .string_units("")?
            .iter()
            .copied()
            .eq(text.encode_utf16())
            .then_some(text)
    }

    /// Borrow a positional token only when it is valid Unicode text.
    #[must_use]
    pub fn positional_arg_utf8(&self, index: usize) -> Option<&str> {
        let value = self.positional_args.get(index)?;
        let text = value.string_at("")?;
        value
            .string_units("")?
            .iter()
            .copied()
            .eq(text.encode_utf16())
            .then_some(text)
    }
}

/// Parse one exact slash-command source string.
///
/// Mirrors Native 2.1.293's `Iv`/`Pve`: trim JavaScript whitespace around the
/// line, split the command name at the first JavaScript whitespace unit, then
/// trim both ends of the raw argument tail.
#[must_use]
pub fn parse_slash_command(input: &Utf16JsonProjection) -> Option<ParsedSlashCommand> {
    let input_units = input.string_units("")?;
    let units = trim_js_whitespace_units(&input_units);
    if units.first().copied()? != b'/' as u16 {
        return None;
    }
    let tail = &units[1..];
    let separator = tail.iter().position(|unit| is_js_whitespace_unit(*unit));
    let (mut name, mut raw_args) = match separator {
        Some(index) => (
            tail[..index].to_vec(),
            trim_js_whitespace_units(&tail[index + 1..]).to_vec(),
        ),
        None => (tail.to_vec(), Vec::new()),
    };

    // Native `Iv` treats a slash with no command name as a syntax failure,
    // including when whitespace follows the slash. This check precedes the
    // optional MCP marker handling in the source parser as well.
    if name.is_empty() {
        return None;
    }

    // Native recognizes the MCP disambiguator as part of the command name.
    // Its remaining argument string starts after the marker's whitespace.
    const MCP_MARKER: &[u16] = &[
        b'(' as u16,
        b'M' as u16,
        b'C' as u16,
        b'P' as u16,
        b')' as u16,
    ];
    if raw_args.starts_with(MCP_MARKER)
        && (raw_args.len() == MCP_MARKER.len() || is_js_whitespace_unit(raw_args[MCP_MARKER.len()]))
    {
        name.extend(" (MCP)".encode_utf16());
        raw_args = trim_js_whitespace_units(&raw_args[MCP_MARKER.len()..]).to_vec();
    }

    let positional_args = tokenize_args_source(&raw_args);
    Some(ParsedSlashCommand {
        name: projection_from_units(name),
        raw_args: projection_from_units(raw_args),
        positional_args,
    })
}

/// Trim ECMAScript `String.trim` whitespace from both ends without decoding
/// source UTF-16, so unpaired surrogates remain untouched.
pub(crate) fn trim_js_whitespace_units(input: &[u16]) -> &[u16] {
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

pub(crate) fn is_js_whitespace_unit(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(lingxi_core::host::effort::javascript_whitespace)
}

fn projection_from_units(units: Vec<u16>) -> Utf16JsonProjection {
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units)
        .expect("display projection is derived from the same UTF-16 units")
}

/// Tokenize an argument string the way the TS port does (ARGS.1).
///
/// Faithful to `claude-code/src/utils/argumentSubstitution.ts:parseArguments`,
/// which runs `shell-quote@1.8.1`'s `parse(args, key => "$" + key)` and then
/// keeps **only the string tokens** — operator (`| & ; ( ) < > …`), glob
/// (`*`/`?`), and comment (`#…`) entries become non-string `ParseEntry` objects
/// that are dropped. On a shell-quote parse error (a `${…}` "Bad substitution")
/// the TS code falls back to `args.split(/\s+/).filter(Boolean)`; we mirror that
/// with ECMAScript whitespace splitting.
///
/// Shared with [`crate::argument_substitution::parse_arguments`].
#[must_use]
pub(crate) fn tokenize_args(s: &str) -> Vec<String> {
    match shell_quote_parse(s) {
        Ok(tokens) => tokens,
        // TS: `tryParseShellCommand` failed -> `args.split(/\s+/).filter(Boolean)`.
        Err(ShellParseError::BadSubstitution) => s
            .split(lingxi_core::host::effort::javascript_whitespace)
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .collect(),
    }
}

pub(crate) fn tokenize_args_source(input: &[u16]) -> Vec<Utf16JsonProjection> {
    match shell_quote_parse_units(input) {
        Ok(tokens) => tokens.into_iter().map(projection_from_units).collect(),
        Err(ShellParseError::BadSubstitution) => {
            let mut tokens = Vec::new();
            let mut start = 0;
            for (index, unit) in input.iter().copied().enumerate() {
                if is_js_whitespace_unit(unit) {
                    if start < index {
                        tokens.push(projection_from_units(input[start..index].to_vec()));
                    }
                    start = index + 1;
                }
            }
            if start < input.len() {
                tokens.push(projection_from_units(input[start..].to_vec()));
            }
            tokens
        }
    }
}

fn flush_word_units(
    out: &mut Vec<Vec<u16>>,
    current: &mut Vec<u16>,
    started: &mut bool,
    glob: &mut bool,
) {
    if *started {
        if *glob {
            current.clear();
        } else {
            out.push(std::mem::take(current));
        }
    }
    *started = false;
    *glob = false;
}

fn parse_env_var_units(chars: &[u16], index: &mut usize) -> Result<Vec<u16>, ShellParseError> {
    let length = chars.len();
    *index += 1;
    let name = match chars.get(*index).copied() {
        Some(unit) if unit == b'{' as u16 => {
            *index += 1;
            if chars.get(*index).copied() == Some(b'}' as u16) {
                return Err(ShellParseError::BadSubstitution);
            }
            let end = chars[*index..]
                .iter()
                .position(|unit| *unit == b'}' as u16)
                .map(|offset| *index + offset)
                .ok_or(ShellParseError::BadSubstitution)?;
            let name = chars[*index..end].to_vec();
            *index = end;
            name
        }
        Some(unit)
            if unit == b'*' as u16
                || unit == b'@' as u16
                || unit == b'#' as u16
                || unit == b'?' as u16
                || unit == b'$' as u16
                || unit == b'!' as u16
                || unit == b'_' as u16
                || unit == b'-' as u16 =>
        {
            *index += 1;
            vec![unit]
        }
        _ => {
            let start = *index;
            while *index < length
                && ((chars[*index] >= b'A' as u16 && chars[*index] <= b'Z' as u16)
                    || (chars[*index] >= b'a' as u16 && chars[*index] <= b'z' as u16)
                    || (chars[*index] >= b'0' as u16 && chars[*index] <= b'9' as u16)
                    || chars[*index] == b'_' as u16)
            {
                *index += 1;
            }
            if *index == length {
                chars[start..length].to_vec()
            } else {
                let name = chars[start..*index].to_vec();
                *index = (*index).saturating_sub(1);
                name
            }
        }
    };
    let mut output = vec![b'$' as u16];
    output.extend(name);
    Ok(output)
}

fn shell_quote_parse_units(input: &[u16]) -> Result<Vec<Vec<u16>>, ShellParseError> {
    const SINGLE_QUOTE: u16 = 39;
    const DOUBLE_QUOTE: u16 = 34;
    const BACKSLASH: u16 = 92;
    const DOLLAR: u16 = 36;

    let mut output = Vec::<Vec<u16>>::new();
    let mut current = Vec::new();
    let mut started = false;
    let mut glob = false;
    let mut quote: Option<u16> = None;
    let mut escaped = false;
    let mut index = 0;

    while index < input.len() {
        let unit = input[index];
        if quote.is_none() && (unit == b'*' as u16 || unit == b'?' as u16) {
            glob = true;
        }
        if escaped {
            current.push(unit);
            started = true;
            escaped = false;
            index += 1;
            continue;
        }
        if let Some(active_quote) = quote {
            if unit == active_quote {
                quote = None;
                index += 1;
                continue;
            }
            if active_quote == SINGLE_QUOTE {
                current.push(unit);
                index += 1;
                continue;
            }
            if unit == BACKSLASH {
                match input.get(index + 1).copied() {
                    Some(next) if next == DOUBLE_QUOTE || next == BACKSLASH || next == DOLLAR => {
                        current.push(next)
                    }
                    Some(next) => {
                        current.push(BACKSLASH);
                        current.push(next);
                    }
                    None => current.push(BACKSLASH),
                }
                started = true;
                index += 2;
                continue;
            }
            if unit == DOLLAR {
                current.extend(parse_env_var_units(input, &mut index)?);
                started = true;
                index += 1;
                continue;
            }
            current.push(unit);
            started = true;
            index += 1;
            continue;
        }
        if unit == DOUBLE_QUOTE || unit == SINGLE_QUOTE {
            quote = Some(unit);
            started = true;
            index += 1;
            continue;
        }
        if unit == b'&' as u16
            || unit == b';' as u16
            || unit == b'(' as u16
            || unit == b')' as u16
            || unit == b'|' as u16
            || unit == b'<' as u16
            || unit == b'>' as u16
        {
            flush_word_units(&mut output, &mut current, &mut started, &mut glob);
            index += 1;
            continue;
        }
        if unit == b'#' as u16 {
            if !current.is_empty() {
                output.push(std::mem::take(&mut current));
            }
            return Ok(output);
        }
        if unit == BACKSLASH {
            escaped = true;
            started = true;
            index += 1;
            continue;
        }
        if unit == DOLLAR {
            current.extend(parse_env_var_units(input, &mut index)?);
            started = true;
            index += 1;
            continue;
        }
        if is_js_whitespace_unit(unit) {
            flush_word_units(&mut output, &mut current, &mut started, &mut glob);
            index += 1;
            continue;
        }
        current.push(unit);
        started = true;
        index += 1;
    }
    flush_word_units(&mut output, &mut current, &mut started, &mut glob);
    Ok(output)
}

/// The only way `shell-quote@1.8.1`'s `parse()` throws: a `${…}` substitution
/// with an empty or unterminated brace group.
enum ShellParseError {
    BadSubstitution,
}

/// `true` for the META control characters `shell-quote` treats as their own
/// operator tokens (`| & ; ( ) < >`). The multi-char operators (`|| && ;; |&
/// <( <<< >> >& <&`) decompose into these single chars; since every operator
/// token is filtered out by `parseArguments`, the per-char treatment is
/// string-token-equivalent to the real chunker.
fn is_control_char(c: char) -> bool {
    matches!(c, '&' | ';' | '(' | ')' | '|' | '<' | '>')
}

/// `[A-Za-z0-9_]` — the chars `shell-quote`'s `parseEnvVar` treats as `\w`
/// (ASCII-only, matching JS `\w`).
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The special single-char variable names `shell-quote`'s `parseEnvVar` accepts
/// after `$`: `[*@#?$!_-]`.
fn is_special_var_char(c: char) -> bool {
    matches!(c, '*' | '@' | '#' | '?' | '$' | '!' | '_' | '-')
}

/// `getVar` with `env = key => "$" + key`: the model never expands variables,
/// so a `$name` reference maps back to the literal `"$" + name`.
fn env_value(varname: &str) -> String {
    format!("${varname}")
}

/// Push the in-progress word to `out`, applying `shell-quote`'s glob rule: a
/// word that saw an unquoted `*`/`?` becomes a `{ op: 'glob' }` entry and is
/// dropped; otherwise it is emitted as a string (even when empty, e.g. `""`).
fn flush_word(out: &mut Vec<String>, cur: &mut String, started: &mut bool, glob: &mut bool) {
    if *started {
        if *glob {
            cur.clear();
        } else {
            out.push(std::mem::take(cur));
        }
    }
    *started = false;
    *glob = false;
}

/// `shell-quote@1.8.1`'s `parse()`, specialised to `env = key => "$" + key` and
/// projected onto the string tokens that `parseArguments` keeps.
///
/// This is a single pass that fuses shell-quote's two phases (the whitespace /
/// operator chunker and the per-character word scanner). The behaviour it
/// reproduces:
/// * single/double quotes, with adjacent quoted/unquoted runs concatenating
///   into one token (`foo"bar"baz` -> `foobarbaz`);
/// * backslash escapes outside quotes, and the `\" \\ \$`-aware rules inside
///   double quotes;
/// * unquoted `*`/`?` mark the word a glob -> dropped (note: shell-quote checks
///   `isGlob` *before* the escape branch, so even `\*` is treated as a glob);
/// * unquoted operator chars split words and are dropped;
/// * an unquoted `#` starts a comment: the word-so-far is emitted (ignoring the
///   glob flag, matching the TS short-circuit) and the remainder is discarded;
/// * `$name` / `${name}` / `$<special>` map through `env_value`, leaving the
///   literal `$…` text (`${name}` strips the braces, as shell-quote does).
///
/// ## Fidelity boundary
///
/// Two `shell-quote` quirks on *malformed/adversarial* input are intentionally
/// not reproduced (they never affect the verified vectors or realistic args):
/// * an **unterminated quote** is treated here as quoting the remainder (one
///   token), whereas shell-quote drops the dangling quote char and may split
///   the surrounding text into two tokens;
/// * the single-quote `\'` chunker bug (the `shellQuote.ts` security note) is
///   not modelled — single quotes are literal up to the next `'`.
fn shell_quote_parse(s: &str) -> Result<Vec<String>, ShellParseError> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut glob = false;
    let mut quote: Option<char> = None;
    let mut esc = false;
    let mut i = 0;

    while i < n {
        let c = chars[i];
        // shell-quote evaluates `isGlob` at the top of every iteration, before
        // the escape branch — so an escaped `\*` still flags the word a glob.
        if quote.is_none() && (c == '*' || c == '?') {
            glob = true;
        }

        if esc {
            cur.push(c);
            started = true;
            esc = false;
            i += 1;
            continue;
        }

        if let Some(q) = quote {
            if c == q {
                quote = None;
                i += 1;
                continue;
            }
            if q == '\'' {
                // Single quote: every char literal.
                cur.push(c);
                i += 1;
                continue;
            }
            // Double quote.
            if c == '\\' {
                // shell-quote: `i += 1; c = s.charAt(i)` then decide. A trailing
                // backslash (next char OOB) yields a lone literal backslash.
                match chars.get(i + 1).copied() {
                    Some(nc @ ('"' | '\\' | '$')) => cur.push(nc),
                    Some(nc) => {
                        cur.push('\\');
                        cur.push(nc);
                    }
                    None => cur.push('\\'),
                }
                started = true;
                i += 2;
                continue;
            }
            if c == '$' {
                cur.push_str(&parse_env_var(&chars, &mut i)?);
                started = true;
                i += 1;
                continue;
            }
            cur.push(c);
            started = true;
            i += 1;
            continue;
        }

        // Outside any quote.
        if c == '"' || c == '\'' {
            quote = Some(c);
            started = true;
            i += 1;
            continue;
        }
        if is_control_char(c) {
            // Operator token: split the word, drop the operator.
            flush_word(&mut out, &mut cur, &mut started, &mut glob);
            i += 1;
            continue;
        }
        if c == '#' {
            // Comment: emit the word-so-far (TS checks `out.length`, ignoring
            // the glob flag) and discard everything after.
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            return Ok(out);
        }
        if c == '\\' {
            esc = true;
            started = true;
            i += 1;
            continue;
        }
        if c == '$' {
            cur.push_str(&parse_env_var(&chars, &mut i)?);
            started = true;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            flush_word(&mut out, &mut cur, &mut started, &mut glob);
            i += 1;
            continue;
        }
        // Plain bareword character.
        cur.push(c);
        started = true;
        i += 1;
    }

    flush_word(&mut out, &mut cur, &mut started, &mut glob);
    Ok(out)
}

/// `shell-quote`'s `parseEnvVar`, specialised to `env = key => "$" + key`.
///
/// On entry `*i` indexes the `$`. The cursor mutations mirror the original
/// closure exactly (including the special-char branch's off-by-one over-consume
/// relative to the word-char branch), so that the caller's trailing `*i += 1`
/// resumes scanning at the same place shell-quote's `for`-loop would.
fn parse_env_var(chars: &[char], i: &mut usize) -> Result<String, ShellParseError> {
    let n = chars.len();
    *i += 1; // step past '$'
    let ch = chars.get(*i).copied();

    let varname: String = match ch {
        Some('{') => {
            *i += 1;
            if chars.get(*i).copied() == Some('}') {
                return Err(ShellParseError::BadSubstitution);
            }
            // varend = indexOf('}', i)
            let mut varend = None;
            let mut k = *i;
            while k < n {
                if chars[k] == '}' {
                    varend = Some(k);
                    break;
                }
                k += 1;
            }
            let Some(ve) = varend else {
                return Err(ShellParseError::BadSubstitution);
            };
            let name: String = chars[*i..ve].iter().collect();
            *i = ve; // sit on '}' (caller's +1 steps past it)
            name
        }
        Some(c) if is_special_var_char(c) => {
            *i += 1;
            c.to_string()
        }
        _ => {
            // slice from i; find the first non-`\w` char.
            let start = *i;
            let mut rel = None;
            let mut k = start;
            while k < n {
                if !is_word_char(chars[k]) {
                    rel = Some(k - start);
                    break;
                }
                k += 1;
            }
            match rel {
                None => {
                    let name: String = chars[start..n].iter().collect();
                    *i = n;
                    name
                }
                Some(r) => {
                    let name: String = chars[start..start + r].iter().collect();
                    // shell-quote: `i += varend.index - 1` (i currently == start).
                    *i = start + r - 1;
                    name
                }
            }
        }
    };

    Ok(env_value(&varname))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_parser_trims_the_complete_line_and_argument_tail() {
        let parsed = parse_slash_command(&Utf16JsonProjection::from(
            "\u{feff} \t/effort\u{2003}  low\u{00a0} ",
        ))
        .unwrap();
        assert_eq!(parsed.name.string_at(""), Some("effort"));
        assert_eq!(parsed.raw_args.string_at(""), Some("low"));
    }

    #[test]
    fn native_mcp_marker_becomes_a_command_name_suffix() {
        let parsed = parse_slash_command(&Utf16JsonProjection::from(
            "/github:review (MCP)\u{2003}owner",
        ))
        .unwrap();
        assert_eq!(parsed.name.string_at(""), Some("github:review (MCP)"));
        assert_eq!(parsed.raw_args.string_at(""), Some("owner"));

        let empty =
            parse_slash_command(&Utf16JsonProjection::from("/github:review (MCP)")).unwrap();
        assert_eq!(empty.raw_args.string_at(""), Some(""));
    }

    #[test]
    fn native_parser_rejects_empty_command_names() {
        for input in ["/", "/ ", "/\t", "/\u{2003}", "/ (MCP)"] {
            assert!(
                parse_slash_command(&Utf16JsonProjection::from(input)).is_none(),
                "expected empty command name to be a parse failure for {input:?}"
            );
        }
    }

    #[test]
    fn unicode_separators_use_ecmascript_whitespace_and_complete_utf8_characters() {
        for separator in ["\u{00a0}", "\u{2003}", "\u{feff}"] {
            let parsed =
                parse_slash_command(&Utf16JsonProjection::from(format!("/effort{separator}low")))
                    .unwrap();
            assert_eq!(parsed.name.string_at(""), Some("effort"));
            assert_eq!(parsed.raw_args.string_at(""), Some("low"));
        }
        let parsed =
            parse_slash_command(&Utf16JsonProjection::from("/effort \u{0085}low\u{0085}")).unwrap();
        assert_eq!(parsed.raw_args.string_at(""), Some("\u{0085}low\u{0085}"));
    }

    #[test]
    fn exact_source_name_and_argument_units_survive_tokenization() {
        let mut units = "/cmd".encode_utf16().collect::<Vec<_>>();
        units.push(0xd800);
        units.push(b' ' as u16);
        units.extend([
            b'a' as u16,
            0xdfff,
            b' ' as u16,
            b'"' as u16,
            b'b' as u16,
            0xd800,
            b'"' as u16,
        ]);
        let input =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units).unwrap();
        let parsed = parse_slash_command(&input).unwrap();
        assert_eq!(
            parsed.name.string_units(""),
            Some(vec![b'c' as u16, b'm' as u16, b'd' as u16, 0xd800])
        );
        assert_eq!(
            parsed.raw_args.string_units(""),
            Some(vec![
                b'a' as u16,
                0xdfff,
                b' ' as u16,
                b'"' as u16,
                b'b' as u16,
                0xd800,
                b'"' as u16
            ])
        );
        assert_eq!(
            parsed.positional_args[0].string_units(""),
            Some(vec![b'a' as u16, 0xdfff])
        );
        assert_eq!(
            parsed.positional_args[1].string_units(""),
            Some(vec![b'b' as u16, 0xd800])
        );
    }

    fn toks(s: &str) -> Vec<String> {
        tokenize_args(s)
    }

    #[test]
    fn parses_name_and_args() {
        let p = parse_slash_command(&Utf16JsonProjection::from("/memory add foo bar")).unwrap();
        assert_eq!(p.name.string_at(""), Some("memory"));
        assert_eq!(
            p.positional_args
                .iter()
                .map(|value| value.string_at(""))
                .collect::<Vec<_>>(),
            vec![Some("add"), Some("foo"), Some("bar")]
        );
        assert_eq!(p.raw_args.string_at(""), Some("add foo bar"));
    }

    #[test]
    fn handles_quoted_args() {
        let p =
            parse_slash_command(&Utf16JsonProjection::from("/skill run \"git commit\"")).unwrap();
        assert_eq!(
            p.positional_args
                .iter()
                .map(|value| value.string_at(""))
                .collect::<Vec<_>>(),
            vec![Some("run"), Some("git commit")]
        );
    }

    // ----- ARGS.1: shell-quote@1.8.1 parity vectors -----

    #[test]
    fn glob_word_is_dropped() {
        // `*.ts` is an unquoted glob -> dropped; `foo` survives.
        assert_eq!(toks("*.ts foo"), vec!["foo"]);
        // A lone glob word leaves nothing.
        assert_eq!(toks("src/*.rs"), Vec::<String>::new());
    }

    #[test]
    fn redirect_and_pipe_operators_are_dropped() {
        assert_eq!(toks("report > out.txt"), vec!["report", "out.txt"]);
        assert_eq!(toks("a | b"), vec!["a", "b"]);
        assert_eq!(toks("a; b"), vec!["a", "b"]);
        // Operators split words even without surrounding whitespace.
        assert_eq!(toks("a&b"), vec!["a", "b"]);
    }

    #[test]
    fn comment_truncates_remainder() {
        assert_eq!(toks("a # b"), vec!["a"]);
        // A mid-word `#` ends the token there.
        assert_eq!(toks("a#b"), vec!["a"]);
        // Leading `#` -> entire line is a comment.
        assert_eq!(toks("# nope"), Vec::<String>::new());
    }

    #[test]
    fn escaped_space_joins_one_token() {
        // Source string `a\ b` -> one token "a b".
        assert_eq!(toks("a\\ b"), vec!["a b"]);
    }

    #[test]
    fn adjacent_quoted_runs_concatenate() {
        assert_eq!(toks("foo\"bar\"baz"), vec!["foobarbaz"]);
        assert_eq!(toks("'a'b\"c\""), vec!["abc"]);
    }

    #[test]
    fn quoted_glob_is_kept() {
        // Quoted `*`/`?` are NOT globs -> the token survives verbatim.
        assert_eq!(toks("\"*.ts\" foo"), vec!["*.ts", "foo"]);
    }

    #[test]
    fn dollar_variable_syntax_preserved() {
        // env = key => "$" + key, so `$name` stays literal; `${name}` loses braces.
        assert_eq!(toks("$HOME path"), vec!["$HOME", "path"]);
        assert_eq!(toks("${HOME} path"), vec!["$HOME", "path"]);
    }

    #[test]
    fn bad_substitution_falls_back_to_whitespace_split() {
        // `${}` makes shell-quote throw -> parseArguments splits on whitespace.
        // Without the fallback, the glob/operator filtering would change these.
        assert_eq!(toks("a ${} *.ts"), vec!["a", "${}", "*.ts"]);
        assert_eq!(toks("x ${unclosed"), vec!["x", "${unclosed"]);
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(toks("").is_empty());
        assert!(toks("   ").is_empty());
    }
}
