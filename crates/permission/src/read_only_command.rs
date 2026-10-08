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
//! # Faithful scope vs. documented divergence
//! The TS read-only allowlist is enormous (~1500 lines of git/gh/docker/ripgrep
//! per-subcommand flag configs + getopt-aware flag parsing). Porting that whole
//! machine 1:1 is a separate effort; here we port the BASE [`READONLY_COMMANDS`]
//! simple-command set + the simple-command regex SHAPE, PLUS the git slice of
//! `GIT_READ_ONLY_COMMANDS` (the read-only subcommand allowlist + the
//! branch/tag/reflog/remote positional-write guard callbacks; see
//! [`git_subcommand_is_read_only`]). The gh/docker/ripgrep per-flag tables
//! remain out of scope. TS
//! `makeRegexForSafeCommand` anchors the base word, then allows a trailing run
//! of characters that excludes the shell metacharacters (redirection, command
//! substitution, pipe, brace/paren group, background, list, newline) — plus the
//! hand-written read-only regexes for echo / pwd / whoami / ls / find / cd /
//! grep / rg that the common case needs.
//!
//! The crucial property is the SAFE DIRECTION: this only ever returns `true`
//! for a command whose base word is on the allowlist and that carries no active
//! shell operator, redirection, or substitution. Quoted operators remain
//! literal arguments; malformed quoting fails closed. Anything else returns
//! `false` and falls through to the normal ask path.
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
];

const GH_READ_ONLY_COMMANDS: &[(&str, &str)] = &[
    ("pr", "view"),
    ("pr", "list"),
    ("pr", "diff"),
    ("pr", "checks"),
    ("pr", "status"),
    ("issue", "view"),
    ("issue", "list"),
    ("issue", "status"),
    ("repo", "view"),
    ("run", "list"),
    ("run", "view"),
    ("auth", "status"),
    ("release", "list"),
    ("release", "view"),
    ("workflow", "list"),
    ("workflow", "view"),
    ("label", "list"),
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
/// never safe enough to auto-allow. Shell globs are deliberately allowed:
/// expanding an argument list does not turn an allowlisted reader into a
/// writer, and Claude's git/read-command fixtures include glob patterns.
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
    subs.iter().all(|sub| is_read_only_subcommand(sub))
}

