//! Faithful ~370-line port of claude-code's `BashTool/prompt.ts`
//! `getSimplePrompt()` — the model-facing description of the Bash tool.
//!
//! This is a PROMPT-TEXT parity batch (BASH.6): the goal is byte-faithful
//! reproduction of the EXTERNAL-USER (non-`ant`) prompt that claude-code ships,
//! assembled from the same pieces — header, tool-preference bullets, instruction
//! items, the sandbox section, and the git/PR section.
//!
//! ## Divergences from `prompt.ts` (documented per spec)
//!
//! - **`ant`-only branches dropped.** claude-code's `getSimplePrompt` /
//!   `getCommitAndPRInstructions` have `process.env.USER_TYPE === 'ant'`
//!   branches (undercover instructions, `/commit` + `/commit-push-pr` skill
//!   pointers, the short git section). We are always on the external path, so
//!   those branches are omitted entirely.
//! - **Embedded-search-tools branch dropped.** `hasEmbeddedSearchTools()` gates
//!   whether to steer away from `find`/`grep` (ant-native builds alias them to
//!   bundled bfs/ugrep). External builds always steer toward `Glob`/`Grep`, so
//!   we hardcode the non-embedded path (Glob/Grep bullets present, `find`/`grep`
//!   in the avoid-list, no `find -regex` alternation note).
//! - **Monitor-tool sleep bullets dropped.** The TS `feature('MONITOR_TOOL')`
//!   branch adds Monitor-specific bullets; `Monitor` is a deferred tool here, so
//!   we take the non-Monitor branch verbatim.
//! - **Sandbox section reflects the Rust [`SandboxRuntimeConfig`].** claude-code
//!   drives `getSimpleSandboxSection` off `SandboxManager` getters; we drive the
//!   same shape off the Rust `SandboxRuntimeConfig`. `read.allowWithinDeny`
//!   (from `filesystem.allow_read`), `network.deniedHosts` (from
//!   `network.denied_domains`), and the `$TMPDIR` cross-user temp-dir
//!   normalization (via [`lingxi_temp_dir`] / [`normalize_allow_only`]) are all
//!   reproduced. See [`sandbox_section`] for the field-by-field mapping.
//! - **Tool-name literals are STRING LITERALS** (`"Glob"`, `"Grep"`, `"Read"`,
//!   `"Edit"`, `"Write"`, `"Bash"`) matching the claude-code wire names, rather
//!   than imported constants from other crates (avoids a cross-crate dep).
//! - **Co-Authored-By attribution.** claude-code injects a dynamic
//!   `getAttributionTexts()` commit/PR attribution into FIVE slots across the
//!   two git sections. Its DEFAULT (no settings) is NON-empty, so
//!   [`attribution_texts`] reproduces the default pair and the five slots keep
//!   the oracle's conditional shape. Only the settings half
//!   (`includeCoAuthoredBy: false` / a custom `attribution` object) is
//!   unmodelled here — see [`attribution_texts`] for the residual.
//!
//! ## BASH.4 note (cwd persistence)
//!
//! The header sentence "The working directory persists between commands, but
//! shell state does not." is kept verbatim because it is what claude-code sends
//! (prompt parity). The actual cwd-persistence BEHAVIOR is a SEPARATE deferred
//! batch (BASH.4): today each `BashTool::call` spawns a fresh shell with
//! `cwd = workspace`, so this sentence is currently ASPIRATIONAL until BASH.4
//! lands the per-session cwd carry-over.

use crate::bash::{bash_default_timeout_ms, bash_max_timeout_ms};
use sandbox::runtime_config::SandboxRuntimeConfig;

// ===== Wire tool-name literals (string literals, NOT cross-crate imports) ====

/// `Bash` tool wire name — matches claude-code `BASH_TOOL_NAME`.
const BASH_TOOL_NAME: &str = "Bash";
/// `Glob` tool wire name — matches claude-code `GLOB_TOOL_NAME`.
const GLOB_TOOL_NAME: &str = "Glob";
/// `Grep` tool wire name — matches claude-code `GREP_TOOL_NAME`.
const GREP_TOOL_NAME: &str = "Grep";
/// `Read` tool wire name — matches claude-code `FILE_READ_TOOL_NAME`.
const FILE_READ_TOOL_NAME: &str = "Read";
/// `Edit` tool wire name — matches claude-code `FILE_EDIT_TOOL_NAME`.
const FILE_EDIT_TOOL_NAME: &str = "Edit";
/// `Write` tool wire name — matches claude-code `FILE_WRITE_TOOL_NAME`.
const FILE_WRITE_TOOL_NAME: &str = "Write";

// ===== Helpers ==============================================================

/// A bullet-list node: a top-level item or a group of subitems indented under
/// the preceding top-level item. Mirrors the TS `Array<string | string[]>`.
enum Bullet {
    /// Top-level item, rendered as ` - {item}` (one leading space).
    Item(String),
    /// Subitems, each rendered as `  - {subitem}` (two leading spaces).
    Sub(Vec<String>),
}

/// Port of claude-code `prependBullets` (`constants/prompts.ts:167`):
/// top-level items get ` - `, subitems get `  - `.
fn prepend_bullets(items: &[Bullet]) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Bullet::Item(s) => out.push(format!(" - {s}")),
            Bullet::Sub(subs) => {
                for sub in subs {
                    out.push(format!("  - {sub}"));
                }
            }
        }
    }
    out
}

/// Port of `isEnvTruthy` for the one env var this module gates on. claude-code's
/// `isEnvTruthy` (`envUtils.ts:32-37`) is a strict allowlist: unset/empty ⇒
/// false; otherwise the lowercased, trimmed value must be one of
/// `1`/`true`/`yes`/`on`. Delegates to the canonical [`platform_api::env::is_env_truthy`]
/// so the gating cannot drift from the single shared allowlist.
fn is_env_truthy(name: &str) -> bool {
    platform_api::env::is_env_truthy(std::env::var(name).ok().as_deref())
}

/// Port of claude-code 2.1.263 `Dl()`: the runtime disable latch OR
/// `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS`. The latch is set by the MCP-serve
/// HTTP host, which this port does not expose; it is not a settings key.
/// The shared platform gate is also consumed by SDK controls, the prompt, and
/// `gnr`'s input schema (`Dl() ? dnr().omit({run_in_background:!0, …}) : …`).
pub(crate) fn background_tasks_disabled() -> bool {
    // Single source of truth: the SDK `background_tasks` control request in
    // `apps/cli` reads the same gate and cannot reach this crate.
    platform_api::env::background_tasks_disabled()
}

/// Port of `getBackgroundUsageNote` (`prompt.ts:35`). Returns `None` when
/// `LINGXI_DISABLE_BACKGROUND_TASKS` is truthy.
fn background_usage_note() -> Option<String> {
    if background_tasks_disabled() {
        return None;
    }
    Some(
        "You can use the `run_in_background` parameter to run the command in the background. \
         Only use this if you don't need the result immediately and are OK being notified when \
         the command completes later. You do not need to check the output right away - you'll be \
         notified when it finishes. You do not need to use '&' at the end of the command when \
         using this parameter."
            .to_string(),
    )
}

/// Port of claude-code 2.1.238 `JQd()` — the gate on the CONCISE prompt's
/// "Commands are cheap to run…" bullet:
///
/// ```js
/// function Q$r(e,t,r){return e||sti(r)||VC()?.[t]===!0||it(t,!1)}
/// function JQd(){return Q$r(V.CLAUDE_CODE_GORSE_PLOVER,DKb,void 0)}
/// ```
///
/// Four sources OR'd together; three of them (the cohort helper, the flag
/// override map, and the statsig gate `it(DKb,false)`) are host-runtime signals
/// with no seam in this crate and all default FALSE, so only the env half is
/// modelled — the same shape as [`background_tasks_disabled`].
///
/// NOTE the truthiness rule: `Q$r`'s first term is a BARE `e`, i.e. plain JS
/// string truthiness (any non-empty value enables it), NOT the strict
/// `isEnvTruthy` allowlist [`is_env_truthy`] implements.
fn cheap_commands_bullet_enabled() -> bool {
    std::env::var("LINGXI_GORSE_PLOVER").is_ok_and(|v| !v.is_empty())
}

/// Port of `shouldIncludeGitInstructions` — oracle `iQ()`:
///
/// ```js
/// let e = a.CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS;
/// if (e !== void 0) return !e;
/// return Ge().includeGitInstructions ?? !0;
/// ```
///
/// The env var is THREE-valued and wins outright whenever it is DEFINED, in
/// both directions: `…=1` removes the sections, and an explicit `…=0` puts them
/// back even against a settings `false`. Only an UNDEFINED env falls through to
/// the setting, which defaults to on.
///
/// (The previous note here said the toggle lived at `git.includeGitInstructions`
/// and that its default made the env the only observable gate. Both were wrong:
/// `Ge()` reads it at the TOP level, and a settings `false` is observable
/// whenever the env var is absent — which is the normal case.)
fn should_include_git_instructions() -> bool {
    include_git_instructions_from(
        env_tristate("LINGXI_DISABLE_GIT_INSTRUCTIONS"),
        platform_api::session_flags::include_git_instructions(),
    )
}

/// `iQ()` with both inputs passed in, so the precedence is testable without
/// touching the process environment.
///
/// `disable_env` is the PARSED env var: `None` when unset, else its boolean
/// value. Returning `!e` for a defined env is what makes an explicit `0`
/// override a settings `false` — collapsing "unset" into "false" here would
/// silently turn that override into a no-op.
fn include_git_instructions_from(disable_env: Option<bool>, setting: Option<bool>) -> bool {
    if let Some(disabled) = disable_env {
        return !disabled;
    }
    setting.unwrap_or(true)
}

/// Read an env var as the oracle's parsed-proxy boolean: `None` when the
/// variable is absent, else whether its value is truthy.
fn env_tristate(name: &str) -> Option<bool> {
    std::env::var(name)
        .ok()
        .map(|raw| platform_api::env::is_env_truthy(Some(raw.as_str())))
}

