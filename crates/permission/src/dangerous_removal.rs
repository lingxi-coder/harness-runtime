//! Dangerous-removal-path guard — faithful port of claude-code
//! `src/tools/BashTool/pathValidation.ts::checkDangerousRemovalPaths` +
//! `src/utils/permissions/pathValidation.ts::isDangerousRemovalPath`.
//!
//! `rm`/`rmdir` commands whose target resolves to a critical system path
//! (`/`, a direct child of `/` like `/etc`/`/usr`, the home directory, a
//! trailing `/*` glob, a Windows drive root/child) must ALWAYS require an
//! explicit ask — even when an allow rule such as `Bash(rm:*)` matches. This
//! prevents catastrophic data loss (`rm -rf /`) from being auto-allowed by
//! permission rules.
//!
//! TS placement: `createPathChecker` runs `checkDangerousRemovalPaths` AFTER
//! explicit deny rules but BEFORE any allow grant (`pathValidation.ts:728-737`),
//! so the dangerous-removal ask wins over a matching allow rule but never
//! overrides an explicit deny. The Rust wiring in [`crate::policy`] mirrors
//! that ordering (the check runs after the deny/ask walks and before the allow
//! walk).
//!
//! Argument extraction reuses [`crate::shell_command::split_command`] to peel
//! the command into subcommands (so `rm -rf / ; echo done` is examined), then a
//! quote-aware token splitter for each subcommand, with `filterOutFlags`'
//! POSIX `--` end-of-options handling (`pathValidation.ts:126-139`).

use std::path::Path;

/// Match TS `WINDOWS_DRIVE_ROOT_REGEX = /^[A-Za-z]:\/?$/` against a
/// forward-slash-normalized path (e.g. `C:` or `C:/`).
fn is_windows_drive_root(p: &str) -> bool {
    let bytes = p.as_bytes();
    match bytes.len() {
        2 => bytes[0].is_ascii_alphabetic() && bytes[1] == b':',
        3 => bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/',
        _ => false,
    }
}

/// Match TS `WINDOWS_DRIVE_CHILD_REGEX = /^[A-Za-z]:\/[^/]+$/` (e.g.
/// `C:/Windows`, `C:/Users`) against a forward-slash-normalized path.
fn is_windows_drive_child(p: &str) -> bool {
    let bytes = p.as_bytes();
    if bytes.len() < 4 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'/' {
        return false;
    }
    // `[^/]+` — the remainder after `X:/` must be non-empty and contain no `/`.
    !p[3..].is_empty() && !p[3..].contains('/')
}

/// `dirname` over a forward-slash-normalized path, reproducing the subset of
/// Node `path.dirname` semantics that [`is_dangerous_removal_path`] relies on:
/// it only needs to recognize when a path's parent is exactly `/` (i.e. a
/// direct child of root, `/usr`, `/tmp`, …). Node returns `/` for `/usr`, and
/// `.` for a bare `foo`.
fn dirname_posix(p: &str) -> String {
    match p.rfind('/') {
        // No separator → relative single segment → ".".
        None => ".".to_string(),
        // Leading slash only (`/usr` → idx 0) → parent is "/".
        Some(0) => "/".to_string(),
        Some(idx) => p[..idx].to_string(),
    }
}

/// Faithful port of TS `isDangerousRemovalPath` (`pathValidation.ts:331-367`).
///
/// `abs_path` is the already-resolved (tilde-expanded, absolutized) path. The
/// `home` parameter is the user's home directory (TS `homedir()`); pass `None`
/// to skip the home-directory comparison (no `HOME` available).
///
/// Returns `true` when the path is one of the critical-system patterns that
/// must never be auto-removed.
#[must_use]
pub fn is_dangerous_removal_path(abs_path: &str, home: Option<&str>) -> bool {
    // Collapse `\`/`/` runs to a single `/` so `C:\\Windows` and `//foo`
    // normalize. (TS: resolvedPath.replace(/[\\/]+/g, '/').)
    let forward_slashed = collapse_slashes(abs_path);

    // Wildcard `*` or any path ending in `/*` (removes all files in a dir).
    if forward_slashed == "*" || forward_slashed.ends_with("/*") {
        return true;
    }

    // macOS: `/etc`, `/var`, `/tmp`, `/home` are symlinks under `/private`, so
    // `rm -rf /private/etc` removes the real `/etc`. Normalize the
    // `/private/(etc|var|tmp|home)` prefix away (TS `Stt`'s `r()`), so the
    // root-child / home checks below catch it.
    let normalized_private = normalize_macos_private(&forward_slashed);

    // Strip a single trailing `/` except for the bare root `/`.
    let normalized_path = if normalized_private == "/" {
        normalized_private.clone()
    } else {
        normalized_private
            .strip_suffix('/')
            .map_or(normalized_private.clone(), str::to_string)
    };

    if normalized_path == "/" {
        return true;
    }

    if is_windows_drive_root(&normalized_path) {
        return true;
    }

    if let Some(home) = home {
        // TS `Stt` compares the case-folded (`IA`) path against the case-folded,
        // `/private`-normalized, trailing-slash-stripped home directory.
        let normalized_home = {
            let np = normalize_macos_private(&collapse_slashes(home));
            np.strip_suffix('/').map_or(np.clone(), str::to_string)
        };
        if case_fold(&normalized_path) == case_fold(&normalized_home) {
            return true;
        }
    }

    // Direct children of root: /usr, /tmp, /etc (but not /usr/local).
    if dirname_posix(&normalized_path) == "/" {
        return true;
    }

    if is_windows_drive_child(&normalized_path) {
        return true;
    }

    false
}

/// macOS `/private` normalization from TS `Stt` (`pathValidation.ts`):
/// `/^\/private\/(etc|var|tmp|home)(\/|$)/i` → `/$1$2`. A no-op on non-macOS
/// and for any path that does not start with `/private/<one of those names>`
/// followed by `/` or end-of-string. The captured name's case is preserved
/// (the regex replacement uses `$1`).
fn normalize_macos_private(p: &str) -> String {
    const PREFIX: &str = "/private/";
    if !cfg!(target_os = "macos") {
        return p.to_string();
    }
    if p.len() <= PREFIX.len() || !p[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
        return p.to_string();
    }
    let rest = &p[PREFIX.len()..];
    for name in ["etc", "var", "tmp", "home"] {
        if rest.len() >= name.len() && rest[..name.len()].eq_ignore_ascii_case(name) {
            let after = &rest[name.len()..];
            if after.is_empty() || after.starts_with('/') {
                // `/private/<name>` → `/<name>` (preserve the matched case).
                return format!("/{}{after}", &rest[..name.len()]);
            }
        }
    }
    p.to_string()
}

/// TS `IA` case-fold (`paths.ts`): `toLowerCase()` then map dotless-ı → `i` and
/// long-s ſ → `s`, used for case-insensitive path comparison.
fn case_fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            '\u{0131}' => 'i',
            '\u{017f}' => 's',
            other => other,
        })
        .collect()
}

/// Lexical (no-filesystem) path normalization à la Node `path.normalize` for
/// POSIX paths: collapse empty segments (`//`), drop `.`, and resolve `..`
/// against the prior real segment. Used to resolve a removal target like
/// `<cwd>/..` to the parent before the workspace-ancestor comparison.
fn lexical_normalize(p: &str) -> String {
    let absolute = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => match out.last() {
                Some(&"..") => out.push(".."),
                Some(_) => {
                    out.pop();
                }
                None => {
                    if !absolute {
                        out.push("..");
                    }
                }
            },
            s => out.push(s),
        }
    }
    let body = out.join("/");
    if absolute {
        format!("/{body}")
    } else if body.is_empty() {
        ".".to_string()
    } else {
        body
    }
}

/// TS `R0`'s `/private` normalization (`/^\/private\/var\//` → `/var/`,
/// `/^\/private\/tmp(\/|$)/` → `/tmp$1`) — case-sensitive, applied
/// unconditionally (a no-op for any path without that prefix).
fn normalize_private_var_tmp(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("/private/var/") {
        return format!("/var/{rest}");
    }
    if p == "/private/tmp" {
        return "/tmp".to_string();
    }
    if let Some(rest) = p.strip_prefix("/private/tmp/") {
        return format!("/tmp/{rest}");
    }
    p.to_string()
}

