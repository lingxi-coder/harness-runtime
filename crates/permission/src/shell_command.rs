//! Shell command ↔ permission-rule evaluation — faithful port of the CORE of
//! claude-code `src/tools/BashTool/bashPermissions.ts`
//! (`filterRulesByContentsMatchingInput` + the compound aggregation in
//! `bashToolCheckPermission` / `bashToolHasPermission`).
//!
//! A shell command is split into subcommands (on `&&`, `||`, `;`, `;;`, `|`,
//! quote-aware) and each subcommand is matched against rule CONTENT via
//! [`crate::shell_rule_matching`]. The two aggregations differ by design:
//!
//! - **deny / ask** — the command matches a rule if ANY subcommand matches,
//!   after AGGRESSIVE stripping (safe wrappers + ALL leading env-var prefixes).
//!   A denied command must stay denied even when wrapped (`FOO=bar denied`,
//!   `timeout 5 denied`, `echo ok && denied`). (claude-code `stripAllEnvVars:
//!   true`.)
//! - **allow** — the command is allowed only if EVERY subcommand is covered by
//!   some allow rule, after SAFE-LIST stripping only. One allow rule matching
//!   one subcommand of `echo ok && rm -rf /` must NOT allow the whole command.
//!
//! ## Deferred (documented)
//! Tree-sitter AST parsing (we use the quote-aware delimiter splitter — the
//! analogue of claude-code `splitCommand_DEPRECATED`), `checkPathConstraints`
//! (redirection-target / `cd` path validation, heredoc extraction), sed/mode/
//! read-only validation, and the ANT-only env-var safelist. Output-redirection
//! stripping is best-effort here (used only to let `Bash(python:*)` match
//! `python x.py > out`); full quote-aware redirection parsing lives with the
//! deferred path-constraint check. None of the deferrals can WEAKEN a deny —
//! they only make matching more conservative (fall through to ask).

use crate::shell_rule_matching::{parse_shell_rule, ShellRule};
use regex::Regex;
use std::collections::BTreeSet;
use std::sync::OnceLock;

/// Extract the `command` string from a shell tool's JSON input.
#[must_use]
pub fn command_from_input(input: &serde_json::Value) -> Option<&str> {
    input.get("command").and_then(serde_json::Value::as_str)
}

/// Is this a shell tool whose content rules are command patterns?
///
/// `Bash` (desktop) + `PowerShell` (Windows) + `Shell` — the Android mobile
/// shell tool (`tools/shell-mobile` `TOOL_NAME`), which runs mksh/sh-compatible
/// commands through the in-engine sandbox and so must get the SAME per-command
/// narrowing + dangerous-rule analysis + content matching as desktop `Bash`.
#[must_use]
pub fn is_shell_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Bash" | "PowerShell" | "Shell")
}

/// Split `command` into subcommands on the shell list separators
/// `&&`, `||`, `;;`, `;`, `|` and a bare newline — quote- and escape-aware so
/// separators inside `'...'` / `"..."` or after `\` are NOT split points. The
/// analogue of claude-code `splitCommand_DEPRECATED` (likewise quote-aware and
/// newline-splitting via its tokenizer). Background `&` is NOT a separator
/// (matches `COMMAND_LIST_SEPARATORS`).
///
/// A bash newline is a command separator UNLESS it is a `\`-continuation. The
/// escape arm below consumes `\<newline>` as a pair (odd backslash =
/// continuation → joined), while a bare or even-backslash-preceded `\n` reaches
/// the separator arm — reproducing claude-code's continuation rule. Lines that
/// are entirely `#` comments are dropped (claude-code `stripCommentLines`).
#[must_use]
pub fn split_command(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < chars.len() {
        let c = chars[i];
        if in_single {
            cur.push(c);
            if c == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            cur.push(c);
            if c == '\\' && i + 1 < chars.len() {
                // In double quotes a backslash escapes the next char; keep both.
                cur.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            '\\' if i + 1 < chars.len() => {
                cur.push(c);
                cur.push(chars[i + 1]);
                i += 2;
                continue;
            }
            '\'' => {
                in_single = true;
                cur.push(c);
            }
            '"' => {
                in_double = true;
                cur.push(c);
            }
            _ => {
                // Two-char separators first.
                let two = if i + 1 < chars.len() {
                    Some((c, chars[i + 1]))
                } else {
                    None
                };
                match two {
                    Some(('&', '&') | ('|', '|') | (';', ';')) => {
                        parts.push(std::mem::take(&mut cur));
                        i += 2;
                        continue;
                    }
                    _ if c == ';' || c == '|' || c == '\n' => {
                        parts.push(std::mem::take(&mut cur));
                        i += 1;
                        continue;
                    }
                    _ => cur.push(c),
                }
            }
        }
        i += 1;
    }
    parts.push(cur);
    parts
        .into_iter()
        .map(|s| s.trim().to_string())
        // Drop empties and full-line `#` comments (claude-code `stripCommentLines`).
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
        .collect()
}

/// Best-effort strip of output redirections (`> f`, `>> f`, `2>&1`, `2> f`,
/// `&> f`) so `Bash(python:*)` matches `python x.py > out.txt`. Simplified
/// relative to claude-code's quote-aware/heredoc extractor (that machinery is
/// part of the deferred path-constraint check); only affects whether an ALLOW
/// rule matches, never a deny.
///
/// Also reused by the 2c bash-safety layer ([`crate::policy`]) to feed each
/// subcommand to the injection validators with its redirect already stripped —
/// matching claude-code, where `splitCommand` yields redirect-stripped
/// subcommands before `bashCommandIsSafe` runs and `checkPathConstraints`
/// validates the redirect target separately.
pub(crate) fn strip_output_redirections(cmd: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // `N>&M`, `N>>`/`N>` followed by an optional target token, `&>`/`&>>`.
        Regex::new(r"\s*(?:[0-9]*>&[0-9-]+|&>>?|[0-9]*>>?)\s*[^\s|&;<>]*").unwrap()
    });
    re.replace_all(cmd.trim(), "").trim().to_string()
}

