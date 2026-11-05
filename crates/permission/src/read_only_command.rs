//! Bash read-only command inference — a faithful, conservative port of the CORE
//! of claude-code `src/tools/BashTool/readOnlyValidation.ts`
//! (`checkReadOnlyConstraints` / `isCommandReadOnly`) and its base allowlist
//! `READONLY_COMMANDS` + `READONLY_COMMAND_REGEXES`
//! (`utils/shell/readOnlyCommandValidation.ts`).
//!
//! # What this decides
//! `bashToolHasPermission` step 7 auto-ALLOWS a bash command when EVERY
//! subcommand is read-only (`BashTool.isReadOnly` → `checkReadOnlyConstraints`
//! → `splitCommand_DEPRECATED(command).every(isCommandReadOnly)`). This module
//! reproduces that "all subcommands read-only" inference so [`crate::policy`]
//! can wire the read-only allow layer in TS order (after the allow walk + the
//! sed/mode layers, before the generic ask fallthrough).
//!
//! # Native tables and verification boundary
//! The 2.1.318 `pVt` command families and GitHub CLI tables
//! are retained as inert data in `bundled/read_only_318_flags.json`. Their
//! getopt walk (`lXe`) rejects unknown flags, validates flag values and short
//! bundles, and runs the command's positional/format callback. In particular,
//! ripgrep's executable `--pre` flag cannot inherit a read-only grant.
//! `scripts/tests/read_only_318_oracle.mjs` verifies these 74 configurations
//! and their finite replay cases against the hash-pinned native functions.
//! The 22 GH configurations are inert in Native's current `c0o` dispatch;
//! their presence in the artifact does not grant a read-only allowance.
//!
//! Base commands and quote-aware operator/expansion rejection remain here;
//! the surrounding Bash parser remains outside that finite table replay.
//! Sed reuses the shared read-only expression validator; the oracle injects
//! an accepted grammar preflight for its simple token fixtures. The native
//! Anthropic-only `aki` search client has no local integration. Quoted operators remain literal
//! arguments and malformed quoting fails closed. A `false` result continues
//! through the ordinary permission checks rather than running the command.
//!
//! Per-subcommand splitting reuses [`crate::shell_command::split_command`] (the
//! crate's `splitCommand_DEPRECATED` analogue), matching the TS
//! `splitCommand_DEPRECATED(command).every(...)` structure.

/// Shell metacharacters that disqualify a candidate from the simple-command
/// read-only allowlist — the complement of `makeRegexForSafeCommand`'s trailing
/// character class (which excludes exactly these). A subcommand carrying ANY of
/// these (a redirection, a pipe, a brace/paren group, a background, a list, or a
/// newline) is NOT auto-allowed as read-only.
///
/// Quote state is handled by [`contains_active_metachar`]. The two
/// expansion metacharacters `$` and backtick are handled separately by
/// [`contains_unquoted_expansion`].
const READONLY_METACHARS: &[char] = &['<', '>', '(', ')', '|', '{', '}', '&', ';', '\n', '\r'];

/// Base (simple) commands that are read-only — 1:1 with the TS `READONLY_COMMANDS`
/// array (`readOnlyValidation.ts:1432-1499`, the `makeRegexForSafeCommand` set)
/// plus the `EXTERNAL_READONLY_COMMANDS` cross-platform pair folded in as their
/// own base words (`docker` is intentionally NOT a bare base word — only the
/// `docker ps`/`docker images` two-word forms are read-only, handled by
/// [`is_read_only_subcommand`]). Every entry here permits ONLY flag/path
/// arguments free of shell metacharacters.
const READONLY_BASE_COMMANDS: &[&str] = &[
    // Time and date
    "cal", "uptime", // File content viewing
    "cat", "head", "tail", "wc", "stat", "strings", "hexdump", "od", "nl", // System info
    "id", "uname", "free", "df", "du", "locale", "groups", "nproc",
    // Path information
    "basename", "dirname", "realpath", "readlink", // Text processing
    "cut", "paste", "tr", "column", "tac", "rev", "fold", "expand", "unexpand", "fmt", "comm",
    "cmp", "numfmt", // File comparison
    "diff",   // true / false
    "true", "false",
    // Misc. safe commands. (Binary `vho` = `…,"expr","seq","tsort","pr"` — NO
    // `test`/`getconf`; the port previously over-allowed those two read-only.)
    "sleep", "which", "type", "expr", "seq", "tsort", "pr",
    // Hand-written read-only regex commands whose simple forms reduce to a
    // base-word + metachar-free-args shape (`ls`, `find`, `cd`).
    // `pwd`/`whoami`/`alias`/`arch`/`date` are NOT base words — they stay in
    // the exact-match set `OPg` / the `arch` regex (bare / `-h` / `--help` only),
    // gated in `is_read_only_subcommand`. `echo`/`grep`/`rg`/`jq`/`uniq`/
    // `history` are handled with their TS-specific guards there too.
    "ls", "find", "cd",
];

/// `find` primary actions that WRITE / execute / side-effect — binary `vDp`
/// reject set. A `find` invocation carrying any of these is NOT read-only; most
/// notably `-delete` has no shell metacharacter to trip the generic guard, so
/// without this it would be wrongly auto-allowed.
const FIND_DANGEROUS_ACTIONS: &[&str] = &[
    "-delete",
    "-rm",
    "-exec",
    "-execdir",
    "-ok",
    "-okdir",
    "-fprint",
    "-fprint0",
    "-fls",
    "-fprintf",
    "-files0-from",
];

/// Docker flags that can redirect a nominally read-only `ps`/`images` command to
/// a remote daemon or alternate control plane. Reject them in the read-only
/// classifier so `docker ps --host tcp://…` cannot auto-allow.
const DOCKER_REMOTE_CONTROL_FLAGS: &[&str] = &[
    "--url",
    "--connection",
    "--identity",
    "--context",
    "--host",
    "-H",
    "-c",
    "-r",
    "--config",
    "--tlscacert",
    "--tlscert",
    "--tlskey",
    "--remote",
    "--module",
    "--out",
];

const READONLY_EXACT_COMMANDS: &[&str] = &[
    "date",
    "ip addr",
    "claude -h",
    "claude --help",
    "node -v",
    "node --version",
    "python --version",
    "python3 --version",
];

/// Quote-aware scan for an ACTIVE `$` expansion or backtick command
/// substitution, mirroring TS `containsUnquotedExpansion`
/// (`readOnlyValidation.ts:1600`) for `$`, and the backtick arm of
/// `validateDangerousPatterns` (`bashSecurity.ts:853`,
/// `hasUnescapedChar(withDoubleQuotes, backtick)`) for backtick.
///
/// Per bash semantics, `$` and backtick are LITERAL only inside SINGLE quotes;
/// they are ACTIVE (an expansion / command substitution → NOT read-only) when
/// UNQUOTED or inside DOUBLE quotes. The other shell metacharacters stay
/// quote-naive in [`READONLY_METACHARS`]; only these two are quote-aware.
///
/// Walks the (sub)command char by char tracking single/double-quote state,
/// matching the TS tracker exactly:
/// - A backslash escapes the next char ONLY outside single quotes (inside
///   `'...'`, `\` is literal — bash does not escape there).
/// - A single quote toggles single-quote state only when not inside a double
///   quote; a double quote toggles double-quote state only when not inside a
///   single quote.
/// - Inside single quotes everything is literal → skipped.
/// - An unescaped `$` followed by a variable/special-parameter char
///   `[A-Za-z_@*#?!$0-9-]` (TS `containsUnquotedExpansion:1651`) flags an
///   expansion. (`${`/`$(` are caught by the brace/paren metachars instead.)
/// - An unescaped backtick (anywhere not single-quoted) flags command
///   substitution.
///
/// Returns `true` when such an active expansion/substitution is present → the
/// subcommand is NOT read-only.
fn contains_unquoted_expansion(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // Skip the char following an escape (escape only set outside single
        // quotes — see the `\\` arm below).
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }

        // Backslash escapes the next char ONLY outside single quotes (TS
        // `containsUnquotedExpansion:1626`). Inside `'...'`, `\` is literal.
        if c == '\\' && !in_single_quote {
            escaped = true;
            i += 1;
            continue;
        }

        // Quote-state toggles (TS lines 1632-1640).
        if c == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            i += 1;
            continue;
        }
        if c == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            i += 1;
            continue;
        }

        // Inside single quotes everything is literal → skip (TS lines 1643-1645).
        if in_single_quote {
            i += 1;
            continue;
        }

        // Active `$` expansion: `$` followed by a variable/special-parameter
        // char. Expands inside double quotes AND unquoted (TS lines 1649-1654).
        if c == '$' {
            if let Some(&next) = chars.get(i + 1) {
                if next.is_ascii_alphanumeric()
                    || matches!(next, '_' | '@' | '*' | '#' | '?' | '!' | '$' | '-')
                {
                    return true;
                }
            }
        }

        // Active backtick command substitution: any unescaped backtick that is
        // not single-quoted (TS `bashSecurity.ts:853`, checked against
        // `withDoubleQuotes` which excludes only single-quoted content → a
        // double-quoted backtick still substitutes).
        if c == '`' {
            return true;
        }

        i += 1;
    }

    false
}