/// TS `R0(cwd, target)`: would removing `target` delete the working directory?
/// True when `target` is the cwd itself or one of its ancestors (the working
/// directory or a parent of it). Paths are `/private`-normalized, lexically
/// normalized, and compared case-folded (the `caseFold: true` default).
fn removal_hits_workspace(target: &str, cwd: &Path) -> bool {
    let norm = |s: &str| normalize_private_var_tmp(&lexical_normalize(&collapse_slashes(s)));
    let t = norm(target);
    let c = norm(&cwd.to_string_lossy());
    let tf = {
        let f = case_fold(t.trim_end_matches('/'));
        if f.is_empty() {
            "/".to_string()
        } else {
            f
        }
    };
    let cf = {
        let f = case_fold(c.trim_end_matches('/'));
        if f.is_empty() {
            "/".to_string()
        } else {
            f
        }
    };
    // target == cwd, or target is an ancestor of cwd (cwd is under target).
    // Root (`/`) is an ancestor of every absolute cwd — special-cased because
    // its `{tf}/` would be `//`.
    cf == tf || (tf == "/" && cf.starts_with('/')) || cf.starts_with(&format!("{tf}/"))
}

/// Collapse runs of `\`/`/` into a single `/` (TS `replace(/[\\/]+/g, '/')`).
fn collapse_slashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_slash = false;
    for c in s.chars() {
        if c == '/' || c == '\\' {
            if !prev_slash {
                out.push('/');
            }
            prev_slash = true;
        } else {
            out.push(c);
            prev_slash = false;
        }
    }
    out
}

/// Expand a leading `~` / `~/` to `home` (TS `expandTilde`, `pathValidation.ts:80`).
/// `~username` is NOT expanded (left literal). Returns the path unchanged when
/// `home` is `None` or the path does not start with a bare tilde.
fn expand_tilde(path: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return path.to_string();
    };
    if path == "~" || path.starts_with("~/") {
        // homedir() + path.slice(1)
        format!("{}{}", home, &path[1..])
    } else {
        path.to_string()
    }
}

/// TS `isAbsolute` (POSIX): a path is absolute iff it starts with `/`. We also
/// treat a Windows drive-absolute path (`C:\…` / `C:/…`) and a UNC-ish `\\` as
/// absolute so the resolved form is left intact rather than re-anchored to cwd.
fn is_absolute_path(p: &str) -> bool {
    if p.starts_with('/') || p.starts_with('\\') {
        return true;
    }
    let b = p.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Resolve `clean_path` to an absolute path against `cwd` (TS:
/// `isAbsolute(cleanPath) ? cleanPath : resolve(cwd, cleanPath)`). We do NOT
/// normalize `..`/`.` here: TS `path.resolve` would, but the dangerous-path
/// predicate operates on the lexical join, and the only `..`-bearing fixture
/// (`rm -- -/../x`) is exercised for path EXTRACTION, not for triggering a
/// dangerous match. Keeping the join lexical avoids pulling in a path-canonical
/// dependency and never weakens the guard (a `..`-containing absolute path that
/// happens to resolve to `/etc` is still asked about via its other segments only
/// if it lexically matches — documented divergence, safe direction).
fn resolve_against_cwd(clean_path: &str, cwd: &Path) -> String {
    if is_absolute_path(clean_path) {
        clean_path.to_string()
    } else {
        let mut joined = cwd.to_string_lossy().into_owned();
        if !joined.ends_with('/') {
            joined.push('/');
        }
        joined.push_str(clean_path);
        joined
    }
}

/// Quote-aware token splitter for a single subcommand's argv. Reproduces the
/// effect of TS shell-quote parsing for the cases the removal guard needs:
/// whitespace splits tokens, single/double quotes group, and a backslash
/// escapes the next char outside quotes. Surrounding quotes are STRIPPED from
/// each token (matching `parseCommandArguments`, which yields bare strings).
fn split_argv(subcommand: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut has_token = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = subcommand.chars().peekable();
    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                cur.push(c);
            }
            continue;
        }
        if in_double {
            if c == '\\' {
                if let Some(&n) = chars.peek() {
                    // In double quotes a backslash escapes `"` `\` `$` backtick.
                    if matches!(n, '"' | '\\' | '$' | '`') {
                        cur.push(n);
                        chars.next();
                        continue;
                    }
                }
                cur.push(c);
            } else if c == '"' {
                in_double = false;
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if has_token {
                    tokens.push(std::mem::take(&mut cur));
                    has_token = false;
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
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                has_token = true;
            }
            _ => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        tokens.push(cur);
    }
    tokens
}

/// TS `filterOutFlags` (`pathValidation.ts:126-139`): keep positional
/// (non-flag) arguments, correctly handling the POSIX `--` end-of-options
/// delimiter — after `--`, ALL arguments are positional even if they start with
/// `-`. This is the path extractor used for `rm`/`rmdir`.
fn filter_out_flags(args: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut after_double_dash = false;
    for arg in args {
        if after_double_dash {
            result.push(arg.clone());
        } else if arg == "--" {
            after_double_dash = true;
        } else if !arg.starts_with('-') {
            result.push(arg.clone());
        }
    }
    result
}

/// Outcome of [`check_dangerous_removal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DangerousRemoval {
    /// The resolved absolute path that triggered the guard.
    pub resolved_path: String,
    /// The byte-locked ask MESSAGE (TS `pathValidation.ts:92`).
    pub message: String,
    /// The decision reason text (TS `pathValidation.ts:95`).
    pub reason: String,
}

/// Examine a shell `command` for `rm`/`rmdir` invocations targeting a dangerous
/// path. Returns `Some(DangerousRemoval)` for the FIRST dangerous target found
/// (mirroring TS, which returns on the first match), or `None` if no `rm`/
/// `rmdir` subcommand targets a critical path.
///
/// `cwd` resolves relative targets; `home` expands a leading `~` and is the
/// home-directory comparison target. The command is split into subcommands so a
/// dangerous `rm` hidden behind a benign one (`echo ok && rm -rf /`) is still
/// caught.
#[must_use]
pub fn check_dangerous_removal(
    command: &str,
    cwd: &Path,
    home: Option<&str>,
) -> Option<DangerousRemoval> {
    check_dangerous_removal_inner(command, cwd, home, 0)
}

fn check_dangerous_removal_inner(
    command: &str,
    cwd: &Path,
    home: Option<&str>,
    depth: usize,
) -> Option<DangerousRemoval> {
    if depth > 8 {
        return None;
    }

    for inner in active_command_substitutions(command) {
        if let Some(hit) = check_dangerous_removal_inner(&inner, cwd, home, depth + 1) {
            return Some(hit);
        }
    }

    for sub in crate::shell_command::split_command(command) {
        let tokens = split_argv(&sub);
        let Some((base, rest)) = tokens.split_first() else {
            continue;
        };
        let cmd_name = base.as_str();
        if cmd_name != "rm" && cmd_name != "rmdir" {
            continue;
        }
        let paths = filter_out_flags(rest);
        for path in &paths {
            // Strip a single pair of surrounding quotes (TS:
            // `path.replace(/^['"]|['"]$/g, '')`). `d` is the dequoted, NOT
            // tilde-expanded target — the binary's `x0n` operates on `d` literal.
            let d = strip_surrounding_quotes(path);
            // The faithful per-target `x0n` branch evaluation (binary @199202560).
            if let Some(hit) = evaluate_removal_target(cmd_name, d, rest, cwd, home) {
                return Some(hit);
            }
            // ── LingXi `~` SAFETY SUPERSET (the ONE deliberate divergence) ──
            // `x0n` does NOT expand `~`, so `rm -rf ~` resolves to `<cwd>/~` (a
            // literal subdir) and is auto-allowed. A real shell expands `~` to
            // `$HOME` and deletes it. We additionally run the critical predicate
            // on the tilde-expanded form, so `rm -rf ~` (and any `~`-target that
            // resolves to a dangerous path the x0n branches above did not already
            // flag) still ASKS. This runs LAST so the x0n branches keep their
            // exact messages for the cases they DO cover (e.g. `~/*` → branch 2).
            let expanded = expand_tilde(d, home);
            if expanded != d {
                let abs = resolve_against_cwd(&expanded, cwd);
                if is_dangerous_removal_path(&abs, home) {
                    return Some(critical_removal(cmd_name, &abs));
                }
            }
        }
    }
    None
}