/// claude-code `BINARY_HIJACK_VARS = /^(LD_|DYLD_|PATH$)/` (bashPermissions.ts:708).
/// Env vars that can make a *different binary* run (injection / resolution
/// hijack). Matches ANY `LD_*`, ANY `DYLD_*`, and exactly `PATH`.
#[must_use]
pub fn is_binary_hijack_var(name: &str) -> bool {
    name.starts_with("LD_") || name.starts_with("DYLD_") || name == "PATH"
}

/// claude-code `stripCommentLines` (bashPermissions.ts:508-522): drop lines whose
/// `trim()` is empty or starts with `#`. If NOTHING survives, return the ORIGINAL
/// command unchanged.
#[must_use]
fn strip_comment_lines(command: &str) -> String {
    let kept: Vec<&str> = command
        .split('\n')
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .collect();
    if kept.is_empty() {
        command.to_string()
    } else {
        kept.join("\n")
    }
}

/// PARITY 2.1.263 `sle` — the canonical safe-env-var set, verified entry-for-entry
/// and IN ORDER against the binary. It has three consumers in the oracle, so it
/// lives here once rather than per-consumer:
///
///   * `EC()` / `stripSafeWrappers` — strip a leading `NAME=value` prefix before
///     permission and exclusion matching (this file);
///   * `KQt`'s prefix narrowing — an unsafe NAME pins the suggestion to the EXACT
///     command so a `SECRET=… cmd` grant never widens to `cmd *`
///     (`allow_suggestion.rs`, `sandbox_auto_allow.rs`);
///   * `eO()` under the read block — an unsafe NAME makes the command
///     unanalyzable (`command_path_containment.rs`).
///
/// 🚨 NOT to be confused with [`crate::bash_ast_security::SAFE_ENV_VARS`], which
/// is a different table under the same oracle name: shell-CONTROLLED variables
/// (`BASHPID`, …) whose value the parser can resolve. Same spelling, other layer.
///
/// The ANT-only set (`bashPermissions.ts:447-497`) is intentionally NOT ported.
pub(crate) const SAFE_ENV_VARS: &[&str] = &[
    // Go
    "GOEXPERIMENT",
    "GOOS",
    "GOARCH",
    "CGO_ENABLED",
    "GO111MODULE",
    // Rust
    "RUST_BACKTRACE",
    "RUST_LOG",
    // Node
    "NODE_ENV",
    // Python
    "PYTHONUNBUFFERED",
    "PYTHONDONTWRITEBYTECODE",
    // Pytest
    "PYTEST_DISABLE_PLUGIN_AUTOLOAD",
    "PYTEST_DEBUG",
    // API keys
    "ANTHROPIC_API_KEY",
    // Locale / encoding
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_TIME",
    "CHARSET",
    // Terminal / display
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "FORCE_COLOR",
    "TZ",
    // Color config
    "LS_COLORS",
    "LSCOLORS",
    "GREP_COLOR",
    "GREP_COLORS",
    "GCC_COLORS",
    // Display formatting
    "TIME_STYLE",
    "BLOCK_SIZE",
    "BLOCKSIZE",
    // Terminal geometry / CI / non-interactive
    "COLUMNS",
    "LINES",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "CI",
    "DEBIAN_FRONTEND",
    "GIT_TERMINAL_PROMPT",
];

/// PARITY 2.1.263 `eO(name)` — `return sle.has(name) || !1`. The `|| false` is a
/// truthiness coercion on a `Set.has` that already returns a boolean, so this is
/// a plain membership test.
#[must_use]
pub(crate) fn is_safe_env_var(name: &str) -> bool {
    SAFE_ENV_VARS.contains(&name)
}