/// Reject shell operators only when they are active in the shell grammar.
/// Quoted separators are ordinary argument bytes, while an unmatched quote is
/// never safe enough to auto-allow. Unquoted globs have a separate command
/// allowlist below: expansion can inject write/execute flags into other tools.
fn contains_active_metachar(command: &str) -> bool {
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    for c in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && !in_single_quote {
            escaped = true;
            continue;
        }
        if c == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            continue;
        }
        if c == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            continue;
        }
        if in_single_quote || in_double_quote {
            continue;
        }
        if READONLY_METACHARS.contains(&c) {
            return true;
        }
    }

    escaped || in_single_quote || in_double_quote
}

/// Is `command` read-only? — the public entry, mirroring TS
/// `BashTool.isReadOnly` → `checkReadOnlyConstraints` →
/// `splitCommand_DEPRECATED(command).every(isCommandReadOnly)`: the WHOLE
/// command is read-only iff EVERY subcommand is. An empty command is NOT
/// read-only (nothing to allow).
///
/// This is the conservative inference [`crate::policy`] consults for the
/// read-only auto-allow layer (TS `bashToolHasPermission` step 7). It never
/// returns `true` for a command bearing a redirection / substitution / a
/// non-allowlisted base word (see the module docs' safe-direction note).
#[must_use]
pub fn command_is_read_only(command: &str) -> bool {
    // PARITY Native 2.1.291 `l9n`: `if (command.length > uM)` returns
    // `passthrough` with `Command too long for read-only analysis`, where
    // `uM = 1e4` and JavaScript `String.length` counts UTF-16 code units.
    // Over the threshold the oracle refuses to CLASSIFY the command at all, so
    // it can never be auto-allowed as read-only. Without this the port would
    // happily walk a 10k+ command and allow it, which is looser than the binary
    // — and the overlength ask in `policy.rs` is a LingXi-only backstop that
    // must not be the only thing standing between a giant command and an
    // auto-allow.
    if command.encode_utf16().count() > 10_000 {
        return false;
    }
    let subs = crate::shell_command::split_command(command);
    if subs.is_empty() {
        return false;
    }
    let has_glob = contains_unquoted_glob(command);
    subs.iter()
        .all(|sub| is_read_only_subcommand(sub, has_glob))
}

/// Native `x2e` distinguishes active globs from quoted/escaped literals.
fn contains_unquoted_glob(command: &str) -> bool {
    let mut single = false;
    let mut double = false;
    let mut backtick = false;
    let mut escaped = false;
    let mut bracket = false;
    let mut word_start = true;
    let mut comment = false;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        if comment {
            if ch == '\n' {
                comment = false;
                word_start = true;
            }
            continue;
        }
        if escaped {
            escaped = false;
            word_start = false;
            continue;
        }
        if ch == '\\' && !single {
            if chars.peek() == Some(&'\n') {
                chars.next();
            } else {
                escaped = true;
            }
            continue;
        }
        if backtick {
            if ch == '`' {
                backtick = false;
                word_start = false;
            }
            continue;
        }
        if ch == '`' && !single {
            backtick = true;
            word_start = false;
            continue;
        }
        if ch == '#' && word_start && !single && !double {
            comment = true;
            continue;
        }
        if ch == '\'' && !double {
            single = !single;
            word_start = false;
            continue;
        }
        if ch == '"' && !single {
            double = !double;
            word_start = false;
            continue;
        }
        if single || double {
            continue;
        }
        if matches!(
            ch,
            ' ' | '\t' | '\n' | '|' | '&' | ';' | '(' | ')' | '<' | '>'
        ) {
            bracket = false;
            word_start = true;
            continue;
        }
        word_start = false;
        if matches!(ch, '*' | '?') || (ch == ']' && bracket) {
            return true;
        }
        if ch == '[' {
            bracket = true;
        }
    }
    false
}

/// Is a SINGLE subcommand read-only? — the per-subcommand arm of TS
/// `isCommandReadOnly` (`readOnlyValidation.ts:1678`), in its conservative core.
fn is_read_only_subcommand(sub: &str, command_has_glob: bool) -> bool {
    // TS strips a trailing ` 2>&1` before matching (`isCommandReadOnly:1683`).
    let mut test = sub.trim();
    if let Some(stripped) = test.strip_suffix("2>&1") {
        test = stripped.trim_end();
    }
    if test.is_empty() {
        return false;
    }
    // Operators disqualify only when active. Quoted separators are literal
    // argument bytes.
    if contains_active_metachar(test) {
        return false;
    }
    // Quote-aware `$`/backtick guard: an ACTIVE expansion or command
    // substitution disqualifies (TS `containsUnquotedExpansion` for `$`; the
    // backtick arm of `validateDangerousPatterns`). Both are literal — and so
    // remain read-only — inside SINGLE quotes; active when unquoted or inside
    // double quotes. (A `$`-bearing token can expand to an arbitrary flag at
    // runtime, so TS refuses it too.)
    if contains_unquoted_expansion(test) {
        return false;
    }
    if READONLY_EXACT_COMMANDS.contains(&test) {
        return true;
    }
    let Some(tokens) = read_only_shell_words(test) else {
        return false;
    };
    let mut words = tokens.iter().map(String::as_str);
    let Some(base) = words.next() else {
        return false;
    };
    // Native `vat` checks this before getopt/table dispatch. A filename such
    // as `--output=.env` must not turn `sort *` into an approved writer.
    if contains_unquoted_glob(test)
        || (command_has_glob
            && tokens.iter().any(|word| {
                word.contains(['*', '?'])
                    || word
                        .find('[')
                        .is_some_and(|start| word[start..].contains(']'))
            }))
    {
        return [
            "ls",
            "cat",
            "head",
            "tail",
            "wc",
            "stat",
            "grep",
            "egrep",
            "fgrep",
            "diff",
            "du",
            "df",
            "echo",
            "strings",
            "hexdump",
            "od",
            "nl",
            "cut",
            "column",
            "tr",
            "tac",
            "rev",
            "cmp",
            "basename",
            "dirname",
            "realpath",
            "readlink",
            "sha256sum",
            "sha1sum",
            "md5sum",
            "cd",
        ]
        .contains(&base);
    }
    // Native table configurations have no regex exception for backticks,
    // including when a shell quote makes the character literal.
    if (matches!(base, "git" | "gh")
        || (base == "docker"
            && tokens
                .get(1)
                .is_some_and(|word| matches!(word.as_str(), "logs" | "inspect"))))
        && test.contains('`')
    {
        return false;
    }
    // `docker ps` / `docker images` — the only read-only `docker` forms
    // (`EXTERNAL_READONLY_COMMANDS`). A bare `docker` or any other subcommand is
    // NOT read-only.
    if base == "docker" {
        let rest: Vec<&str> = words.collect();
        return docker_subcommand_is_read_only(&rest);
    }
    // `git <subcommand>` — the read-only slice of TS `GIT_READ_ONLY_COMMANDS`
    // (`utils/shell/readOnlyCommandValidation.ts:107-923`). Delegates to
    // [`git_subcommand_is_read_only`], which recognises the read-only git
    // subcommand allowlist and applies the positional-write guard callbacks
    // (`git branch`/`git tag`/`git reflog`/`git remote`). A bare `git` or any
    // write subcommand (`commit`/`push`/`add`/`checkout`/…) is NOT read-only.
    if base == "git" {
        let rest: Vec<&str> = words.collect();
        return git_subcommand_is_read_only(&rest);
    }
    if base == "gh" {
        // The artifact retains a separate GH table, but Native c0o selects
        // only pVt. GH is absent from that active read-only dispatch.
        return false;
    }
    if base == "jq" {
        let rest: Vec<&str> = words.collect();
        return jq_is_read_only(&rest);
    }
    if read_only_flag_configs().contains_key(base) {
        if test.contains('`')
            || (matches!(base, "rg" | "grep" | "egrep" | "fgrep") && test.contains(['\n', '\r']))
        {
            return false;
        }
        if base == "hostname" {
            static HOSTNAME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            if !HOSTNAME
                .get_or_init(|| {
                    regex::Regex::new(r"^hostname(?:\s+(?:-[a-zA-Z]|--[a-zA-Z-]+))*\s*$")
                        .expect("static hostname pattern")
                })
                .is_match(test)
            {
                return false;
            }
        }
        let words = tokens.iter().map(String::as_str).collect::<Vec<_>>();
        return other_read_only_command(&words);
    }
    // `echo` with metacharacter-free args is read-only (the TS echo regex; the
    // metachar guard above already rejected the dangerous `` ` ``/`$`/`<>` forms
    // the TS regex excludes).
    if base == "echo" {
        return true;
    }
    // `history` — bare or with a numeric argument only (TS
    // `/^history(?:\s+\d+)?\s*$/`).
    if base == "history" {
        return match words.next() {
            None => true,
            Some(arg) => words.next().is_none() && arg.bytes().all(|b| b.is_ascii_digit()),
        };
    }
    // `find` is read-only ONLY when it carries no destructive/side-effecting
    // action (binary `vDp`). The generic metachar guard catches `-exec … {} \;`
    // forms (via `{`/`}`/`;`), but `-delete` and bare `-fls`/`-fprint*` slip
    // through metachar-free, so reject them explicitly.
    if base == "find" {
        return !words.any(|w| FIND_DANGEROUS_ACTIONS.contains(&w));
    }
    // `pwd`/`whoami`/`alias` — 2.1.211 exact-match-only set `OPg`: read-only
    // ONLY as a single bare token (`pwd -L`, `alias k=v` ask).
    if matches!(base, "pwd" | "whoami" | "alias") {
        return words.next().is_none();
    }
    // `arch` — 2.1.211 `/^arch(?:\s+(?:--help|-h))?\s*$/`: bare, or a lone
    // `-h`/`--help`.
    if base == "arch" {
        return match words.next() {
            None => true,
            Some(a) => words.next().is_none() && matches!(a, "-h" | "--help"),
        };
    }
    READONLY_BASE_COMMANDS.contains(&base)
}