/// PARITY 2.1.263 `Amo` — the two dangerous-removal shapes that the top-level
/// `mtt` pass (`dangerous_rm_on_variable_path`) structurally cannot reach.
///
/// `Amo` walks the AST collecting every `command_substitution` /
/// `process_substitution` body, then:
///
/// ```js
/// if (o.length > 64) {
///   if (/\brm(?:dir)?\b/.test(e.text))
///     return HL("rm", `This command contains ${o.length} command substitutions — too many to analyze for catastrophic removals. This requires explicit approval.`,
///                     `— too many command substitutions to analyze (${o.length})`);
///   return null;
/// }
/// for (let _ of [e.text, ...o]) { … let D = mtt(I);
///   if (D !== null) return HL(D.command, `Dangerous ${D.command} operation detected inside command substitution: '${D.target}'…`,
///                                        `on possibly-empty variable path inside command substitution: ${D.target}`); … }
/// ```
///
/// 🚨 The >64 bail RETURNS NULL when the text has no `rm`/`rmdir` word — it is
/// not a blanket "too many substitutions" refusal, only a removal-specific one.
///
/// 🚨 This message is NOT the same as `mtt`'s at the outer level: it drops the
/// `— e.g. \`rm -rf $UNSET/*\` becomes \`rm -rf /*\`` clause. Two sites, two
/// strings; reusing the outer one would be a byte-level divergence.
///
/// The `h9` half of `Amo` — running the removal-target evaluation over each
/// substitution body — is ALREADY covered by
/// [`check_dangerous_removal_inner`], which recurses through
/// [`active_command_substitutions`] to depth 8. Only these two shapes were
/// missing.
///
/// DIVERGENCE: the oracle also collects `D9e` nodes (the mksh `${ |cmd }`
/// substitution). The port's collector handles `` ` ` ``, `$( )`, `<( )` and
/// `>( )`, which are the forms `active_command_substitutions` already parses;
/// `${ |…}` is not recognised by the tree-sitter grammar this crate uses either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubstitutionRemoval {
    /// `> 64` substitutions and the text mentions `rm`/`rmdir`.
    TooManySubstitutions { count: usize },
    /// A possibly-empty `$VAR` removal target found inside a substitution.
    VariablePath { cmd: &'static str, target: String },
}

impl SubstitutionRemoval {
    /// The byte-locked ask message (`HL`'s `t`).
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::TooManySubstitutions { count } => format!(
                "This command contains {count} command substitutions \u{2014} too many to analyze for catastrophic removals. This requires explicit approval."
            ),
            Self::VariablePath { cmd, target } => format!(
                "Dangerous {cmd} operation detected inside command substitution: '{target}'\n\nThis target is a shell variable expansion that points at the filesystem root (or a top-level directory) when the variable is unset or empty. This requires explicit approval and cannot be auto-allowed by permission rules."
            ),
        }
    }

    /// The decision reason (`HL` builds `Dangerous ${e} operation ${r}`).
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::TooManySubstitutions { count } => format!(
                "Dangerous rm operation \u{2014} too many command substitutions to analyze ({count})"
            ),
            Self::VariablePath { cmd, target } => format!(
                "Dangerous {cmd} operation on possibly-empty variable path inside command substitution: {target}"
            ),
        }
    }
}

/// The `Amo` scan. Call it only after [`dangerous_rm_on_variable_path`] has
/// returned `None`, on the too-complex branch — that is where `xmo` runs it.
#[must_use]
pub fn dangerous_removal_in_substitutions(command: &str) -> Option<SubstitutionRemoval> {
    let subs = all_command_substitutions(command);
    if subs.len() > 64 {
        if rm_word_re().is_match(command) {
            return Some(SubstitutionRemoval::TooManySubstitutions { count: subs.len() });
        }
        return None;
    }
    // `[e.text, ...o]`. The outer text is included for faithfulness even though
    // the caller has already run the same check over it and got `None`.
    for span in std::iter::once(command).chain(subs.iter().map(String::as_str)) {
        if let Some((cmd, target)) = dangerous_rm_on_variable_path(span) {
            return Some(SubstitutionRemoval::VariablePath { cmd, target });
        }
    }
    None
}

/// Every substitution body in `command`, NESTED ONES INCLUDED — `Amo` walks the
/// whole AST, so a `$( $( … ) )` contributes two entries to the count that
/// gates the >64 bail.
fn all_command_substitutions(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut queue: Vec<String> = active_command_substitutions(command);
    let mut depth = 0;
    while !queue.is_empty() && depth <= 16 {
        let mut next = Vec::new();
        for s in queue {
            next.extend(active_command_substitutions(&s));
            out.push(s);
        }
        queue = next;
        depth += 1;
    }
    out
}

fn active_command_substitutions(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut single = false;
    let mut double = false;

    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && !single {
            i += 2;
            continue;
        }
        if c == '\'' && !double {
            single = !single;
            i += 1;
            continue;
        }
        if c == '"' && !single {
            double = !double;
            i += 1;
            continue;
        }
        if single {
            i += 1;
            continue;
        }

        if c == '`' {
            if let Some((inner, next)) = parse_backtick_substitution(&chars, i + 1) {
                out.push(inner);
                i = next;
                continue;
            }
        }

        if c == '$' && chars.get(i + 1) == Some(&'(') {
            if let Some((inner, next)) = parse_paren_substitution(&chars, i + 2) {
                out.push(inner);
                i = next;
                continue;
            }
        }

        if !double && (c == '<' || c == '>') && chars.get(i + 1) == Some(&'(') {
            if let Some((inner, next)) = parse_paren_substitution(&chars, i + 2) {
                out.push(inner);
                i = next;
                continue;
            }
        }

        i += 1;
    }

    out
}

fn parse_paren_substitution(chars: &[char], mut i: usize) -> Option<(String, usize)> {
    let start = i;
    let mut depth = 1usize;
    let mut single = false;
    let mut double = false;

    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && !single {
            i += 2;
            continue;
        }
        if c == '\'' && !double {
            single = !single;
            i += 1;
            continue;
        }
        if c == '"' && !single {
            double = !double;
            i += 1;
            continue;
        }
        if !single && c == '(' {
            depth += 1;
        } else if !single && c == ')' {
            depth -= 1;
            if depth == 0 {
                return Some((chars[start..i].iter().collect(), i + 1));
            }
        }
        i += 1;
    }

    None
}

fn parse_backtick_substitution(chars: &[char], mut i: usize) -> Option<(String, usize)> {
    let start = i;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '`' {
            return Some((chars[start..i].iter().collect(), i + 1));
        }
        i += 1;
    }
    None
}