/// claude-code `stripSafeWrappers` (bashPermissions.ts:524-615). Two-phase
/// fixed-point: Phase 1 strips leading SAFE_ENV_VARS assignments + comment
/// lines; Phase 2 strips wrapper commands (`timeout` with GNU flags, `time`,
/// `nice` bare/`-N`/`-n N`, `stdbuf`, `nohup`) + comment lines, never env vars.
///
/// NOTE: `sudo`/`env`/`sudo -E` are deliberately NOT wrappers here — `env bash
/// -c evil` must stay caught (see TS `BARE_SHELL_PREFIXES`).
#[must_use]
pub fn strip_safe_wrappers(command: &str) -> String {
    static ENV_VAR_PATTERN: OnceLock<Regex> = OnceLock::new(); // TS:575
    let env_re = ENV_VAR_PATTERN.get_or_init(|| {
        Regex::new(r"^([A-Za-z_][A-Za-z0-9_]*)=([A-Za-z0-9_./:-]+)[ \t]+").unwrap()
    });
    static WRAPPERS: OnceLock<Vec<Regex>> = OnceLock::new(); // TS:532-560
    let wrappers = WRAPPERS.get_or_init(|| {
        vec![
            Regex::new(r"^timeout[ \t]+(?:(?:--(?:foreground|preserve-status|verbose)|--(?:kill-after|signal)=[A-Za-z0-9_.+-]+|--(?:kill-after|signal)[ \t]+[A-Za-z0-9_.+-]+|-v|-[ks][ \t]+[A-Za-z0-9_.+-]+|-[ks][A-Za-z0-9_.+-]+)[ \t]+)*(?:--[ \t]+)?\d+(?:\.\d+)?[smhd]?[ \t]+").unwrap(),
            Regex::new(r"^time[ \t]+(?:--[ \t]+)?").unwrap(),
            Regex::new(r"^nice(?:[ \t]+-n[ \t]+-?\d+|[ \t]+-\d+)?[ \t]+(?:--[ \t]+)?").unwrap(),
            Regex::new(r"^stdbuf(?:[ \t]+-[ioe][LN0-9]+)+[ \t]+(?:--[ \t]+)?").unwrap(),
            Regex::new(r"^nohup[ \t]+(?:--[ \t]+)?").unwrap(),
        ]
    });

    let mut stripped = command.to_string();
    // Phase 1: leading SAFE env vars + comment lines.
    let mut previous = String::new();
    while stripped != previous {
        previous = stripped.clone();
        stripped = strip_comment_lines(&stripped);
        if let Some(c) = env_re.captures(&stripped) {
            let name = c.get(1).unwrap().as_str();
            if SAFE_ENV_VARS.contains(&name) {
                let end = c.get(0).unwrap().end();
                stripped = stripped[end..].to_string();
            }
        }
    }
    // Phase 2: wrapper commands + comment lines. Do NOT strip env vars here.
    previous = String::new();
    while stripped != previous {
        previous = stripped.clone();
        stripped = strip_comment_lines(&stripped);
        for w in wrappers.iter() {
            stripped = w.replace(&stripped, "").into_owned();
        }
    }
    stripped.trim().to_string()
}

/// claude-code `stripAllLeadingEnvVars(command, blocklist?)` (bashPermissions.ts
/// :733-776). Iteratively strips each leading `KEY=value` token; if
/// `blocklist(KEY)` is true it BREAKS, leaving that var and the rest in place.
///
/// - `blocklist = None` => strip every leading env var (deny/ask path).
/// - `blocklist = Some(is_binary_hijack_var)` => excludedCommands path: stop at
///   the first `LD_*`/`DYLD_*`/`PATH` assignment.
#[must_use]
pub fn strip_all_leading_env_vars(command: &str, blocklist: Option<fn(&str) -> bool>) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    // KEY or KEY[idx], optional `+=`/`=`, then a (quoted or bare) value, then
    // mandatory horizontal whitespace. Mirrors claude-code's ENV_VAR_PATTERN.
    let re = RE.get_or_init(|| {
        Regex::new(r#"^([A-Za-z_][A-Za-z0-9_]*(?:\[[^\]]*\])?)\+?=(?:'[^'\n\r]*'|"(?:\\.|[^"$`\\\n\r])*"|\\.|[^ \t\n\r$`;|&()<>\\'"])*[ \t]+"#).unwrap()
    });
    let mut stripped = command.to_string();
    let mut previous = String::new();
    while stripped != previous {
        previous = stripped.clone();
        stripped = strip_comment_lines(&stripped);
        if let Some(m) = re.captures(&stripped) {
            let name = m.get(1).unwrap().as_str();
            if let Some(bl) = blocklist {
                if bl(name) {
                    break;
                }
            }
            let end = m.get(0).unwrap().end();
            stripped = stripped[end..].to_string();
        }
    }
    stripped.trim().to_string()
}

/// claude-code `containsExcludedCommand` candidate worklist
/// (shouldUseSandbox.ts:82-101): fixed-point over
/// `stripAllLeadingEnvVars(_, BINARY_HIJACK_VARS)` and `stripSafeWrappers`.
/// Returns the deduped candidate list (the trimmed original is included).
#[must_use]
pub fn strip_env_and_wrappers_fixedpoint(cmd: &str) -> Vec<String> {
    let mut out = vec![cmd.trim().to_string()];
    let mut seen: BTreeSet<String> = out.iter().cloned().collect();
    let mut start = 0;
    while start < out.len() {
        let end = out.len();
        for i in start..end {
            let c = out[i].clone();
            let env_stripped = strip_all_leading_env_vars(&c, Some(is_binary_hijack_var));
            if seen.insert(env_stripped.clone()) {
                out.push(env_stripped);
            }
            let wrap_stripped = strip_safe_wrappers(&c);
            if seen.insert(wrap_stripped.clone()) {
                out.push(wrap_stripped);
            }
        }
        start = end;
    }
    out
}

/// Build the set of candidate strings a single subcommand should be matched
/// against: the subcommand itself (redirections stripped) plus fixed-point
/// application of safe-wrapper stripping and — when `aggressive_env` — leading
/// env-var stripping. Mirrors `filterRulesByContentsMatchingInput`'s
/// `commandsToTry` construction.
#[must_use]
fn candidates(subcommand: &str, aggressive_env: bool, seed_original: bool) -> Vec<String> {
    // 2.1.211 seeds `(r==="exact"?[a,l]:[l])` where `a` is the trimmed command
    // and `l` its redirection-stripped form. So EXACT mode seeds BOTH the
    // original (redirections preserved, so an exact rule like `Bash(cat x 2>&1)`
    // can match) and the stripped form; PREFIX mode seeds ONLY the stripped form
    // (a rule whose content contains redirection syntax is honored solely by the
    // whole-command exact check, never a prefix-mode subcommand match).
    //
    // `seed_original` controls whether the redirection-preserving `a` is seeded:
    // - EXACT-mode allow (`command_exact_allowed`) → true (matches CC `[a,l]`).
    // - PREFIX-mode ALLOW (`command_fully_allowed`) → false (matches CC `[l]`;
    //   dropping `a` only ever REMOVES an allow match → safe, more conservative).
    // - DENY/ASK (`rule_matches_any_subcommand`) → true. CC's `[l]`-only prefix
    //   seed is compensated by a SEPARATE whole-command exact-mode deny check;
    //   the Rust deny evaluation (`policy.rs`) has no such companion, so keeping
    //   `a` here preserves the deny of a redirection-content exact rule. Dropping
    //   it would UNDER-DENY — the unsafe direction — so it is deliberately kept.
    let trimmed = subcommand.trim().to_string();
    let stripped = strip_output_redirections(subcommand);
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    if seed_original {
        seen.insert(trimmed.clone());
        out.push(trimmed);
    }
    if seen.insert(stripped.clone()) {
        out.push(stripped);
    }
    let mut start = 0;
    while start < out.len() {
        let end = out.len();
        for idx in start..end {
            let c = out[idx].clone();
            let w = strip_safe_wrappers(&c);
            if seen.insert(w.clone()) {
                out.push(w);
            }
            if aggressive_env {
                // Deny/ask path: strip ALL leading env vars (no blocklist).
                let e = strip_all_leading_env_vars(&c, None);
                if seen.insert(e.clone()) {
                    out.push(e);
                }
            }
        }
        start = end;
    }
    out
}