#[cfg(test)]
fn gh_is_read_only(args: &[&str]) -> bool {
    let words = std::iter::once("gh")
        .chain(args.iter().copied())
        .collect::<Vec<_>>();
    let Some((name, count)) = validate_read_only_flags(&words) else {
        return false;
    };
    name.starts_with("gh search ") || !gh_argument_is_dangerous(&words[count..])
}

fn other_read_only_command(words: &[&str]) -> bool {
    let Some((name, count)) = validate_read_only_flags(words) else {
        return false;
    };
    let args = &words[count..];
    match name {
        "sed" => {
            // Read-only sed validation does not consult these roots: the
            // shared validator's allow_file_writes=false branch only reads
            // the command grammar and expression allowlist.
            let command = "sed ".to_owned()
                + &args
                    .iter()
                    .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
                    .collect::<Vec<_>>()
                    .join(" ");
            let roots = crate::filesystem::FsRoots {
                cwd: "/".into(),
                home: None,
                lingxi_home: "/".into(),
            };
            crate::sed_validation::sed_constraint_verdict(&command, false, &roots, &[])
                == crate::sed_validation::SedVerdict::Safe
        }
        "help" => !args
            .iter()
            .any(|arg| arg.contains(['~', '/', '\\']) || has_ambiguous_shell_value(arg)),
        "hostname" => args.iter().all(|arg| {
            arg.strip_prefix("--").is_some_and(|flag| {
                !flag.is_empty() && flag.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
            }) || (arg.len() == 2
                && arg.starts_with('-')
                && arg.as_bytes()[1].is_ascii_alphabetic())
        }),
        "date" => {
            let mut index = 0;
            while index < args.len() {
                let arg = args[index];
                if arg.starts_with("--") && arg.contains('=') {
                    index += 1;
                } else if arg.starts_with('-') {
                    index += if ["-d", "--date", "-r", "--reference", "--rfc-3339"].contains(&arg) {
                        2
                    } else {
                        1
                    };
                } else {
                    if !arg.starts_with('+') {
                        return false;
                    }
                    index += 1;
                }
            }
            true
        }
        "ps" => {
            let has_environment_positional = args
                .iter()
                .any(|arg| !native_flag_spelling(arg) && arg.contains(['e', 'E']));
            let has_environment_flag = args.iter().any(|arg| {
                arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg[1..]
                        .chars()
                        .take_while(char::is_ascii_alphabetic)
                        .any(|c| c == 'e')
            });
            let basic_flags_only = args.iter().all(|arg| {
                *arg == "--forest"
                    || arg.strip_prefix('-').is_some_and(|flag| {
                        !flag.is_empty() && flag.chars().all(|c| "AacdeFfjlwHLTm".contains(c))
                    })
            });
            let trees = args
                .iter()
                .map(|arg| {
                    if *arg == "--forest" {
                        1
                    } else {
                        arg.chars().filter(|c| "HLTm".contains(*c)).count()
                    }
                })
                .sum::<usize>();
            !has_environment_positional
                && (!has_environment_flag || (basic_flags_only && trees <= 1))
        }
        "man" => man_arguments_are_read_only(args),
        "lsof" => lsof_arguments_are_read_only(args),
        "tput" => {
            let blocked = [
                "init", "reset", "rs1", "rs2", "rs3", "is1", "is2", "is3", "iprog", "if", "rf",
                "clear", "flash", "mc0", "mc4", "mc5", "mc5i", "mc5p", "pfkey", "pfloc", "pfx",
                "pfxl", "smcup", "rmcup",
            ];
            let mut index = 0;
            let mut after_dash = false;
            while index < args.len() {
                let arg = args[index];
                if arg == "--" {
                    after_dash = true;
                    index += 1;
                } else if !after_dash && arg.starts_with('-') {
                    if arg == "-S" || (!arg.starts_with("--") && arg.len() > 2 && arg.contains('S'))
                    {
                        return false;
                    }
                    index += if arg == "-T" { 2 } else { 1 };
                } else {
                    if blocked.contains(&arg) {
                        return false;
                    }
                    index += 1;
                }
            }
            true
        }
        "ss" => ss_arguments_are_read_only(args),
        "test" => {
            if args
                .iter()
                .any(|arg| ["-v", "-R", "-a", "-o"].contains(arg) || arg.contains('['))
            {
                return false;
            }
            for (index, arg) in args.iter().enumerate() {
                if ["-eq", "-ne", "-lt", "-le", "-gt", "-ge"].contains(arg) {
                    for operand in [
                        index.checked_sub(1).and_then(|i| args.get(i)),
                        args.get(index + 1),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        if !shell_test_integer(operand) {
                            return false;
                        }
                    }
                }
                if *arg == "-t"
                    && args
                        .get(index + 1)
                        .is_some_and(|arg| !shell_test_integer(arg))
                {
                    return false;
                }
            }
            true
        }
        _ => true,
    }
}

fn native_flag_spelling(arg: &str) -> bool {
    arg.starts_with('-')
        && arg.len() > 1
        && arg.as_bytes()[1].is_ascii()
        && (arg.as_bytes()[1].is_ascii_alphanumeric() || matches!(arg.as_bytes()[1], b'_' | b'-'))
}

fn shell_test_integer(arg: &str) -> bool {
    static INTEGER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    INTEGER
        .get_or_init(|| {
            regex::Regex::new(r"^-?(0[xX][0-9a-fA-F]+|[0-9]+#[0-9a-zA-Z]+|[0-9]+)$")
                .expect("static shell integer pattern")
        })
        .is_match(arg)
}