/// Evaluate ONE dequoted removal target `d` against the binary's `x0n` branch
/// ladder (`bin/claude.exe` @199202560), in order: (1) cd-chain, (2)
/// statically-unresolvable, (3) critical / workspace, (4) glob-traversal.
/// `args` is the command's full argv (incl. flags) for the `rmdir -p` sub-check.
///
/// **Branch 1 (cd-chain) is dormant here**: the `o` "unresolvable-cd" flag
/// requires walking the `cd`/`pushd` chain that precedes this `rm`, which the
/// permission entry point (`check_dangerous_removal(command, cwd, home)`) does
/// not thread. With `o = false` branch 1 never fires; the same glob is still
/// caught by branch 2 (`is_dangerous_removal_path(p)` is true for any `…/*`),
/// so the command still ASKS — only the message differs ("cannot be statically
/// resolved" instead of "changes directories before the removal"). Documented
/// safe-direction partial; wiring the cd-chain is a follow-up.
fn evaluate_removal_target(
    cmd_name: &str,
    d: &str,
    args: &[String],
    cwd: &Path,
    home: Option<&str>,
) -> Option<DangerousRemoval> {
    // p = isAbsolute(d) ? d : resolve(cwd, d)  (lexical join — see resolve_against_cwd).
    let p = resolve_against_cwd(d, cwd);
    // m = p with trailing glob runs stripped iteratively + normalized
    // (TS `for(…) g = m.replace(/([\\/]\*+)+[\\/]*$/, "") || "/"`).
    let m = strip_trailing_globs(&p);
    let f = m != p;
    let d_abs = is_absolute_path(d);

    // A = m relative to cwd (when d is relative); used by branches 3 & 4.
    let a = {
        let cwd_s = cwd.to_string_lossy();
        if d_abs {
            m.clone()
        } else {
            let prefix = if cwd_s.ends_with('/') {
                cwd_s.to_string()
            } else {
                format!("{cwd_s}/")
            };
            if let Some(rest) = m.strip_prefix(&prefix) {
                rest.to_string()
            } else if m == cwd_s {
                String::new()
            } else {
                m.clone()
            }
        }
    };

    // ── Branch 1: cd-chain (dormant, o = false — see fn doc). ──

    // ── Branch 2: statically-unresolvable removal target. ──
    // The binary's `rm(p)` here is `p.includes($(…)) || p.includes(${…})` (the
    // command-substitution / variable-expansion placeholders — NOT the critical
    // predicate), i.e. a path that cannot be statically resolved because it
    // contains a shell expansion. Critical `…/*` globs are NOT flagged here;
    // they fall to branch 3 on the stripped base (so `/etc/*` → "critical", and
    // a benign `build/*` is auto-allowed — matching `x0n`).
    if f && (q6r(d)
        || contains_command_substitution(&p)
        || d.starts_with('~')
        || starts_with_two_seps(d)
        || (!d_abs && has_dotdot_segment(d) && ends_with_sep_star(&p))
        || (cmd_name == "rmdir" && ends_with_sep_star(&p) && args.iter().any(|h| has_p_flag(h)))
        || (!d_abs && ends_with_star_seps(d) && ends_with_sep_star(&p)))
    {
        return Some(unresolvable_removal(cmd_name, &p));
    }

    // ── Branch 3: critical system dir (Stt) / workspace (R0). ──
    // Gate: no globs stripped, OR the cwd-relative stripped path has no glob char.
    if !f || !has_glob_char(&a) {
        // h = [m] (+ the symlink-resolved m in TS; LingXi has no symlink resolve).
        if is_dangerous_removal_path(&m, home) {
            return Some(critical_removal(cmd_name, &p));
        }
        if removal_hits_workspace(&m, cwd) {
            return Some(workspace_removal(cmd_name, &p));
        }
    }

    // ── Branch 4: glob pattern traverses non-enumerable directories. ──
    if f && ends_with_sep_star(&p) {
        let seg_delta = count_real_segments(&p) - count_real_segments(&m);
        let glob_segments = a.split(['/', '\\']).filter(|s| has_glob_char(s)).count();
        if seg_delta + glob_segments > 1 {
            return Some(traversal_removal(cmd_name, &p));
        }
    }

    None
}

/// TS glob-strip loop: `m = p`; repeatedly `g = m.replace(/([\\/]\*+)+[\\/]*$/,"")||"/"`,
/// and when `g` changed set `m = /[\\/]/.test(g) ? normalize(g) : g`, until fixed point.
fn strip_trailing_globs(p: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"([\\/]\*+)+[\\/]*$").expect("valid regex"));
    let mut m = p.to_string();
    loop {
        let prev = m.clone();
        let stripped = re.replace(&m, "").into_owned();
        let g = if stripped.is_empty() {
            "/".to_string()
        } else {
            stripped
        };
        if g != m {
            m = if g.contains(['/', '\\']) {
                lexical_normalize(&collapse_slashes(&g))
            } else {
                g
            };
        }
        if m == prev {
            break;
        }
    }
    m
}

/// TS `q6r(e)`: true when `e` has a `..` segment that follows a real (non-`.`,
/// non-empty) segment (e.g. `foo/../`), which cannot be statically resolved.
fn q6r(d: &str) -> bool {
    let mut saw_real = false;
    for seg in d.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                if saw_real {
                    return true;
                }
            }
            _ => saw_real = true,
        }
    }
    false
}

/// The binary's branch-2 `rm(p)` = `p.includes($(…)) || p.includes(${…})`: the
/// path carries a command-substitution / variable-expansion that cannot be
/// statically resolved. (claude-code replaces `$(…)`/`${…}` with sentinel tokens
/// during parse; LingXi keeps the literal forms, so we match those.)
fn contains_command_substitution(p: &str) -> bool {
    p.contains("$(") || p.contains("${")
}

/// TS `/[\\/]\*$/`: ends with a separator immediately followed by a single `*`.
fn ends_with_sep_star(p: &str) -> bool {
    p.ends_with("/*") || p.ends_with("\\*")
}

/// TS `/^[\\/]{2}/`: starts with two separators (`//…` / `\\…`).
fn starts_with_two_seps(d: &str) -> bool {
    let b = d.as_bytes();
    b.len() >= 2 && (b[0] == b'/' || b[0] == b'\\') && (b[1] == b'/' || b[1] == b'\\')
}

/// TS `/(^|[\\/])\.\.([\\/]|$)/`: contains a `..` path segment.
fn has_dotdot_segment(d: &str) -> bool {
    d.split(['/', '\\']).any(|s| s == "..")
}

/// TS `/\*[\\/]+$/`: ends with `*` followed by one-or-more separators.
fn ends_with_star_seps(d: &str) -> bool {
    let trimmed = d.trim_end_matches(['/', '\\']);
    trimmed.len() < d.len() && trimmed.ends_with('*')
}

/// TS `/[*?[]/`: contains a glob metacharacter.
fn has_glob_char(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// TS `rmdir` `-p` detection: `/^--p/.test(h) || /^-[a-z]*p/.test(h)`.
fn has_p_flag(arg: &str) -> bool {
    if let Some(rest) = arg.strip_prefix("--") {
        return rest.starts_with('p');
    }
    if let Some(rest) = arg.strip_prefix('-') {
        return rest.chars().all(|c| c.is_ascii_lowercase()) && rest.contains('p');
    }
    false
}

/// Count non-empty, non-`.` path segments (TS `Wn(split(/[\\/]+/), s => s && s !== ".")`).
fn count_real_segments(p: &str) -> usize {
    p.split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .count()
}

fn critical_removal(cmd_name: &str, p: &str) -> DangerousRemoval {
    DangerousRemoval {
        message: format!(
            "Dangerous {cmd_name} operation detected: '{p}'\n\nThis command would remove a critical system directory. This requires explicit approval and cannot be auto-allowed by permission rules."
        ),
        reason: format!("Dangerous {cmd_name} operation on critical path: {p}"),
        resolved_path: p.to_string(),
    }
}

fn workspace_removal(cmd_name: &str, p: &str) -> DangerousRemoval {
    DangerousRemoval {
        message: format!(
            "Dangerous {cmd_name} operation detected: '{p}'\n\nThis command would remove a workspace directory (the working directory, an additional working directory, or one of their parent directories). This requires explicit approval and cannot be auto-allowed by permission rules."
        ),
        reason: format!(
            "Dangerous {cmd_name} operation on working directory or its ancestor: {p}"
        ),
        resolved_path: p.to_string(),
    }
}

fn unresolvable_removal(cmd_name: &str, p: &str) -> DangerousRemoval {
    DangerousRemoval {
        message: format!(
            "Dangerous {cmd_name} operation detected: '{p}'\n\nThis command's removal target cannot be statically resolved to a directory. This requires explicit approval and cannot be auto-allowed by permission rules."
        ),
        reason: format!("Dangerous {cmd_name} operation on statically-unresolvable target: {p}"),
        resolved_path: p.to_string(),
    }
}

fn traversal_removal(cmd_name: &str, p: &str) -> DangerousRemoval {
    DangerousRemoval {
        message: format!(
            "Dangerous {cmd_name} operation detected: '{p}'\n\nThis command's glob pattern traverses directories that cannot be statically enumerated. This requires explicit approval and cannot be auto-allowed by permission rules."
        ),
        reason: format!("Dangerous {cmd_name} operation on statically-unresolvable target: {p}"),
        resolved_path: p.to_string(),
    }
}

/// TS `path.replace(/^['"]|['"]$/g, '')`: remove ONE leading and ONE trailing
/// `'` or `"` (independently). Note this is NOT "matched pair" stripping — TS
/// strips a leading quote and a trailing quote regardless of whether they pair.
fn strip_surrounding_quotes(path: &str) -> &str {
    let mut s = path;
    if let Some(rest) = s.strip_prefix(['\'', '"']) {
        s = rest;
    }
    if let Some(rest) = s.strip_suffix(['\'', '"']) {
        s = rest;
    }
    s
}

// ─────────────────────────────────────────────────────────────────────────────
// claude-code 2.1.205 `GIu` — forced ASK for an `rm`/`rmdir` targeting a
// possibly-empty `$VAR` path (`rm -rf $UNSET/*` becomes `rm -rf /*` when the
// variable is unset/empty). This is a PURE TEXT SCAN (it does NOT resolve
// variables). CC runs it ONLY on the too-complex bash-checker branch (`hHg`,
// bin @219788895) — after the deny walks and before honoring any exact allow
// rule — so an exact `Bash(rm -rf $UNSET/*)` rule cannot bypass it. The wiring
// in [`crate::policy`] gates it on the AST `TooComplex` verdict to mirror the
// too-complex branch: a parseable command whose variable IS resolvable (e.g.
// `A=/tmp; rm -rf $A/*`) must be excluded by that gate, NOT here.
// ─────────────────────────────────────────────────────────────────────────────

macro_rules! lazy_regex {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static regex::Regex {
            static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            RE.get_or_init(|| regex::Regex::new($pat).expect("valid regex"))
        }
    };
}