// ===== Commit / PR attribution ==============================================

/// Commit trailer — the port's spelling of claude-code 2.1.238 `rcT`'s
/// `Co-Authored-By: ${modelDisplayName} <noreply@anthropic.com>`.
///
/// The oracle resolves the display name as
/// `FP(model) ? firstPartyName : Hhm(model) ? name(model) : "Claude"` — i.e.
/// plain `"Claude"` for anything it cannot recognise as a first-party model.
/// This crate is handle-free (no model catalog, and the VERBOSE builder gets no
/// model at all), so it takes that fallback arm — the SAME literal the port's
/// equally handle-free `/commit` handler already ships
/// (`commands/core/src/commit.rs`).
const COMMIT_ATTRIBUTION: &str = "Co-Authored-By: Claude <noreply@anthropic.com>";

/// PR footer — claude-code 2.1.238 `Ohm()`:
/// `` `🤖 Generated with [Claude Code](${CLAUDE_CODE_URL})` `` (the
/// `tengu_pr_footer_surface_suffix` gate that appends `" via <surface>"` is
/// default-false, so the bare footer is the shipped text).
///
/// Re-branded exactly as the port already re-brands it in
/// `commands/core/src/commit_push_pr.rs` (product name swapped, URL kept).
const PR_ATTRIBUTION: &str = "🤖 Generated with [LingXi](https://claude.com/claude-code)";

/// Port of claude-code 2.1.238 `hvt()` → `rcT()` — the `{commit, pr}` pair the
/// Bash git sections interpolate:
///
/// ```js
/// function rcT(){…let n=`Co-Authored-By: ${t} <noreply@anthropic.com>`,r=Ohm(),o=Vo(),i=o.attribution;
///  if(i!==void 0&&V8s(i))return{commit:i.commit??n,pr:i.pr??r};
///  if(o.includeCoAuthoredBy===!1)return …{commit:"",pr:""};
///  return{commit:n,pr:r}}
/// ```
///
/// The DEFAULT (no settings) arm is `{commit: n, pr: r}` — NON-empty — which is
/// what this returns.
///
/// RESIDUAL: the two settings-driven arms (`attribution.commit` /
/// `attribution.pr` overrides, and `includeCoAuthoredBy: false` ⇒ both empty)
/// have no settings reader in this crate, the same way
/// [`should_include_git_instructions`] models only the env half of `aOt()`.
/// Every consumer below keeps the oracle's `${x ? … : …}` conditional shape, so
/// wiring a settings source later is a one-line change here and nothing else.
/// `hvt()`'s outer session-URL decoration (`ecT(e, url, …)`, gated on
/// `I_l()` — remote/teleport sessions only) is EXCLUDED surface.
fn attribution_texts() -> (String, String) {
    let (commit, pr) = platform_api::session_flags::attribution();
    resolve_attribution(
        commit,
        pr,
        platform_api::session_flags::include_co_authored_by(),
        COMMIT_ATTRIBUTION,
        PR_ATTRIBUTION,
    )
}

/// The settings arms of `$gs()`, pure so the ARM ORDER is testable without a
/// settings source:
///
/// ```js
/// let i = o.attribution;
/// if (i !== void 0 && V8s(i)) return { commit: i.commit ?? n, pr: i.pr ?? r };
/// if (o.includeCoAuthoredBy === !1) return { commit: "", pr: "" };
/// return { commit: n, pr: r };
/// ```
///
/// Three things the order encodes, each pinned by a test below:
/// * `V8s(i)` (`RWn`) accepts the object only when it names `commit` or `pr`, so
///   an `attribution: {}` falls through to `includeCoAuthoredBy` rather than
///   blanking both trailers.
/// * The object WINS over `includeCoAuthoredBy`, so `attribution.commit` still
///   applies alongside `includeCoAuthoredBy: false`.
/// * `?? n` is null-coalescing, so an explicit EMPTY STRING is kept as a value
///   — that is how a user disables one trailer while keeping the other.
fn resolve_attribution(
    commit: Option<String>,
    pr: Option<String>,
    include_co_authored_by: Option<bool>,
    default_commit: &str,
    default_pr: &str,
) -> (String, String) {
    if commit.is_some() || pr.is_some() {
        return (
            commit.unwrap_or_else(|| default_commit.to_string()),
            pr.unwrap_or_else(|| default_pr.to_string()),
        );
    }
    if include_co_authored_by == Some(false) {
        return (String::new(), String::new());
    }
    (default_commit.to_string(), default_pr.to_string())
}

// ===== Sandbox section ======================================================

/// Compact `JSON.stringify`-equivalent for the sandbox config objects. serde's
/// default `to_string` is compact (no spaces) — same as `jsonStringify` with no
/// `space` arg.
fn json_compact(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Dedup helper mirroring TS `dedup<T>` — preserves first-seen order.
fn dedup(items: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for s in items {
        if seen.insert(s.clone()) {
            out.push(s.clone());
        }
    }
    out
}

/// Maximum number of sandbox paths/hosts rendered into the prompt before the
/// list is truncated — claude-code 2.1.238 `D_l = 50` (`prompt.ts`, oracle
/// binary @132228439 for the marker string).
const SANDBOX_PROMPT_LIST_MAX: usize = 50;

/// Port of claude-code `Phr` (2.1.238):
///
/// ```js
/// function Phr(e){if(!e||e.length<=D_l)return e;let t=e.length-D_l;
///   return[...e.slice(0,D_l),`... and ${t} more (truncated for prompt size)`]}
/// ```
///
/// Applied to EVERY list rendered into the `## Command sandbox` JSON —
/// `read.denyOnly`, `read.allowWithinDeny`, `write.allowOnly`,
/// `write.denyWithinAllow`, `deniedHosts`, `allowUnixSockets` —
/// AFTER dedup / `$TMPDIR` normalization, matching the oracle's
/// `Phr(gXr(list))` / `Phr(l(t.allowOnly))` nesting.
fn truncate_for_prompt(items: Vec<String>) -> Vec<String> {
    if items.len() <= SANDBOX_PROMPT_LIST_MAX {
        return items;
    }
    let extra = items.len() - SANDBOX_PROMPT_LIST_MAX;
    let mut out: Vec<String> = items
        .into_iter()
        .take(SANDBOX_PROMPT_LIST_MAX)
        .collect::<Vec<_>>();
    out.push(format!("... and {extra} more (truncated for prompt size)"));
    out
}

/// Port of claude-code `getClaudeTempDir` (`utils/permissions/filesystem.ts:331`)
/// + `getClaudeTempDirName` (`:307`).
///
/// `baseTmpDir = LINGXI_TMPDIR || (windows ? tmpdir() : "/tmp")`, then the
/// base is realpath-resolved (`/tmp` → `/private/tmp` on macOS) falling back to
/// the unresolved base on failure. The directory NAME is `claude` on Windows
/// (tmpdir is already per-user) or `claude-{uid}` elsewhere. The result is
/// `join(resolvedBase, name) + sep` (trailing separator included).
fn lingxi_temp_dir() -> String {
    resolved_temp_dir(temp_dir_base())
}

/// The temp-dir BASE — `LINGXI_TMPDIR || (windows ? tmpdir() : "/tmp")`.
fn temp_dir_base() -> std::path::PathBuf {
    std::env::var_os("LINGXI_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(target_os = "windows") {
                std::env::temp_dir()
            } else {
                std::path::PathBuf::from("/tmp")
            }
        })
}

/// The per-user temp-dir NAME (TS `getClaudeTempDirName`).
fn temp_dir_name() -> String {
    if cfg!(target_os = "windows") {
        "claude".to_string()
    } else {
        format!("claude-{}", current_uid())
    }
}

/// `realpath(base) + name + sep` — the shared tail of `_8()` / `s2r()`.
fn resolved_temp_dir(base: std::path::PathBuf) -> String {
    // Resolve symlinks; fall back to the unresolved base on failure.
    let resolved_base = std::fs::canonicalize(&base).unwrap_or(base);
    let joined = resolved_base.join(temp_dir_name());
    let mut s = joined.to_string_lossy().into_owned();
    // Append the trailing platform separator (TS `+ sep`).
    s.push(std::path::MAIN_SEPARATOR);
    s
}

/// Byte-length ceiling above which claude-code refuses to host child-process
/// sockets under the configured temp dir and falls back to `/tmp` — oracle
/// 2.1.238 `O2b = 44` (cc-238.js @284355803, the `sun_path` headroom).
const CHILD_PROCESS_TMPDIR_MAX_BYTES: usize = 44;

/// Port of claude-code 2.1.238 `s2r()` — the CHILD-PROCESS temp dir, the SECOND
/// member of the `$TMPDIR` normalization set (`new Set([_8(), s2r()])`).
///
/// ```js
/// function n2r(){…let t=qzd(e);if(Buffer.byteLength(t)<=O2b)return t;
///   let o=join("/tmp",`claude-${process.getuid?.()??0}`),i=o;
///   try{mkdirSync(o,{recursive:!0,mode:448}),MDt(o)}catch{i=t}return i}
/// function s2r(){…let t=n2r();…try{n=realpathSync(t)}catch{}return n+sep}
/// ```
///
/// So this is the SAME path as [`lingxi_temp_dir`] whenever the configured temp
/// dir fits in [`CHILD_PROCESS_TMPDIR_MAX_BYTES`] bytes (the default `/tmp` case
/// always does), and `/tmp/claude-{uid}` only when a long `LINGXI_TMPDIR` pushes
/// it over. Both then feed the same `Set`, so the two collapse to ONE `$TMPDIR`
/// entry once the substitution runs — which is why the dedup must come AFTER the
/// substitution (see [`normalize_allow_only_with`]).
///
/// RESIDUAL: the oracle CREATES the fallback directory (`mkdirSync` + an
/// ownership check) and falls back to the configured dir when that fails.
/// Building a prompt must not have filesystem side effects, so this resolves the
/// path without creating it.
fn child_process_temp_dir() -> String {
    let base = temp_dir_base();
    // `Buffer.byteLength(t)` over the UNRESOLVED `join(base, name)`.
    let unresolved_len = base.join(temp_dir_name()).to_string_lossy().len();
    if unresolved_len <= CHILD_PROCESS_TMPDIR_MAX_BYTES {
        return lingxi_temp_dir();
    }
    resolved_temp_dir(std::path::PathBuf::from("/tmp"))
}