fn man_arguments_are_read_only(args: &[&str]) -> bool {
    let sections = ["-S", "-s"];
    let lookup_flags = ["-k", "-f", "--apropos", "--whatis"];
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        if arg == "--" || arg == "-" || !arg.starts_with('-') {
            break;
        }
        let section = if sections.contains(&arg) {
            index += 1;
            Some(*args.get(index).unwrap_or(&""))
        } else if arg.starts_with("-S=") || arg.starts_with("-s=") {
            Some(&arg[2..])
        } else {
            None
        };
        if section.is_some_and(|value| {
            value.is_empty()
                || !value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | ','))
        }) {
            return false;
        }
        index += 1;
    }
    let lookup_mode = args.iter().take_while(|arg| **arg != "--").any(|arg| {
        if arg.starts_with("--") {
            lookup_flags.contains(arg)
        } else {
            arg.starts_with('-') && arg[1..].contains(['k', 'f'])
        }
    });
    let mut lookup_seen = false;
    let mut after_dash = false;
    index = 0;
    while index < args.len() {
        let arg = args[index];
        if !after_dash && arg == "--" {
            after_dash = true;
            index += 1;
            continue;
        }
        if !(after_dash || !arg.starts_with('-') || arg == "-") {
            if lookup_flags.contains(&arg) {
                lookup_seen = true;
            } else if sections.contains(&arg) {
                index += 1;
            }
            index += 1;
            continue;
        }
        after_dash = true;
        if has_ambiguous_shell_value(arg)
            || (lookup_mode && arg.starts_with('-'))
            || (!lookup_seen && arg.contains(['/', '\\', '~']))
        {
            return false;
        }
        index += 1;
    }
    true
}

fn lsof_arguments_are_read_only(args: &[&str]) -> bool {
    static ATTACHED: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static FLAG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let attached = ATTACHED.get_or_init(|| {
        regex::Regex::new(r"^-[a-zA-Z]*i\S*@").expect("static lsof attached pattern")
    });
    let flag =
        FLAG.get_or_init(|| regex::Regex::new(r"^-[a-zA-Z]*i$").expect("static lsof flag pattern"));
    args.iter().enumerate().all(|(index, arg)| {
        if arg.starts_with("+m") {
            return false;
        }
        if attached.is_match(arg)
            && arg.split_once('@').is_some_and(|(_, host)| {
                host.split(':')
                    .next()
                    .unwrap_or("")
                    .chars()
                    .any(|c| c.is_ascii_alphabetic())
            })
        {
            return false;
        }
        if flag.is_match(arg) {
            let next = args.get(index + 1).copied().unwrap_or("");
            if next.contains('~')
                || next.split_once('@').is_some_and(|(_, host)| {
                    host.split(':')
                        .next()
                        .unwrap_or("")
                        .chars()
                        .any(|c| c.is_ascii_alphabetic())
                })
            {
                return false;
            }
        }
        true
    })
}

fn ss_arguments_are_read_only(args: &[&str]) -> bool {
    let mut operands = Vec::new();
    let mut after_dash = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        if !after_dash && arg == "--" {
            after_dash = true;
            index += 1;
            continue;
        }
        if !after_dash && arg.starts_with('-') {
            if ["-f", "--family", "-A", "--query", "--socket"].contains(&arg) {
                index += 1;
            }
        } else {
            operands.push(arg);
        }
        index += 1;
    }
    if operands.iter().any(|arg| arg.contains('~')) {
        return false;
    }
    let text = operands.join(" ");
    let tokens = text
        .split(|c: char| c.is_whitespace() || "()=!<>&|,".contains(c))
        .filter(|part| !part.is_empty());
    let mut skip = false;
    for token in tokens {
        if skip {
            skip = false;
            continue;
        }
        if [
            "dst",
            "src",
            "dport",
            "sport",
            "and",
            "or",
            "not",
            "eq",
            "ne",
            "ge",
            "le",
            "gt",
            "lt",
            "autobound",
            "state",
            "exclude",
            "dev",
            "fwmark",
            "cgroup",
        ]
        .contains(&token)
        {
            skip = [
                "state", "exclude", "dport", "sport", "dev", "fwmark", "cgroup",
            ]
            .contains(&token);
            continue;
        }
        if token
            .chars()
            .any(|c| c.is_ascii_alphabetic() && !"abcdefABCDEF".contains(c))
            || (token.chars().any(|c| "abcdefABCDEF".contains(c))
                && (token.contains('.') || !token.contains(':')))
        {
            return false;
        }
    }
    true
}

#[derive(serde::Deserialize)]
struct ReadOnlyFlagConfig {
    #[serde(rename = "safeFlags")]
    flags: std::collections::HashMap<String, String>,
    #[serde(rename = "respectsDoubleDash", default)]
    respects_double_dash: Option<bool>,
}

fn read_only_flag_configs() -> &'static std::collections::HashMap<String, ReadOnlyFlagConfig> {
    static CONFIGS: std::sync::OnceLock<std::collections::HashMap<String, ReadOnlyFlagConfig>> =
        std::sync::OnceLock::new();
    CONFIGS.get_or_init(|| {
        serde_json::from_str(include_str!("bundled/read_only_318_flags.json"))
            .expect("the hash-pinned read-only flag table is valid")
    })
}

/// Native `lXe` getopt walk, including numeric short-form arguments and
/// no-argument short bundles. Unknown flags never inherit a command's grant.
fn validate_read_only_flags(words: &[&str]) -> Option<(&'static str, usize)> {
    if cfg!(target_os = "windows") && words.first() == Some(&"xargs") {
        return None;
    }
    let (name, config) = read_only_flag_configs()
        .iter()
        .filter(|(name, _)| {
            let prefix = name.split(' ').collect::<Vec<_>>();
            words.starts_with(&prefix)
        })
        .max_by_key(|(name, _)| name.split(' ').count())?;
    let count = name.split(' ').count();
    if words[count..].iter().any(|word| {
        word.contains('$') || (word.contains('{') && (word.contains(',') || word.contains("..")))
    }) {
        return None;
    }
    let command = *words.first()?;
    if cfg!(target_os = "windows")
        && command == "git"
        && words[count..].iter().any(|word| {
            has_ambiguous_shell_value(word)
                || word.as_bytes().windows(3).any(|part| {
                    part[0] == b':'
                        && matches!(part[1], b'/' | b'\\')
                        && matches!(part[2], b'/' | b'\\')
                })
        })
    {
        return None;
    }
    let mut index = count;
    while index < words.len() {
        let word = words[index];
        if word.is_empty() {
            index += 1;
            continue;
        }
        if command == "xargs" && (!word.starts_with('-') || word == "--") {
            let target = if word == "--" {
                *words.get(index + 1)?
            } else {
                word
            };
            return [
                "echo", "printf", "wc", "grep", "egrep", "fgrep", "head", "tail",
            ]
            .contains(&target)
            .then_some((name.as_str(), count));
        }
        if word == "--" {
            if config.respects_double_dash != Some(false) {
                break;
            }
            index += 1;
            continue;
        }
        if word.starts_with('-') && word.len() > 1 {
            let (flag, attached) = word
                .split_once('=')
                .map_or((word, None), |(a, b)| (a, Some(b)));
            let Some(kind) = config.flags.get(flag).map(String::as_str) else {
                if command == "git" && flag.strip_prefix('-').is_some_and(ascii_digits) {
                    index += 1;
                    continue;
                }
                if matches!(command, "rg" | "grep" | "egrep" | "fgrep")
                    && flag.starts_with('-')
                    && !flag.starts_with("--")
                    && flag.len() > 2
                    && flag.is_char_boundary(2)
                {
                    let (short, value) = flag.split_at(2);
                    if ascii_digits(value)
                        && config
                            .flags
                            .get(short)
                            .is_some_and(|kind| matches!(kind.as_str(), "number" | "string"))
                    {
                        index += 1;
                        continue;
                    }
                }
                if flag.starts_with("--")
                    || flag.len() <= 2
                    || !flag[1..].chars().all(|short| {
                        config
                            .flags
                            .get(&format!("-{short}"))
                            .is_some_and(|kind| kind == "none")
                    })
                {
                    return None;
                }
                index += 1;
                continue;
            };
            if kind == "none" {
                if attached.is_some() {
                    return None;
                }
                index += 1;
                continue;
            }
            let value = if let Some(value) = attached {
                index += 1;
                value
            } else {
                let value = *words.get(index + 1)?;
                if value.starts_with('-')
                    && value.len() > 1
                    && value
                        .as_bytes()
                        .get(1)
                        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                {
                    return None;
                }
                index += 2;
                value
            };
            if ambiguous_shell_value_starts(value)
                || (kind == "string"
                    && value.starts_with('-')
                    && !(command == "git"
                        && flag == "--sort"
                        && value.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic)))
                || !match kind {
                    "number" => ascii_digits(value),
                    "string" => true,
                    "char" => value.encode_utf16().count() == 1,
                    "{}" => value == "{}",
                    "EOF" => value == "EOF",
                    _ => false,
                }
            {
                return None;
            }
        } else {
            if has_ambiguous_shell_value(word) {
                return None;
            }
            index += 1;
        }
    }
    Some((name.as_str(), count))
}

fn ascii_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