/// claude-code `fou(pattern)` (the allow-mode xargs-retry gate): does `pattern`
/// end (after `trimEnd`) in an UNESCAPED `*`? A trailing `*` preceded by an even
/// number of backslashes is unescaped. Used to decide whether an ALLOW wildcard
/// rule may retry against the `xargs <pattern>` form.
fn pattern_ends_in_unescaped_star(pattern: &str) -> bool {
    let t = pattern.trim_end();
    let chars: Vec<char> = t.chars().collect();
    if chars.last() != Some(&'*') {
        return false;
    }
    // Count backslashes immediately before the trailing `*`.
    let mut backslashes = 0usize;
    let mut n = chars.len() as isize - 2;
    while n >= 0 && chars[n as usize] == '\\' {
        backslashes += 1;
        n -= 1;
    }
    backslashes % 2 == 0
}

/// Does a parsed rule match a single candidate subcommand? Faithful to the
/// per-rule arm of `filterRulesByContentsMatchingInput`.
///
/// `guard_compound` reproduces claude-code's `isCompoundCommand` second-line
/// defense (bashPermissions.ts:884-893, 923-928): for ALLOW matching a
/// prefix/wildcard rule must NOT match a candidate that is itself still
/// compound (a backslash-escaped operator can survive the first split), so a
/// hidden uncovered subcommand can't be smuggled past `command_fully_allowed`.
/// Deny/ask pass `false` (claude-code `skipCompoundCheck: true`) — a denied
/// command stays denied even when compounded.
///
/// `deny_or_ask` is the rule's behavior class (2.1.211 `ruleBehavior === "deny"
/// || "ask"`). It controls the wildcard-rule `xargs`-wrapped retry: after the
/// direct match fails, a deny/ask wildcard rule ALWAYS retries against
/// `xargs <pattern>` (so `Bash(rm *)` still denies `xargs rm -rf foo`); an allow
/// wildcard rule retries only when its pattern ends in an unescaped `*` (`fou`).
/// Both direct and retry matches use the whitespace-normalizing `cxt` semantics
/// (`Ale(...,!1,!0)`).
fn rule_matches_candidate(
    rule: &ShellRule,
    candidate: &str,
    guard_compound: bool,
    deny_or_ask: bool,
) -> bool {
    match rule {
        ShellRule::Exact(s) => candidate == s,
        ShellRule::Prefix(prefix) => {
            if guard_compound && split_command(candidate).len() > 1 {
                return false;
            }
            prefix_matches(prefix, candidate)
        }
        ShellRule::Wildcard(pattern) => {
            if guard_compound && split_command(candidate).len() > 1 {
                return false;
            }
            // `cxt(pattern, candidate)` — case-sensitive, whitespace-normalized.
            if crate::shell_rule_matching::match_wildcard_pattern_ex(
                pattern, candidate, false, true,
            ) {
                return true;
            }
            // Allow rules retry the xargs form only when the pattern ends in an
            // unescaped `*`; deny/ask rules always retry.
            if !deny_or_ask && !pattern_ends_in_unescaped_star(pattern) {
                return false;
            }
            crate::shell_rule_matching::match_wildcard_pattern_ex(
                &format!("xargs {pattern}"),
                candidate,
                false,
                true,
            )
        }
    }
}