/// The real (not effective) UID, mirroring TS `process.getuid?.() ?? 0`. On
/// non-unix hosts (where this never feeds the dir name anyway) we return 0.
#[cfg(unix)]
fn current_uid() -> u32 {
    // `nix::unistd::getuid()` is a SAFE wrapper around `getuid(2)` (always-succeeds,
    // no preconditions), so this crate keeps its `#![forbid(unsafe_code)]`. Same
    // pattern as `apps/cli/src/bypass_env.rs::real_uid`.
    nix::unistd::getuid().as_raw()
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

/// Port of claude-code 2.1.238 `Ojr()`:
/// `Wt()==="windows" || Bze()!=="relaxed"` — TRUE when the sandbox exports
/// `$TMPDIR`, which is what selects BOTH the temp-file bullet wording and the
/// substitute-vs-filter arm of [`normalize_allow_only_with`].
///
/// `Bze()` resolves the 2.1.238 `filesystemPolicy` setting
/// (`strict` | `relaxed` | `relaxedIfForced`, default `strict`, forced `strict`
/// on Windows), so the shipped default is TRUE.
///
/// RESIDUAL: this crate has no settings reader (same shape as
/// [`should_include_git_instructions`]'s unmodelled settings half), and unlike
/// `aOt()` the oracle exposes NO env override for `filesystemPolicy` — so
/// inventing one here would invent surface the oracle does not have. The
/// function therefore returns the shipped default and is the SINGLE place a
/// settings source has to be wired; both arms of both consumers are implemented
/// and unit-tested through [`normalize_allow_only_with`] / [`temp_file_bullet`].
fn sandbox_exports_tmpdir() -> bool {
    // `Wt()==="windows"` short-circuits to true; `Bze()` defaults to "strict".
    true
}

/// Port of claude-code 2.1.238 `Khm`'s `l` (the `write.allowOnly` normalizer):
///
/// ```js
/// let s=new Set([_8(),s2r()]),a=Ojr(),
///     l=(m)=>to(a?m.map((h)=>s.has(h)?"$TMPDIR":h):m.filter((h)=>!s.has(h)));
/// ```
///
/// Two things the port previously got wrong (BASH-12):
/// 1. the set has TWO members — the claude temp dir AND the child-process temp
///    dir ([`child_process_temp_dir`]) — not one;
/// 2. `to()` (dedup) runs AFTER the map, so two DISTINCT temp dirs collapse to a
///    single `"$TMPDIR"` entry. Dedup-then-map left the second one verbatim.
///
/// Applied ONLY to `write.allowOnly` — never to the deny/read lists, which take
/// the plain `gXr()` dedup.
fn normalize_allow_only_with(paths: &[String], exports_tmpdir: bool) -> Vec<String> {
    let temp_dirs = [lingxi_temp_dir(), child_process_temp_dir()];
    let is_temp_dir = |p: &String| temp_dirs.iter().any(|t| t == p);
    let mapped: Vec<String> = if exports_tmpdir {
        paths
            .iter()
            .map(|p| {
                if is_temp_dir(p) {
                    "$TMPDIR".to_string()
                } else {
                    p.clone()
                }
            })
            .collect()
    } else {
        // `relaxed`: the sandbox does not export `$TMPDIR`, so the temp dirs are
        // DROPPED from the rendered list rather than collapsed into a literal.
        paths.iter().filter(|p| !is_temp_dir(p)).cloned().collect()
    };
    dedup(&mapped)
}

/// [`normalize_allow_only_with`] under the live [`sandbox_exports_tmpdir`].
fn normalize_allow_only(paths: &[String]) -> Vec<String> {
    normalize_allow_only_with(paths, sandbox_exports_tmpdir())
}

/// The sandbox section's temp-file bullet — claude-code 2.1.238 `Khm`'s
/// `Ojr()?"…$TMPDIR…":"…mktemp -d…"` ternary (both arms are plain string
/// literals in the oracle, NOT interpolation slots). The `relaxed` arm is NEW in
/// 2.1.238 (0 hits in 2.1.220). Em dash is U+2014.
fn temp_file_bullet(exports_tmpdir: bool) -> &'static str {
    if exports_tmpdir {
        "For temporary files, always use the `$TMPDIR` environment variable. TMPDIR is automatically set to the correct sandbox-writable directory in sandbox mode. Do NOT use `/tmp` directly - use `$TMPDIR` instead."
    } else {
        "For temporary files, create a scratch directory with `mktemp -d` and reference it by absolute path. Do NOT assume `$TMPDIR` is set \u{2014} the sandbox does not export it in this configuration."
    }
}

/// Port of `getSimpleSandboxSection` (`prompt.ts:172`), driven by the Rust
/// [`SandboxRuntimeConfig`] instead of the TS `SandboxManager` getters.
///
/// Field mapping (TS → Rust):
/// - read `denyOnly`           → `filesystem.deny_read`
/// - read `allowWithinDeny`    → `filesystem.allow_read` (only when non-empty)
/// - write `allowOnly`         → `filesystem.allow_write` (via [`normalize_allow_only`])
/// - write `denyWithinAllow`   → `filesystem.deny_write`
/// - network `deniedHosts`     → `network.denied_domains` (only when non-empty)
/// - `allowUnixSockets`        → `network.allow_unix_sockets` (only when non-empty)
/// - `ignoreViolations`        → `ignore_violations`
/// - unsandboxed cmds allowed  → `are_unsandboxed_commands_allowed()`
fn sandbox_section(cfg: &SandboxRuntimeConfig) -> String {
    if !cfg.enabled {
        return String::new();
    }

    let allow_unsandboxed_commands = cfg.are_unsandboxed_commands_allowed();

    // read object: denyOnly always; allowWithinDeny only when non-empty (TS
    // conditional-spread `...(x && { x })`). Insertion order: denyOnly first.
    let mut read = serde_json::Map::new();
    read.insert(
        "denyOnly".into(),
        serde_json::json!(truncate_for_prompt(dedup(&cfg.filesystem.deny_read))),
    );
    if !cfg.filesystem.allow_read.is_empty() {
        read.insert(
            "allowWithinDeny".into(),
            serde_json::json!(truncate_for_prompt(dedup(&cfg.filesystem.allow_read))),
        );
    }

    // Filesystem config object (read + write.allowOnly/denyWithinAllow). The
    // write.allowOnly list is run through `normalize_allow_only` so the per-UID
    // Claude temp dir collapses to the `$TMPDIR` literal.
    let filesystem = serde_json::json!({
        "read": serde_json::Value::Object(read),
        "write": {
            "allowOnly": truncate_for_prompt(normalize_allow_only(&cfg.filesystem.allow_write)),
            "denyWithinAllow": truncate_for_prompt(dedup(&cfg.filesystem.deny_write)),
        },
    });

    // Network config object — only emit keys that have values, mirroring the
    // TS conditional-spread shape (`...(x && { x })`). Latest Claude Code only
    // surfaces deniedHosts/allowUnixSockets in the prompt; allowedHosts are
    // intentionally omitted even when the allowlist is non-empty.
    let mut network = serde_json::Map::new();
    if !cfg.network.denied_domains.is_empty() {
        network.insert(
            "deniedHosts".into(),
            serde_json::json!(truncate_for_prompt(dedup(&cfg.network.denied_domains))),
        );
    }
    if !cfg.network.allow_unix_sockets.is_empty() {
        network.insert(
            "allowUnixSockets".into(),
            serde_json::json!(truncate_for_prompt(dedup(&cfg.network.allow_unix_sockets))),
        );
    }

    let mut restriction_lines: Vec<String> = Vec::new();
    restriction_lines.push(format!("Filesystem: {}", json_compact(&filesystem)));
    let has_network_prompt = !network.is_empty();
    if has_network_prompt {
        restriction_lines.push(format!(
            "Network: {}",
            json_compact(&serde_json::Value::Object(network))
        ));
    }
    if !cfg.ignore_violations.is_empty() {
        restriction_lines.push(format!(
            "Ignored violations: {}",
            json_compact(&serde_json::json!(cfg.ignore_violations))
        ));
    }

    let sandbox_override_items: Vec<Bullet> = if allow_unsandboxed_commands {
        vec![
            Bullet::Item("You should always default to running commands within the sandbox. Do NOT attempt to set `dangerouslyDisableSandbox: true` unless:".into()),
            Bullet::Sub(vec![
                "The user *explicitly* asks you to bypass sandbox".into(),
                "A specific command just failed and you see evidence of sandbox restrictions causing the failure. Note that commands can fail for many reasons unrelated to the sandbox (missing files, wrong arguments, network issues, etc.).".into(),
            ]),
            Bullet::Item("Evidence of sandbox-caused failures includes:".into()),
            Bullet::Sub(vec![
                "\"Operation not permitted\" errors for file/network operations".into(),
                "Access denied to specific paths outside allowed directories".into(),
                "Network connection failures to non-whitelisted hosts".into(),
                "Unix socket connection errors".into(),
            ]),
            Bullet::Item("When you see evidence of sandbox-caused failure:".into()),
            Bullet::Sub(vec![
                "Immediately retry with `dangerouslyDisableSandbox: true` (don't ask, just do it)".into(),
                "Briefly explain what sandbox restriction likely caused the failure. Be sure to mention that the user can use the `/sandbox` command to manage restrictions.".into(),
                "This goes through the permission gate (a user prompt, or the auto-mode classifier when auto mode is active)".into(),
            ]),
            Bullet::Item("Treat each command you execute with `dangerouslyDisableSandbox: true` individually. Even if you have recently run a command with this setting, you should default to running future commands within the sandbox.".into()),
            Bullet::Item("Do not suggest adding sensitive paths like ~/.bashrc, ~/.zshrc, ~/.ssh/*, or credential files to the sandbox allowlist.".into()),
        ]
    } else {
        vec![
            Bullet::Item("All commands MUST run in sandbox mode - the `dangerouslyDisableSandbox` parameter is disabled by policy.".into()),
            Bullet::Item("Commands cannot run outside the sandbox under any circumstances.".into()),
            Bullet::Item("If a command fails due to sandbox restrictions, work with the user to adjust sandbox settings instead.".into()),
        ]
    };

    let mut items = sandbox_override_items;
    if has_network_prompt {
        items.push(Bullet::Item("Network egress goes through a filtering proxy. Attempt requests and read the error rather than predicting whether a host is reachable; denied connections are reported in a `<sandbox_violations>` block explaining the reason.".into()));
    }
    items.push(Bullet::Item(
        temp_file_bullet(sandbox_exports_tmpdir()).into(),
    ));

    let mut lines: Vec<String> = vec![
        String::new(),
        "## Command sandbox".into(),
        "By default, your command will be run in a sandbox. This sandbox controls which directories and network hosts commands may access or modify without an explicit override.".into(),
        String::new(),
        "The sandbox has the following restrictions:".into(),
        restriction_lines.join("\n"),
        String::new(),
    ];
    lines.extend(prepend_bullets(&items));
    lines.join("\n")
}