fn has_ambiguous_shell_value(value: &str) -> bool {
    ["__CMDSUB_OUTPUT__", "__TRACKED_VAR__", "__UNCERTAIN_VAR__"]
        .iter()
        .any(|marker| value.contains(marker))
}

fn ambiguous_shell_value_starts(value: &str) -> bool {
    ["__CMDSUB_OUTPUT__", "__TRACKED_VAR__", "__UNCERTAIN_VAR__"]
        .iter()
        .any(|marker| value.starts_with(marker))
}

fn has_tilde_expansion(value: &str) -> bool {
    value.starts_with('~')
        || value.contains("=~")
        || value.contains(":~")
        || (value.contains('~') && value.contains('{'))
}

/// Native `je`: view/status commands permit owner/repo names, but not URLs,
/// alternate identity syntax, or paths with two separators.
#[cfg(test)]
fn gh_argument_is_dangerous(args: &[&str]) -> bool {
    args.iter().any(|arg| {
        if arg.contains('~') {
            return true;
        }
        let value = if arg.starts_with('-') {
            let Some((_, value)) = arg.split_once('=') else {
                return false;
            };
            value
        } else {
            arg
        };
        has_ambiguous_shell_value(value)
            || value.contains("://")
            || value.contains('@')
            || value.chars().filter(|c| *c == '/').count() >= 2
    })
}

/// Native `Ft` rejects Git formatting that can request signature verification
/// or defer a format/ordering expression until execution.
fn git_format_is_dangerous(args: &[&str]) -> bool {
    static SIGNATURE_FORMAT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let signature_format = SIGNATURE_FORMAT.get_or_init(|| {
        regex::Regex::new(r"%[-+ ]?G|%\(\*?signature").expect("static signature pattern")
    });
    args.iter().enumerate().any(|(index, arg)| {
        if signature_format.is_match(arg) {
            return true;
        }
        for flag in ["--format", "--pretty", "--sort", "--group"] {
            let value = if *arg == flag {
                args.get(index + 1).copied()
            } else {
                arg.strip_prefix(&format!("{flag}="))
            };
            if let Some(value) = value {
                if has_tilde_expansion(value)
                    || has_ambiguous_shell_value(value)
                    || signature_format.is_match(value)
                    || value.contains("signature")
                    || (matches!(flag, "--format" | "--pretty")
                        && !value.is_empty()
                        && !value.contains('%')
                        && !value.starts_with("format:")
                        && !value.starts_with("tformat:")
                        && ![
                            "oneline", "short", "medium", "full", "fuller", "email", "raw",
                        ]
                        .contains(&value))
                {
                    return true;
                }
            }
        }
        false
    })
}

