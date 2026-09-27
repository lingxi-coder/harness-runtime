//! Redirect-borne `sed` risk analysis — faithful port of the 2.1.218 oracle's
//! `bgd` walk and its `xDs`/`_gd` orchestration (the second, redirect-borne
//! branch of the `sed` static-validation gate that emits
//! [`crate::sed_validation::SED_REDIRECT_BORNE_REASON`]).
//!
//! # What the oracle does
//! A `sed` command whose in-place-edit target (or swallowed args / heredoc /
//! expansion in a redirect target) is *redirect-borne* cannot be statically
//! validated by the allowlist checker, so it MUST ask. `bgd` parses the WHOLE
//! bash command with tree-sitter and walks it: for every `sed` command that
//! carries a redirect, it asks whether that redirect is "risky" — a swallowed
//! extra destination ([`x6s`]), an unquoted/backslash heredoc delimiter
//! ([`q6s`]), any expansion node ([`y6s`]), an ansi-c / translated string
//! ([`j6s`]), an unescaped expansion/glob/brace in the redirect's raw text
//! ([`dollar_jd`]), a non-simple target component ([`z6s`]), or a `=`-prefixed
//! word. A `sed` command whose redirect is fully static (a plain or properly
//! quoted target word) is *not* flagged.
//!
//! # Whole-module gating
//! The entire module is compiled only under the `bash-ast` feature (it needs the
//! tree-sitter-bash parser). Under the default build the redirect-borne branch is
//! a documented no-op — [`crate::sed_validation::sed_redirect_borne_verdict`]
//! then runs only the over-length / untokenizable precheck.
//!
//! # Documented divergences (see the individual `fn` docs)
//! - **`xse` (env/wrapper strip).** The oracle's `xse` strips
//!   `Hon`-allowlisted env assignments + comment lines + all EIGHT safe wrappers
//!   + unquotes the command name. This port's [`xse`] reuses
//!   [`crate::shell_command::strip_safe_wrappers`] for the SAFE-env allowlist +
//!   comment lines + the five `timeout/time/nice/stdbuf/nohup` wrappers, and adds
//!   the missing `command`/`builtin`/`noglob` wrappers
//!   ([`strip_command_builtin_noglob`]) and the first-token unquote
//!   ([`unquote_first_token`]) — driven to a fixed point. The only structural
//!   difference from the oracle is the interleaving order of the strip passes,
//!   which cannot change the converged first token the isSed check reads.
//! - **`\s` / `trimStart`.** JS `\s` and `String.prototype.trimStart` are
//!   approximated by [`char::is_whitespace`]. The tiny code-point differences
//!   (BOM, NEL) are immaterial to `sed` detection and can only over-ask.

#![allow(clippy::needless_range_loop)]

use crate::sed_validation::sed_command_untokenizable;
use std::sync::LazyLock;
use tree_sitter::Node;

/// `Jxe` = `1e4`: the maximum command length the redirect-borne walk parses.
/// Mirrors [`crate::sed_validation::SED_MAX_COMMAND_LEN`]; anything longer asks.
const SED_MAX_COMMAND_LEN: usize = 10_000;

// ── oracle node-type sets ────────────────────────────────────────────────────

/// `z6s`: expansion node types (a redirect target reaching one of these is
/// dynamic and cannot be statically validated).
const Z6S: &[&str] = &[
    "command_substitution",
    "process_substitution",
    "expansion",
    "simple_expansion",
    "arithmetic_expansion",
];

/// `lxy`: ANSI-C / locale-translated string node types.
const LXY: &[&str] = &["ansi_c_string", "translated_string"];

/// `BJd`: the redirect OPERATOR / heredoc-structural node types that do NOT count
/// as a "swallowed" destination argument.
const BJD: &[&str] = &[
    "<",
    ">",
    ">>",
    "<<",
    "<<-",
    "<<<",
    "<&",
    ">&",
    "&>",
    "&>>",
    ">|",
    ">&-",
    "<&-",
    "file_descriptor",
    "heredoc_start",
    "heredoc_body",
    "heredoc_content",
    "heredoc_end",
];

/// `cxy`: node types that are a "simple" redirect-target component by kind alone.
const CXY: &[&str] = &["word", "string", "raw_string", "number"];

/// `_ns` (`V0e`): the command-node types.
const NS: &[&str] = &["command", "declaration_command"];

#[inline]
fn in_set(set: &[&str], kind: &str) -> bool {
    set.contains(&kind)
}

// ── oracle regexes ───────────────────────────────────────────────────────────