// ===== Git / PR section =====================================================

/// Port of `getCommitAndPRInstructions` (`prompt.ts:42`), EXTERNAL-USER branch.
///
/// The `ant` undercover/skills branches are dropped. Returns an empty string
/// when [`should_include_git_instructions`] is false (matching the TS
/// `undercoverSection` early return, which is empty on the external path).
fn commit_and_pr_instructions() -> String {
    if !should_include_git_instructions() {
        return String::new();
    }

    // Attribution (`{commit:o, pr:i}=hvt()`) feeds THREE slots in this section,
    // each keeping the oracle's conditional shape (`fcT`, cc-238.js @231042884):
    //   step 3   `- Create the commit with a message${o?` ending with:\n   ${o}`:"."}`
    //   HEREDOC  `   Commit message here.${o?`\n\n   ${o}`:""}`
    //   PR body  `${Ajt()}${i?`\n\n${i}`:""}`
    // Note the THREE-space indent on both commit slots — the oracle indents them
    // to match the surrounding numbered-step / HEREDOC body, and the PR footer is
    // NOT indented.
    let (commit_attribution, pr_attribution) = attribution_texts();
    let commit_step_suffix = if commit_attribution.is_empty() {
        ".".to_string()
    } else {
        format!(" ending with:\n   {commit_attribution}")
    };
    let commit_heredoc_suffix = if commit_attribution.is_empty() {
        String::new()
    } else {
        format!("\n\n   {commit_attribution}")
    };
    let pr_body_suffix = if pr_attribution.is_empty() {
        String::new()
    } else {
        format!("\n\n{pr_attribution}")
    };

    // `r=tH()?$F:_U`: the task-management tool name is `TaskCreate` when V2
    // task tools are enabled (default) and `TodoWrite` when
    // `LINGXI_ENABLE_TASKS` is a defined-falsy value — the same `tH()`/`TE()`
    // gate `is_todo_v2_enabled` uses. The agent tool (`gi`) is always `Agent`.
    let task_tool = if platform_api::env::is_env_defined_falsy(
        std::env::var("LINGXI_ENABLE_TASKS").ok().as_deref(),
    ) {
        "TodoWrite"
    } else {
        "TaskCreate"
    };
    "# Committing changes with git

Only create commits when requested by the user. If unclear, ask first. When the user asks you to create a new git commit, follow these steps carefully:

You can call multiple tools in a single response. When multiple independent pieces of information are requested and all commands are likely to succeed, run multiple tool calls in parallel for optimal performance. The numbered steps below indicate which commands should be batched in parallel.

Git Safety Protocol:
- NEVER update the git config
- NEVER run destructive git commands (push --force, reset --hard, checkout ., restore ., clean -f, branch -D) unless the user explicitly requests these actions. Taking unauthorized destructive actions is unhelpful and can result in lost work, so it's best to ONLY run these commands when given direct instructions\u{20}
- NEVER skip hooks (--no-verify, --no-gpg-sign, etc) unless the user explicitly requests it
- NEVER run force push to main/master, warn the user if they request it
- CRITICAL: Always create NEW commits rather than amending, unless the user explicitly requests a git amend. When a pre-commit hook fails, the commit did NOT happen — so --amend would modify the PREVIOUS commit, which may result in destroying work or losing previous changes. Instead, after hook failure, fix the issue, re-stage, and create a NEW commit
- When staging files, prefer adding specific files by name rather than using \"git add -A\" or \"git add .\", which can accidentally include sensitive files (.env, credentials) or large binaries
- NEVER commit changes unless the user explicitly asks you to. It is VERY IMPORTANT to only commit when explicitly asked, otherwise the user will feel that you are being too proactive

1. Run the following bash commands in parallel, each using the Bash tool:
  - Run a git status command to see all untracked files. IMPORTANT: Never use the -uall flag as it can cause memory issues on large repos.
  - Run a git diff command to see both staged and unstaged changes that will be committed.
  - Run a git log command to see recent commit messages, so that you can follow this repository's commit message style.
2. Analyze all staged changes (both previously staged and newly added) and draft a commit message:
  - Summarize the nature of the changes (eg. new feature, enhancement to an existing feature, bug fix, refactoring, test, docs, etc.). Ensure the message accurately reflects the changes and their purpose (i.e. \"add\" means a wholly new feature, \"update\" means an enhancement to an existing feature, \"fix\" means a bug fix, etc.).
  - Do not commit files that likely contain secrets (.env, credentials.json, etc). Warn the user if they specifically request to commit those files
  - Draft a concise (1-2 sentences) commit message that focuses on the \"why\" rather than the \"what\"
  - Ensure it accurately reflects the changes and their purpose
3. Run the following commands in parallel:
   - Add relevant untracked files to the staging area.
   - Create the commit with a message{COMMIT_STEP_SUFFIX}
   - Run git status after the commit completes to verify success.
   Note: git status depends on the commit completing, so run it sequentially after the commit.
4. If the commit fails due to pre-commit hook: fix the issue and create a NEW commit

Important notes:
- NEVER run additional commands to read or explore code, besides git bash commands
- NEVER use the {TASK_TOOL} or Agent tools
- DO NOT push to the remote repository unless the user explicitly asks you to do so
- IMPORTANT: Never use git commands with the -i flag (like git rebase -i or git add -i) since they require interactive input which is not supported.
- IMPORTANT: Do not use --no-edit with git rebase commands, as the --no-edit flag is not a valid option for git rebase.
- If there are no changes to commit (i.e., no untracked files and no modifications), do not create an empty commit
- In order to ensure good formatting, ALWAYS pass the commit message via a HEREDOC, a la this example:
<example>
git commit -m \"$(cat <<'EOF'
   Commit message here.{COMMIT_HEREDOC_SUFFIX}
   EOF
   )\"
</example>

# Creating pull requests
Use the gh command via the Bash tool for ALL GitHub-related tasks including working with issues, pull requests, checks, and releases. If given a Github URL use the gh command to get the information needed.

IMPORTANT: When the user asks you to create a pull request, follow these steps carefully:

1. Run the following bash commands in parallel using the Bash tool, in order to understand the current state of the branch since it diverged from the main branch:
   - Run a git status command to see all untracked files (never use -uall flag)
   - Run a git diff command to see both staged and unstaged changes that will be committed
   - Check if the current branch tracks a remote branch and is up to date with the remote, so you know if you need to push to the remote
   - Run a git log command and `git diff [base-branch]...HEAD` to understand the full commit history for the current branch (from the time it diverged from the base branch)
2. Analyze all changes that will be included in the pull request, making sure to look at all relevant commits (NOT just the latest commit, but ALL commits that will be included in the pull request!!!), and draft a pull request title and summary:
   - Keep the PR title short (under 70 characters)
   - Use the description/body for details, not the title
3. Run the following commands in parallel:
   - Create new branch if needed
   - Push to remote with -u flag if needed
   - Create PR using gh pr create with the format below. Use a HEREDOC to pass the body to ensure correct formatting.
<example>
gh pr create --title \"the pr title\" --body \"$(cat <<'EOF'
## Summary
<1-3 bullet points>

## Test plan
[Bulleted markdown checklist of TODOs for testing the pull request...]{PR_BODY_SUFFIX}
EOF
)\"
</example>

Important:
- DO NOT use the {TASK_TOOL} or Agent tools
- Return the PR URL when you're done, so the user can see it

# Other common operations
- View comments on a Github PR: gh api repos/foo/bar/pulls/123/comments"
        .replace("{TASK_TOOL}", task_tool)
        .replace("{COMMIT_STEP_SUFFIX}", &commit_step_suffix)
        .replace("{COMMIT_HEREDOC_SUFFIX}", &commit_heredoc_suffix)
        .replace("{PR_BODY_SUFFIX}", &pr_body_suffix)
}

// ===== Public entry point ===================================================