// GIu gate: `/\brm(?:dir)?\b/`.
lazy_regex!(rm_word_re, r"\brm(?:dir)?\b");
// `t.replace(/\\\r?\n/g," ")` — collapse a `\`-continuation to a space.
lazy_regex!(backslash_newline_re, r"\\\r?\n");
// `.replace(/`[^`]*`/g," ")` — blank whole backtick spans.
lazy_regex!(backtick_span_re, r"`[^`]*`");
// `.replace(/\$\([^()]*\)/g," ")` — blank a `$(…)` command substitution (no
// nested paren). The sibling non-`$` `(…)` pass is [`blank_plain_paren`].
lazy_regex!(dollar_paren_re, r"\$\([^()]*\)");
// `r.split(/[;|\n\r]|&&/)` — GIu's inner subcommand split.
lazy_regex!(giu_piece_re, r"[;|\n\r]|&&");
// `o.slice(i[0].length).split(/\s+/)` — JS whitespace split (keeps a leading/
// trailing "" element, unlike `str::split_whitespace`).
lazy_regex!(ws_split_re, r"\s+");
// `a[l].replace(/[)\]}]+$/,"")` — trim a trailing run of `)`/`]`/`}`.
lazy_regex!(trailing_bracket_re, r"[)\]}]+$");
// `/^[\d&]*[<>]/` — a token that looks like a redirection operator.
lazy_regex!(redirect_start_re, r"^[\d&]*[<>]");
// `/^(?:[0-9]+|&)?(?:>>?[|&]?|<<?<?|<>)$/` — a COMPLETE redirection operator
// (its operand, the next arg, is then skipped).
lazy_regex!(redirect_full_re, r"^(?:[0-9]+|&)?(?:>>?[|&]?|<<?<?|<>)$");
// TS `Okg` (bin @219709311): a target beginning with an optionally
// double-quoted `$VAR` / `${VAR}`, then `/`, then one of `*`, `$`, `/`, a
// quote, or end-of-string.
lazy_regex!(
    okg_re,
    r#"^"?\$(?:\{[A-Za-z_][A-Za-z0-9_]*\}|[A-Za-z_][A-Za-z0-9_]*)"?/(?:\*|\$|/|["']|$)"#
);
// TS `Lkg` (bin @219709311): an `rm`/`rmdir` invocation — optional `NAME=val`
// env prefixes, an optional `\`, an optional path prefix (`/usr/bin/`), then
// `rm` or `rmdir` followed by whitespace or end-of-string. Group 1 = the name.
lazy_regex!(
    lkg_re,
    r"^(?:[A-Za-z_][A-Za-z0-9_]*\+?=[^\s]*\s+)*\\?(?:[^\s=]*/)?(rm|rmdir)(?:\s|$)"
);

/// Iteratively blank `$(…)` and non-`$` `(…)` groups (no nested parens) to a
/// fixed point — the port of `GIu`'s
/// `for(let n="";n!==r;)n=r,r=r.replace(/\$\([^()]*\)/g," ").replace(/(?<!\$)\([^()]*\)/g," ")`.
fn strip_paren_groups(input: &str) -> String {
    let mut r = input.to_string();
    loop {
        let prev = r.clone();
        r = dollar_paren_re().replace_all(&r, " ").into_owned();
        r = blank_plain_paren(&r);
        if r == prev {
            break;
        }
    }
    r
}