/// `Lns` = `/(?:^|[^\\])(?:\\\\)*[`$]/` — an unescaped backtick or dollar.
static LNS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?:^|[^\\])(?:\\\\)*[`$]").unwrap());
/// `Mns` = `/(?:^|[^\\])(?:\\\\)*['"]/` — an unescaped single/double quote.
static MNS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?:^|[^\\])(?:\\\\)*['"]"#).unwrap());
/// `uxy` = `/(?:^|[^\\])(?:\\\\)*[;|&<>]/` — an unescaped shell metacharacter.
static UXY: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?:^|[^\\])(?:\\\\)*[;|&<>]").unwrap());
/// `dxy` = `/(?:^|[^\\])(?:\\\\)*\\$/` — a trailing odd-count backslash.
static DXY: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?:^|[^\\])(?:\\\\)*\\$").unwrap());

// ── node-text helpers ────────────────────────────────────────────────────────

/// tree-sitter `Node` has no `.text`; read it out of the source bytes. Mirrors
/// the oracle `node.text`.
#[inline]
fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// Iterate ALL children (named + anonymous operator tokens) — mirrors the
/// oracle `node.children`.
fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

/// `/\s/.test(text)` — does the text contain a whitespace character?
#[inline]
fn has_whitespace(text: &str) -> bool {
    text.chars().any(char::is_whitespace)
}

// ── Don: does the parse tree contain an ERROR node? ──────────────────────────

/// `Don(e)` = `e.type==="ERROR"||e.children.some(Don)`.
fn don(node: Node) -> bool {
    node.kind() == "ERROR" || children(node).iter().any(|c| don(*c))
}

// ── helper predicates ────────────────────────────────────────────────────────

/// `Wun(node)`: unquote a node's text (used only by [`x6s`]'s fd `-` check).
fn wun(node: Node, src: &[u8]) -> String {
    let text = node_text(node, src);
    match node.kind() {
        "raw_string" => char_slice_inner(text),
        "string" => unescape_string_body(&char_slice_inner(text)),
        "word" => unescape_word(text),
        _ => text.to_string(),
    }
}

/// `text.slice(1,-1)` — drop the first and last char (the surrounding quotes).
fn char_slice_inner(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 2 {
        return String::new();
    }
    chars[1..chars.len() - 1].iter().collect()
}