/// Port of `getSimplePrompt` (`prompt.ts:275`) — EXTERNAL-USER path.
///
/// `sandbox` drives [`sandbox_section`]; pass the live
/// `BuiltinToolContext::sandbox_runtime`.
#[must_use]
pub fn simple_prompt(sandbox: &SandboxRuntimeConfig) -> String {
    let max_timeout_ms = bash_max_timeout_ms();
    let default_timeout_ms = bash_default_timeout_ms();

    let tool_preference_items = vec![
        Bullet::Item(format!(
            "File search: Use {GLOB_TOOL_NAME} (NOT find or ls)"
        )),
        Bullet::Item(format!(
            "Content search: Use {GREP_TOOL_NAME} (NOT grep or rg)"
        )),
        Bullet::Item(format!(
            "Read files: Use {FILE_READ_TOOL_NAME} (NOT cat/head/tail)"
        )),
        Bullet::Item(format!(
            "Edit files: Use {FILE_EDIT_TOOL_NAME} (NOT sed/awk)"
        )),
        Bullet::Item(format!(
            "Write files: Use {FILE_WRITE_TOOL_NAME} (NOT echo >/cat <<EOF)"
        )),
        Bullet::Item("Communication: Output text directly (NOT echo/printf)".into()),
    ];

    // External (non-embedded) avoid-list includes find/grep.
    let avoid_commands = "`find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo`";

    // NOTE: claude-code's Bash `u` instruction list has NO "When issuing
    // multiple commands:" item — in 2.1.238 or 2.1.220. The only oracle hit for
    // that header belongs to the PowerShell tool prompt (cc-238.js @230911843),
    // with different wording. Do not re-add a Bash-flavoured rewrite here.

    let git_subitems = vec![
        "Prefer to create a new commit rather than amending an existing commit.".to_string(),
        "Before running destructive operations (e.g., git reset --hard, git push --force, git checkout --), consider whether there is a safer alternative that achieves the same goal. Only use destructive operations when they are truly the best approach.".to_string(),
        "Never skip hooks (--no-verify) or bypass signing (--no-gpg-sign, -c commit.gpgsign=false) unless the user has explicitly asked for it. If a hook fails, investigate and fix the underlying issue.".to_string(),
    ];

    // Non-Monitor branch (Monitor is a deferred tool here).
    let sleep_subitems = vec![
        "Do not sleep between commands that can run immediately — just run them.".to_string(),
        "If your command is long running and you would like to be notified when it finishes — use `run_in_background`. No sleep needed.".to_string(),
        "Do not retry failing commands in a sleep loop — diagnose the root cause.".to_string(),
        "If waiting for a background task you started with `run_in_background`, you will be notified when it completes — do not poll.".to_string(),
        "If you must poll an external process, use a check command (e.g. `gh run view`) rather than sleeping first.".to_string(),
        "If you must sleep, keep the duration short to avoid blocking the user.".to_string(),
    ];

    let background_note = background_usage_note();

    let mut instruction_items: Vec<Bullet> = vec![
        Bullet::Item("If your command will create new directories or files, first use this tool to run `ls` to verify the parent directory exists and is the correct location.".into()),
        Bullet::Item("Always quote file paths that contain spaces with double quotes in your command (e.g., cd \"path with spaces/file.txt\")".into()),
        Bullet::Item("Try to maintain your current working directory throughout the session by using absolute paths and avoiding usage of `cd`. You may use `cd` if the User explicitly requests it. In particular, never prepend `cd <current-directory>` to a `git` command \u{2014} `git` already operates on the current working tree, and the compound triggers a permission prompt.".into()),
        Bullet::Item(format!(
            "You may specify an optional timeout in milliseconds (up to {max_timeout_ms}ms / {} minutes). By default, your command will timeout after {default_timeout_ms}ms ({} minutes).",
            max_timeout_ms / 60_000,
            default_timeout_ms / 60_000
        )),
    ];
    if let Some(note) = background_note {
        instruction_items.push(Bullet::Item(note));
    }
    instruction_items.push(Bullet::Item("For git commands:".into()));
    instruction_items.push(Bullet::Sub(git_subitems));
    instruction_items.push(Bullet::Item("Avoid unnecessary `sleep` commands:".into()));
    instruction_items.push(Bullet::Sub(sleep_subitems));

    let mut lines: Vec<String> = vec![
        "Executes a given bash command and returns its output.".into(),
        String::new(),
        // KEPT VERBATIM — prompt parity (see module-doc BASH.4 note: cwd
        // persistence is currently aspirational until BASH.4 lands).
        "The working directory persists between commands, but shell state does not. The shell environment is initialized from the user's profile (bash or zsh).".into(),
        String::new(),
        format!("IMPORTANT: Avoid using this tool to run {avoid_commands} commands, unless explicitly instructed or after you have verified that a dedicated tool cannot accomplish your task. Instead, use the appropriate dedicated tool as this will provide a much better experience for the user:"),
        String::new(),
    ];
    lines.extend(prepend_bullets(&tool_preference_items));
    lines.push(format!("While the {BASH_TOOL_NAME} tool can do similar things, it\u{2019}s better to use the built-in tools as they provide a better user experience and make it easier to review tool calls and give permission."));
    lines.push(String::new());
    lines.push("# Instructions".into());
    lines.extend(prepend_bullets(&instruction_items));
    lines.push(sandbox_section(sandbox));

    let git = commit_and_pr_instructions();
    if !git.is_empty() {
        lines.push(String::new());
        lines.push(git);
    }

    lines.join("\n")
}

// ===== CONCISE (current-gen) variant ========================================

/// Port of claude-code's CONCISE Bash git section `$Up(e)` (binary @202732xxx),
/// the SHORT-prompt analogue of [`commit_and_pr_instructions`]. EXTERNAL-USER
/// path.
///
/// The TS shape is:
/// ```js
/// function $Up(e){
///   if(!aOt()) return "";                 // shouldIncludeGitInstructions
///   let n="", {commit:r, pr:o}=qdt(),      // commit/pr ATTRIBUTION
///       i=[r?`- End git commit messages with:\n${r}`:null,
///          o?`- End PR bodies with:\n${o}`:null].filter(Boolean).join("\n"),
///       a=_Xa() /* "" */, l=null, c=nqn(e) /* "" */;
///   return `${n}# Git
/// - Interactive flags (...) are not supported in this environment.
/// - Use the \`gh\` CLI for GitHub operations (PRs, issues, API).
/// - Commit or push only when the user asks${c?...:""}. If on the default branch, branch first.${i?`\n${i}`:""}${c?`\n- ${c}`:""}${a?`\n\n${a}`:""}${l?`\n\n${l}`:""}`
/// }
/// ```
///
/// Resolutions for the LingXi default (posix, external build):
/// - `aOt()` → [`should_include_git_instructions`] (always-on stub here);
/// - `_Xa()`/`nqn(e)` BOTH return `""` in the binary (`a`/`c` empty), and `l`
///   is hard-`null`, so the trailing `${c}`/`${a}`/`${l}` interpolations vanish
///   and the "only after the pre-ship checks below" clause never appears;
/// - `qdt()` (2.1.238 `hvt()`) is the commit/PR ATTRIBUTION. Its DEFAULT is
///   NON-empty, so the two `- End …` bullets ARE emitted — see
///   [`attribution_texts`]. They form the oracle's `i`/`a` block:
///   `[commit?`- End git commit messages with:\n${commit}`:null,
///     pr?`- End PR bodies with:\n${pr}`:null].filter(Boolean).join("\n")`,
///   appended to the `- Commit or push only when the user asks…` bullet as
///   `${a?`\n${a}`:""}`. The attribution VALUE is NOT indented — it sits at
///   column 0 on its own line under each bullet.
fn concise_git_section() -> String {
    if !should_include_git_instructions() {
        return String::new();
    }
    // `c`/`a`/`l` (pre-ship gate, `bash_lean` extras) are all empty in the
    // shipped build, so the section is the three fixed bullets plus the
    // attribution block.
    let mut section = "# Git\n\
     - Interactive flags (`-i`, e.g. `git rebase -i`, `git add -i`) are not supported in this environment.\n\
     - Use the `gh` CLI for GitHub operations (PRs, issues, API).\n\
     - Commit or push only when the user asks. If on the default branch, branch first."
        .to_string();

    let (commit_attribution, pr_attribution) = attribution_texts();
    let mut attribution_lines: Vec<String> = Vec::new();
    if !commit_attribution.is_empty() {
        attribution_lines.push(format!(
            "- End git commit messages with:\n{commit_attribution}"
        ));
    }
    if !pr_attribution.is_empty() {
        attribution_lines.push(format!("- End PR bodies with:\n{pr_attribution}"));
    }
    if !attribution_lines.is_empty() {
        section.push('\n');
        section.push_str(&attribution_lines.join("\n"));
    }
    section
}