/// Is a SINGLE subcommand read-only? — the per-subcommand arm of TS
/// `isCommandReadOnly` (`readOnlyValidation.ts:1678`), in its conservative core.
fn is_read_only_subcommand(sub: &str) -> bool {
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
    let mut words = test.split_whitespace();
    let Some(base) = words.next() else {
        return false;
    };
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
        let rest: Vec<&str> = words.collect();
        return gh_is_read_only(&rest);
    }
    if base == "jq" {
        let rest: Vec<&str> = words.collect();
        return jq_is_read_only(&rest);
    }
    // `echo` with metacharacter-free args is read-only (the TS echo regex; the
    // metachar guard above already rejected the dangerous `` ` ``/`$`/`<>` forms
    // the TS regex excludes).
    if base == "echo" {
        return true;
    }
    // `grep` / `rg` (ripgrep) reading files: flags + paths only, no writes.
    // (ripgrep has no write flag; `grep` likewise only reads. The metachar
    // guard rejects the `>`/`|` forms.)
    if matches!(base, "grep" | "egrep" | "fgrep" | "rg") {
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

fn gh_is_read_only(args: &[&str]) -> bool {
    args.len() >= 2
        && GH_READ_ONLY_COMMANDS.contains(&(args[0], args[1]))
        && !args[2..]
            .iter()
            .any(|arg| matches!(*arg, "--web" | "-w" | "--show-token"))
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
    if !matches!(subcommand, "ps" | "images") {
        return false;
    }
    !docker_has_remote_control_flag(&rest[1..])
}

fn docker_has_remote_control_flag(args: &[&str]) -> bool {
    for arg in args {
        if *arg == "--" {
            break;
        }
        if DOCKER_REMOTE_CONTROL_FLAGS.contains(arg) {
            return true;
        }
        if let Some((flag, _)) = arg.split_once('=') {
            if DOCKER_REMOTE_CONTROL_FLAGS.contains(&flag) {
                return true;
            }
        }
        if arg.starts_with("-H") && arg.len() > 2 {
            return true;
        }
    }
    false
}

/// Git flags that WRITE to disk regardless of subcommand — the file-output
/// vector shared by the diff-generating read-only subcommands (`git diff` /
/// `git log` / `git show` all accept `--output=<file>`). TS omits `--output`
/// from every subcommand's `safeFlags` allowlist, so validation rejects it;
/// this port keeps the SAFE-DIRECTION invariant with an explicit denylist
/// instead of porting the full ~800-line per-flag `safeFlags` machine.
const GIT_WRITE_FLAGS: &[&str] = &["--output", "--output-directory"];

/// Is `rest` (the tokens after `git`) a read-only git invocation? — the port of
/// the git slice of TS `GIT_READ_ONLY_COMMANDS`
/// (`utils/shell/readOnlyCommandValidation.ts:107-923`).
///
/// This recognises the read-only git subcommand allowlist (the KEYS of
/// `GIT_READ_ONLY_COMMANDS`) and applies the four positional-write guard
/// callbacks (`git branch`/`git tag`/`git reflog`/`git remote`/`git remote
/// show`). Longer multi-word forms (`git remote show`, `git stash list`, …) are
/// matched before their one-word prefixes so e.g. `git remote show` wins over
/// `git remote`.
///
/// # Conservative divergence (documented)
/// The full TS machine additionally validates every flag against a
/// per-subcommand `safeFlags` allowlist (~800 lines of getopt-aware parsing).
/// This port does NOT reproduce that whole table; instead it relies on:
/// * the caller's shell-metacharacter + expansion guards (already run before
///   this fn), which reject redirection/substitution/`$`-bearing forms;
/// * the positional-write callbacks below, which reject the branch/tag creation
///   and reflog/remote write forms (every git write reachable from a read-only
///   subcommand needs a positional arg the callback catches); and
/// * the [`GIT_WRITE_FLAGS`] denylist for the one file-write flag (`--output`)
///   reachable from an otherwise read-only subcommand.
///
/// The SAFE DIRECTION is preserved: a git write subcommand (`commit`/`push`/…)
/// is simply absent from the allowlist → not read-only, and the callbacks +
/// denylist reject the write forms of the allowlisted subcommands.
fn git_subcommand_is_read_only(rest: &[&str]) -> bool {
    let Some(&sub) = rest.first() else {
        // Bare `git` is not read-only.
        return false;
    };
    // Reject the shared file-write flag regardless of subcommand (SAFE
    // DIRECTION). TS achieves this by omitting `--output` from every
    // `safeFlags` map.
    if rest.iter().any(|a| {
        let flag = a.split('=').next().unwrap_or(a);
        GIT_WRITE_FLAGS.contains(&flag)
    }) {
        return false;
    }
    // Multi-word read-only prefixes — checked BEFORE the one-word forms so the
    // longer, more specific key matches first (TS orders `git remote show`
    // before `git remote`).
    if rest.len() >= 2 {
        match (rest[0], rest[1]) {
            // `git remote show <name>` — callback: exactly one alphanumeric
            // remote name (TS `git remote show` callback, lines 478-487). Args
            // to the callback are the tokens after `show` (`rest[2..]`).
            ("remote", "show") => return !git_remote_show_is_dangerous(&rest[2..]),
            // `git stash list` / `git stash show` — read-only (bare `git stash`
            // is NOT: it writes the stash).
            ("stash", "list" | "show") => return true,
            // `git config --get …` — read-only config read (bare `git config
            // k v` writes).
            ("config", "--get") => return true,
            // `git worktree list` — read-only (bare `git worktree add` writes).
            ("worktree", "list") => return true,
            _ => {}
        }
    }
    match sub {
        // Pure read-only subcommands with no positional-write form.
        "diff" | "log" | "show" | "shortlog" | "ls-remote" | "status" | "blame" | "ls-files"
        | "merge-base" | "rev-parse" | "rev-list" | "describe" | "cat-file" | "for-each-ref"
        | "grep" => true,
        // `git reflog` — block the write subcommands `expire`/`delete`/`exists`
        // (TS callback, lines 283-303).
        "reflog" => !git_reflog_is_dangerous(&rest[1..]),
        // `git remote` (bare / `-v`) — only `-v`/`--verbose`, no positional
        // (TS callback, lines 495-501).
        "remote" => !git_remote_is_dangerous(&rest[1..]),
        // `git tag` — block tag creation via a positional arg without `-l`
        // (TS callback, lines 739-805).
        "tag" => !git_tag_is_dangerous(&rest[1..]),
        // `git branch` — block branch creation/deletion/rename via a positional
        // arg without `-l`/a filtering flag (TS callback, lines 851-921).
        "branch" => !git_branch_is_dangerous(&rest[1..]),
        // Everything else (`commit`/`push`/`add`/`checkout`/`stash`/`config`/
        // `worktree`/…) is NOT read-only.
        _ => false,
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

    // Binary `vho` read-only base set excludes `test`/`getconf`; the port
    // previously over-allowed them.
    #[test]
    fn test_and_getconf_are_not_read_only() {
        assert!(!command_is_read_only("test -f foo"));
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
        assert!(command_is_read_only("gh pr view 42 --json title"));
        assert!(command_is_read_only("gh issue list"));
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
        assert!(command_is_read_only("grep '$x' file"));
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
        assert!(command_is_read_only("git branch --list feature/*"));
        assert!(command_is_read_only("git branch --merged"));
        assert!(command_is_read_only("git tag"));
        assert!(command_is_read_only("git tag -l v1.*"));
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