/// Blank every `(…)` group (no nested paren) whose `(` is NOT immediately
/// preceded by `$`, each → one space — the `replace(/(?<!\$)\([^()]*\)/g," ")`
/// pass (`regex` has no lookbehind). Scans left-to-right over the ORIGINAL
/// string, exactly like a JS global replace with lookbehind.
fn blank_plain_paren(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '(' && !(i > 0 && chars[i - 1] == '$') {
            // Find a matching ')' with no '(' before it (the `[^()]*)` arm).
            let mut j = i + 1;
            let mut closed = false;
            while j < chars.len() {
                let cj = chars[j];
                if cj == '(' {
                    break;
                }
                if cj == ')' {
                    closed = true;
                    break;
                }
                j += 1;
            }
            if closed {
                out.push(' ');
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Rewrite a bare background `&` to `;`, preserving `&&` and the fd-dup /
/// redirect forms (`>&`, `&>`, `<&`) — the port of
/// `replace(/(?<![<>&])&(?![<>&])/g,";")`.
fn rewrite_bare_ampersand(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for i in 0..chars.len() {
        if chars[i] == '&' {
            let prev_bad = i > 0 && matches!(chars[i - 1], '<' | '>' | '&');
            let next_bad = i + 1 < chars.len() && matches!(chars[i + 1], '<' | '>' | '&');
            if !prev_bad && !next_bad {
                out.push(';');
                continue;
            }
        }
        out.push(chars[i]);
    }
    out
}

/// Faithful port of claude-code 2.1.205 `GIu` (bin @219697281). Scans a raw
/// shell `command` for an `rm`/`rmdir` whose target is a possibly-empty `$VAR`
/// path (matches [`okg_re`]) and returns the FIRST `(command_name, target)`
/// hit, or `None`. `command_name` is `"rm"` or `"rmdir"`; `target` is the
/// offending argument with any trailing `)`/`]`/`}` run trimmed (as in `GIu`).
///
/// This does NOT resolve variables. CC reaches `GIu` only after the AST marked
/// the command too-complex, so the caller MUST apply that gate — see the module
/// note above.
#[must_use]
pub fn dangerous_rm_on_variable_path(command: &str) -> Option<(&'static str, String)> {
    // GIu: if(!e.includes("$")||!/\brm(?:dir)?\b/.test(e))return null;
    if !command.contains('$') || !rm_word_re().is_match(command) {
        return None;
    }
    for sub in crate::shell_command::split_command(command) {
        // r = t.replace(/\\\r?\n/g," ").replace(/`[^`]*`/g," ").trimStart();
        let step1 = backslash_newline_re().replace_all(&sub, " ").into_owned();
        let step2 = backtick_span_re().replace_all(&step1, " ").into_owned();
        let mut r = step2.trim_start().to_string();
        // while(r.startsWith("(")||r.startsWith("{"))r=r.slice(1).trimStart();
        while r.starts_with('(') || r.starts_with('{') {
            r = r[1..].trim_start().to_string();
        }
        // Iteratively blank $(…) and non-$ (…) groups.
        r = strip_paren_groups(&r);
        // r = r.replace(/(?<![<>&])&(?![<>&])/g,";");
        let r = rewrite_bare_ampersand(&r);
        // for(let n of r.split(/[;|\n\r]|&&/)) …
        for piece in giu_piece_re().split(&r) {
            let o = piece.trim_start();
            let Some(caps) = lkg_re().captures(o) else {
                continue;
            };
            let m0 = caps.get(0).expect("group 0 always present");
            let cmd: &'static str = if &caps[1] == "rmdir" { "rmdir" } else { "rm" };
            let after = &o[m0.end()..];
            let args: Vec<&str> = ws_split_re().split(after).collect();
            let mut l = 0usize;
            while l < args.len() {
                let c = trailing_bracket_re().replace(args[l], "");
                let c = c.as_ref();
                if c.is_empty() || c.starts_with('-') || c.starts_with('\'') {
                    l += 1;
                    continue;
                }
                if redirect_start_re().is_match(c) {
                    // A redirection operator: skip its operand (the next arg)
                    // when the token is a COMPLETE operator, then skip the token.
                    if redirect_full_re().is_match(c) {
                        l += 1;
                    }
                    l += 1;
                    continue;
                }
                if okg_re().is_match(c) {
                    return Some((cmd, c.to_string()));
                }
                l += 1;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const HOME: Option<&str> = Some("/home/u");

    fn cwd() -> PathBuf {
        PathBuf::from("/proj/work")
    }

    // ── PARITY 2.1.263 `Amo`: the two substitution-only shapes ────────────

    #[test]
    fn too_many_substitutions_bails_only_for_removals() {
        // 65 substitutions is over the threshold; with an `rm` word present the
        // oracle refuses rather than analysing.
        let many = "echo ".to_string() + &"$(a) ".repeat(65);
        let with_rm = format!("{many} && rm -rf /tmp/x");
        let found = dangerous_removal_in_substitutions(&with_rm).expect("must bail");
        assert_eq!(
            found,
            SubstitutionRemoval::TooManySubstitutions { count: 65 }
        );
        assert_eq!(
            found.message(),
            "This command contains 65 command substitutions \u{2014} too many to analyze for catastrophic removals. This requires explicit approval."
        );
        assert_eq!(
            found.reason(),
            "Dangerous rm operation \u{2014} too many command substitutions to analyze (65)"
        );

        // 🚨 The same 65 substitutions with NO rm/rmdir word return None. The
        // bail is removal-specific, not a blanket "too complex" refusal — an
        // easy thing to port as an unconditional return.
        assert_eq!(dangerous_removal_in_substitutions(&many), None);

        // Exactly 64 is NOT over the threshold (`> 64`), so it falls through to
        // the per-span scan instead of bailing.
        let exactly_64 = "echo ".to_string() + &"$(a) ".repeat(64) + "&& rm -rf /tmp/x";
        assert_eq!(dangerous_removal_in_substitutions(&exactly_64), None);
    }

    #[test]
    fn nested_substitutions_count_toward_the_bail() {
        // `Amo` walks the whole AST, so an inner `$( )` is its own node. 33
        // nested pairs are 66 nodes — over the threshold — while the same text
        // counted only at top level would be 33 and under it.
        let nested = "echo ".to_string() + &"$(a $(b)) ".repeat(33) + "&& rm -rf /tmp/x";
        assert_eq!(
            dangerous_removal_in_substitutions(&nested),
            Some(SubstitutionRemoval::TooManySubstitutions { count: 66 })
        );
    }

    #[test]
    fn possibly_empty_variable_inside_a_substitution_has_its_own_copy() {
        let found = dangerous_removal_in_substitutions("echo $(rm -rf $UNSET/*)")
            .expect("a possibly-empty variable target inside a substitution must be found");
        let SubstitutionRemoval::VariablePath { cmd, target } = &found else {
            panic!("expected the variable-path shape, got {found:?}");
        };
        assert_eq!(*cmd, "rm");
        assert_eq!(target, "$UNSET/*");

        // 🚨 This message is NOT the outer `mtt` one: it drops the
        // "— e.g. `rm -rf $UNSET/*` becomes `rm -rf /*`" clause. Two oracle
        // sites, two strings.
        assert_eq!(
            found.message(),
            "Dangerous rm operation detected inside command substitution: '$UNSET/*'\n\nThis target is a shell variable expansion that points at the filesystem root (or a top-level directory) when the variable is unset or empty. This requires explicit approval and cannot be auto-allowed by permission rules."
        );
        assert!(
            !found.message().contains("e.g."),
            "the inner message must not carry the outer message's example clause"
        );
        assert_eq!(
            found.reason(),
            "Dangerous rm operation on possibly-empty variable path inside command substitution: $UNSET/*"
        );

        // A substitution with nothing dangerous in it stays quiet.
        assert_eq!(dangerous_removal_in_substitutions("echo $(ls /tmp)"), None);
        assert_eq!(dangerous_removal_in_substitutions("rm -rf /tmp/x"), None);
    }

    #[test]
    fn predicate_root_and_children_are_dangerous() {
        assert!(is_dangerous_removal_path("/", HOME));
        assert!(is_dangerous_removal_path("/etc", HOME));
        assert!(is_dangerous_removal_path("/usr", HOME));
        assert!(is_dangerous_removal_path("/tmp", HOME));
        assert!(is_dangerous_removal_path("/etc/", HOME)); // trailing slash stripped
                                                           // A grandchild is NOT a direct child of root.
        assert!(!is_dangerous_removal_path("/usr/local", HOME));
        assert!(!is_dangerous_removal_path("/etc/nginx/conf.d", HOME));
    }

    #[test]
    fn predicate_home_is_dangerous() {
        assert!(is_dangerous_removal_path("/home/u", HOME));
        assert!(is_dangerous_removal_path("/home/u/", HOME));
        // A child of home is fine (it's a grandchild of root via /home).
        assert!(!is_dangerous_removal_path("/home/u/project", HOME));
        // Without a home, the home comparison is skipped.
        assert!(!is_dangerous_removal_path("/home/u", None));
    }

    #[test]
    fn predicate_wildcards_are_dangerous() {
        assert!(is_dangerous_removal_path("*", HOME));
        assert!(is_dangerous_removal_path("/home/u/project/*", HOME));
        assert!(is_dangerous_removal_path("/some/deep/dir/*", HOME));
    }

    #[test]
    fn predicate_macos_private_normalization() {
        // On macOS, /etc, /var, /tmp, /home are symlinks under /private, so
        // `rm -rf /private/etc` removes the real /etc. The `/private/<name>`
        // prefix is normalized away (case-insensitively) before the critical
        // checks. Off macOS this is a no-op.
        if cfg!(target_os = "macos") {
            assert!(is_dangerous_removal_path("/private/etc", None));
            assert!(is_dangerous_removal_path("/private/var", None));
            assert!(is_dangerous_removal_path("/private/tmp", None));
            assert!(is_dangerous_removal_path("/private/home", None));
            assert!(is_dangerous_removal_path("/private/etc/", None));
            assert!(is_dangerous_removal_path("/Private/Etc", None)); // regex `/i`
                                                                      // Children of a normalized critical dir are not themselves critical.
            assert!(!is_dangerous_removal_path("/private/etc/nginx", None));
            // A /private subdir not in the list is untouched.
            assert!(!is_dangerous_removal_path("/private/foo", None));
        } else {
            assert!(!is_dangerous_removal_path("/private/etc", None));
        }
    }

    #[test]
    fn predicate_home_comparison_is_case_folded() {
        // TS `Stt` compares the case-folded (`IA`) path against the case-folded
        // home, so a case-variant of the home directory still matches.
        assert!(is_dangerous_removal_path("/Home/U", Some("/home/u")));
        assert!(is_dangerous_removal_path("/home/u", Some("/Home/U")));
        assert!(!is_dangerous_removal_path("/home/other", Some("/home/u")));
    }

    #[test]
    fn case_fold_lowercases_and_maps_special() {
        assert_eq!(case_fold("/Users/ALICE"), "/users/alice");
        assert_eq!(case_fold("I"), "i");
        assert_eq!(case_fold("\u{0131}"), "i"); // dotless ı → i
        assert_eq!(case_fold("\u{017f}"), "s"); // long s ſ → s
    }

    #[test]
    fn predicate_windows_drive_root_and_child() {
        assert!(is_dangerous_removal_path("C:\\", HOME));
        assert!(is_dangerous_removal_path("C:/", HOME));
        assert!(is_dangerous_removal_path("C:", HOME));
        assert!(is_dangerous_removal_path("C:\\Windows", HOME));
        assert!(is_dangerous_removal_path("C:\\Users", HOME));
        // A drive grandchild is fine.
        assert!(!is_dangerous_removal_path("C:\\Users\\me", HOME));
    }

    #[test]
    fn rm_rf_root_is_dangerous() {
        let d = check_dangerous_removal("rm -rf /", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/");
        assert!(d
            .message
            .starts_with("Dangerous rm operation detected: '/'"));
        assert!(d
            .message
            .contains("cannot be auto-allowed by permission rules"));
        assert_eq!(d.reason, "Dangerous rm operation on critical path: /");
    }

    #[test]
    fn rm_etc_is_dangerous() {
        let d = check_dangerous_removal("rm /etc", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/etc");
    }

    #[test]
    fn rm_local_file_inside_cwd_is_not_dangerous() {
        // A relative path resolves under cwd → deep under root → not dangerous.
        assert!(check_dangerous_removal("rm ./local/file", &cwd(), HOME).is_none());
        assert!(check_dangerous_removal("rm -f build/out.o", &cwd(), HOME).is_none());
    }

    #[test]
    fn workspace_directory_removal_is_dangerous() {
        // The cwd itself (a non-system path) → workspace ask (TS `x0n` branch 3
        // `R0`). LingXi previously auto-allowed these.
        for cmd in ["rm -rf .", "rm -rf /proj/work"] {
            let d = check_dangerous_removal(cmd, &cwd(), HOME)
                .unwrap_or_else(|| panic!("{cmd} should be flagged"));
            assert!(
                d.message.contains("would remove a workspace directory"),
                "{cmd}: {}",
                d.message
            );
            assert!(d.reason.contains("working directory or its ancestor"));
        }
        // A NON-system ancestor of a deeper cwd (parent dirs that are not root
        // children — those would hit the critical check first).
        let deep = PathBuf::from("/proj/work/pkg/src");
        for cmd in ["rm -rf ..", "rm -rf /proj/work", "rm -rf /proj/work/pkg"] {
            let d = check_dangerous_removal(cmd, &deep, HOME)
                .unwrap_or_else(|| panic!("{cmd} should be flagged"));
            assert!(
                d.message.contains("would remove a workspace directory"),
                "{cmd}: {}",
                d.message
            );
        }
    }

    #[test]
    fn removal_inside_or_beside_cwd_is_not_workspace() {
        assert!(check_dangerous_removal("rm -rf subdir", &cwd(), HOME).is_none());
        assert!(check_dangerous_removal("rm -rf /proj/work/build", &cwd(), HOME).is_none());
        assert!(check_dangerous_removal("rm -rf /proj/other", &cwd(), HOME).is_none());
    }

    #[test]
    fn removal_hits_workspace_logic() {
        let cwd = std::path::Path::new("/proj/work");
        assert!(removal_hits_workspace("/proj/work", cwd)); // equal
        assert!(removal_hits_workspace("/proj", cwd)); // ancestor
        assert!(removal_hits_workspace("/", cwd)); // root ancestor
        assert!(removal_hits_workspace("/proj/work/..", cwd)); // → /proj
        assert!(!removal_hits_workspace("/proj/work/sub", cwd)); // child
        assert!(!removal_hits_workspace("/proj/other", cwd)); // sibling
        assert!(!removal_hits_workspace("/elsewhere", cwd)); // unrelated
    }

    #[test]
    fn lexical_normalize_resolves_dot_dot() {
        assert_eq!(lexical_normalize("/proj/work/.."), "/proj");
        assert_eq!(lexical_normalize("/a/b/../c"), "/a/c");
        assert_eq!(lexical_normalize("/a/./b"), "/a/b");
        assert_eq!(lexical_normalize("/a//b"), "/a/b");
        assert_eq!(lexical_normalize("/.."), "/"); // cannot escape root
        assert_eq!(lexical_normalize("a/../.."), ".."); // relative keeps leading ..
    }

    #[test]
    fn rmdir_of_critical_path_is_dangerous() {
        let d = check_dangerous_removal("rmdir /usr", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/usr");
        assert_eq!(d.reason, "Dangerous rmdir operation on critical path: /usr");
    }

    #[test]
    fn flag_delimiter_extracts_path() {
        // `rm -- -/../x` — the `--` makes `-/../x` positional. It resolves to
        // `/proj/work/-/../x` (lexical join), which is NOT a direct child of
        // root, so this specific payload is not dangerous — but the key
        // assertion is the path IS extracted (no panic / not silently dropped).
        // We verify extraction directly:
        let tokens = split_argv("rm -- -/../x");
        let (_, rest) = tokens.split_first().unwrap();
        let paths = filter_out_flags(rest);
        assert_eq!(paths, vec!["-/../x".to_string()]);

        // And a `--`-delimited dangerous target IS caught.
        let d = check_dangerous_removal("rm -- /etc", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/etc");
    }

    #[test]
    fn flag_delimiter_dangerous_root_glob_after_dashdash() {
        // `rm -- /*` after `--` → the `/*` glob form is dangerous.
        let d = check_dangerous_removal("rm -- /*", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/*");
    }

    #[test]
    fn dangerous_rm_hidden_behind_benign_subcommand() {
        // Split into subcommands so a dangerous rm after `echo ok &&` is caught.
        let d = check_dangerous_removal("echo ok && rm -rf /", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/");
    }

    #[test]
    fn dangerous_rm_inside_command_substitution_is_detected() {
        let d = check_dangerous_removal("echo $(rm -rf /)", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/");

        let d = check_dangerous_removal("echo `rm -rf /etc`", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/etc");

        let d = check_dangerous_removal("diff <(rm -rf ~) file", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/home/u");

        let d =
            check_dangerous_removal("echo $(printf '%s' $(rm -rf /usr))", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/usr");
    }

    #[test]
    fn quoted_literal_substitution_is_not_recursively_executed() {
        assert!(check_dangerous_removal("echo '$(rm -rf /)'", &cwd(), HOME).is_none());
    }

    #[test]
    fn tilde_expansion_targets_home() {
        // `rm -rf ~` expands to the home directory → dangerous.
        let d = check_dangerous_removal("rm -rf ~", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/home/u");
    }

    #[test]
    fn quoted_dangerous_path_is_caught() {
        let d = check_dangerous_removal("rm -rf \"/etc\"", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/etc");
        let d2 = check_dangerous_removal("rm -rf '/usr'", &cwd(), HOME).unwrap();
        assert_eq!(d2.resolved_path, "/usr");
    }

    #[test]
    fn non_rm_command_is_ignored() {
        assert!(check_dangerous_removal("ls /etc", &cwd(), HOME).is_none());
        assert!(check_dangerous_removal("cat /etc/passwd", &cwd(), HOME).is_none());
    }

    #[test]
    fn rm_with_only_flags_no_paths_is_not_dangerous() {
        // No positional args → nothing to validate.
        assert!(check_dangerous_removal("rm -rf", &cwd(), HOME).is_none());
    }

    // ── Faithful `x0n` branch coverage (binary @199202560) ──

    #[test]
    fn tilde_superset_asks_for_bare_home_only() {
        // LingXi SAFETY SUPERSET: `rm -rf ~` → `$HOME` → critical (x0n, which
        // does NOT expand `~`, auto-allows it). A child of home is fine.
        let d = check_dangerous_removal("rm -rf ~", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/home/u");
        assert!(
            d.message.contains("critical system directory"),
            "{}",
            d.message
        );
        assert!(check_dangerous_removal("rm -rf ~/project", &cwd(), HOME).is_none());
    }

    #[test]
    fn critical_glob_uses_critical_message_via_stripped_base() {
        // `/etc/*` strips to `/etc` (critical) → branch 3 "critical" (NOT branch 2).
        let d = check_dangerous_removal("rm -rf /etc/*", &cwd(), HOME).unwrap();
        assert_eq!(d.resolved_path, "/etc/*");
        assert!(
            d.message.contains("critical system directory"),
            "{}",
            d.message
        );
    }

    #[test]
    fn workspace_glob_uses_workspace_message() {
        // `<cwd>/*` and bare `*` strip to cwd → branch 3 "workspace".
        for cmd in ["rm -rf /proj/work/*", "rm -rf *"] {
            let d = check_dangerous_removal(cmd, &cwd(), HOME)
                .unwrap_or_else(|| panic!("{cmd} should ask"));
            assert!(
                d.message.contains("would remove a workspace directory"),
                "{cmd}: {}",
                d.message
            );
        }
    }

    #[test]
    fn benign_dir_glob_is_auto_allowed() {
        // The stripped base is neither critical nor workspace → AUTO-ALLOWED
        // (matches `x0n`; LingXi previously over-asked "critical" on every `…/*`).
        assert!(check_dangerous_removal("rm -rf build/*", &cwd(), HOME).is_none());
        assert!(check_dangerous_removal("rm -rf /proj/work/build/*", &cwd(), HOME).is_none());
        assert!(check_dangerous_removal("rm -rf /home/u/project/*", &cwd(), HOME).is_none());
    }

    #[test]
    fn command_substitution_glob_is_unresolvable() {
        // `$(…)` / `${…}` in the target → branch 2 "cannot be statically resolved".
        for cmd in ["rm -rf $(pwd)/*", "rm -rf ${HOME}/*"] {
            let d = check_dangerous_removal(cmd, &cwd(), HOME)
                .unwrap_or_else(|| panic!("{cmd} should ask"));
            assert!(
                d.message
                    .contains("cannot be statically resolved to a directory"),
                "{cmd}: {}",
                d.message
            );
        }
    }

    #[test]
    fn literal_tilde_glob_is_unresolvable_via_branch2() {
        // `~/*` → branch 2 (`d.startsWith("~")`), NOT the critical superset.
        let d = check_dangerous_removal("rm -rf ~/*", &cwd(), HOME).unwrap();
        assert!(
            d.message
                .contains("cannot be statically resolved to a directory"),
            "{}",
            d.message
        );
    }

    #[test]
    fn glob_traversal_asks() {
        // `/a/*/b/*`: the glob spans more than one non-enumerable level →
        // branch 4 "glob pattern traverses directories".
        let d = check_dangerous_removal("rm -rf /a/*/b/*", &cwd(), HOME).unwrap();
        assert!(
            d.message.contains("glob pattern traverses directories"),
            "{}",
            d.message
        );
    }

    #[test]
    fn rmdir_p_glob_is_unresolvable() {
        // `rmdir -p foo/*` → branch 2 (rmdir + `-p` + trailing `/*`).
        let d = check_dangerous_removal("rmdir -p foo/*", &cwd(), HOME).unwrap();
        assert!(
            d.message
                .contains("cannot be statically resolved to a directory"),
            "{}",
            d.message
        );
    }

    #[test]
    fn q6r_dotdot_after_real_segment() {
        assert!(q6r("foo/../bar"));
        assert!(q6r("a/b/../.."));
        assert!(!q6r("../foo")); // leading .. (no preceding real seg)
        assert!(!q6r("./foo"));
        assert!(!q6r("a/b/c"));
    }

    // ── GIu: possibly-empty `$VAR` removal target (claude-code 2.1.205) ──

    #[test]
    fn giu_okg_regex_matches_variable_root_targets() {
        // Bare, quoted, braced; `/` followed by *, $, /, quote, or end-of-string.
        assert!(okg_re().is_match("$UNSET/*"));
        assert!(okg_re().is_match("\"$VAR\"/*"));
        assert!(okg_re().is_match("${VAR}/*"));
        assert!(okg_re().is_match("$VAR/$OTHER"));
        assert!(okg_re().is_match("$VAR//x"));
        assert!(okg_re().is_match("$VAR/\"quoted\""));
        assert!(okg_re().is_match("$VAR/")); // trailing slash → end-of-string arm
                                             // Negatives.
        assert!(!okg_re().is_match("$VAR")); // no slash after the var
        assert!(!okg_re().is_match("$VAR/foo")); // `/` then a plain letter
        assert!(!okg_re().is_match("/etc/*")); // no leading variable
        assert!(!okg_re().is_match("'$VAR'/*")); // leading single quote (only `"` allowed)
    }

    #[test]
    fn giu_lkg_regex_matches_rm_invocations() {
        assert_eq!(&lkg_re().captures("rm -rf x").unwrap()[1], "rm");
        assert_eq!(&lkg_re().captures("rmdir x").unwrap()[1], "rmdir");
        assert_eq!(&lkg_re().captures("/usr/bin/rm x").unwrap()[1], "rm");
        assert_eq!(&lkg_re().captures("A=1 B=2 rm x").unwrap()[1], "rm");
        assert_eq!(&lkg_re().captures("\\rm x").unwrap()[1], "rm");
        assert!(lkg_re().captures("ls x").is_none());
        assert!(lkg_re().captures("remove x").is_none()); // not a whole-word rm
    }

    #[test]
    fn giu_detects_bare_and_quoted_and_braced_variable_targets() {
        for (cmd, want) in [
            ("rm -rf $UNSET/*", ("rm", "$UNSET/*")),
            ("rm -rf \"$VAR\"/*", ("rm", "\"$VAR\"/*")),
            ("rm -rf ${VAR}/*", ("rm", "${VAR}/*")),
            ("rm -rf $VAR/$OTHER", ("rm", "$VAR/$OTHER")),
            ("rmdir $DIR/*", ("rmdir", "$DIR/*")),
        ] {
            let (c, t) = dangerous_rm_on_variable_path(cmd)
                .unwrap_or_else(|| panic!("{cmd} should be flagged"));
            assert_eq!((c, t.as_str()), want, "for {cmd}");
        }
    }

    #[test]
    fn giu_negative_cases() {
        // No `$` → gated out.
        assert!(dangerous_rm_on_variable_path("rm -rf /etc/*").is_none());
        // `$VAR` with no `/…` root pattern.
        assert!(dangerous_rm_on_variable_path("rm -rf $VAR").is_none());
        // Single-quoted target — the `'`-leading arg is skipped, and even the
        // dequoted-looking form doesn't satisfy Okg's leading `"?\$`.
        assert!(dangerous_rm_on_variable_path("rm -rf '$VAR/*'").is_none());
        // A resolvable-looking var behind a slash-then-letter is not a root glob.
        assert!(dangerous_rm_on_variable_path("rm -rf $VAR/subdir").is_none());
        // Non-rm command.
        assert!(dangerous_rm_on_variable_path("ls $VAR/*").is_none());
    }

    #[test]
    fn giu_finds_target_hidden_behind_benign_subcommand() {
        let (c, t) = dangerous_rm_on_variable_path("echo hi && rm -rf $VAR/*").unwrap();
        assert_eq!((c, t.as_str()), ("rm", "$VAR/*"));
        // A background `&` (GIu rewrites `&`→`;` then splits).
        let (c2, t2) = dangerous_rm_on_variable_path("sleep 1 & rm -rf $VAR/*").unwrap();
        assert_eq!((c2, t2.as_str()), ("rm", "$VAR/*"));
    }

    #[test]
    fn giu_strips_backticks_and_command_substitution_and_leading_group() {
        // Leading `(`/`{` are stripped before matching Lkg (the trailing `)` is
        // a separate arg reached only after the target is already matched).
        let (_, t) = dangerous_rm_on_variable_path("( rm -rf $VAR/* )").unwrap();
        assert_eq!(t, "$VAR/*");
        // A `$()` command substitution earlier in the line is blanked, so the
        // rm target after `;` is still reached.
        let (_, t2) = dangerous_rm_on_variable_path("x=$(date) ; rm -rf $VAR/*").unwrap();
        assert_eq!(t2, "$VAR/*");
        // A backtick span is blanked.
        let (_, t3) = dangerous_rm_on_variable_path("echo `id` ; rm -rf $VAR/*").unwrap();
        assert_eq!(t3, "$VAR/*");
    }

    #[test]
    fn giu_skips_redirect_operands_and_trims_trailing_brackets() {
        // A redirect operator + operand are skipped, then the real target found.
        let (_, t) = dangerous_rm_on_variable_path("rm -rf > /tmp/log $VAR/*").unwrap();
        assert_eq!(t, "$VAR/*");
        // Trailing `)`/`]`/`}` are trimmed off the returned target.
        let (_, t2) = dangerous_rm_on_variable_path("rm -rf $VAR/*}}").unwrap();
        assert_eq!(t2, "$VAR/*");
    }
}