/// Port of `getSimplePrompt`'s CONCISE branch `qUp(e)` (binary @202741xxx) —
/// the `Dh(model)`-true Bash prompt that claude-code serves current-gen models
/// (`claude-opus-4-8` / `claude-fable-5` / `claude-mythos-5`). EXTERNAL-USER,
/// posix, non-Monitor, non-embedded-search-tools defaults.
///
/// The TS builder:
/// ```js
/// function qUp(e){
///   let t=gXa()!==null,            // background-usage note exists?
///       n=$Up(e),                  // CONCISE git section
///       r=yXa(),                   // SANDBOX section (SAME as VERBOSE)
///       o=Zw()?"…":"`find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo`",
///       s=[];
///   if(t){ let a="- `run_in_background` runs the command detached: it keeps running across turns and re-invokes you when it exits. No `&` needed.";
///          if(sq()) a+=" Foreground `sleep` is blocked; use Monitor with an until-loop to wait on a condition.";
///          s.push(a); }
///   let i=hXa();                   // i = null on non-win32
///   return ["Executes a bash command and returns its output.",
///     ...i?["",i]:[],              // (posix ⇒ empty)
///     "",
///     "- Working directory persists between calls, but prefer absolute paths — `cd` in a compound command can trigger a permission prompt. Shell state (env vars, functions) does not persist; the shell is initialized from the user's profile.",
///     `- IMPORTANT: Avoid using this tool to run ${o} commands, unless explicitly instructed or after you have verified that a dedicated tool cannot accomplish your task. Instead, use the appropriate dedicated tool as this will provide a much better experience for the user.`,
///     `- \`timeout\` is in milliseconds: default ${d4t()}, max ${Wdt()}.`,
///     ...s,
///     ...r?[r]:[],
///     ...n?["",n]:[]].join("\n")
/// }
/// ```
///
/// Resolutions (LingXi default):
/// - `i=hXa()` is `null` on non-win32 (a Windows-only "Git Bash (POSIX sh)"
///   advisory, NOT model attribution) → the leading spread is empty. Bash is
///   unsupported on Windows here anyway, mirroring the VERBOSE prompt which
///   likewise omits its `d=hXa()`.
/// - `o` = avoid-list. `Zw()` (embedded-search-tools) is the gated ant-native
///   path; the external default takes the find/grep-INCLUSIVE list — IDENTICAL
///   to the VERBOSE prompt's hardcoded `avoid_commands`.
/// - 2.1.238 `hcT` additionally emits
///   `"- Command output is displayed to you, not reliably to the user."`
///   UNCONDITIONALLY, right after the IMPORTANT avoid-list bullet and before the
///   `timeout` bullet. 2.1.220's `$ry` gated it on `KFc(t)` /
///   `CLAUDE_CODE_MARL_CORMORANT`; that env gate no longer exists in 2.1.238 and
///   the builder lost its `model` parameter, so the `_model` argument here is
///   vestigial (kept for call-site compatibility).
/// - `t` (`gXa()!==null`) ⟺ [`background_usage_note`] is `Some` (gated by
///   `LINGXI_DISABLE_BACKGROUND_TASKS`). When present, the detached
///   `run_in_background` bullet is emitted; `sq()` (the `tengu_amber_sentinel`
///   / Monitor gate) is default-false here, so the trailing "Foreground `sleep`
///   is blocked…" clause is omitted — same default as the VERBOSE sleep block.
/// - `r=yXa()` is the SAME [`sandbox_section`] the VERBOSE prompt uses, so the
///   CONCISE variant IS sandbox-dependent (appended when non-empty).
/// - `d4t()`/`Wdt()` are [`BASH_DEFAULT_TIMEOUT_MS`]/[`BASH_MAX_TIMEOUT_MS`]
///   rendered RAW (no `/ N minutes` conversion, unlike VERBOSE).
///
/// Em-dash is U+2014 (the binary stores it as the JS escape `—`).
#[must_use]
pub fn simple_prompt_concise(sandbox: &SandboxRuntimeConfig, _model: Option<&str>) -> String {
    // CONCISE avoid-list — claude-code 2.1.238 `hcT` picks it with the SAME
    // `VH()` (embedded-search-tools) predicate the VERBOSE builder `Yhm` uses:
    //   VH() ? "`cat`, …" : "`find`, `grep`, `cat`, …"
    // LingXi ships Glob and Grep as real tools, i.e. the non-embedded branch —
    // exactly what the VERBOSE prompt already hardcodes. Both prompts must agree
    // on the one boolean, so the CONCISE list is the find/grep-INCLUSIVE one.
    let avoid_commands = "`find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo`";

    let mut lines: Vec<String> = vec![
        "Executes a bash command and returns its output.".into(),
        // `i=hXa()` is null on posix ⇒ no leading advisory; first real line is "".
        String::new(),
        "- Working directory persists between calls, but prefer absolute paths \u{2014} `cd` in a compound command can trigger a permission prompt. Shell state (env vars, functions) does not persist; the shell is initialized from the user's profile.".into(),
        format!("- IMPORTANT: Avoid using this tool to run {avoid_commands} commands, unless explicitly instructed or after you have verified that a dedicated tool cannot accomplish your task. Instead, use the appropriate dedicated tool as this will provide a much better experience for the user."),
    ];
    // UNCONDITIONAL in 2.1.238: `hcT` emits this as a bare array element and no
    // longer receives a model argument at all. The 2.1.220 gate
    // (`KFc(t)` / `CLAUDE_CODE_MARL_CORMORANT`) was deleted between the builds,
    // so every lean-prompt model gets the bullet, not just opus-5.
    lines.push("- Command output is displayed to you, not reliably to the user.".into());
    // `c` — the gated "cheap to run" bullet, between the output-visibility
    // bullet and the timeout bullet (`…,"- Command output …",...c,`- \`timeout\`
    // …`,…`). `JQd()` is default-false, so this is inert in the stock config.
    if cheap_commands_bullet_enabled() {
        lines.push("- Commands are cheap to run and their errors are informative: run the straightforward command rather than perfecting it mentally first, and adjust from what it prints.".into());
    }
    lines.push(format!(
        "- `timeout` is in milliseconds: default {}, max {}.",
        bash_default_timeout_ms(),
        bash_max_timeout_ms()
    ));

    // `s` — the detached-run bullet, present iff the background note exists.
    // `sq()` (Monitor / amber sentinel) is default-false ⇒ no Monitor clause.
    if background_usage_note().is_some() {
        lines.push("- `run_in_background` runs the command detached: it keeps running across turns and re-invokes you when it exits. No `&` needed.".into());
    }

    // `r=yXa()` — sandbox section (same as VERBOSE), appended when non-empty.
    let sandbox_text = sandbox_section(sandbox);
    if !sandbox_text.is_empty() {
        lines.push(sandbox_text);
    }

    // `n=$Up(e)` — CONCISE git section, preceded by a blank line when non-empty.
    let git = concise_git_section();
    if !git.is_empty() {
        lines.push(String::new());
        lines.push(git);
    }

    lines.join("\n")
}