/// Collapse runs of spaces/tabs to a single space — 2.1.211 `replace(/[ \t]+/g," ")`.
/// Applied to both the rule prefix and the candidate before prefix matching so a
/// deny/allow prefix rule authored with single spaces still matches a candidate
/// with doubled internal whitespace (`Bash(rm -rf:*)` vs `rm  -rf /`). NOT applied
/// to exact-rule comparisons (CC keeps `f.command === m` byte-exact).
fn collapse_ws(s: &str) -> std::borrow::Cow<'_, str> {
    if !s
        .as_bytes()
        .windows(2)
        .any(|w| matches!(w[0], b' ' | b'\t') && matches!(w[1], b' ' | b'\t'))
        && !s.contains('\t')
    {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c == ' ' || c == '\t' {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    std::borrow::Cow::Owned(out)
}

/// `prefix:*` word-boundary match: the candidate equals the prefix, starts with
/// `prefix ` (space boundary, so `ls:*` does NOT match `lsof`), or is the same
/// under a bare `xargs ` invocation (`Bash(grep:*)` matches `xargs grep p`).
/// Both sides are whitespace-collapsed first (2.1.211 `cxt`'s prefix arm).
fn prefix_matches(prefix: &str, candidate: &str) -> bool {
    let prefix = collapse_ws(prefix);
    let candidate = collapse_ws(candidate);
    let (prefix, candidate) = (prefix.as_ref(), candidate.as_ref());
    if candidate == prefix || candidate.starts_with(&format!("{prefix} ")) {
        return true;
    }
    let xargs = format!("xargs {prefix}");
    candidate == xargs || candidate.starts_with(&format!("{xargs} "))
}

/// `excludedCommands` pattern match — the SHARED dispatch core used by BOTH
/// `sandbox::decision::contains_excluded_command` and
/// `permission::sandbox_auto_allow::would_sandbox`. Both callers split the
/// command into subcommands and strip env/wrappers themselves, then ask this
/// per-(pattern, candidate) predicate.
///
/// Dispatch on [`parse_shell_rule`]:
/// - `Prefix(p)` matches `candidate == p || candidate.starts_with("{p} ")`
///   (space boundary; intentionally does NOT also match `xargs {p}` — that is
///   the richer `match_shell_rule`/`prefix_matches` behavior, NOT this simpler
///   excludedCommands one);
/// - `Exact(e)` matches `candidate == e` STRICTLY (NOT first-token);
/// - `Wildcard(w)` via [`crate::shell_rule_matching::match_wildcard_pattern`]
///   (case-sensitive).
///
/// Behavior is byte-identical to the two inline dispatches it replaces.
#[must_use]
pub fn matches_excluded_pattern(pattern: &str, candidate: &str) -> bool {
    match parse_shell_rule(pattern) {
        ShellRule::Prefix(p) => candidate == p || candidate.starts_with(&format!("{p} ")),
        ShellRule::Exact(e) => candidate == e,
        ShellRule::Wildcard(w) => {
            // 2.1.211 excluded-commands check uses `cxt` (whitespace-normalized,
            // case-sensitive): `case"wildcard":if(cxt(u.pattern,d))return!0`.
            crate::shell_rule_matching::match_wildcard_pattern_ex(&w, candidate, false, true)
        }
    }
}

/// DENY/ASK aggregation: does `rule_content` match ANY subcommand of `command`
/// (with aggressive env/wrapper stripping)? A denied command stays denied even
/// when wrapped or compounded.
#[must_use]
pub fn rule_matches_any_subcommand(rule_content: &str, command: &str) -> bool {
    let rule = parse_shell_rule(rule_content);
    for sub in split_command(command) {
        for cand in candidates(&sub, true, true) {
            // deny/ask: no compound guard (skipCompoundCheck) — stay denied, and
            // wildcard rules always retry the `xargs`-wrapped form.
            if rule_matches_candidate(&rule, &cand, false, true) {
                return true;
            }
        }
    }
    false
}

/// ALLOW aggregation: is EVERY subcommand of `command` covered by at least one
/// of `allow_contents` (safe-list stripping only)? Returns `false` for an empty
/// command or when any subcommand is uncovered.
#[must_use]
pub fn command_fully_allowed(allow_contents: &[&str], command: &str) -> bool {
    let subs = split_command(command);
    if subs.is_empty() {
        return false;
    }
    let rules: Vec<ShellRule> = allow_contents.iter().map(|c| parse_shell_rule(c)).collect();
    subs.iter().all(|sub| {
        let cands = candidates(sub, false, false);
        rules.iter().any(|rule| {
            // allow: guard against a still-compound candidate over-covering; a
            // wildcard rule retries the `xargs` form only when it ends in `*`.
            cands
                .iter()
                .any(|cand| rule_matches_candidate(rule, cand, true, false))
        })
    })
}

/// EXACT-mode ALLOW match: does the FULL trimmed command (NOT split into
/// subcommands) exactly match one of `allow_contents`? Faithful to claude-code
/// `bashToolCheckExactMatchPermission` → `matchingRulesForInput(..., 'exact')` →
/// `filterRulesByContentsMatchingInput(..., 'exact')` for the ALLOW bucket
/// (bashPermissions.ts:870-934, the `matchMode === 'exact'` arm):
///
/// - an `exact`-type rule matches iff `rule.command === candidate`;
/// - a `prefix`-type rule (`Bash(foo:*)`) matches iff `rule.prefix === candidate`
///   (the full command equals the bare prefix, no trailing args — the
///   `case 'exact': return bashRule.prefix === cmdToMatch` arm);
/// - a `wildcard`-type rule NEVER matches in exact mode (the documented
///   "SECURITY FIX: In exact match mode, wildcards must NOT match" arm).
///
/// The candidate set is the whole command + redirection-stripped + safe-wrapper
/// fixed-point (`candidates(command, false)`), i.e. the same `commandsForMatching`
/// / `commandsToTry` construction TS uses in exact mode WITHOUT `stripAllEnvVars`
/// (allow rules never strip arbitrary env prefixes). No subcommand split and no
/// compound guard — exact mode matches the unparsed command string.
#[must_use]
pub fn command_exact_allowed(allow_contents: &[&str], command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }
    let cands = candidates(trimmed, false, true);
    allow_contents.iter().any(|content| {
        let rule = parse_shell_rule(content);
        cands.iter().any(|cand| match &rule {
            // Exact rule: full-string equality (TS `bashRule.command === cmdToMatch`).
            ShellRule::Exact(s) => cand == s,
            // Prefix rule in exact mode: only the bare prefix with no args, both
            // sides whitespace-collapsed (2.1.211 `cxt` exact arm `g === y`).
            ShellRule::Prefix(prefix) => collapse_ws(cand) == collapse_ws(prefix),
            // Wildcard never matches in exact mode (TS returns false).
            ShellRule::Wildcard(_) => false,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_tool_names_include_mobile_shell() {
        // Desktop Bash + Windows PowerShell + the Android mobile `Shell` tool
        // (`tools/shell-mobile` `TOOL_NAME`) all carry command-pattern content
        // rules, so all three must get the per-command narrowing + dangerous-rule
        // analysis + content matching.
        assert!(is_shell_tool("Bash"));
        assert!(is_shell_tool("PowerShell"));
        assert!(is_shell_tool("Shell"));
        // Non-shell tools are unaffected.
        assert!(!is_shell_tool("Read"));
        assert!(!is_shell_tool("Agent"));
    }

    #[test]
    fn shell_tool_command_roots_for_mksh_style_commands() {
        // mksh/sh-compatible commands narrow to sensible per-command roots.
        assert!(command_fully_allowed(&["git status"], "git status"));
        assert!(command_fully_allowed(&["ls:*"], "ls -la"));
        // and a denied subcommand stays denied behind a compound under `Shell`.
        assert!(rule_matches_any_subcommand(
            "rm:*",
            "echo ok && rm -rf /tmp/x"
        ));
    }

    #[test]
    fn splits_on_separators_quote_aware() {
        assert_eq!(split_command("echo a && echo b"), vec!["echo a", "echo b"]);
        assert_eq!(split_command("a; b ; c"), vec!["a", "b", "c"]);
        assert_eq!(split_command("cat x | grep y"), vec!["cat x", "grep y"]);
        // quotes protect separators
        assert_eq!(split_command("echo 'a && b'"), vec!["echo 'a && b'"]);
        assert_eq!(split_command("echo \"x | y\""), vec!["echo \"x | y\""]);
        // background & is not a separator
        assert_eq!(
            split_command("sleep 1 & echo done"),
            vec!["sleep 1 & echo done"]
        );
    }

    #[test]
    fn splits_on_newline_but_not_continuation() {
        assert_eq!(split_command("ls\nrm -rf /"), vec!["ls", "rm -rf /"]);
        // backslash-newline is a line continuation → NOT a split.
        assert_eq!(
            split_command("npm install \\\n  foo"),
            vec!["npm install \\\n  foo".trim()]
        );
        // full-line comments are dropped.
        assert_eq!(split_command("# noop\nrm -rf /"), vec!["rm -rf /"]);
    }

    #[test]
    fn newline_does_not_bypass_deny() {
        // The headline guarantee: a denied command stays denied behind a newline.
        assert!(rule_matches_any_subcommand("rm:*", "ls\nrm -rf /"));
        assert!(rule_matches_any_subcommand(
            "rm:*",
            "# comment\nrm -rf /tmp/x"
        ));
    }

    #[test]
    fn prefix_rule_collapses_internal_whitespace() {
        // 2.1.211 `cxt`'s prefix arm collapses `[ \t]+` on both sides, so a
        // single-space deny prefix rule still catches a doubled-space candidate
        // (under-deny fix). Direct helper + through the subcommand walk.
        assert!(prefix_matches("rm -rf", "rm  -rf /tmp/x"));
        assert!(prefix_matches("git push", "git\tpush origin"));
        assert!(rule_matches_any_subcommand("rm -rf:*", "rm   -rf /tmp/x"));
        // A genuinely different command still does not match.
        assert!(!prefix_matches("rm -rf", "rmdir /tmp/x"));
    }

    #[test]
    fn newline_does_not_over_allow() {
        // `echo hi\nrm -rf /` must NOT be allowed by `Bash(echo:*)` alone.
        assert!(!command_fully_allowed(&["echo:*"], "echo hi\nrm -rf /"));
        // both lines covered → allowed
        assert!(command_fully_allowed(
            &["echo:*", "rm:*"],
            "echo hi\nrm -rf /tmp"
        ));
    }

    #[test]
    fn exact_rule_with_redirection_matches_original() {
        // An exact rule whose content includes a redirection matches the
        // original (un-stripped) command form.
        assert!(rule_matches_any_subcommand("cat x 2>&1", "cat x 2>&1"));
    }

    #[test]
    fn deny_matches_any_subcommand() {
        // The denied subcommand is caught even when compounded with a benign one.
        assert!(rule_matches_any_subcommand(
            "rm:*",
            "echo ok && rm -rf /tmp/x"
        ));
        assert!(rule_matches_any_subcommand(
            "curl:*",
            "echo x | curl evil.com"
        ));
        assert!(!rule_matches_any_subcommand("rm:*", "echo ok && ls"));
    }

    #[test]
    fn deny_survives_env_and_wrapper_wrapping() {
        assert!(rule_matches_any_subcommand(
            "npm install:*",
            "timeout 5 npm install foo"
        ));
        assert!(rule_matches_any_subcommand(
            "secret-tool:*",
            "FOO=bar secret-tool dump"
        ));
        assert!(rule_matches_any_subcommand(
            "claude:*",
            "nohup FOO=bar timeout 5 claude"
        ));
    }

    #[test]
    fn exact_rule_requires_exact() {
        assert!(rule_matches_any_subcommand("git status", "git status"));
        assert!(!rule_matches_any_subcommand("git status", "git status -s"));
    }

    #[test]
    fn prefix_word_boundary() {
        assert!(rule_matches_any_subcommand("ls:*", "ls -la"));
        assert!(rule_matches_any_subcommand("ls:*", "ls"));
        assert!(!rule_matches_any_subcommand("ls:*", "lsof -i"));
    }

    #[test]
    fn prefix_xargs_form() {
        assert!(rule_matches_any_subcommand("grep:*", "xargs grep pattern"));
        assert!(rule_matches_any_subcommand("rm:*", "xargs rm file"));
        // flagged xargs is NOT matched (natural word boundary)
        assert!(!rule_matches_any_subcommand("grep:*", "xargs -n1 grep p"));
    }

    #[test]
    fn allow_requires_every_subcommand_covered() {
        // both covered
        assert!(command_fully_allowed(
            &["echo:*", "ls:*"],
            "echo hi && ls -l"
        ));
        // second subcommand uncovered → not allowed (the key over-allow guard)
        assert!(!command_fully_allowed(&["echo:*"], "echo ok && rm -rf /"));
        // single covered
        assert!(command_fully_allowed(&["git status"], "git status"));
        // empty
        assert!(!command_fully_allowed(&["echo:*"], ""));
    }

    #[test]
    fn allow_through_safe_wrapper() {
        assert!(command_fully_allowed(
            &["npm install:*"],
            "timeout 10 npm install foo"
        ));
    }

    #[test]
    fn allow_redirection_stripped() {
        assert!(command_fully_allowed(
            &["python:*"],
            "python a.py > out.txt"
        ));
        assert!(command_fully_allowed(&["echo:*"], "echo hi 2>&1"));
    }

    #[test]
    fn wildcard_allow() {
        assert!(command_fully_allowed(&["git *"], "git add ."));
        assert!(command_fully_allowed(&["git *"], "git"));
        // single trailing star → bare `npm run` is covered (TS trailing-optional)
        assert!(command_fully_allowed(&["npm run *"], "npm run"));
        // but an unrelated command is not
        assert!(!command_fully_allowed(&["git *"], "npm run"));
    }

    // ----- PERM-BASH-01: wildcard Bash rules retry the `xargs`-wrapped form -----

    #[test]
    fn wildcard_deny_retries_xargs_form() {
        // A deny wildcard rule catches an `xargs`-prefixed command — the direct
        // match fails, and deny/ask ALWAYS retries `xargs <pattern>`.
        assert!(rule_matches_any_subcommand("rm *", "xargs rm -rf foo"));
        // deny retries even when the pattern does NOT end in `*` (fou irrelevant
        // for deny/ask).
        assert!(rule_matches_any_subcommand("rm * bar", "xargs rm x bar"));
        // an unrelated xargs command still is not denied.
        assert!(!rule_matches_any_subcommand("rm *", "xargs ls foo"));
    }

    #[test]
    fn wildcard_allow_xargs_gated_on_trailing_star() {
        // Allow wildcard with trailing unescaped `*` (fou true) retries xargs.
        assert!(command_fully_allowed(&["rm *"], "xargs rm foo"));
        // Allow wildcard WITHOUT trailing `*` (fou false) does NOT retry xargs,
        // so the xargs-wrapped form is NOT allowed (stays conservative).
        assert!(!command_fully_allowed(&["rm * bar"], "xargs rm x bar"));
        // Sanity: the same rule DOES allow the un-wrapped form.
        assert!(command_fully_allowed(&["rm * bar"], "rm x bar"));
    }

    #[test]
    fn wildcard_deny_xargs_direct_still_works() {
        // The non-xargs direct match is unaffected by the retry.
        assert!(rule_matches_any_subcommand("rm *", "rm -rf /tmp/x"));
    }

    // ----- PERM-CAND-01: prefix/allow mode seeds only the redirection-stripped
    //       candidate; the redirection-preserving original is exact-mode only. --

    #[test]
    fn allow_prefix_mode_drops_redirection_preserving_seed() {
        // An exact allow rule whose CONTENT contains redirection syntax must NOT
        // be honored via the prefix-mode subcommand path (CC seeds only `[l]` in
        // prefix mode). `foo > bar` seeds only the stripped `foo`, which the
        // exact rule `foo > bar` does not equal → not allowed.
        assert!(!command_fully_allowed(&["foo > bar"], "foo > bar"));
        // But the whole-command EXACT-mode allow path DOES seed `[a,l]`, so the
        // same rule matches there (redirection-preserving original retained).
        assert!(command_exact_allowed(&["foo > bar"], "foo > bar"));
    }

    #[test]
    fn deny_keeps_redirection_preserving_seed() {
        // SAFETY: the deny/ask aggregation keeps the redirection-preserving seed
        // (the Rust deny path has no companion whole-command exact check), so a
        // redirection-content exact deny rule still denies.
        assert!(rule_matches_any_subcommand("cat x 2>&1", "cat x 2>&1"));
        assert!(rule_matches_any_subcommand("foo > bar", "foo > bar"));
    }

    // ----- ENV_VAR_PATTERN byte-faithfulness (R3-1) -----

    /// Direct unit test of `strip_all_leading_env_vars` on BOTH paths. Round 2
    /// had no direct test of this fn, which is why the regex bug false-passed.
    /// Each row mirrors claude-code's `ENV_VAR_PATTERN`/`stripAllLeadingEnvVars`
    /// run against node (see R3-1 spec decision table).
    #[test]
    fn strip_all_leading_env_vars_byte_faithful() {
        let deny = |c: &str| strip_all_leading_env_vars(c, None);
        let excl = |c: &str| strip_all_leading_env_vars(c, Some(is_binary_hijack_var));

        // Standard / quoted values strip on both paths.
        assert_eq!(deny("FOO=bar bazel build"), "bazel build");
        assert_eq!(excl("FOO=bar bazel build"), "bazel build");
        assert_eq!(deny("FOO=\"a b\" bazel build"), "bazel build");
        assert_eq!(excl("FOO=\"a b\" bazel build"), "bazel build");
        assert_eq!(deny("FOO='a b' bazel build"), "bazel build");
        assert_eq!(excl("FOO='a b' bazel build"), "bazel build");

        // Backslash-escape unit (`\\.`) — the bug under-stripped to `b bazel build`.
        assert_eq!(deny("FOO=a\\ b bazel build"), "bazel build");
        assert_eq!(excl("FOO=a\\ b bazel build"), "bazel build");

        // Concatenated adjacent segments — the bug left these fully unstripped.
        assert_eq!(deny("FOO='x'y\"z\" bazel build"), "bazel build");
        assert_eq!(excl("FOO='x'y\"z\" bazel build"), "bazel build");
        assert_eq!(deny("FOO=a\"b\" bazel build"), "bazel build");
        assert_eq!(excl("FOO=a\"b\" bazel build"), "bazel build");

        // `$` excluded from value classes — `$VAR`/`"$x"` are NOT stripped
        // (matches TS; the bug over-stripped these).
        assert_eq!(deny("FOO=$VAR bazel build"), "FOO=$VAR bazel build");
        assert_eq!(excl("FOO=$VAR bazel build"), "FOO=$VAR bazel build");
        assert_eq!(deny("FOO=\"$x\" bazel build"), "FOO=\"$x\" bazel build");
        assert_eq!(excl("FOO=\"$x\" bazel build"), "FOO=\"$x\" bazel build");

        // Multiple leading vars stripped in a loop.
        assert_eq!(deny("A=1 B=2 cmd"), "cmd");
        assert_eq!(excl("A=1 B=2 cmd"), "cmd");

        // PATH: deny path strips like any var; excl path BREAKS on the KEY
        // (blocklist) leaving the var + rest in place.
        assert_eq!(deny("PATH=/evil bazel build"), "bazel build");
        assert_eq!(excl("PATH=/evil bazel build"), "PATH=/evil bazel build");
    }

    /// Round-2 regression locks must still hold after the regex fix.
    #[test]
    fn strip_all_leading_env_vars_round2_locks() {
        // FOO=bar stripped (deny path).
        assert_eq!(
            strip_all_leading_env_vars("FOO=bar secret-tool dump", None),
            "secret-tool dump"
        );
        // GOOS= stripped (a SAFE_ENV_VAR key — still a plain assignment).
        assert_eq!(
            strip_all_leading_env_vars("GOOS=linux go build", None),
            "go build"
        );
        // LD_AUDIT= breaks on the KEY in the excl path → stays in place.
        assert_eq!(
            strip_all_leading_env_vars("LD_AUDIT=/evil.so go build", Some(is_binary_hijack_var)),
            "LD_AUDIT=/evil.so go build"
        );
        // DYLD_INSERT_LIBRARIES= likewise breaks on the KEY (excl path).
        assert_eq!(
            strip_all_leading_env_vars(
                "DYLD_INSERT_LIBRARIES=x.dylib clang -c",
                Some(is_binary_hijack_var)
            ),
            "DYLD_INSERT_LIBRARIES=x.dylib clang -c"
        );
    }

    /// deny/ask path: a `Bash(bazel:*)` deny rule must catch env-prefixed forms
    /// the OLD regex failed on, and must NOT match the `$`-expansion forms.
    #[test]
    fn deny_catches_env_prefixed_bazel_faithfully() {
        // \\.-escape: was under-stripped → deny failed.
        assert!(rule_matches_any_subcommand(
            "bazel:*",
            "FOO=a\\ b bazel build"
        ));
        // concatenated segments: was unstripped → deny failed.
        assert!(rule_matches_any_subcommand(
            "bazel:*",
            "FOO='x'y\"z\" bazel build"
        ));
        assert!(rule_matches_any_subcommand(
            "bazel:*",
            "FOO=a\"b\" bazel build"
        ));
        // standard form still caught.
        assert!(rule_matches_any_subcommand(
            "bazel:*",
            "FOO=bar bazel build"
        ));

        // `$`/quoted-`$` forms are NOT stripped → the bazel deny does NOT match
        // (faithful to TS: those stay sandboxed, not denied via env-strip).
        assert!(!rule_matches_any_subcommand(
            "bazel:*",
            "FOO=$VAR bazel build"
        ));
        assert!(!rule_matches_any_subcommand(
            "bazel:*",
            "FOO=\"$x\" bazel build"
        ));
    }

    /// excludedCommands path: env-prefixed forms decide identically to TS.
    /// `strip_env_and_wrappers_fixedpoint` uses `is_binary_hijack_var`, so the
    /// `bazel` candidate must surface (or not) exactly as the value regex allows.
    #[test]
    fn excluded_commands_env_prefixed_bazel_faithfully() {
        let has_bare_bazel = |cmd: &str| {
            strip_env_and_wrappers_fixedpoint(cmd)
                .iter()
                .any(|c| c == "bazel build")
        };
        // Strippable env prefixes surface the bare `bazel build` candidate.
        assert!(has_bare_bazel("FOO=bar bazel build"));
        assert!(has_bare_bazel("FOO=a\\ b bazel build"));
        assert!(has_bare_bazel("FOO='x'y\"z\" bazel build"));
        assert!(has_bare_bazel("FOO=a\"b\" bazel build"));
        // `$`-forms are not stripped → no bare `bazel build` candidate.
        assert!(!has_bare_bazel("FOO=$VAR bazel build"));
        assert!(!has_bare_bazel("FOO=\"$x\" bazel build"));
        // Binary-hijack KEY breaks the strip → no bare candidate (stays guarded).
        assert!(!has_bare_bazel("PATH=/evil bazel build"));
    }
}