/// `.replace(/\\([$`"\\\n])/g, (_,r)=>r==="\n"?"":r)` — unescape a double-quoted
/// string body.
fn unescape_string_body(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\'
            && i + 1 < chars.len()
            && matches!(chars[i + 1], '$' | '`' | '"' | '\\' | '\n')
        {
            if chars[i + 1] != '\n' {
                out.push(chars[i + 1]);
            }
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// `.replace(/\\([\s\S])/g, (_,r)=>r==="\n"?"":r)` — unescape a word.
fn unescape_word(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            if chars[i + 1] != '\n' {
                out.push(chars[i + 1]);
            }
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// `X6s(e)`: a redirect node with MORE non-operator children than expected
/// (swallowed extra destination argument).
fn x6s(node: Node, src: &[u8]) -> bool {
    let k = node.kind();
    if k.ends_with("_redirect") {
        let kids = children(node);
        let t: Vec<Node> = kids
            .iter()
            .copied()
            .filter(|i| !in_set(BJD, i.kind()))
            .collect();
        let r = kids.iter().any(|i| i.kind() == ">&-" || i.kind() == "<&-");
        let n = !r
            && kids.iter().any(|i| i.kind() == ">&" || i.kind() == "<&")
            && t.iter().any(|i| wun(*i, src).starts_with('-'));
        let o = if k == "heredoc_redirect" || r || n {
            0
        } else {
            1
        };
        if t.len() > o {
            return true;
        }
    }
    children(node).iter().any(|c| x6s(*c, src))
}

/// `Q6s(e)`: a heredoc with an unquoted or backslash-bearing delimiter.
fn q6s(node: Node, src: &[u8]) -> bool {
    if node.kind() == "heredoc_redirect" {
        let r = children(node)
            .iter()
            .find(|o| o.kind() == "heredoc_start")
            .map(|o| node_text(*o, src).to_string())
            .unwrap_or_default();
        let rc: Vec<char> = r.chars().collect();
        let quoted = rc.len() >= 2
            && ((rc[0] == '\'' && rc[rc.len() - 1] == '\'')
                || (rc[0] == '"' && rc[rc.len() - 1] == '"'));
        if !quoted || r.contains('\\') {
            return true;
        }
    }
    children(node).iter().any(|c| q6s(*c, src))
}

/// `Y6s(e)`: does the subtree contain any expansion node?
fn y6s(node: Node) -> bool {
    if in_set(Z6S, node.kind()) {
        return true;
    }
    children(node).iter().any(|c| y6s(*c))
}

/// `J6s(e)`: does the subtree contain any ansi-c / translated string?
fn j6s(node: Node) -> bool {
    if in_set(LXY, node.kind()) {
        return true;
    }
    children(node).iter().any(|c| j6s(*c))
}

/// `Z6s(node, hasWhitespace)`: is the node a "simple/safe" redirect-target
/// component?
fn z6s(node: Node, src: &[u8], has_ws: bool) -> bool {
    match node.kind() {
        "concatenation" => children(node).iter().all(|r| z6s(*r, src, has_ws)),
        "word" => {
            let text = node_text(node, src);
            if LNS.is_match(text) {
                return false;
            }
            if UXY.is_match(text) || DXY.is_match(text) {
                return false;
            }
            if has_ws && MNS.is_match(text) {
                return false;
            }
            true
        }
        k @ ("string" | "raw_string") => {
            let q = if k == "raw_string" { '\'' } else { '"' };
            let chars: Vec<char> = node_text(node, src).chars().collect();
            chars.len() >= 2 && chars[0] == q && chars[chars.len() - 1] == q
        }
        k => in_set(CXY, k),
    }
}

/// `$Jd(text)`: does the redirect node's FULL text carry an unescaped
/// expansion / command substitution / glob / brace-expansion?
fn dollar_jd(text: &str) -> bool {
    let e: Vec<char> = text.chars().collect();
    let n = e.len();
    let peek = |o: usize| e.get(o).copied();
    let mut t: Option<char> = None; // quote state
    let mut r = false; // inside a `{...`
    let mut nn = false; // brace-expansion has a `,` or `..`
    let mut o = 0usize;
    while o < n {
        let i = e[o];
        if t == Some('\'') {
            if i == '\'' {
                t = None;
            }
            o += 1;
            continue;
        }
        if t == Some('"') {
            if i == '\\' && o + 1 < n && matches!(peek(o + 1), Some('$' | '`' | '"' | '\\')) {
                o += 2;
                continue;
            }
            if i == '`' {
                return true;
            }
            if i == '$' && in_dollar_class(peek(o + 1)) {
                return true;
            }
            if i == '"' {
                t = None;
            }
            o += 1;
            continue;
        }
        if i == '\\' {
            o += 2;
            continue;
        }
        if i == '`' {
            return true;
        }
        if i == '$' && (peek(o + 1) == Some('\'') || peek(o + 1) == Some('"')) {
            return true;
        }
        if i == '$' && in_dollar_class(peek(o + 1)) {
            return true;
        }
        if i == '=' && peek(o + 1) == Some('(') {
            return true;
        }
        if i == '*' || i == '?' || i == '[' {
            return true;
        }
        if i == '\'' || i == '"' {
            t = Some(i);
            o += 1;
            continue;
        }
        if i == '\n' {
            return false;
        }
        if i == ' ' || i == '\t' {
            r = false;
            nn = false;
            o += 1;
            continue;
        }
        if i == '{' {
            r = true;
            o += 1;
            continue;
        }
        if r && (i == ',' || (i == '.' && peek(o + 1) == Some('.'))) {
            nn = true;
            o += 1;
            continue;
        }
        if i == '}' && r && nn {
            return true;
        }
        o += 1;
    }
    t.is_some()
}

/// `/[A-Za-z0-9_{(@*#?$!-]/.test(next)` — the class the `$` look-ahead uses.
#[inline]
fn in_dollar_class(c: Option<char>) -> bool {
    match c {
        Some(c) => {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '{' | '(' | '@' | '*' | '#' | '?' | '$' | '!' | '-')
        }
        None => false,
    }
}

// ── the redirect-risk predicate `n` ──────────────────────────────────────────

/// `n(redirectNode)`: does this redirect carry risk?
fn redirect_is_risky(s: Node, src: &[u8]) -> bool {
    if x6s(s, src) || q6s(s, src) || y6s(s) || j6s(s) {
        return true;
    }
    if s.kind() != "heredoc_redirect" {
        if dollar_jd(node_text(s, src)) {
            return true;
        }
        let all_simple = children(s)
            .iter()
            .all(|a| in_set(BJD, a.kind()) || z6s(*a, src, has_whitespace(node_text(*a, src))));
        if !all_simple {
            return true;
        }
        if children(s)
            .iter()
            .any(|a| a.kind() == "word" && node_text(*a, src).starts_with('='))
        {
            return true;
        }
    }
    false
}

// ── innermost-command finder `o` ─────────────────────────────────────────────

/// `o(node)`: the innermost command node (skipping expansion subtrees and
/// redirect children), returning the LAST such command found.
fn find_command(s: Node<'_>) -> Option<Node<'_>> {
    if in_set(Z6S, s.kind()) {
        return None;
    }
    if s.kind() == "command" {
        return Some(s);
    }
    let mut a = None;
    for l in children(s) {
        if l.kind().ends_with("_redirect") {
            continue;
        }
        if let Some(c) = find_command(l) {
            a = Some(c);
        }
    }
    a
}

// ── the recursive risk walk `i` ──────────────────────────────────────────────

/// `i(node)`: the recursive risk walk. `is_sed(text)` is the oracle `t`.
fn walk_risk(s: Node, src: &[u8], is_sed: &dyn Fn(&str) -> bool) -> bool {
    if in_set(Z6S, s.kind()) {
        return false;
    }
    if s.kind() == "redirected_statement" {
        let a = children(s)
            .into_iter()
            .find(|c| c.kind() == "command")
            .or_else(|| find_command(s));
        let l = match a {
            Some(node) => is_sed(node_text(node, src)),
            None => true,
        };
        for c in children(s) {
            if c.kind().ends_with("_redirect") {
                if l && redirect_is_risky(c, src) {
                    return true;
                }
            } else if walk_risk(c, src, is_sed) {
                return true;
            }
        }
        return false;
    }
    if s.kind() == "command" {
        let a = is_sed(node_text(s, src));
        for l in children(s) {
            if l.kind().ends_with("_redirect") {
                if a && redirect_is_risky(l, src) {
                    return true;
                }
            } else if walk_risk(l, src, is_sed) {
                return true;
            }
        }
        return false;
    }
    if s.kind().ends_with("_redirect") {
        return redirect_is_risky(s, src);
    }
    children(s).iter().any(|c| walk_risk(*c, src, is_sed))
}

// ── bgd: the redirect-borne walk ─────────────────────────────────────────────

/// `bgd(e, isSed)`: returns `true` (= ASK) when the whole bash command `e`
/// carries a redirect-borne `sed` risk.
fn bgd(e: &str, is_sed: &dyn Fn(&str) -> bool) -> bool {
    if e.is_empty() {
        return false;
    }
    if e.len() > SED_MAX_COMMAND_LEN || sed_command_untokenizable(e) {
        return true;
    }
    let Some(tree) = crate::bash_tree_sitter::parse_raw(e) else {
        return true;
    };
    let src = e.as_bytes();
    let root = tree.root_node();
    if don(root) {
        return true;
    }
    walk_risk(root, src, is_sed)
}

/// Public entry (`bgd` with the default `kDs` isSed and an empty exempt set):
/// does the whole bash command carry a redirect-borne `sed` risk that cannot be
/// statically validated?
#[must_use]
pub fn sed_command_has_redirect_borne_risk(whole_command: &str) -> bool {
    bgd(whole_command, &|text| kds(text))
}

// ── isSed chain: kDs / ygd / d$_ / hpr / p$_ / mgd / V0e / xse ────────────────

/// `command`/`builtin`/`noglob` wrappers (`^command(?:[ \t]+-p+)*(?:[ \t]+--)?[ \t]+(?!-)`,
/// `^builtin(?:[ \t]+--)?[ \t]+(?!-)`, `^noglob[ \t]+(?!-)`). Rust `regex` has no
/// look-ahead, so each is matched THROUGH its trailing `[ \t]+` and then the
/// `(?!-)` is enforced by requiring the remaining text not to start with `-`.
/// The port's [`crate::shell_command::strip_safe_wrappers`] covers the other five
/// wrappers (timeout/time/nice/stdbuf/nohup) + the SAFE-env allowlist + comment
/// lines, but NOT these three — the gap the reviewers flagged.
static CMD_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^command(?:[ \t]+-p+)*(?:[ \t]+--)?[ \t]+").unwrap());
static BUILTIN_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^builtin(?:[ \t]+--)?[ \t]+").unwrap());
static NOGLOB_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^noglob[ \t]+").unwrap());

/// Strip a leading `command`/`builtin`/`noglob` wrapper (once each), enforcing
/// the oracle's `(?!-)` tail: only strip when the token after the wrapper is a
/// real word (not a `-flag`). Not stripping a `<wrapper> -flag` form only ever
/// UNDER-detects a NON-sed command, so it never causes an under-ask for a real
/// `sed` (the security-relevant direction).
fn strip_command_builtin_noglob(text: &str) -> String {
    let mut s = text.to_string();
    for re in [&*CMD_RE, &*BUILTIN_RE, &*NOGLOB_RE] {
        let stripped = match re.find(&s) {
            Some(m) if !s[m.end()..].starts_with('-') => Some(s[m.end()..].to_string()),
            _ => None,
        };
        if let Some(new_s) = stripped {
            s = new_s;
        }
    }
    s
}

/// `xse(text)`'s first-token unquote closure `i`: unquote the leading
/// (`^([^\s]+)`) token, char-by-char, with bash single/double-quote + backslash
/// rules. If the token has an UNTERMINATED quote or a trailing escape, the
/// oracle returns the input unchanged (`if(c!==null||u)return s`).
fn unquote_first_token(s: &str) -> String {
    // `^([^\s]+)([\s\S]*)$` requires a leading non-whitespace run.
    if s.is_empty() || s.starts_with(char::is_whitespace) {
        return s.to_string();
    }
    let split_at = s.find(char::is_whitespace).unwrap_or(s.len());
    let l: Vec<char> = s[..split_at].chars().collect();
    let rest = &s[split_at..];
    let mut c: Option<char> = None;
    let mut u = false;
    let mut d = String::new();
    let mut p = 0;
    while p < l.len() {
        u = false;
        let f = l[p];
        if c == Some('\'') {
            if f == '\'' {
                c = None;
            } else {
                d.push(f);
            }
        } else if c == Some('"') {
            if f == '\\' {
                match l.get(p + 1) {
                    Some(&m) if matches!(m, '$' | '`' | '"' | '\\') => {
                        d.push(m);
                        p += 1;
                    }
                    None => u = true,
                    Some(_) => d.push(f),
                }
            } else if f == '"' {
                c = None;
            } else {
                d.push(f);
            }
        } else if f == '\\' {
            match l.get(p + 1) {
                None => u = true,
                Some(&m) => {
                    d.push(m);
                    p += 1;
                }
            }
        } else if f == '"' || f == '\'' {
            c = Some(f);
        } else {
            d.push(f);
        }
        p += 1;
    }
    if c.is_some() || u {
        return s.to_string();
    }
    format!("{d}{rest}")
}

/// `xse(text)`: strip SAFE-env assignments + comment lines + ALL EIGHT safe
/// wrappers (timeout/time/nice/stdbuf/nohup via
/// [`crate::shell_command::strip_safe_wrappers`], plus command/builtin/noglob
/// here) + unquote the command name — to a fixed point. The oracle applies the
/// env/comment strip, then the unquote, then the wrapper strips with a re-unquote
/// after each pass; this fixed-point loop converges to the SAME fully-stripped
/// command (all that the isSed check reads is the first token), and additionally
/// handles a QUOTED wrapper name (e.g. `"timeout" 5 sed`) revealed after an
/// unquote pass.
fn xse(text: &str) -> String {
    let mut n = text.to_string();
    loop {
        let before = n.clone();
        // env allowlist + comment lines + timeout/time/nice/stdbuf/nohup
        n = crate::shell_command::strip_safe_wrappers(&n);
        // command/builtin/noglob (the three the port's helper omits)
        n = strip_command_builtin_noglob(&n);
        // unquote the leading command name (`\sed`, `"sed"`, `'sed'` → `sed`)
        n = unquote_first_token(&n);
        if n == before {
            break;
        }
    }
    n.trim().to_string()
}

/// Public isSed predicate for the whole-command sed-presence gate (the `_gd`
/// `ygd(o)!==null` check). Unquoting- and wrapper-aware, unlike a bare
/// `base_command == "sed"` test.
#[must_use]
pub fn is_sed_command(text: &str) -> bool {
    kds(text)
}

/// `a.split(/\s+/)[0]` — JS split-on-whitespace first element. A string starting
/// with whitespace yields `""` (the empty leading element), matching JS.
fn js_split_ws_first(a: &str) -> &str {
    if a.starts_with(char::is_whitespace) {
        return "";
    }
    match a.find(char::is_whitespace) {
        Some(idx) => &a[..idx],
        None => a,
    }
}

/// The oracle `t=(a)=>a.split(/\s+/)[0]==="sed"?a:null` closure.
fn sed_or_null(a: String) -> Option<String> {
    if js_split_ws_first(&a) == "sed" {
        Some(a)
    } else {
        None
    }
}

/// `mgd(chars)`: index (in chars) just past the next redirect-target token
/// (quote / escape aware); `None` if unterminated.
fn mgd(e: &[char]) -> Option<usize> {
    let mut t = 0usize;
    while t < e.len() {
        let r = e[t];
        if r == ' ' || r == '\t' {
            break;
        }
        if r == '\\' && t + 1 < e.len() {
            t += 2;
            continue;
        }
        if r == '\'' {
            match e[t + 1..].iter().position(|&c| c == '\'') {
                Some(rel) => {
                    t = (t + 1) + rel + 1;
                    continue;
                }
                None => return None,
            }
        }
        if r == '"' {
            let mut nn = t + 1;
            loop {
                if nn >= e.len() {
                    return None;
                }
                if e[nn] == '\\' && nn + 1 < e.len() {
                    nn += 2;
                    continue;
                }
                if e[nn] == '"' {
                    break;
                }
                nn += 1;
            }
            t = nn + 1;
            continue;
        }
        t += 1;
    }
    Some(t)
}

/// Conditional `trimStart` — the oracle's `(s.startsWith(" ")||s.startsWith("\t"))?s.trimStart():s`.
fn cond_trim_start(s: &[char]) -> &[char] {
    if s.first() == Some(&' ') || s.first() == Some(&'\t') {
        let mut i = 0;
        while i < s.len() && s[i].is_whitespace() {
            i += 1;
        }
        &s[i..]
    } else {
        s
    }
}

/// Unconditional `trimStart` returning an owned char vec.
fn trim_start_ws(s: &[char]) -> Vec<char> {
    let mut i = 0;
    while i < s.len() && s[i].is_whitespace() {
        i += 1;
    }
    s[i..].to_vec()
}

/// Match a leading redirect operator — the hand-ported
/// `/^(?:\d*|&)(?:>>(?!\()|>\|(?!\()|>&(?!\()|<&(?!\()|<>(?!\()|>(?![>|&(])|<(?![<>&(]))/`
/// (Rust `regex` has no look-ahead). Returns the matched length in chars.
fn match_redirect_op(t: &[char]) -> Option<usize> {
    // Prefix `(?:\d*|&)`: greedy digits, else a single `&`, else empty.
    if !t.is_empty() && t[0].is_ascii_digit() {
        let mut p = 0;
        while p < t.len() && t[p].is_ascii_digit() {
            p += 1;
        }
        return match_op_at(&t[p..]).map(|l| p + l);
    }
    if t.first() == Some(&'&') {
        return match_op_at(&t[1..]).map(|l| 1 + l);
    }
    match_op_at(t)
}

/// The operator alternation (left-to-right, first match wins).
fn match_op_at(s: &[char]) -> Option<usize> {
    let peek = |i: usize| s.get(i).copied();
    if peek(0) == Some('>') && peek(1) == Some('>') && peek(2) != Some('(') {
        return Some(2);
    }
    if peek(0) == Some('>') && peek(1) == Some('|') && peek(2) != Some('(') {
        return Some(2);
    }
    if peek(0) == Some('>') && peek(1) == Some('&') && peek(2) != Some('(') {
        return Some(2);
    }
    if peek(0) == Some('<') && peek(1) == Some('&') && peek(2) != Some('(') {
        return Some(2);
    }
    if peek(0) == Some('<') && peek(1) == Some('>') && peek(2) != Some('(') {
        return Some(2);
    }
    if peek(0) == Some('>') && !matches!(peek(1), Some('>') | Some('|') | Some('&') | Some('(')) {
        return Some(1);
    }
    if peek(0) == Some('<') && !matches!(peek(1), Some('<') | Some('>') | Some('&') | Some('(')) {
        return Some(1);
    }
    None
}

/// `p$_(text)`: strip ONE leading redirect operator + its target token; identity
/// if none. Repeats the strip until no leading redirect remains.
fn p_dollar(e: &str) -> String {
    let mut t: Vec<char> = e.chars().collect();
    loop {
        // `<<<` here-string (not `<<<<` and not `<<<(` process-sub).
        if t.len() >= 3
            && t[0] == '<'
            && t[1] == '<'
            && t[2] == '<'
            && t.get(3) != Some(&'<')
            && t.get(3) != Some(&'(')
        {
            let s = &t[3..];
            let a = cond_trim_start(s);
            if a.is_empty() {
                return t.iter().collect();
            }
            let Some(l) = mgd(a) else {
                return t.iter().collect();
            };
            let next = trim_start_ws(&a[l..]);
            t = next;
            continue;
        }
        match match_redirect_op(&t) {
            None => return t.iter().collect(),
            Some(oplen) => {
                let n = &t[oplen..];
                let o = cond_trim_start(n);
                if o.is_empty() {
                    return t.iter().collect();
                }
                let Some(i) = mgd(o) else {
                    return t.iter().collect();
                };
                let next = trim_start_ws(&o[i..]);
                t = next;
            }
        }
    }
}

/// `hpr(text)`: repeatedly strip wrappers (`xse`) + one redirect prefix (`p$_`)
/// to a fixed point.
fn hpr(e: &str) -> String {
    let mut t = e.trim().to_string();
    loop {
        let r = p_dollar(&xse(&t));
        if r == t {
            break;
        }
        t = r;
    }
    t
}

/// `V0e(node, parent)`: find the command node in a (sub)tree.
fn v0e<'a>(e: Node<'a>, parent: Option<Node<'a>>) -> Option<Node<'a>> {
    let r = e.kind();
    if in_set(NS, r) {
        return Some(e);
    }
    if r == "variable_assignment" {
        if let Some(p) = parent {
            return children(p)
                .into_iter()
                .find(|o| in_set(NS, o.kind()) && o.start_byte() > e.start_byte());
        }
        return None;
    }
    if r == "pipeline" {
        for o in children(e) {
            if let Some(i) = v0e(o, Some(e)) {
                return Some(i);
            }
        }
        return None;
    }
    if r == "redirected_statement" {
        return children(e).into_iter().find(|o| in_set(NS, o.kind()));
    }
    for o in children(e) {
        if let Some(i) = v0e(o, Some(e)) {
            return Some(i);
        }
    }
    None
}

/// `d$_(text)`: tree route to extract the sed command string, or `None`.
fn d_dollar(e: &str) -> Option<String> {
    if e.len() > SED_MAX_COMMAND_LEN {
        return sed_or_null(hpr(e));
    }
    if sed_command_untokenizable(e) {
        return sed_or_null(hpr(e));
    }
    let Some(tree) = crate::bash_tree_sitter::parse_raw(e) else {
        return sed_or_null(hpr(e));
    };
    let root = tree.root_node();
    if don(root) {
        return sed_or_null(hpr(e));
    }
    let top: Vec<Node> = children(root)
        .into_iter()
        .filter(|a| a.kind() != "comment")
        .collect();
    let ok = top.len() == 1
        && (top[0].kind() == "command"
            || (top[0].kind() == "redirected_statement"
                && children(top[0]).iter().any(|a| a.kind() == "command")));
    if !ok {
        return sed_or_null(hpr(e));
    }
    let o = v0e(root, None)?;
    let i = children(o)
        .into_iter()
        .find(|a| a.kind() == "command_name")?;
    let start = i.start_byte();
    let s = &e[start..];
    sed_or_null(hpr(s))
}

/// `ygd(text)`: `d$_` route then the `xse` fallback.
fn ygd(e: &str) -> Option<String> {
    let t = e.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(r) = d_dollar(t) {
        return Some(r);
    }
    if js_split_ws_first(&xse(t)) == "sed" {
        return Some(hpr(t));
    }
    None
}

/// `kDs(text)` — the isSed predicate.
fn kds(e: &str) -> bool {
    ygd(e).is_some()
}

#[cfg(all(test, feature = "bash-ast"))]
mod tests {
    use super::*;

    fn risk(cmd: &str) -> bool {
        sed_command_has_redirect_borne_risk(cmd)
    }

    // ── isSed chain ─────────────────────────────────────────────────────────

    #[test]
    fn is_sed_basic_and_wrapped() {
        assert!(kds("sed -i 's/a/b/' f"));
        assert!(kds("sed s/x/y/ f"));
        // wrapper: timeout 5 sed … is still sed (xse strips the wrapper).
        assert!(kds("timeout 5 sed -i s/x/y/ f"));
        // redirect-prefixed: `>out sed …` route through hpr/p$_.
        assert!(kds(">out sed s/x/y/ f"));
        // non-sed
        assert!(!kds("cat f"));
        assert!(!kds("grep x f"));
        assert!(!kds(""));
    }

    #[test]
    fn is_sed_command_wrapper_and_quote_forms() {
        // command/builtin/noglob wrappers (the 3 `strip_safe_wrappers` omits).
        assert!(kds("command sed -i s/x/y/ f"));
        assert!(kds("builtin sed s/x/y/ f"));
        assert!(kds("noglob sed s/x/y/ f"));
        // quoted / backslash-escaped command name (the `xse` unquote step).
        assert!(kds("\"sed\" -i s/x/y/ f"));
        assert!(kds("'sed' s/x/y/ f"));
        assert!(kds("\\sed s/x/y/ f"));
        // `command -v sed` is a LOOKUP, not a wrapper (`(?!-)` tail) — NOT sed.
        assert!(!kds("command -v sed"));
        // a genuinely different wrapped/quoted command stays non-sed.
        assert!(!kds("command cat f"));
        assert!(!kds("\"cat\" f"));
    }

    #[test]
    fn wrapper_and_quoted_sed_with_risky_redirect_asks() {
        // The reviewer-found under-asks: a wrapper- or quote-nested `sed` whose
        // redirect is redirect-borne must still ASK (isSed now unquoting/wrapper
        // aware). Before the fix these all returned `false` (silent passthrough).
        assert!(risk("command sed -i s/x/y/ f >$(evil)"));
        assert!(risk("builtin sed -i s/x/y/ f >$(evil)"));
        assert!(risk("noglob sed -i s/x/y/ f >$(evil)"));
        assert!(risk("\"sed\" -i s/x/y/ f >$(evil)"));
        assert!(risk("'sed' -i s/x/y/ f >*.txt"));
        assert!(risk("\\sed -i s/x/y/ f >${VAR}"));
        // …but a wrapped/quoted NON-sed command with the same redirect does NOT
        // ask (the redirect is only checked for a sed command).
        assert!(!risk("command cat f >$(evil)"));
        assert!(!risk("\"cat\" f >$(evil)"));
    }

    // ── positive cases (must ASK) ───────────────────────────────────────────

    #[test]
    fn swallowed_extra_destination_asks() {
        // A redirect that swallows an extra destination word (X6s): `>out extra`.
        assert!(risk("sed -i 's/x/y/' file >out extra"));
    }

    #[test]
    fn expansion_in_redirect_target_asks() {
        assert!(risk("sed -i 's/x/y/' f >$(evil)")); // command substitution
        assert!(risk("sed -i 's/x/y/' f >${VAR}")); // parameter expansion
        assert!(risk("sed -i 's/x/y/' f >$VAR")); // simple expansion
    }

    #[test]
    fn glob_in_redirect_target_asks() {
        assert!(risk("sed -i 's/x/y/' f >*.txt")); // $Jd glob
    }

    #[test]
    fn brace_expansion_in_redirect_target_asks() {
        assert!(risk("sed -i 's/x/y/' f >{a,b}.txt")); // $Jd brace-expansion
    }

    #[test]
    fn unquoted_heredoc_delimiter_asks() {
        // An unquoted heredoc delimiter (Q6s): `<<EOF` (not `<<'EOF'`).
        assert!(risk("sed -i 's/x/y/' f <<EOF\nfoo\nEOF\n"));
    }

    #[test]
    fn ansi_c_string_in_redirect_target_asks() {
        // An ansi-c `$'…'` redirect target (J6s).
        assert!(risk("sed -i 's/x/y/' f >$'\\n'"));
    }

    #[test]
    fn equals_prefixed_redirect_word_asks() {
        // A `=`-prefixed redirect word (the last `n` clause). tree-sitter parses
        // the destination `=out` as a `word` starting with `=`.
        assert!(risk("sed -i 's/x/y/' f >=out"));
    }

    #[test]
    fn wrapper_wrapped_sed_with_risky_redirect_asks() {
        // The timeout-wrapper case: isSed resolves through xse, and the risky
        // redirect target still asks.
        assert!(risk("timeout 5 sed -i s/x/y/ f >$(evil)"));
    }

    // ── negative cases (passthrough) ────────────────────────────────────────

    #[test]
    fn safe_plain_target_passthrough() {
        assert!(!risk("sed -i 's/x/y/' f > out.txt"));
        assert!(!risk("sed -i 's/x/y/' f >out.txt"));
    }

    #[test]
    fn safe_quoted_target_passthrough() {
        assert!(!risk("sed -i 's/x/y/' f > 'out.txt'"));
        assert!(!risk("sed -i 's/x/y/' f > \"out.txt\""));
    }

    #[test]
    fn no_redirect_passthrough() {
        assert!(!risk("sed s/x/y/ f"));
        assert!(!risk("sed -i 's/x/y/' f"));
        assert!(!risk("sed -n p file"));
    }

    #[test]
    fn quoted_heredoc_delimiter_passthrough() {
        // A properly single-quoted heredoc delimiter is NOT risky.
        assert!(!risk("sed 's/x/y/' <<'EOF'\nfoo\nEOF\n"));
    }

    #[test]
    fn non_sed_command_with_risky_redirect_passthrough() {
        // The risky redirect belongs to a NON-sed command → not flagged (the
        // redirect risk only matters for sed).
        assert!(!risk("cat f >$(evil)"));
        assert!(!risk("echo hi >${VAR}"));
    }

    #[test]
    fn sed_safe_but_other_command_risky_passthrough() {
        // sed's own redirect is safe; a sibling non-sed command's risky redirect
        // must not make the whole thing ask.
        assert!(!risk("sed -i 's/x/y/' f > out.txt && cat g >$(evil)"));
    }
}