/// One lock for every test in this crate that reads or writes the
/// process-global env vars the Bash prompt gates on
/// (`LINGXI_DISABLE_BACKGROUND_TASKS` in particular).
///
/// This exists because the crate previously had THREE separate locks guarding
/// that one global — `prompt::tests::ENV_LOCK`, `bash::tests::SLEEP_GATE_LOCK`
/// and `bash::tests::BACKGROUND_TASKS_ENV_LOCK`. Three locks over one global is
/// no mutual exclusion at all: a `prompt.rs` test setting the var could flip the
/// `run_in_background` bullet out from under a `bash.rs` prompt assertion, which
/// failed roughly once per full-workspace run and passed every time in
/// isolation. Take THIS lock, not a new one.
///
/// It is a `std` mutex on purpose: the guard is held across `.await` in the
/// `#[tokio::test]` cases, which is sound because those run on a current-thread
/// runtime whose future is not required to be `Send`.
#[cfg(test)]
pub(crate) static BACKGROUND_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Poison-tolerant guard for [`BACKGROUND_ENV_LOCK`] (payload is `()`, so a
/// panicking test never invalidates the lock for the rest of the run).
#[cfg(test)]
pub(crate) fn background_env_lock() -> std::sync::MutexGuard<'static, ()> {
    BACKGROUND_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::background_env_lock as env_lock;

    fn disabled_sandbox() -> SandboxRuntimeConfig {
        SandboxRuntimeConfig::default()
    }

    #[test]
    fn prompt_contains_locked_anchors() {
        let _g = env_lock();
        std::env::remove_var("LINGXI_DISABLE_BACKGROUND_TASKS");
        let p = simple_prompt(&disabled_sandbox());

        // Header.
        assert!(
            p.contains("Executes a given bash command and returns its output."),
            "missing header anchor"
        );
        // cwd-persistence sentence (kept verbatim for parity).
        assert!(
            p.contains(
                "The working directory persists between commands, but shell state does not."
            ),
            "missing cwd-persistence anchor"
        );
        // run_in_background note (present when env not disabled).
        assert!(
            p.contains("You can use the `run_in_background` parameter to run the command in the background."),
            "missing run_in_background note"
        );
        // Git safety protocol header.
        assert!(
            p.contains("Git Safety Protocol:"),
            "missing git safety protocol header"
        );
        // Committing-changes section header.
        assert!(
            p.contains("# Committing changes with git"),
            "missing committing-changes header"
        );
        assert!(
            p.contains("given direct instructions \n- NEVER skip hooks"),
            "2.1.238 verbose git prompt requires the trailing space before the newline"
        );
        // cwd/`cd` bullet — byte-locked to claude-code v2.1.183 (incl. the
        // trailing git/cd sentence; em-dash is U+2014).
        assert!(
            p.contains(
                "Try to maintain your current working directory throughout the session by using absolute paths and avoiding usage of `cd`. You may use `cd` if the User explicitly requests it. In particular, never prepend `cd <current-directory>` to a `git` command \u{2014} `git` already operates on the current working tree, and the compound triggers a permission prompt."
            ),
            "missing/incorrect cwd `cd`/git bullet (git/cd sentence must be present, em-dash U+2014)"
        );
        // sleep bullet — byte-locked to claude-code v2.1.183 (NO "(1-5 seconds)").
        assert!(
            p.contains("If you must sleep, keep the duration short to avoid blocking the user."),
            "missing/incorrect sleep bullet"
        );
        assert!(
            !p.contains("(1-5 seconds)"),
            "sleep bullet must not contain the invented \"(1-5 seconds)\" qualifier"
        );
    }

    /// `- NEVER use the ${r} or ${gi} tools` / `- DO NOT use the ${r} or ${gi}
    /// tools`: `r=tH()?$F:_U` (TaskCreate by default, TodoWrite when
    /// `LINGXI_ENABLE_TASKS` is defined-falsy); `gi="Agent"` always.
    #[test]
    fn git_prompt_interpolates_task_and_agent_tool_names() {
        let _g = env_lock();

        // Default (V2 tasks enabled) → "TaskCreate or Agent".
        std::env::remove_var("LINGXI_ENABLE_TASKS");
        let p = simple_prompt(&disabled_sandbox());
        assert!(
            p.contains("- NEVER use the TaskCreate or Agent tools"),
            "default NEVER bullet"
        );
        assert!(
            p.contains("- DO NOT use the TaskCreate or Agent tools"),
            "default DO NOT bullet"
        );
        assert!(
            !p.contains("TodoWrite or Task tools"),
            "must not carry the stale hardcoded names"
        );

        // Defined-falsy LINGXI_ENABLE_TASKS → "TodoWrite or Agent".
        std::env::set_var("LINGXI_ENABLE_TASKS", "0");
        let p = simple_prompt(&disabled_sandbox());
        assert!(
            p.contains("- NEVER use the TodoWrite or Agent tools"),
            "disabled NEVER bullet"
        );
        assert!(
            p.contains("- DO NOT use the TodoWrite or Agent tools"),
            "disabled DO NOT bullet"
        );
        std::env::remove_var("LINGXI_ENABLE_TASKS");
    }

    #[test]
    fn timeout_sentence_substitutes_locked_constants() {
        let _g = env_lock();
        std::env::remove_var("LINGXI_DISABLE_BACKGROUND_TASKS");
        let p = simple_prompt(&disabled_sandbox());
        // 120000ms / 600000ms substituted from BASH_DEFAULT_TIMEOUT_MS /
        // BASH_MAX_TIMEOUT_MS — and the minute conversions.
        assert_eq!(crate::bash::BASH_MAX_TIMEOUT_MS, 600_000);
        assert_eq!(crate::bash::BASH_DEFAULT_TIMEOUT_MS, 120_000);
        assert!(
            p.contains(
                "You may specify an optional timeout in milliseconds (up to 600000ms / 10 minutes). By default, your command will timeout after 120000ms (2 minutes)."
            ),
            "missing/incorrect timeout sentence; got prompt:\n{p}"
        );
    }

    #[test]
    fn tool_preference_bullets_steer_to_builtin_tools() {
        let _g = env_lock();
        let p = simple_prompt(&disabled_sandbox());
        assert!(p.contains(" - File search: Use Glob (NOT find or ls)"));
        assert!(p.contains(" - Content search: Use Grep (NOT grep or rg)"));
        assert!(p.contains(" - Read files: Use Read (NOT cat/head/tail)"));
        assert!(p.contains(" - Edit files: Use Edit (NOT sed/awk)"));
        assert!(p.contains(" - Write files: Use Write (NOT echo >/cat <<EOF)"));
    }

    #[test]
    fn background_note_absent_when_env_disabled() {
        let _g = env_lock();
        std::env::set_var("LINGXI_DISABLE_BACKGROUND_TASKS", "1");
        let p = simple_prompt(&disabled_sandbox());
        std::env::remove_var("LINGXI_DISABLE_BACKGROUND_TASKS");
        assert!(
            !p.contains("You can use the `run_in_background` parameter"),
            "run_in_background note should be absent when LINGXI_DISABLE_BACKGROUND_TASKS=1"
        );
    }

    #[test]
    fn git_section_absent_when_disabled_via_env() {
        // R-MINOR: LINGXI_DISABLE_GIT_INSTRUCTIONS (truthy) omits the git/PR
        // section (claude-code `aOt()`); default-unset keeps it.
        let _g = env_lock();
        std::env::remove_var("LINGXI_DISABLE_GIT_INSTRUCTIONS");
        assert!(
            simple_prompt(&disabled_sandbox()).contains("# Committing changes with git"),
            "git section present by default"
        );
        std::env::set_var("LINGXI_DISABLE_GIT_INSTRUCTIONS", "1");
        let p = simple_prompt(&disabled_sandbox());
        std::env::remove_var("LINGXI_DISABLE_GIT_INSTRUCTIONS");
        assert!(
            !p.contains("# Committing changes with git"),
            "git section should be absent when LINGXI_DISABLE_GIT_INSTRUCTIONS=1"
        );
    }

    #[test]
    fn sandbox_section_absent_when_disabled() {
        let _g = env_lock();
        let p = simple_prompt(&disabled_sandbox());
        assert!(
            !p.contains("## Command sandbox"),
            "sandbox section should be absent when sandbox disabled"
        );
    }

    #[test]
    fn concise_prompt_output_visibility_bullet_is_unconditional() {
        // 2.1.238 `hcT` emits this bullet as a bare array element — the 2.1.220
        // `CLAUDE_CODE_MARL_CORMORANT` gate was deleted and the builder no
        // longer takes a model at all, so EVERY lean-prompt model gets it.
        let bullet = "- Command output is displayed to you, not reliably to the user.";
        for model in [
            None,
            Some("claude-opus-5[1m]"),
            Some("claude-opus-4-8"),
            Some("claude-fable-5-1"),
        ] {
            let p = simple_prompt_concise(&disabled_sandbox(), model);
            assert!(p.contains(bullet), "missing for model {model:?}");
            assert!(
                p.contains("IMPORTANT: Avoid using this tool"),
                "the confirmed existing warning must not be deleted"
            );
        }
    }

    #[test]
    fn concise_prompt_avoid_list_matches_the_verbose_one() {
        // Oracle `hcT` and `Yhm` read the SAME `VH()` predicate; LingXi is on
        // the non-embedded branch (Glob/Grep are real tools), so both prompts
        // must carry the find/grep-INCLUSIVE list.
        let long = "`find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo`";
        let p = simple_prompt_concise(&disabled_sandbox(), Some("claude-opus-5[1m]"));
        assert!(
            p.contains(long),
            "concise avoid-list must include find/grep"
        );
    }

    #[test]
    fn sandbox_section_present_when_enabled() {
        let _g = env_lock();
        let cfg = SandboxRuntimeConfig {
            enabled: true,
            // Default is now `true` (allow unsandboxed) — set `false` explicitly so
            // the "All commands MUST run in sandbox mode" assertion still holds.
            allow_unsandboxed_commands: false,
            filesystem: sandbox::runtime_config::FilesystemRestrictionConfig {
                allow_write: vec!["/work".into()],
                ..Default::default()
            },
            network: sandbox::runtime_config::NetworkRestrictionConfig {
                allowed_domains: vec!["example.com".into()],
                denied_domains: vec!["blocked.example".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let p = simple_prompt(&cfg);
        assert!(p.contains("## Command sandbox"), "sandbox section missing");
        assert!(
            p.contains("By default, your command will be run in a sandbox."),
            "sandbox intro missing"
        );
        // Filesystem + network restriction lines rendered from the config.
        assert!(p.contains("Filesystem: "), "filesystem line missing");
        assert!(
            p.contains("\"allowOnly\":[\"/work\"]"),
            "writable path not inlined; got:\n{p}"
        );
        assert!(
            p.contains("Network egress goes through a filtering proxy.")
                && !p.contains("allowedHosts"),
            "network line missing; got:\n{p}"
        );
        // disabled-by-policy branch (allow_unsandboxed_commands=false).
        assert!(
            p.contains("All commands MUST run in sandbox mode"),
            "policy-disabled override branch missing"
        );
        // $TMPDIR bullet always present.
        assert!(p.contains("`$TMPDIR` environment variable"));
    }

    #[test]
    fn sandbox_section_unsandboxed_allowed_branch() {
        let _g = env_lock();
        let cfg = SandboxRuntimeConfig {
            enabled: true,
            allow_unsandboxed_commands: true,
            ..Default::default()
        };
        let p = simple_prompt(&cfg);
        assert!(
            p.contains("You should always default to running commands within the sandbox."),
            "allow-unsandboxed override branch missing"
        );
        assert!(
            !p.contains("All commands MUST run in sandbox mode"),
            "policy-disabled branch should not appear when unsandboxed cmds allowed"
        );
    }

    #[test]
    fn sandbox_section_emits_allow_within_deny_when_allow_read_set() {
        let _g = env_lock();
        // allow_read non-empty ⇒ read object carries denyOnly THEN allowWithinDeny.
        let cfg = SandboxRuntimeConfig {
            enabled: true,
            filesystem: sandbox::runtime_config::FilesystemRestrictionConfig {
                deny_read: vec!["/etc".into()],
                allow_read: vec!["/etc/hosts".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let p = simple_prompt(&cfg);
        assert!(
            p.contains("\"read\":{\"denyOnly\":[\"/etc\"],\"allowWithinDeny\":[\"/etc/hosts\"]}"),
            "read object should carry denyOnly then allowWithinDeny in order; got:\n{p}"
        );

        // Empty allow_read ⇒ allowWithinDeny ABSENT.
        let cfg2 = SandboxRuntimeConfig {
            enabled: true,
            filesystem: sandbox::runtime_config::FilesystemRestrictionConfig {
                deny_read: vec!["/etc".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let p2 = simple_prompt(&cfg2);
        assert!(
            !p2.contains("allowWithinDeny"),
            "allowWithinDeny must be absent when allow_read empty; got:\n{p2}"
        );
    }

    // ===== BASH-11 — `Phr` truncation at D_l = 50 =========================

    /// `function Phr(e){if(!e||e.length<=D_l)return e;let t=e.length-D_l;
    ///  return[...e.slice(0,D_l),`... and ${t} more (truncated for prompt size)`]}`
    #[test]
    fn truncate_for_prompt_caps_at_fifty_with_the_oracle_marker() {
        // At the cap: untouched, no marker.
        let fifty: Vec<String> = (0..50).map(|i| format!("/p{i}")).collect();
        assert_eq!(truncate_for_prompt(fifty.clone()), fifty);
        // One over: 50 kept + the marker (51 entries), NOT 49 + marker.
        let fifty_one: Vec<String> = (0..51).map(|i| format!("/p{i}")).collect();
        let out = truncate_for_prompt(fifty_one);
        assert_eq!(out.len(), 51);
        assert_eq!(out[49], "/p49");
        assert_eq!(out[50], "... and 1 more (truncated for prompt size)");
        // Well over.
        let many: Vec<String> = (0..73).map(|i| format!("/p{i}")).collect();
        let out = truncate_for_prompt(many);
        assert_eq!(
            out.last().unwrap(),
            "... and 23 more (truncated for prompt size)"
        );
    }

    /// The cap must apply to EVERY list the sandbox section renders, AFTER the
    /// dedup / `$TMPDIR` normalization (oracle `Phr(gXr(list))` /
    /// `Phr(l(t.allowOnly))`).
    #[test]
    fn sandbox_section_truncates_every_oversized_list() {
        let _g = env_lock();
        let paths =
            |prefix: &str| -> Vec<String> { (0..60).map(|i| format!("{prefix}{i}")).collect() };
        let cfg = SandboxRuntimeConfig {
            enabled: true,
            filesystem: sandbox::runtime_config::FilesystemRestrictionConfig {
                deny_read: paths("/dr"),
                allow_read: paths("/ar"),
                allow_write: paths("/aw"),
                deny_write: paths("/dw"),
                ..Default::default()
            },
            network: sandbox::runtime_config::NetworkRestrictionConfig {
                allowed_domains: (0..60).map(|i| format!("a{i}.com")).collect(),
                denied_domains: (0..60).map(|i| format!("d{i}.com")).collect(),
                allow_unix_sockets: paths("/sock"),
                ..Default::default()
            },
            ..Default::default()
        };
        let p = simple_prompt(&cfg);
        // Six lists × one marker each (allowedHosts no longer render).
        assert_eq!(
            p.matches("... and 10 more (truncated for prompt size)")
                .count(),
            6,
            "every sandbox list must be capped at 50; got:\n{p}"
        );
        // The 50th entry survives, the 51st does not.
        assert!(p.contains("\"/dr49\""), "entry 50 must survive; got:\n{p}");
        assert!(
            !p.contains("\"/dr50\""),
            "entry 51 must be dropped; got:\n{p}"
        );
    }

    #[test]
    fn sandbox_section_emits_denied_hosts_in_order() {
        let _g = env_lock();
        let cfg = SandboxRuntimeConfig {
            enabled: true,
            network: sandbox::runtime_config::NetworkRestrictionConfig {
                allowed_domains: vec!["a.com".into()],
                denied_domains: vec!["b.com".into()],
                allow_unix_sockets: vec!["/s".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let p = simple_prompt(&cfg);
        assert!(
            p.contains("\"deniedHosts\":[\"b.com\"],\"allowUnixSockets\":[\"/s\"]"),
            "network keys must be deniedHosts,allowUnixSockets in order; got:\n{p}"
        );
        assert!(
            !p.contains("allowedHosts"),
            "allowedHosts must be omitted from the prompt; got:\n{p}"
        );

        // Empty denied_domains ⇒ deniedHosts ABSENT.
        let cfg2 = SandboxRuntimeConfig {
            enabled: true,
            network: sandbox::runtime_config::NetworkRestrictionConfig {
                allowed_domains: vec!["a.com".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let p2 = simple_prompt(&cfg2);
        assert!(
            !p2.contains("deniedHosts"),
            "deniedHosts must be absent when denied_domains empty; got:\n{p2}"
        );
        assert!(
            !p2.contains("Network egress goes through a filtering proxy."),
            "filtering proxy guidance must be absent without a rendered network object; got:\n{p2}"
        );
    }

    #[test]
    fn sandbox_section_normalizes_lingxi_temp_dir_to_tmpdir_literal() {
        let _g = env_lock();
        // Pin the temp-dir base via LINGXI_TMPDIR so lingxi_temp_dir() is
        // deterministic across hosts/users.
        let tmp_base = tempfile::tempdir().unwrap();
        let prior = std::env::var_os("LINGXI_TMPDIR");
        std::env::set_var("LINGXI_TMPDIR", tmp_base.path());

        let lingxi_dir = lingxi_temp_dir();
        let cfg = SandboxRuntimeConfig {
            enabled: true,
            filesystem: sandbox::runtime_config::FilesystemRestrictionConfig {
                // The Claude temp dir collapses to $TMPDIR; an unrelated path is
                // emitted verbatim.
                allow_write: vec![lingxi_dir.clone(), "/work/project".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let p = simple_prompt(&cfg);

        match prior {
            Some(v) => std::env::set_var("LINGXI_TMPDIR", v),
            None => std::env::remove_var("LINGXI_TMPDIR"),
        }

        assert!(
            p.contains("\"allowOnly\":[\"$TMPDIR\",\"/work/project\"]"),
            "claude temp dir must normalize to $TMPDIR (not the literal); got:\n{p}"
        );
        assert!(
            !p.contains(&lingxi_dir),
            "the per-UID temp dir literal must NOT appear; got:\n{p}"
        );
    }

    #[test]
    fn sandbox_section_policy_disabled_branch_when_bool_false() {
        let _g = env_lock();
        // false ⇒ the policy-disabled override branch.
        let disabled = SandboxRuntimeConfig {
            enabled: true,
            allow_unsandboxed_commands: false,
            ..Default::default()
        };
        let p = simple_prompt(&disabled);
        assert!(
            p.contains("All commands MUST run in sandbox mode"),
            "policy-disabled branch missing when allow_unsandboxed_commands=false; got:\n{p}"
        );

        // true ⇒ the default-to-sandbox override branch.
        let allowed = SandboxRuntimeConfig {
            enabled: true,
            allow_unsandboxed_commands: true,
            ..Default::default()
        };
        let p2 = simple_prompt(&allowed);
        assert!(
            p2.contains("You should always default to running commands within the sandbox."),
            "default-to-sandbox branch missing when allow_unsandboxed_commands=true; got:\n{p2}"
        );
    }

    #[test]
    fn prepend_bullets_indentation() {
        let items = vec![Bullet::Item("top".into()), Bullet::Sub(vec!["sub".into()])];
        let out = prepend_bullets(&items);
        assert_eq!(out, vec![" - top".to_string(), "  - sub".to_string()]);
    }

    /// With no settings at all, both trailers keep their defaults — the arm the
    /// port has always had, and the one every other test here depends on.
    #[test]
    fn no_attribution_settings_keeps_both_defaults() {
        let (commit, pr) = resolve_attribution(None, None, None, "COMMIT", "PR");
        assert_eq!(commit, "COMMIT");
        assert_eq!(pr, "PR");
    }

    /// `{ commit: i.commit ?? n, pr: i.pr ?? r }` — naming ONE field overrides
    /// only that one. The other must keep its default rather than blanking,
    /// which is the whole point of a per-trailer override.
    #[test]
    fn naming_one_trailer_leaves_the_other_at_its_default() {
        let (commit, pr) = resolve_attribution(Some("mine".into()), None, None, "COMMIT", "PR");
        assert_eq!(commit, "mine");
        assert_eq!(pr, "PR", "an unnamed trailer must not be erased");

        let (commit, pr) = resolve_attribution(None, Some("mine".into()), None, "COMMIT", "PR");
        assert_eq!(commit, "COMMIT");
        assert_eq!(pr, "mine");
    }

    /// `?? ` is null-coalescing, so an explicit EMPTY STRING is a VALUE, not an
    /// absence: it disables that one trailer while the other stays put. If
    /// "unset" and "set to empty" ever collapse, this silently stops working in
    /// the direction that keeps emitting the trailer.
    #[test]
    fn an_empty_string_disables_just_that_trailer() {
        let (commit, pr) = resolve_attribution(Some(String::new()), None, None, "COMMIT", "PR");
        assert_eq!(commit, "", "an explicit empty commit trailer is honoured");
        assert_eq!(pr, "PR", "and does not touch the PR line");
    }

    /// `if (o.includeCoAuthoredBy === !1) return { commit: "", pr: "" }` — the
    /// coarse switch empties BOTH, and only on an explicit `false`. Unset is a
    /// third state and must not behave like `false`.
    #[test]
    fn include_co_authored_by_false_empties_both_but_unset_does_not() {
        let (commit, pr) = resolve_attribution(None, None, Some(false), "COMMIT", "PR");
        assert_eq!((commit.as_str(), pr.as_str()), ("", ""));

        let (commit, pr) = resolve_attribution(None, None, None, "COMMIT", "PR");
        assert_eq!((commit.as_str(), pr.as_str()), ("COMMIT", "PR"));

        let (commit, pr) = resolve_attribution(None, None, Some(true), "COMMIT", "PR");
        assert_eq!((commit.as_str(), pr.as_str()), ("COMMIT", "PR"));
    }

    /// ARM ORDER: the object is tested FIRST, so a named trailer still applies
    /// even alongside `includeCoAuthoredBy: false`. Reversing the two arms would
    /// blank a trailer the user explicitly set, and nothing else here would
    /// notice.
    #[test]
    fn the_attribution_object_wins_over_include_co_authored_by() {
        let (commit, pr) =
            resolve_attribution(Some("mine".into()), None, Some(false), "COMMIT", "PR");
        assert_eq!(
            commit, "mine",
            "an explicit trailer survives the coarse switch"
        );
        assert_eq!(
            pr, "PR",
            "and the unnamed one falls back to its DEFAULT, not to empty"
        );
    }

    /// `iQ()`'s precedence. The env var wins whenever DEFINED, in both
    /// directions; only an absent env falls through to the setting.
    #[test]
    fn git_instructions_env_wins_in_both_directions() {
        // Absent env: the setting decides, defaulting to on.
        assert!(include_git_instructions_from(None, None), "default is on");
        assert!(include_git_instructions_from(None, Some(true)));
        assert!(
            !include_git_instructions_from(None, Some(false)),
            "a settings false IS observable — it is the normal case, since the \
             env var is usually absent"
        );

        // Defined env: `return !e`, so it overrides the setting either way.
        assert!(!include_git_instructions_from(Some(true), Some(true)));
        assert!(
            include_git_instructions_from(Some(false), Some(false)),
            "an explicit env `0` puts the sections back over a settings false"
        );
    }

    /// "Unset" and "set to false" are different env states. Collapsing them
    /// turns the re-enabling override into a no-op, and nothing else notices.
    #[test]
    fn an_unset_env_is_not_the_same_as_an_env_set_false() {
        assert!(
            !include_git_instructions_from(None, Some(false)),
            "unset env: the setting applies"
        );
        assert!(
            include_git_instructions_from(Some(false), Some(false)),
            "env explicitly false: it wins and re-enables"
        );
    }
}