/// Shell-word projection for flag validation. Operators and expansions are
/// rejected before this walk; quote delimiters are not part of an argument.
fn read_only_shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut single = false;
    let mut double = false;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !double => {
                single = !single;
                started = true;
            }
            '"' if !single => {
                double = !double;
                started = true;
            }
            '\\' if !single => {
                let next = chars.next()?;
                if double && !matches!(next, '$' | '`' | '"' | '\\' | '\n') {
                    word.push('\\');
                }
                if next != '\n' {
                    word.push(next);
                }
                started = true;
            }
            '#' if !single && !double && !started => break,
            ch if ch.is_whitespace() && !single && !double => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(ch);
                started = true;
            }
        }
    }
    if single || double {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

fn jq_is_read_only(args: &[&str]) -> bool {
    !args.iter().any(|arg| {
        matches!(
            *arg,
            "-f" | "--from-file" | "-L" | "--library-path" | "--argfile" | "--slurpfile"
        ) || arg.starts_with("--from-file=")
            || arg.starts_with("--library-path=")
            || arg.starts_with("--argfile=")
            || arg.starts_with("--slurpfile=")
            || arg.contains("import ")
            || arg.contains("include ")
    })
}

fn docker_subcommand_is_read_only(rest: &[&str]) -> bool {
    let Some(&subcommand) = rest.first() else {
        return false;
    };
    if !matches!(subcommand, "ps" | "images" | "logs" | "inspect") {
        return false;
    }
    if docker_has_remote_control_flag(rest)
        || rest
            .iter()
            .any(|arg| has_ambiguous_shell_value(arg) || has_tilde_expansion(arg))
    {
        return false;
    }
    if matches!(subcommand, "ps" | "images") {
        return true;
    }
    let words = std::iter::once("docker")
        .chain(rest.iter().copied())
        .collect::<Vec<_>>();
    validate_read_only_flags(&words).is_some()
}

fn docker_has_remote_control_flag(args: &[&str]) -> bool {
    args.iter().any(|arg| {
        DOCKER_REMOTE_CONTROL_FLAGS.iter().any(|flag| {
            *arg == *flag
                || arg.starts_with(&format!("{flag}="))
                || (flag.len() == 2 && arg.len() > 2 && arg.starts_with(flag))
        }) || arg
            .strip_prefix('-')
            .filter(|rest| !rest.starts_with('-'))
            .map(|rest| {
                rest.chars()
                    .take_while(char::is_ascii_alphabetic)
                    .collect::<String>()
            })
            .is_some_and(|bundle| {
                bundle.len() >= 2 && bundle.chars().any(|flag| matches!(flag, 'H' | 'c' | 'r'))
            })
    })
}

/// Native 2.1.318 `r0n` table and its positional/format callbacks.
fn git_subcommand_is_read_only(rest: &[&str]) -> bool {
    let words = std::iter::once("git")
        .chain(rest.iter().copied())
        .collect::<Vec<_>>();
    let Some((name, count)) = validate_read_only_flags(&words) else {
        return false;
    };
    let args = &words[count..];
    match name {
        "git log" | "git show" | "git shortlog" | "git rev-list" | "git for-each-ref" => {
            !git_format_is_dangerous(args)
        }
        "git tag" => !git_format_is_dangerous(args) && !git_tag_is_dangerous(args),
        "git branch" => !git_format_is_dangerous(args) && !git_branch_is_dangerous(args),
        "git reflog" => !git_reflog_is_dangerous(args),
        "git remote" => !git_remote_is_dangerous(args),
        "git remote show" => !git_remote_show_is_dangerous(args),
        // Native g0o permits only flags here: a repository/ref positional
        // would contact a caller-selected remote rather than inspect metadata.
        "git ls-remote" => {
            !git_format_is_dangerous(args)
                && !args
                    .iter()
                    .any(|arg| *arg == "--" || *arg == "-" || !arg.starts_with('-'))
        }
        _ => true,
    }
}

/// `git remote show` positional-write guard — port of the 2.1.211 TS callback.
/// `args` are the tokens after `git remote show`. 2.1.211 REQUIRES the `-n`
/// (no-network) flag so only the offline form is auto-allowed: it splits args
/// on `--`, drops `-n` from the pre-`--` segment, then requires exactly ONE
/// remaining token that (a) came with `-n` present and (b) matches
/// `/^[a-zA-Z0-9_][a-zA-Z0-9_-]*$/` (first char NOT `-`). Without `-n`,
/// `git remote show origin` contacts the network and is dangerous.
fn git_remote_show_is_dangerous(args: &[&str]) -> bool {
    // Split on the `--` end-of-options marker (TS `t.indexOf("--")`).
    let (pre, post): (&[&str], Vec<&str>) = match args.iter().position(|a| *a == "--") {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => (args, Vec::new()),
    };
    let has_no_network = pre.contains(&"-n");
    let positional: Vec<&str> = pre
        .iter()
        .copied()
        .filter(|a| *a != "-n")
        .chain(post)
        .collect();
    if positional.len() != 1 {
        return true;
    }
    // The offline `-n` flag is mandatory (2.1.211 `if(!n.includes("-n"))return!0`).
    if !has_no_network {
        return true;
    }
    let name = positional[0];
    // `/^[a-zA-Z0-9_][a-zA-Z0-9_-]*$/`: non-empty, first byte alphanumeric/`_`.
    let mut bytes = name.bytes();
    match bytes.next() {
        None => true,
        Some(first) => {
            !(first.is_ascii_alphanumeric() || first == b'_')
                || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }
    }
}

/// `git remote` positional-write guard — port of the TS callback
/// (`readOnlyCommandValidation.ts:495-501`). `args` are the tokens after
/// `git remote`. Only a bare `git remote` or `git remote -v/--verbose` is
/// read-only; any positional (e.g. `add`/`remove`/`set-url`) is dangerous.
fn git_remote_is_dangerous(args: &[&str]) -> bool {
    args.iter().any(|a| *a != "-v" && *a != "--verbose")
}

/// `git reflog` write-subcommand guard — port of the 2.1.211 TS callback.
/// `args` are the tokens after `git reflog`. 2.1.211 uses an ALLOWLIST gate on
/// the first token (must be a flag, or exactly `show`/`list`; anything else —
/// incl. a bare ref name — is dangerous), PLUS a denylist of write subcommands
/// (`expire`/`delete`/`exists`/`drop`/`write`) matched anywhere in the args.
fn git_reflog_is_dangerous(args: &[&str]) -> bool {
    const SAFE_SUBCOMMANDS: &[&str] = &["show", "list"];
    const DANGEROUS_SUBCOMMANDS: &[&str] = &["expire", "delete", "exists", "drop", "write"];
    // First-token allowlist (TS `if(o&&!o.startsWith("-")&&!r.has(o))return!0`).
    if let Some(first) = args.first() {
        if !first.is_empty() && !first.starts_with('-') && !SAFE_SUBCOMMANDS.contains(first) {
            return true;
        }
    }
    // Denylist anywhere in the args (TS `for(i of t)if(n.has(i))return!0`).
    args.iter().any(|a| DANGEROUS_SUBCOMMANDS.contains(a))
}

/// Does `token` (a `-…` flag) contain a short-flag `l`, marking a list request?
/// Mirrors the TS short-flag-bundle test (`-li`/`-il` contain `l`) shared by the
/// `git tag`/`git branch` callbacks.
fn short_flag_bundle_has_list(token: &str) -> bool {
    let b = token.as_bytes();
    b.first() == Some(&b'-')
        && b.get(1) != Some(&b'-')
        && token.len() > 2
        && !token.contains('=')
        && token[1..].contains('l')
}

/// `git tag` creation guard — port of the TS callback
/// (`readOnlyCommandValidation.ts:739-805`). `args` are the tokens after
/// `git tag`. A positional arg without a preceding `-l`/`--list` is a tag name
/// to CREATE (writes `.git/refs/tags/…`) → dangerous.
fn git_tag_is_dangerous(args: &[&str]) -> bool {
    const FLAGS_WITH_ARGS: &[&str] = &[
        "--contains",
        "--no-contains",
        "--merged",
        "--no-merged",
        "--points-at",
        "--sort",
        "--format",
        "-n",
    ];
    let mut i = 0;
    let mut seen_list_flag = false;
    let mut seen_dash_dash = false;
    while i < args.len() {
        let token = args[i];
        if token.is_empty() {
            i += 1;
            continue;
        }
        // `--` ends flag parsing; subsequent tokens are positional even if they
        // start with `-` (`git tag -- -l` CREATES a tag named `-l`).
        if token == "--" && !seen_dash_dash {
            seen_dash_dash = true;
            i += 1;
            continue;
        }
        if !seen_dash_dash && token.starts_with('-') {
            if token == "--list" || token == "-l" || short_flag_bundle_has_list(token) {
                seen_list_flag = true;
            }
            if token.contains('=') {
                i += 1;
            } else if FLAGS_WITH_ARGS.contains(&token) {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            // Non-flag positional (or post-`--`). Safe only after `-l`/`--list`
            // (then it is a match pattern, not a tag name).
            if !seen_list_flag {
                return true;
            }
            i += 1;
        }
    }
    false
}

/// `git branch` creation/deletion/rename guard — port of the TS callback
/// (`readOnlyCommandValidation.ts:851-921`). `args` are the tokens after
/// `git branch`. A positional arg without `-l`/`--list` or a filtering flag is
/// a branch name to CREATE (or the target of `-d`/`-m`, which also carry a
/// positional) → dangerous.
fn git_branch_is_dangerous(args: &[&str]) -> bool {
    // NOTE `--abbrev` is intentionally NOT here: git does not consume a detached
    // arg for it (PARSE_OPT_OPTARG), so a following number is a positional the
    // callback must catch (TS comment, lines 862-865).
    const FLAGS_WITH_ARGS: &[&str] = &["--contains", "--no-contains", "--points-at", "--sort"];
    const FLAGS_WITH_OPTIONAL_ARGS: &[&str] = &["--merged", "--no-merged"];
    let mut i = 0;
    let mut last_flag = "";
    let mut seen_list_flag = false;
    let mut seen_dash_dash = false;
    while i < args.len() {
        let token = args[i];
        if token.is_empty() {
            i += 1;
            continue;
        }
        if token == "--" && !seen_dash_dash {
            seen_dash_dash = true;
            last_flag = "";
            i += 1;
            continue;
        }
        if !seen_dash_dash && token.starts_with('-') {
            if token == "--list" || token == "-l" || short_flag_bundle_has_list(token) {
                seen_list_flag = true;
            }
            if token.contains('=') {
                last_flag = token.split('=').next().unwrap_or("");
                i += 1;
            } else if FLAGS_WITH_ARGS.contains(&token) {
                last_flag = token;
                i += 2;
            } else {
                last_flag = token;
                i += 1;
            }
        } else {
            // Non-flag positional (or post-`--`). Safe only after `-l`/`--list`
            // or as the optional arg of `--merged`/`--no-merged`.
            let last_flag_has_optional_arg = FLAGS_WITH_OPTIONAL_ARGS.contains(&last_flag);
            if !seen_list_flag && !last_flag_has_optional_arg {
                return true;
            }
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod latest_read_only_tests {
    use super::*;

    #[test]
    fn glob_expansion_cannot_inject_flags_into_table_commands() {
        use crate::{PermissionMode, PermissionPolicy, PermissionResult};
        let policy =
            PermissionPolicy::new(PermissionMode::Default).with_roots(crate::filesystem::FsRoots {
                cwd: "/proj".into(),
                home: None,
                lingxi_home: "/proj/.lingxi".into(),
            });
        for command in [
            "sort *",
            "file ?",
            "sed -n p [ab]",
            "git log *",
            "rg needle *",
        ] {
            assert!(!command_is_read_only(command), "{command}");
            let input = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &serde_json::json!({"command": command}),
            )
            .unwrap();
            assert!(
                !matches!(
                    policy.authorize("Bash", &input),
                    PermissionResult::Allow { .. }
                ),
                "{command}"
            );
        }
        for command in [
            "cat *",
            "sha256sum *.txt",
            "sort '*'",
            "sort \\*",
            "sort \"*\"",
            "sed -n p '[ab]'",
        ] {
            assert!(command_is_read_only(command), "{command}");
        }
        // Native also carries the whole command's glob marker to children.
        assert!(!command_is_read_only("cat * | sort '*'"));
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/read_only_2_1_318.json")).unwrap();
        for case in fixture["publicGlobCases"].as_array().unwrap() {
            let command = case["command"].as_str().unwrap();
            assert_eq!(
                command_is_read_only(command),
                case["expected"].as_bool().unwrap(),
                "Native vat: {command}"
            );
        }
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn read_only_flags_match_hash_pinned_318_native_getopt_and_callbacks() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/read_only_2_1_318.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let words = case["words"]
                .as_array()
                .unwrap()
                .iter()
                .map(|word| word.as_str().unwrap())
                .collect::<Vec<_>>();
            let expected = case["expected"].as_bool().unwrap();
            let actual = match words[0] {
                "git" => git_subcommand_is_read_only(&words[1..]),
                "gh" => gh_is_read_only(&words[1..]),
                "docker" => docker_subcommand_is_read_only(&words[1..]),
                "rg" | "grep" | "egrep" | "fgrep" => validate_read_only_flags(&words).is_some(),
                _ => other_read_only_command(&words),
            };
            assert_eq!(actual, expected, "native getopt/callbacks: {words:?}");
            let quoted = words
                .iter()
                .map(|word| format!("'{}'", word.replace('\'', "'\\''")))
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(
                command_is_read_only(&quoted),
                case.get("quotedExpected")
                    .or_else(|| case.get("readOnlyExpected"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(expected),
                "public quoted shell path: {words:?}"
            );
        }
    }

    #[test]
    fn executable_and_remote_flags_never_inherit_read_only_grants() {
        for command in [
            "rg --pre arbitrary-program pattern",
            "rg --pre-glob '*.txt' pattern",
            "git diff --ext-diff",
            "git log --show-signature",
            "gh pr view 42 --web=false",
            "docker ps --config=alternate",
            "docker ps -- --host=prod",
            "gh pr view 42 --json title",
            "gh search code symbol --json path,repository",
        ] {
            assert!(!command_is_read_only(command), "{command}");
        }
        for command in [
            "docker logs --tail 10 app",
            "docker inspect --format '{{.Name}}' app",
            "rg -in -A5 'two words' '*.rs'",
            "git log --format=%H -10",
        ] {
            assert!(command_is_read_only(command), "{command}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PARITY Native 2.1.291 `l9n`'s `command.length > uM` bail (`uM = 1e4`). Over the
    /// threshold the oracle returns `passthrough` rather than classifying, so a
    /// giant command must never come back read-only however benign it looks.
    #[test]
    fn an_overlength_command_is_never_read_only() {
        // A command made only of unambiguously read-only subcommands.
        let unit = "echo hi && ";
        let short: String = unit.repeat(50) + "echo done";
        assert!(short.encode_utf16().count() < 10_000);
        assert!(
            command_is_read_only(&short),
            "the same shape under the threshold must still classify as read-only"
        );

        let long: String = unit.repeat(1000) + "echo done";
        assert!(
            long.encode_utf16().count() > 10_000,
            "fixture must cross Ox"
        );
        assert!(
            !command_is_read_only(&long),
            "past Ox the oracle refuses to classify, so this must not be read-only"
        );

        // The boundary is `>`, not `>=`.
        let exact = format!("echo {}", "e".repeat(9_995));
        assert_eq!(exact.encode_utf16().count(), 10_000);
        assert!(
            command_is_read_only(&exact),
            "the exact boundary remains classifiable"
        );
    }

    #[test]
    fn astral_command_length_uses_native_utf16_units() {
        let within = format!("echo {}", "\u{1f600}".repeat(4_997));
        assert_eq!(within.encode_utf16().count(), 9_999);
        assert!(within.chars().count() < 10_000);
        assert!(command_is_read_only(&within));

        let over = format!("echo {}", "\u{1f600}".repeat(4_998));
        assert_eq!(over.encode_utf16().count(), 10_001);
        assert!(over.chars().count() < 10_000);
        assert!(!command_is_read_only(&over));
    }

    // Native 2.1.318 has a dedicated guarded `test` table; getconf remains absent.
    #[test]
    fn test_uses_its_guarded_table_and_getconf_remains_unknown() {
        assert!(command_is_read_only("test -f foo"));
        assert!(!command_is_read_only("getconf PAGE_SIZE"));
    }

    // `find` is read-only only without a destructive/side-effecting action
    // (binary `vDp`). `-delete` carries no shell metachar, so it would slip past
    // the generic guard without the explicit reject.
    #[test]
    fn find_read_only_only_without_dangerous_actions() {
        assert!(command_is_read_only("find . -name foo.rs"));
        assert!(command_is_read_only("find src -type f"));
        assert!(!command_is_read_only("find . -delete"));
        assert!(!command_is_read_only("find . -name x -delete"));
        assert!(!command_is_read_only("find . -fls out.txt"));
        assert!(!command_is_read_only("find . -fprint out.txt"));
        assert!(!command_is_read_only("find . -files0-from list"));
    }

    #[test]
    fn core_read_commands_are_read_only() {
        assert!(command_is_read_only("cat foo.txt"));
        assert!(command_is_read_only("ls -la"));
        assert!(command_is_read_only("grep pattern file.rs"));
        assert!(command_is_read_only("pwd"));
        assert!(command_is_read_only("whoami"));
        assert!(command_is_read_only("head -n 5 a.log"));
        assert!(command_is_read_only("wc -l src/main.rs"));
        assert!(command_is_read_only("date"));
        assert!(command_is_read_only("rg needle"));
        assert!(command_is_read_only("echo hello world"));
        assert!(command_is_read_only("echo 'a && b'"));
        assert!(command_is_read_only("find . -name '*.rs'"));
        assert!(command_is_read_only("jq -r .name data.json"));
        assert!(!command_is_read_only("gh pr view 42 --json title"));
        assert!(!command_is_read_only("gh issue list"));
    }

    #[test]
    fn exact_match_base_commands_reject_args() {
        // 2.1.211 `OPg`: pwd/whoami/alias are read-only ONLY as a bare token.
        assert!(command_is_read_only("pwd"));
        assert!(command_is_read_only("whoami"));
        assert!(command_is_read_only("alias"));
        assert!(!command_is_read_only("pwd -L"));
        assert!(!command_is_read_only("whoami --foo"));
        assert!(!command_is_read_only("alias k=v"));
        // 2.1.211 `arch` regex: bare, or a lone `-h`/`--help`.
        assert!(command_is_read_only("arch"));
        assert!(command_is_read_only("arch -h"));
        assert!(command_is_read_only("arch --help"));
        assert!(!command_is_read_only("arch -x"));
        assert!(!command_is_read_only("arch x86_64 uname"));
    }

    #[test]
    fn git_reflog_allowlist_gate() {
        // 2.1.211: first token must be a flag or exactly show/list; the
        // {expire,delete,exists,drop,write} denylist matches anywhere.
        assert!(command_is_read_only("git reflog"));
        assert!(command_is_read_only("git reflog show"));
        assert!(command_is_read_only("git reflog list"));
        assert!(!command_is_read_only("git reflog drop"));
        assert!(!command_is_read_only("git reflog write"));
        assert!(!command_is_read_only("git reflog expire --all"));
        // A bare ref name is no longer auto-allowed (allowlist gate).
        assert!(!command_is_read_only("git reflog main"));
    }

    #[test]
    fn compound_all_read_only_is_read_only() {
        assert!(command_is_read_only("cat a && ls -l"));
        assert!(command_is_read_only("pwd; whoami"));
        // a pipe makes the WHOLE command carry a `|` metachar in each split's
        // boundary? No — split removes the `|`, but `cat x | grep y` splits into
        // `cat x` and `grep y`, both read-only.
        assert!(command_is_read_only("cat x | grep y"));
    }

    #[test]
    fn writing_or_unknown_commands_are_not_read_only() {
        assert!(!command_is_read_only("rm -rf /tmp/x"));
        assert!(!command_is_read_only("curl https://x"));
        assert!(!command_is_read_only("npm install"));
        assert!(!command_is_read_only("mkdir foo"));
        // A read command compounded with a writer is NOT read-only overall.
        assert!(!command_is_read_only("cat a && rm b"));
        assert!(!command_is_read_only("gh pr merge 42"));
        assert!(!command_is_read_only("gh issue close 42"));
        assert!(!command_is_read_only("gh pr view 42 --web"));
        assert!(!command_is_read_only("gh auth status --show-token"));
        assert!(!command_is_read_only("date --set 2030-01-01"));
        assert!(!command_is_read_only("jq -f filter.jq data.json"));
        assert!(command_is_read_only("cat *"));
        assert!(command_is_read_only("cat file?.txt"));
    }

    #[test]
    fn redirection_and_substitution_are_not_read_only() {
        // A redirect target makes the subcommand carry `>` → not read-only.
        assert!(!command_is_read_only("cat a > out.txt"));
        assert!(!command_is_read_only("echo hi > /etc/x"));
        // Command substitution / process substitution.
        assert!(!command_is_read_only("cat $(echo a)"));
        assert!(!command_is_read_only("echo `whoami`"));
        // `$VAR` expansion can smuggle a flag → refused.
        assert!(!command_is_read_only("cat $FILE"));
    }

    #[test]
    fn trailing_stderr_dup_is_tolerated() {
        // `cat x 2>&1` reduces to `cat x` (the `2>&1` is stripped first) → still
        // read-only. (But a non-trailing `2>&1` keeps a `>` metachar → not.)
        assert!(command_is_read_only("cat x 2>&1"));
    }

    #[test]
    fn docker_only_ps_and_images() {
        assert!(command_is_read_only("docker ps"));
        assert!(command_is_read_only("docker images"));
        assert!(command_is_read_only("docker ps -a"));
        assert!(!command_is_read_only("docker run x"));
        assert!(!command_is_read_only("docker"));
        assert!(!command_is_read_only("docker ps --host tcp://remote"));
        assert!(!command_is_read_only("docker ps -H tcp://remote"));
        assert!(!command_is_read_only("docker ps -Htcp://remote"));
        assert!(!command_is_read_only("docker images --context=prod"));
        assert!(!command_is_read_only("docker images --url tcp://remote"));
        assert!(!command_is_read_only(
            "docker images --connection ssh://remote"
        ));
        assert!(!command_is_read_only("docker ps --identity ~/.ssh/id_rsa"));
    }

    #[test]
    fn history_numeric_only() {
        assert!(command_is_read_only("history"));
        assert!(command_is_read_only("history 20"));
        assert!(!command_is_read_only("history -c"));
    }

    #[test]
    fn empty_command_is_not_read_only() {
        assert!(!command_is_read_only(""));
        assert!(!command_is_read_only("   "));
    }
}

#[cfg(test)]
mod test_quote_aware_expansion {
    //! Parity for the quote-aware `$`/backtick handling — mirroring TS
    //! `containsUnquotedExpansion` (`readOnlyValidation.ts:1600`) and the
    //! backtick arm of `validateDangerousPatterns`
    //! (`bashSecurity.ts:853`). Single-quoted `$`/backtick are LITERAL (still
    //! read-only); double-quoted or unquoted are ACTIVE (NOT read-only).
    use super::*;

    #[test]
    fn single_quoted_dollar_is_literal_and_read_only() {
        // Single-quoted `$VAR` is literal in bash → read-only (TS allows; the
        // former quote-naive `$` membership test wrongly denied this).
        assert!(command_is_read_only("echo '$VAR'"));
        // The native grep getopt path rejects dollar-bearing argument values,
        // even when shell quoting makes them literal. Echo's regex permits it.
        assert!(!command_is_read_only("grep '$x' file"));
    }

    #[test]
    fn double_quoted_dollar_expands_not_read_only() {
        // Double-quoted `$VAR` expands at runtime → NOT read-only.
        assert!(!command_is_read_only("echo \"$VAR\""));
    }

    #[test]
    fn unquoted_dollar_expands_not_read_only() {
        // Unquoted `$VAR` expands → NOT read-only.
        assert!(!command_is_read_only("echo $VAR"));
        // `$VAR` expansion can smuggle a flag → refused.
        assert!(!command_is_read_only("cat $FILE"));
    }

    #[test]
    fn single_quoted_backtick_is_literal_and_read_only() {
        // Single-quoted backtick is literal in bash → read-only.
        assert!(command_is_read_only("echo '`id`'"));
    }

    #[test]
    fn double_quoted_backtick_substitutes_not_read_only() {
        // Double-quoted backtick is command substitution → NOT read-only
        // (TS checks backtick against `withDoubleQuotes`, which excludes only
        // single-quoted content).
        assert!(!command_is_read_only("echo \"`id`\""));
        // Unquoted backtick likewise.
        assert!(!command_is_read_only("echo `whoami`"));
    }

    #[test]
    fn compound_quoted_read_then_writer_is_not_read_only() {
        // The `cat '$a'` subcommand is read-only (single-quoted `$` literal),
        // but the `rm x` subcommand fails the read-only base set → the whole
        // compound is NOT read-only.
        assert!(!command_is_read_only("cat '$a'; rm x"));
    }

    #[test]
    fn arithmetic_expansion_still_rejected_via_paren_metachars() {
        // `$((1+1))` is no longer rejected by the `$` guard (the char after
        // `$` is `(`, not a name char), but the `(`/`)` quote-naive metachars
        // still disqualify it → NOT read-only, matching TS (which catches the
        // parens via COMMAND_SUBSTITUTION_PATTERNS).
        assert!(!command_is_read_only("echo $((1+1))"));
    }
}

#[cfg(test)]
mod test_git_read_only {
    //! Parity for the git slice of TS `GIT_READ_ONLY_COMMANDS`
    //! (`utils/shell/readOnlyCommandValidation.ts:107-923`): the read-only
    //! subcommand allowlist + the positional-write guard callbacks.
    use super::*;

    #[test]
    fn embedded_commit_commands_are_read_only() {
        // The four `!`git …`` bodies embedded in /commit (commit.rs:63-66).
        assert!(command_is_read_only("git status"));
        assert!(command_is_read_only("git diff HEAD"));
        assert!(command_is_read_only("git branch --show-current"));
        assert!(command_is_read_only("git log --oneline -10"));
    }

    #[test]
    fn security_review_and_show_commands_are_read_only() {
        // Bodies embedded in /security-review (security_review.rs:28-46) and
        // the required `git show` / `git rev-parse` cases.
        assert!(command_is_read_only("git show"));
        assert!(command_is_read_only("git diff --name-only origin/HEAD..."));
        assert!(command_is_read_only("git log --no-decorate origin/HEAD..."));
        assert!(command_is_read_only("git diff origin/HEAD..."));
        assert!(command_is_read_only("git rev-parse"));
        assert!(command_is_read_only("git rev-parse HEAD"));
        assert!(command_is_read_only("git rev-parse --show-toplevel"));
    }

    #[test]
    fn additional_read_only_subcommands() {
        assert!(command_is_read_only("git branch"));
        assert!(command_is_read_only("git branch -a"));
        assert!(!command_is_read_only("git branch --list feature/*"));
        assert!(command_is_read_only("git branch --list 'feature/*'"));
        assert!(command_is_read_only("git branch --merged"));
        assert!(command_is_read_only("git tag"));
        assert!(!command_is_read_only("git tag -l v1.*"));
        assert!(command_is_read_only("git tag -l 'v1.*'"));
        assert!(command_is_read_only("git reflog"));
        assert!(command_is_read_only("git reflog show"));
        assert!(command_is_read_only("git remote"));
        assert!(command_is_read_only("git remote -v"));
        // 2.1.211: `git remote show` is read-only ONLY with the `-n` (offline)
        // flag; the network-contacting form asks.
        assert!(command_is_read_only("git remote show -n origin"));
        assert!(!command_is_read_only("git remote show origin"));
        assert!(command_is_read_only("git stash list"));
        assert!(command_is_read_only("git stash show"));
        assert!(command_is_read_only("git config --get user.name"));
        assert!(command_is_read_only("git worktree list"));
        assert!(command_is_read_only("git blame src/main.rs"));
        assert!(command_is_read_only("git ls-files"));
        assert!(command_is_read_only("git merge-base HEAD main"));
        assert!(command_is_read_only("git describe --tags"));
    }

    #[test]
    fn write_subcommands_are_not_read_only() {
        // Required negative cases: git commit / push / add.
        assert!(!command_is_read_only("git commit -m x"));
        assert!(!command_is_read_only("git push"));
        assert!(!command_is_read_only("git add ."));
        assert!(!command_is_read_only("git add -A"));
        // Other writes.
        assert!(!command_is_read_only("git checkout main"));
        assert!(!command_is_read_only("git reset --hard"));
        assert!(!command_is_read_only("git merge feature"));
        assert!(!command_is_read_only("git rebase main"));
        assert!(!command_is_read_only("git pull"));
        assert!(!command_is_read_only("git fetch"));
        assert!(!command_is_read_only("git stash"));
        assert!(!command_is_read_only("git worktree add ../wt"));
        assert!(!command_is_read_only("git config user.name x"));
        // Bare `git` is not read-only.
        assert!(!command_is_read_only("git"));
    }

    #[test]
    fn branch_creation_and_mutation_are_not_read_only() {
        // Positional branch name = creation.
        assert!(!command_is_read_only("git branch newbranch"));
        assert!(!command_is_read_only("git branch feature start-point"));
        // Delete / rename carry a positional the callback catches.
        assert!(!command_is_read_only("git branch -d old"));
        assert!(!command_is_read_only("git branch -D old"));
        assert!(!command_is_read_only("git branch -m old new"));
        // `git branch -- -l` creates a branch named `-l`.
        assert!(!command_is_read_only("git branch -- -l"));
    }

    #[test]
    fn tag_creation_is_not_read_only() {
        assert!(!command_is_read_only("git tag v1.0.0"));
        assert!(!command_is_read_only("git tag -d v1.0.0"));
        assert!(!command_is_read_only("git tag -- -l"));
    }

    #[test]
    fn reflog_and_remote_writes_are_not_read_only() {
        assert!(!command_is_read_only("git reflog expire --all"));
        assert!(!command_is_read_only("git reflog delete HEAD@{0}"));
        assert!(!command_is_read_only("git remote add origin url"));
        assert!(!command_is_read_only("git remote remove origin"));
        // `git remote show` requires `-n` + exactly one name whose first char
        // is alphanumeric/underscore (not `-`).
        assert!(!command_is_read_only("git remote show"));
        assert!(!command_is_read_only("git remote show -n a b"));
        assert!(!command_is_read_only("git remote show -n -origin"));
        assert!(command_is_read_only("git remote show -n -- origin"));
    }

    #[test]
    fn output_write_flag_is_not_read_only() {
        // `--output=<file>` writes a file even on a read-only subcommand.
        assert!(!command_is_read_only("git diff --output=/tmp/pwned"));
        assert!(!command_is_read_only("git log --output /tmp/x"));
        assert!(!command_is_read_only("git show --output=/tmp/x"));
    }
}
