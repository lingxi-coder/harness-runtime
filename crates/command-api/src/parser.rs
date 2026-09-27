//! Slash-command parser: splits raw input like `/memory add foo bar` into
//! a name and tokenized argument list, honouring quoted strings.

/// A parsed slash-command invocation.
#[derive(Debug, Clone)]
pub struct ParsedSlashCommand {
    /// Command name (without the leading `/`).
    pub name: String,
    /// The raw argument string after the command name.
    pub raw_args: String,
    /// Argument tokens after quote-aware splitting.
    pub positional_args: Vec<String>,
}

/// Parse a slash-command line.
///
/// Returns `None` if the input does not begin with `/`.
#[must_use]
pub fn parse_slash_command(input: &str) -> Option<ParsedSlashCommand> {
    let trimmed_input = input.strip_prefix('/')?;
    let (name, args_str) = match trimmed_input.find(char::is_whitespace) {
        Some(i) => (&trimmed_input[..i], trimmed_input[i + 1..].trim_start()),
        None => (trimmed_input, ""),
    };
    let positional = tokenize_args(args_str);
    Some(ParsedSlashCommand {
        name: name.to_string(),
        raw_args: args_str.to_string(),
        positional_args: positional,
    })
}

/// Tokenize an argument string the way the TS port does (ARGS.1).
///
/// Faithful to `claude-code/src/utils/argumentSubstitution.ts:parseArguments`,
/// which runs `shell-quote@1.8.1`'s `parse(args, key => "$" + key)` and then
/// keeps **only the string tokens** — operator (`| & ; ( ) < > …`), glob
/// (`*`/`?`), and comment (`#…`) entries become non-string `ParseEntry` objects
/// that are dropped. On a shell-quote parse error (a `${…}` "Bad substitution")
/// the TS code falls back to `args.split(/\s+/).filter(Boolean)`; we mirror that
/// with [`str::split_whitespace`].
///
/// Shared with [`crate::argument_substitution::parse_arguments`].
#[must_use]
pub(crate) fn tokenize_args(s: &str) -> Vec<String> {
    match shell_quote_parse(s) {
        Ok(tokens) => tokens,
        // TS: `tryParseShellCommand` failed -> `args.split(/\s+/).filter(Boolean)`.
        Err(ShellParseError::BadSubstitution) => s.split_whitespace().map(str::to_string).collect(),
    }
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

    fn toks(s: &str) -> Vec<String> {
        tokenize_args(s)
    }

    #[test]
    fn parses_name_and_args() {
        let p = parse_slash_command("/memory add foo bar").unwrap();
        assert_eq!(p.name, "memory");
        assert_eq!(p.positional_args, vec!["add", "foo", "bar"]);
        assert_eq!(p.raw_args, "add foo bar");
    }

    #[test]
    fn handles_quoted_args() {
        let p = parse_slash_command("/skill run \"git commit\"").unwrap();
        assert_eq!(p.positional_args, vec!["run", "git commit"]);
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
