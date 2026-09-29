//! `EnterWorktreeTool` + `ExitWorktreeTool` — manage disposable git worktrees
//! via the M2-01 [`WorktreeManager`] trait. Locks (spec §7 lines 488-489):
//! - Branch prefix: `worktree-<flatten(slug)>`
//! - Path layout:   `<repo>/.lingxi/worktrees/<flatten(slug)>`
//! - Slug flatten:  `'/' -> '+'` (injective; `'+'` outside allowed charset)
//!
//! The slug-validation and flatten helpers are reimplemented locally in this
//! module to match the M2-01 contract byte-for-byte, avoiding a dependency
//! cycle (`lingxi-tools -> lingxi-platform-posix -> lingxi-lsp ->
//! lingxi-tools`). The `parity_workflow_tools` driver cross-checks the
//! locked literals.

use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use lingxi_core::host::worktree::{WorktreeChangeSummary, WorktreeError, WorktreeHandle};
use once_cell::sync::Lazy;
use permission::result::{PermissionMetadata, PermissionPrompt};
use permission::{PermissionDecisionReason, PermissionResult};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{
    ENTER_WORKTREE_COMPLETED, ENTER_WORKTREE_FAILED, ENTER_WORKTREE_STARTED,
    EXIT_WORKTREE_COMPLETED, EXIT_WORKTREE_FAILED, EXIT_WORKTREE_STARTED, WORKTREE_CREATED,
    WORKTREE_ENTERED_EXISTING, WORKTREE_KEPT, WORKTREE_REMOVED,
};
use unicode_normalization::UnicodeNormalization;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext,
};
use tool_api::BuiltinToolContext;

/// Byte-locked git branch prefix for disposable worktrees (spec §7 line 488,
/// M2-01 lock). The full branch is `worktree-<flatten(slug)>`.
pub const WORKTREE_BRANCH_PREFIX: &str = "worktree-";
/// Byte-locked path segment under the repo root (spec §7 line 489, M2-01 lock).
pub const WORKTREE_PATH_SEGMENT: &str = ".lingxi/worktrees";
/// Byte-locked flatten character: every `/` in the slug becomes `+` (M2-01 lock).
pub const WORKTREE_FLATTEN_CHAR: char = '+';
/// Maximum allowed total length of a worktree slug (M2-01 lock).
pub const MAX_WORKTREE_SLUG_LENGTH: usize = 64;

/// Canonical tool name in the registry for `EnterWorktreeTool`.
pub const ENTER_TOOL_NAME: &str = "EnterWorktree";
/// Canonical tool name in the registry for `ExitWorktreeTool`.
pub const EXIT_TOOL_NAME: &str = "ExitWorktree";

/// Validate a caller-supplied worktree slug (mirrors M2-01 `validate_worktree_slug`).
///
/// Rules (mirrors claude-code's `src/utils/worktree.ts` + M2-01 platform-posix):
/// - Total length 1..=64 chars.
/// - Each `/`-separated segment matches `^[a-zA-Z0-9._-]+$`.
/// - No empty segments (rejects `"/foo"`, `"foo/"`, `"a//b"`, `""`).
///
/// # Errors
/// Returns [`WorktreeError::InvalidSlug`] with a human-readable detail.
pub fn validate_worktree_slug(slug: &str) -> Result<(), WorktreeError> {
    // 1:1 with the binary `_Tt`: a length cap (64 = `oac`), then per-`/`-segment
    // checks — reject the `.`/`..` path segments, reject the reserved `.git`
    // directory name (case-insensitive, trailing dots stripped), and require the
    // allowed set `ytf=/^[a-zA-Z0-9._-]+$/` (which also rejects empty segments).
    // Error messages are byte-exact: the binary wraps the slug/segment in LITERAL
    // double-quotes (`"${e}"`/`"${t}"`), so we format `"{slug}"` — NOT `{slug:?}`.
    if slug.len() > MAX_WORKTREE_SLUG_LENGTH {
        return Err(WorktreeError::InvalidSlug(format!(
            "Invalid worktree name: must be {MAX_WORKTREE_SLUG_LENGTH} characters or fewer (got {})",
            slug.len()
        )));
    }
    for segment in slug.split('/') {
        if segment == "." || segment == ".." {
            return Err(WorktreeError::InvalidSlug(format!(
                "Invalid worktree name \"{slug}\": must not contain \".\" or \"..\" path segments"
            )));
        }
        if segment.to_lowercase().trim_end_matches('.') == ".git" {
            return Err(WorktreeError::InvalidSlug(format!(
                "Invalid worktree name \"{slug}\": \"{segment}\" is a reserved git directory name"
            )));
        }
        if segment.is_empty()
            || !segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-')
        {
            return Err(WorktreeError::InvalidSlug(format!(
                "Invalid worktree name \"{slug}\": each \"/\"-separated segment must be non-empty and contain only letters, digits, dots, underscores, and dashes"
            )));
        }
    }
    Ok(())
}

/// Flatten a `/`-separated slug into a single filesystem-friendly name.
/// Replaces every `/` with `+`. Mirrors M2-01 `flatten_slug`.
#[must_use]
pub fn flatten_slug(slug: &str) -> String {
    slug.replace('/', "+")
}

/// Shared 206 success-message template for both the CREATE and ENTER
/// (path-entry) cases of `EnterWorktree` on the MAIN session. Byte-exact to
/// the binary's `SCd.call` non-pinned-agent branch:
/// `` `${o} worktree at ${r.worktreePath}${n}. The session is now working in
/// the worktree. Use ExitWorktree to leave mid-session, or exit the session
/// to be prompted.` `` where `o` is `"Entered"` (path given) or `"Created"`
/// (create). The port has no pinned-agent worktree-entry concept — its
/// `session_cwd.swap` always moves the whole session — so BOTH call sites
/// share this one template; there is no separate "this agent's working
/// directory" message.
#[must_use]
fn worktree_session_message(verb: &str, display_path: &str, suffix: &str) -> String {
    format!(
        "{verb} worktree at {display_path}{suffix}. The session is now working in the worktree. Use ExitWorktree to leave mid-session, or exit the session to be prompted."
    )
}

/// The `{branch}` suffix in the 206 success messages: `` ` on branch {branch}` ``
/// when a real branch exists, else `""`. Byte-exact to `SCd.call`'s
/// `` r.worktreeBranch?` on branch ${r.worktreeBranch}`:"" ``.
///
/// `enter_existing` returns `branch_name == "HEAD"` for a detached-HEAD
/// worktree (posix `git rev-parse --abbrev-ref HEAD` prints the literal
/// string `"HEAD"` when detached) — treated as NO branch, matching claude's
/// falsy-`worktreeBranch` check (`"HEAD"` is truthy in JS, but claude-code
/// never surfaces a bare `"HEAD"` as `worktreeBranch`; the port's contract
/// carves this out explicitly so a detached worktree never renders
/// `" on branch HEAD"`).
#[must_use]
fn branch_suffix(branch_name: &str) -> String {
    if branch_name.is_empty() || branch_name == "HEAD" {
        String::new()
    } else {
        format!(" on branch {branch_name}")
    }
}

/// Detect whether `cwd` is already inside a `.lingxi/worktrees/<slug>`
/// Generate a random worktree name when the caller supplies neither `name`
/// nor `path`. claude-code's `S1e()` picks a word-pair name checked against a
/// collision set; the port has no such word list wired, so this derives a
/// short, slug-legal, effectively-unique name from a fresh ULID/UUID. Not
/// byte-matched to the oracle (the upstream name is itself random), but
/// satisfies the same contract: a fresh, valid, human-scannable slug.
///
/// `pub` (not `pub(crate)`) so the `--worktree`/`--tmux` boot-launch path
/// (`runtime/src/desktop/mod.rs::build`, worktree-tmux-launch plan Task 3)
/// can mint the SAME bare-`-w` random-name behavior as this tool's `name`-less
/// `EnterWorktree` call, instead of duplicating the derivation.
#[must_use]
pub fn gen_random_slug() -> String {
    let id = tool_api::util::ids::ulid_or_uuid();
    let lower: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .take(8)
        .collect();
    format!("session-{lower}")
}

/// 206 `EnterWorktree` input: `{name?, path?}`, no required field (byte-exact
/// `_wy` shape — `E.strictObject({name: ..., path: ...})`, both optional and
/// mutually exclusive). `path` present ⇒ switch into an EXISTING worktree;
/// `path` absent ⇒ create a new one, using `name` as the slug (or a random
/// name when `name` is absent too).
#[derive(Debug, Deserialize, Default)]
struct EnterInput {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

/// 206 guard message (`validateInput`'s `ky()&&!e.path` branch, `errorCode:2`):
/// fires only when CREATING (no `path`) while the session is already inside a
/// worktree. Byte-exact, extracted from the 2.1.206 binary.
const ALREADY_IN_WORKTREE_MESSAGE: &str = "Already in a worktree session. Pass `path` to switch into another existing worktree, or use ExitWorktree to leave this one before creating a new worktree.";

/// EnterWorktree "create from an isolated subagent" guard — byte-faithful to
/// 206's `validateInput` `Tze() && !e.path` refusal. The port swaps the SHARED
/// `session_cwd`, so a subagent with a cwd override (`ctx.cwd.is_some()`)
/// creating a worktree would mutate the parent session's directory. Static head
/// is byte-exact; the tail is 206's brand-free ELSE branch (the port lacks the
/// `Yf` managed-root detection to pick the `.claude/worktrees` first branch, and
/// the else tail is itself a complete 206 string). Em-dash is U+2014. Entering
/// an EXISTING worktree via `path` is still allowed (206 `Tze() && e.path`).
const ENTER_SUBAGENT_CWD_OVERRIDE_MESSAGE: &str = "EnterWorktree cannot create a worktree from a subagent with a cwd override (isolation: \"worktree\" or explicit cwd) \u{2014} it would mutate the parent session's process-wide working directory. To work in a different directory (including a worktree), spawn an Agent with `cwd` set to it.";

/// ExitWorktree "called from an isolated subagent" guard — byte-exact port of
/// 206's `validateInput` `Tze()` refusal (errorCode 5), unconditional. Em-dash
/// is U+2014.
const EXIT_SUBAGENT_CWD_OVERRIDE_MESSAGE: &str = "ExitWorktree cannot be called from a subagent with a cwd override (isolation: \"worktree\" or explicit cwd) \u{2014} it would mutate the parent session's process-wide working directory. This agent is already isolated; use Bash with `cd` for directory changes within it.";

/// ExitWorktree "not the owner" remove-refusal — byte-faithful to 206's
/// `validateInput` `action==="remove" && t.enteredExisting` branch (errorCode 4).
/// The literal `EnterWorktree({path})` `{path}` is text, not interpolated. Two
/// em-dashes are U+2014. `Claude Code` → `LingXi` (CLI-brand rebrand, matching
/// the `<env>` worktree-stash notice's "other LingXi sessions").
fn exit_not_owner_message(worktree_path: &str, original_cwd: &str) -> String {
    format!(
        "This session is not the owner of the worktree at {worktree_path} \u{2014} it either entered a pre-existing worktree via EnterWorktree({{path}}) or resumed into a checkout whose liveness lock another running LingXi session still holds \u{2014} so this tool will not remove it. Use action: \"keep\" to return to {original_cwd}. If no other session is using it, remove it yourself with `git worktree remove`; while a live session's lock is present, git will refuse and name the owner."
    )
}

/// 206 `yCd()` tool-use prompt (byte-exact, extracted via `grep -abo` /
/// latin-1 slicing from the 2.1.206 binary at `function yCd(){return\`...\`}`),
/// with two LingXi rebrands applied to the extracted text:
/// - `.claude/worktrees/` → `.lingxi/worktrees/` (2 occurrences; matches
///   [`WORKTREE_PATH_SEGMENT`]).
/// - `CLAUDE.md` → `LINGXI.md` (3 occurrences; matches the codebase-wide
///   memory-file rebrand, `branding::MEMORY_FILE` /
///   `orchestrator::prompt::memory_section`).
/// `settings.json` is kept verbatim — the port's settings filename is
/// unchanged (only relocated under `.lingxi/`; see
/// `tools/meta/src/config.rs::CONFIG_FILE_NAME`).
const ENTER_WORKTREE_PROMPT: &str = r#"Use this tool ONLY when explicitly instructed to work in a worktree — either by the user directly, or by project instructions (LINGXI.md / memory). This tool creates an isolated git worktree and switches the current session into it.

## When to Use

- The user explicitly says "worktree" (e.g., "start a worktree", "work in a worktree", "create a worktree", "use a worktree")
- LINGXI.md or memory instructions direct you to work in a worktree for the current task

## When NOT to Use

- The user asks to create a branch, switch branches, or work on a different branch — use git commands instead
- The user asks to fix a bug or work on a feature — use normal git workflow unless worktrees are explicitly requested by the user or project instructions
- Never use this tool unless "worktree" is explicitly mentioned by the user or in LINGXI.md / memory instructions

## Requirements

- Must be in a git repository, OR have WorktreeCreate/WorktreeRemove hooks configured in settings.json
- Must not already be in a worktree session when creating a new worktree (`name`); switching into another existing worktree via `path` is allowed

## Behavior

- In a git repository: creates a new git worktree inside `.lingxi/worktrees/` on a new branch. The base ref is governed by the `worktree.baseRef` setting: `fresh` (default) branches from origin/<default-branch>; `head` branches from your current local HEAD
- Outside a git repository: delegates to WorktreeCreate/WorktreeRemove hooks for VCS-agnostic isolation
- Switches the session's working directory to the new worktree
- Use ExitWorktree to leave the worktree mid-session (keep or remove). On session exit, if still in the worktree, the user will be prompted to keep or remove it

## Entering an existing worktree

Pass `path` instead of `name` to switch the session into a worktree that already exists (e.g., one you just created with `git worktree add`). On first entry from the launch directory, the path must appear in `git worktree list` for the repository that owns it — the current repository or, in a multi-repo workspace, a repository nested inside it; paths registered by neither are rejected. ExitWorktree will not remove a worktree entered this way; use `action: "keep"` to return to the original directory.

Switching with `path` also works when the session is already in a worktree (the previous worktree is left on disk, untouched, and only the new one is tracked for exit-time cleanup), and from agents whose working directory was pinned at launch (subagent isolation or explicit cwd). In both cases the target must be a worktree under `.lingxi/worktrees/` of the same repository, and from a pinned agent the switch only affects this agent, not the parent session. After a further switch, previously-visited worktrees are no longer writable — re-issue EnterWorktree with `path` to return to one.

## Parameters

- `name` (optional): A name for a new worktree. If neither `name` nor `path` is provided, a random name is generated.
- `path` (optional): Path to an existing worktree to enter instead of creating one — of the current repository, or (on first entry from the launch directory) of a repository nested inside it. Mutually exclusive with `name`.
"#;

/// 206 `ExitWorktree` action: `"keep"` leaves the worktree and branch intact
/// on disk; `"remove"` deletes both. Byte-exact enum values (`Twy`'s
/// `E.enum(["keep","remove"])`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ExitAction {
    Keep,
    Remove,
}

/// 206 `ExitWorktree` input: `{action, discard_changes?}` (byte-exact `Twy`
/// shape — `E.strictObject({action: E.enum([...]).describe(...),
/// discard_changes: E.boolean().optional().describe(...)})`).
#[derive(Debug, Deserialize)]
struct ExitInput {
    action: ExitAction,
    #[serde(default)]
    discard_changes: Option<bool>,
}

/// 206 `_wy` input schema — both param descriptions extracted verbatim from
/// the 2.1.206 binary (`_wy=ye(()=>E.strictObject({name:...describe('...'),
/// path:...describe("...")}))`). `strictObject` ⇒ `additionalProperties:false`;
/// neither field is required.
static ENTER_INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Optional name for a new worktree. Each \"/\"-separated segment may contain only letters, digits, dots, underscores, and dashes; max 64 chars total. A random name is generated if not provided. Mutually exclusive with `path`."
            },
            "path": {
                "type": "string",
                "description": "Path to an existing worktree to switch into instead of creating a new one. Must appear in `git worktree list` for the current repo — or, on first entry from the launch directory, for a repo nested inside it (multi-repo workspace). Mutually exclusive with `name`."
            }
        },
        "additionalProperties": false
    })
});

/// 206 `Twy` input schema (byte-exact param descriptions, extracted from the
/// 2.1.206 binary): `strictObject` ⇒ `additionalProperties:false`; `action`
/// is required, `discard_changes` is optional.
static EXIT_INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["keep", "remove"],
                "description": "\"keep\" leaves the worktree and branch on disk; \"remove\" deletes both."
            },
            "discard_changes": {
                "type": "boolean",
                "description": "Required true when action is \"remove\" and the worktree has uncommitted files or unmerged commits. The tool will refuse and list them otherwise."
            }
        },
        "additionalProperties": false,
        "required": ["action"]
    })
});

/// 206 `ExitWorktree` no-op message (`errorCode:1`, `Twy.validateInput`'s
/// `!ky()` branch — no active `EnterWorktree` session): byte-exact, extracted
/// from the 2.1.206 binary.
const EXIT_NO_ACTIVE_SESSION_MESSAGE: &str = "No-op: there is no active EnterWorktree session to exit. This tool only operates on worktrees created by EnterWorktree in the current session — it will not touch worktrees created manually or in a previous session. No filesystem changes were made.";

/// 206 `ExitWorktree` refuse-when-can't-verify message (`RCd` returned
/// `null`): byte-exact, extracted from the 2.1.206 binary. `{worktree_path}`
/// is substituted in.
fn exit_cannot_verify_message(worktree_path: &str) -> String {
    format!(
        "Could not verify worktree state at {worktree_path}. Refusing to remove without explicit confirmation. Re-invoke with discard_changes: true to proceed — or use action: \"keep\" to preserve the worktree."
    )
}

/// 206 `ExitWorktree` refuse-when-dirty message (`errorCode:2`,
/// `action==="remove" && !discard_changes && (changedFiles>0 ||
/// commitsAhead>0)`): byte-exact template, extracted from the 2.1.206 binary.
/// `{list}` is the pluralized changed-files/ahead-commits fragments joined by
/// `" and "` (commits fragment first — matches the oracle's push order).
fn exit_has_changes_message(list: &str) -> String {
    format!(
        "Worktree has {list}. Removing will discard this work permanently. Confirm with the user, then re-invoke with discard_changes: true — or use action: \"keep\" to preserve the worktree."
    )
}

/// 206 `vCd()` tool-use prompt (byte-exact, extracted via `grep -abo` /
/// latin-1 slicing from the 2.1.206 binary at `function vCd(){return\`...\`}`).
/// No rebrand substitutions apply — the extracted text contains neither
/// `.claude/worktrees/` nor `CLAUDE.md`.
const EXIT_WORKTREE_PROMPT: &str = "Exit a worktree session created by EnterWorktree and return the session to the original working directory.\n\n## Scope\n\nThis tool ONLY operates on worktrees created by EnterWorktree in this session. It will NOT touch:\n- Worktrees you created manually with `git worktree add`\n- Worktrees from a previous session (even if created by EnterWorktree then)\n- The directory you're in if EnterWorktree was never called\n\nIf called outside an EnterWorktree session, the tool is a **no-op**: it reports that no worktree session is active and takes no action. Filesystem state is unchanged.\n\n## When to Use\n\n- The user explicitly asks to \"exit the worktree\", \"leave the worktree\", \"go back\", or otherwise end the worktree session\n- Do NOT call this proactively — only when the user asks\n\n## Parameters\n\n- `action` (required): `\"keep\"` or `\"remove\"`\n  - `\"keep\"` — leave the worktree directory and branch intact on disk. Use this if the user wants to come back to the work later, or if there are changes to preserve.\n  - `\"remove\"` — delete the worktree directory and its branch. Use this for a clean exit when the work is done or abandoned.\n- `discard_changes` (optional, default false): only meaningful with `action: \"remove\"`. If the worktree has uncommitted files or commits not on the original branch, the tool will REFUSE to remove it unless this is set to `true`. If the tool returns an error listing changes, confirm with the user before re-invoking with `discard_changes: true`.\n\n## Behavior\n\n- Restores the session's working directory to where it was before EnterWorktree\n- Clears CWD-dependent caches (system prompt sections, memory files, plans directory) so the session state reflects the original directory\n- If a tmux session was attached to the worktree: killed on `remove`, left running on `keep` (its name is returned so the user can reattach)\n- Once exited, EnterWorktree can be called again to create a fresh worktree\n";

fn fresh_invocation_id() -> String {
    tool_api::util::ids::ulid_or_uuid()
}

fn verified(s: impl Into<String>) -> AnalyticsValue {
    AnalyticsValue::String(Verified::assert_safe(s.into()).into_inner())
}

fn pii_tagged(s: impl Into<String>) -> AnalyticsValue {
    AnalyticsValue::String(PiiTagged::assert_pii_tagged_column(s.into()).into_inner())
}

/// Display-sanitizer control/format/spacing class — byte-faithful port of the
/// 2.1.207 binary's `wIy` regex used by `Dxs`:
/// `/[\p{Cc}\p{Cf}\p{Cn}\p{Co}\p{Zl}\p{Zp}\p{Default_Ignorable_Code_Point}⠀]|(?! )\p{Zs}/gu`.
/// The JS negative-lookahead `(?! )\p{Zs}` (a space-separator that is not
/// U+0020) is expressed with a class intersection `[\p{Zs}&&[^\x{20}]]`, which
/// the `regex` crate supports directly (it has no lookaround). Every match is
/// replaced with U+FFFD so a model-supplied worktree path can never smuggle
/// control chars, bidi overrides, zero-width joiners, or exotic spaces into the
/// confirmation prompt (anti-spoofing). Matches CC exactly because `regex`
/// bundles the same Unicode general-category + `Default_Ignorable_Code_Point`
/// tables.
static DISPLAY_CTRL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"[\p{Cc}\p{Cf}\p{Cn}\p{Co}\p{Zl}\p{Zp}\p{Default_Ignorable_Code_Point}\x{2800}[\p{Zs}&&[^\x{20}]]]",
    )
    .expect("worktree display-sanitizer control regex must compile")
});

/// Display-sanitizer fancy-quote class — byte-faithful port of the 2.1.207
/// binary's `AIy` regex applied by `n(l)=Dxs(l).replace(AIy,"�")`:
/// `/["“”‟″‶＂〝〞〟ʺˮ˝״]/g`.
/// Replaces the ASCII double-quote and a set of look-alike quote glyphs with
/// U+FFFD so a path cannot forge the `"…"` delimiters wrapping it in the prompt.
static DISPLAY_QUOTE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new("[\"\u{201C}\u{201D}\u{201F}\u{2033}\u{2036}\u{FF02}\u{301D}\u{301E}\u{301F}\u{02BA}\u{02EE}\u{02DD}\u{05F4}]")
        .expect("worktree display-sanitizer quote regex must compile")
});

/// Sanitize a path for display in the `EnterWorktree` confirmation prompt —
/// 2.1.207 `n(l) = Dxs(l).replace(AIy, "�")`: first the control/format
/// class ([`DISPLAY_CTRL_RE`]), then the fancy-quote class ([`DISPLAY_QUOTE_RE`]),
/// each collapsed to U+FFFD. A clean ASCII/normal path passes through unchanged.
#[must_use]
fn sanitize_display(s: &str) -> String {
    let step1 = DISPLAY_CTRL_RE.replace_all(s, "\u{FFFD}");
    DISPLAY_QUOTE_RE
        .replace_all(&step1, "\u{FFFD}")
        .into_owned()
}

/// Lexically normalize a path (collapse `.`/`..`/duplicate separators) WITHOUT
/// touching the filesystem — the normalization half of Node's `path.resolve`.
/// A leading `..` at (or above) the root is dropped, matching Node.
#[must_use]
fn normalize_lexically(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(comp),
            },
            other => out.push(other),
        }
    }
    let mut pb = PathBuf::new();
    for comp in out {
        pb.push(comp.as_os_str());
    }
    pb
}

/// Byte-faithful port of the 2.1.207 binary's `jYr.resolve(Ct(), e.path)` — the
/// LEXICAL resolve of a model-supplied worktree path against the session cwd
/// (`r` in the checkPermissions closure). An absolute `path` is normalized
/// as-is; a relative one is joined onto `cwd` first. Never resolves symlinks
/// (that is the separate realpath step — see [`resolve_enter_target`]).
#[must_use]
fn lexical_resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    normalize_lexically(&joined)
}

/// Walk up from `start` to the nearest ancestor holding a `.git` entry (file or
/// directory) — the port's stand-in for the binary's `em(cwd)` git top-level
/// (`git rev-parse --show-toplevel`). A `.git` FILE marks a linked worktree's
/// root; a `.git` DIRECTORY marks the main checkout. Returns `None` outside any
/// git repository (then no path can be "managed" and the confirmation always
/// fires).
#[must_use]
fn find_repo_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Result of resolving a worktree-entry `path` against the filesystem — the
/// port's analog of the 2.1.207 binary's `N6i(e.path)` return `{targetReal,
/// managed}`. `target_real` is the realpath of the (lexically resolved) target
/// when it exists on disk, else `None` (the binary's `t === null` case, where
/// the prompt omits the "resolves to" clause). `managed` is `true` only when
/// that realpath lives strictly under the repo's `.lingxi/worktrees/` managed
/// root — the silent-allow case.
struct EnterTargetInfo {
    target_real: Option<PathBuf>,
    managed: bool,
}

/// Resolve a lexically-resolved worktree target to its realpath + managed flag
/// (2.1.207 `N6i`). `managed` requires the realpath to sit strictly beneath the
/// realpath of `<repo_root>/.lingxi/worktrees/` (the binary's
/// `phe(o).startsWith(phe(l + sep))` under a symlink-free managed root); a
/// target that IS the managed root itself, or lives outside it, is not managed.
fn resolve_enter_target(cwd: &Path, resolved: &Path) -> EnterTargetInfo {
    let target_real = std::fs::canonicalize(resolved).ok();
    let managed = match (target_real.as_ref(), find_repo_root(cwd)) {
        (Some(real), Some(repo_root)) => {
            std::fs::canonicalize(repo_root.join(WORKTREE_PATH_SEGMENT))
                .ok()
                .is_some_and(|root_real| real.starts_with(&root_real) && *real != root_real)
        }
        _ => false,
    };
    EnterTargetInfo {
        target_real,
        managed,
    }
}

/// `EnterWorktreeTool` — creates a disposable worktree via the M2-01
/// `WorktreeManager` trait. Branch + path layout are byte-locked.
pub struct EnterWorktreeTool {
    ctx: BuiltinToolContext,
    /// Optional sink that persists the recorded [`tool_api::WorktreeSession`] to
    /// the transcript as a `worktree-state` entry (parity 2.1.212), so a later
    /// `--continue`/`--resume` rehydrates the active worktree. `None` (mobile /
    /// offline factory / tests) skips the write — byte-identical to the
    /// pre-persist behavior.
    state_persister: Option<std::sync::Arc<dyn tool_api::WorktreeStatePersister>>,
}

impl EnterWorktreeTool {
    /// Construct a new tool wired to the shared builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self {
            ctx,
            state_persister: None,
        }
    }

    /// Inject the transcript persister (parity 2.1.212 `saveWorktreeState`).
    #[must_use]
    pub fn with_state_persister(
        mut self,
        persister: std::sync::Arc<dyn tool_api::WorktreeStatePersister>,
    ) -> Self {
        self.state_persister = Some(persister);
        self
    }

    /// Persist the CURRENT `worktree_session` cell value to the transcript (a
    /// `worktree-state` entry). Best-effort: no-op when no persister is wired.
    async fn persist_worktree_state(&self) {
        if let Some(persister) = &self.state_persister {
            let snapshot = self.ctx.worktree_session.lock().unwrap().clone();
            persister.persist_worktree_state(snapshot.as_ref()).await;
        }
    }

    async fn emit_started(&self, invocation_id: &str, slug: &str) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("invocation_id".into(), verified(invocation_id));
        md.insert("tool_name".into(), verified(ENTER_TOOL_NAME));
        md.insert("_PROTO_slug".into(), pii_tagged(slug));
        self.ctx.bus.log_event(ENTER_WORKTREE_STARTED, md).await;
    }

    async fn emit_completed(&self, invocation_id: &str, branch_name: &str, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("invocation_id".into(), verified(invocation_id));
        md.insert("tool_name".into(), verified(ENTER_TOOL_NAME));
        md.insert("_PROTO_branch_name".into(), pii_tagged(branch_name));
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(ENTER_WORKTREE_COMPLETED, md).await;
    }

    async fn emit_failed(&self, invocation_id: &str, error_kind: &str, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("invocation_id".into(), verified(invocation_id));
        md.insert("tool_name".into(), verified(ENTER_TOOL_NAME));
        md.insert("error_kind".into(), verified(error_kind));
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(ENTER_WORKTREE_FAILED, md).await;
    }

    /// Fire the 206 byte-exact single success event (`tengu_worktree_created`
    /// / `tengu_worktree_entered_existing`) IN ADDITION to the port's own
    /// started/completed/failed lifecycle triad above. Payload mirrors 206's
    /// `N(...)` call: `{mid_session:true}` for create (@222207339), and
    /// `{mid_session:true, cwd_override:true}` for enter-existing (@222206244).
    async fn emit_worktree_event(&self, event_name: &str) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("mid_session".into(), AnalyticsValue::Bool(true));
        if event_name == WORKTREE_ENTERED_EXISTING {
            md.insert("cwd_override".into(), AnalyticsValue::Bool(true));
        }
        self.ctx.bus.log_event(event_name, md).await;
    }

    /// Record the [`tool_api::WorktreeSession`] substrate BEFORE swapping
    /// `session_cwd` into `handle.path` — captures `ctx.cwd()` as it stands
    /// PRE-swap (worktree parity plan, Task 8's substrate prerequisite). This
    /// is what lets `ExitWorktreeTool` later restore the original directory
    /// and reconstruct the handle it needs to keep/remove. Overwrites any
    /// prior record: switching into a further worktree (via `path`) while
    /// already inside one tracks only the newest for exit-time cleanup —
    /// matching this tool's own prompt ("the previous worktree is left on
    /// disk, untouched, and only the new one is tracked for exit-time
    /// cleanup").
    fn record_worktree_session(&self, handle: &WorktreeHandle, entered_existing: bool) {
        let original_cwd = self.ctx.cwd();
        *self.ctx.worktree_session.lock().unwrap() = Some(tool_api::WorktreeSession {
            original_cwd,
            worktree_path: handle.path.clone(),
            branch_name: handle.branch_name.clone(),
            base_commit: handle.base_commit.clone(),
            // `true` for the `path` (enter-existing) branch, `false` for create —
            // gates `ExitWorktree`'s errorCode:4 "not the owner" remove guard.
            entered_existing,
            // The interactive `EnterWorktreeTool` never attaches tmux (206's
            // tool doesn't either); tmux sessions come only from the boot-time
            // `--worktree --tmux` launch path (`engine-desktop`'s
            // `apply_worktree_launch`), which populates `tmux_session_name`
            // directly on the shared cell.
            tmux_session_name: None,
        });
    }

    /// `path` present: switch the session into an ALREADY-EXISTING worktree
    /// (`SCd.call`'s `e.path` branch, non-pinned-agent case — the port has no
    /// pinned-agent worktree-entry concept). Never creates anything. Uses the
    /// SAME success-message template as `call_create`
    /// ([`worktree_session_message`]), just with the `"Entered"` verb.
    async fn call_enter_existing(
        &self,
        invocation_id: &str,
        started_at: Instant,
        path: String,
    ) -> Result<ToolCallResult, ToolError> {
        self.emit_started(invocation_id, &path).await;
        let result = self.ctx.worktree.enter_existing(Path::new(&path)).await;
        let duration_ms = started_at.elapsed().as_millis() as u64;

        match result {
            Ok(handle) => {
                // Capture the WorktreeSession substrate (Task 8) BEFORE the
                // swap below, while `ctx.cwd()` still reads the PRE-swap
                // (original) directory — this is what `ExitWorktree` later
                // restores. `entered_existing: true` — this session entered a
                // pre-existing worktree via `path`, so it is NOT its owner.
                self.record_worktree_session(&handle, true);
                // (parity 2.1.212) Persist the just-recorded session as a
                // `worktree-state` transcript entry so `--continue`/`--resume`
                // rehydrates it and `ExitWorktree` operates instead of no-oping.
                self.persist_worktree_state().await;
                // Switch the session into the worktree — every FS tool reads
                // through `ctx.cwd()`/`ctx.trusted_dirs()` (Task 2), so this
                // single swap is what makes subsequent tool calls observe the
                // worktree; it also fires the orchestrator's registered
                // cache-invalidation callback (Task 5).
                self.ctx
                    .session_cwd
                    .swap(handle.path.clone(), vec![handle.path.clone()]);
                self.emit_completed(invocation_id, &handle.branch_name, duration_ms)
                    .await;
                self.emit_worktree_event(WORKTREE_ENTERED_EXISTING).await;
                let suffix = branch_suffix(&handle.branch_name);
                let display_path = handle.path.to_string_lossy().into_owned();
                let message = worktree_session_message("Entered", &display_path, &suffix);
                Ok(ToolCallResult {
                    data: json!({
                        "path": display_path,
                        "branch_name": handle.branch_name,
                    }),
                    model_content: Some(message),
                    new_messages: Vec::new(),
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                })
            }
            Err(err) => self.map_error(invocation_id, duration_ms, err).await,
        }
    }

    /// `path` absent: create a NEW worktree, using `name` as the slug (or a
    /// generated random name when `name` is also absent). Refuses when the
    /// session is already inside a worktree (206 `validateInput`'s
    /// `ky()&&!e.path` guard, `errorCode:2`).
    async fn call_create(
        &self,
        invocation_id: &str,
        started_at: Instant,
        name: Option<String>,
    ) -> Result<ToolCallResult, ToolError> {
        // Faithful port of claude's `ky()` "already in a worktree session" flag:
        // the shared `WorktreeSession` record is `Some` exactly while a session is
        // active (written on enter, cleared on exit), regardless of where the
        // worktree lives on disk — so this also blocks a create after entering a
        // worktree via a `path` OUTSIDE `.lingxi/worktrees/` (the case the old
        // path-substring heuristic false-negatived).
        if self.ctx.worktree_session.lock().unwrap().is_some() {
            let duration_ms = started_at.elapsed().as_millis() as u64;
            self.emit_failed(invocation_id, "already_in_worktree", duration_ms)
                .await;
            return Err(ToolError::InvalidInput(
                ALREADY_IN_WORKTREE_MESSAGE.to_string(),
            ));
        }

        let slug = name.unwrap_or_else(gen_random_slug);
        self.emit_started(invocation_id, &slug).await;

        // Pre-flight slug validation so we surface the locked literal even if
        // the manager would otherwise return a different error path.
        if let Err(err) = validate_worktree_slug(&slug) {
            let detail = match err {
                WorktreeError::InvalidSlug(d) => d,
                other => format!("{other:?}"),
            };
            self.emit_failed(
                invocation_id,
                "invalid_slug",
                started_at.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(format!(
                "EnterWorktree: invalid slug: {detail}"
            )));
        }

        let result = self.ctx.worktree.create_worktree(&slug, None, &[]).await;
        let duration_ms = started_at.elapsed().as_millis() as u64;

        match result {
            Ok(handle) => {
                debug_assert_eq!(
                    handle.branch_name,
                    format!("{WORKTREE_BRANCH_PREFIX}{}", flatten_slug(&slug)),
                    "M2-01 contract broken: branch_name must be worktree-<flatten(slug)>"
                );
                // Capture the WorktreeSession substrate (Task 8) BEFORE the
                // swap below — see `record_worktree_session`'s doc.
                // `entered_existing: false` — this session CREATED the worktree,
                // so `ExitWorktree` may remove it.
                self.record_worktree_session(&handle, false);
                // (parity 2.1.212) Persist for resume restoration — see the
                // matching call in `call_enter_existing`.
                self.persist_worktree_state().await;
                self.ctx
                    .session_cwd
                    .swap(handle.path.clone(), vec![handle.path.clone()]);
                self.emit_completed(invocation_id, &handle.branch_name, duration_ms)
                    .await;
                self.emit_worktree_event(WORKTREE_CREATED).await;
                let suffix = branch_suffix(&handle.branch_name);
                let display_path = handle.path.to_string_lossy().into_owned();
                let message = worktree_session_message("Created", &display_path, &suffix);
                Ok(ToolCallResult {
                    data: json!({
                        "path": display_path,
                        "branch_name": handle.branch_name,
                    }),
                    model_content: Some(message),
                    new_messages: Vec::new(),
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                })
            }
            Err(err) => self.map_error(invocation_id, duration_ms, err).await,
        }
    }

    async fn map_error(
        &self,
        invocation_id: &str,
        duration_ms: u64,
        err: WorktreeError,
    ) -> Result<ToolCallResult, ToolError> {
        match err {
            WorktreeError::InvalidSlug(detail) => {
                self.emit_failed(invocation_id, "invalid_slug", duration_ms)
                    .await;
                Err(ToolError::InvalidInput(format!(
                    "EnterWorktree: invalid slug: {detail}"
                )))
            }
            WorktreeError::Unsupported => {
                self.emit_failed(invocation_id, "unsupported", duration_ms)
                    .await;
                Err(ToolError::Internal(
                    "EnterWorktree: worktrees are not supported on this platform".into(),
                ))
            }
            WorktreeError::Git(msg) => {
                self.emit_failed(invocation_id, "git", duration_ms).await;
                Err(ToolError::Internal(format!(
                    "EnterWorktree: git error: {msg}"
                )))
            }
            WorktreeError::Io(msg) => {
                self.emit_failed(invocation_id, "io", duration_ms).await;
                Err(ToolError::Io(format!("EnterWorktree: io error: {msg}")))
            }
            // The pre-flight symlink guard already carries the full
            // byte-faithful "Cannot create worktree: … is a symlink. …" text
            // (CC surfaces its `ev(...)` directly, unprefixed), so pass it
            // through verbatim rather than reframing it as a git/io error.
            WorktreeError::SymlinkRejected(msg) => {
                self.emit_failed(invocation_id, "symlink_rejected", duration_ms)
                    .await;
                Err(ToolError::Internal(msg))
            }
            // Creation never snapshots; kept for exhaustiveness.
            WorktreeError::SnapshotRefused(msg) => {
                self.emit_failed(invocation_id, "snapshot_refused", duration_ms)
                    .await;
                Err(ToolError::Internal(msg))
            }
        }
    }
}

#[async_trait]
impl Tool for EnterWorktreeTool {
    fn name(&self) -> &str {
        ENTER_TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("create an isolated git worktree and switch into it")
    }
    /// 2.1.206 `userFacingName(e){return e?.path?"Entering worktree":"Creating worktree"}`.
    /// `e.path` is a JS truthiness check, so an empty-string `path` counts as
    /// absent here too — matching the `call` dispatch's empty-string handling.
    fn user_facing_name_for_input(&self, input: &Value) -> Option<String> {
        let has_path = input
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|p| !p.is_empty());
        Some(
            if has_path {
                "Entering worktree"
            } else {
                "Creating worktree"
            }
            .to_string(),
        )
    }
    fn input_schema(&self) -> &Value {
        &ENTER_INPUT_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    /// 2.1.206 `shouldDefer:!0`.
    fn should_defer(&self) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    /// 2.1.206/207 `checkPermissions` (byte-extracted from the binary tool
    /// object). Three outcomes:
    /// - No `path` (create) ⇒ `allow` — creating a worktree under the managed
    ///   `.lingxi/worktrees/` root never relocates the permission root to an
    ///   untrusted place.
    /// - `path` resolving strictly UNDER the repo's `.lingxi/worktrees/`
    ///   (`t?.managed`) ⇒ `allow`, with the input's `path` rewritten to the
    ///   realpath (`updatedInput:{...e, path:t.targetReal}`).
    /// - Any other `path` ⇒ `ask` with the byte-exact relocation confirmation
    ///   (`.claude/worktrees/`→`.lingxi/worktrees/` and `CLAUDE.md`→`LINGXI.md`
    ///   per this module's rebrand rule), tagged
    ///   [`PermissionDecisionReason::SafetyCheck`] with
    ///   `classifier_approvable: false` (206 `classifierApprovable:!1`) so no
    ///   classifier or allow-rule can auto-approve a model-supplied worktree
    ///   outside the managed root. The displayed path is passed through
    ///   [`sanitize_display`] and gains a ` (resolves to "…")` clause when the
    ///   realpath differs from the lexical resolve and a
    ///   ` (path sanitized for display)` clause when sanitization changed it —
    ///   byte-faithful to the binary's `${o}${a}${s}` assembly.
    async fn check_permissions(&self, input: &Value, ctx: &ToolUseContext) -> PermissionResult {
        // 206 `e.path` is a JS truthiness check — an empty string is falsy and
        // counts as "no path" (create). `!e.path` ⇒ allow (create branch).
        let Some(path) = input
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        else {
            return PermissionResult::Allow {
                reason: PermissionDecisionReason::Other {
                    reason: "EnterWorktree create stays under the managed worktrees root".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: PermissionMetadata::default(),
            };
        };

        // `Ct()` — a pinned/isolated subagent resolves against its own cwd
        // override, the main session against the shared session cwd.
        let cwd = ctx.cwd.clone().unwrap_or_else(|| self.ctx.cwd());
        let resolved = lexical_resolve(&cwd, path); // 206 `r = jYr.resolve(Ct(), e.path)`
        let target = resolve_enter_target(&cwd, &resolved); // 206 `t = N6i(e.path)`

        // 206 `if(t?.managed)` ⇒ allow, rewriting `path` to the realpath.
        if target.managed {
            let real = target
                .target_real
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned());
            let mut updated = input.clone();
            if let (Some(obj), Some(real)) = (updated.as_object_mut(), real) {
                obj.insert("path".into(), Value::String(real));
            }
            return PermissionResult::Allow {
                reason: PermissionDecisionReason::Other {
                    reason: "EnterWorktree target is under the managed worktrees root".into(),
                },
                updated_input: Some(updated),
                update_destination: None,
                metadata: PermissionMetadata::default(),
            };
        }

        // 206 ask branch: assemble `${o}${a}${s}` from the sanitized lexical
        // resolve (`o`), an optional "resolves to" clause (`a`), and an optional
        // "path sanitized for display" clause (`s`).
        let resolved_str = resolved.to_string_lossy().into_owned(); // 206 `r`
        let o = sanitize_display(&resolved_str); // 206 `o = n(r)`
        let target_real_str = target
            .target_real
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let i = target_real_str.as_deref().map(sanitize_display); // 206 `i = t ? n(t.targetReal) : null`

        // 206 `s = o!==r || (t && i!==t.targetReal) ? " (path sanitized for display)" : ""`.
        let sanitized_changed = o != resolved_str
            || matches!((target_real_str.as_deref(), i.as_deref()), (Some(tr), Some(is)) if is != tr);
        let s = if sanitized_changed {
            " (path sanitized for display)"
        } else {
            ""
        };
        // 206 `a = i!==null && NFC(i)!==NFC(o) ? ` (resolves to "${i}")` : ""`.
        let a = match &i {
            Some(is) if is.nfc().ne(o.nfc()) => format!(" (resolves to \"{is}\")"),
            _ => String::new(),
        };

        let message = format!(
            "Enter the worktree at \"{o}\"{a}{s}? This moves the session's working directory and write access there, and loads project configuration (LINGXI.md, settings) from that location."
        );
        let reason = format!(
            "permission-root relocation to \"{o}\"{a}{s} \u{2014} a model-supplied worktree outside .lingxi/worktrees/"
        );
        PermissionResult::Ask {
            reason: PermissionDecisionReason::SafetyCheck {
                reason,
                classifier_approvable: false,
                circuit_breaker: None,
            },
            prompt: PermissionPrompt {
                title: ENTER_TOOL_NAME.into(),
                message,
                options: vec![],
            },
            pending_classifier_check: None,
            metadata: PermissionMetadata::default(),
        }
    }

    /// 2.1.206 `async description(){return"Creates an isolated worktree (via
    /// git or configured hooks) and switches the session into it"}` (byte-exact).
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Creates an isolated worktree (via git or configured hooks) and switches the session into it".into()
    }
    /// 2.1.206 `yCd()` tool-use prompt, extracted verbatim from the binary and
    /// rebranded: `.claude/worktrees/` → `.lingxi/worktrees/` (path layout,
    /// [`WORKTREE_PATH_SEGMENT`]), `CLAUDE.md` → `LINGXI.md` (memory-file
    /// convention, `branding::MEMORY_FILE`); `settings.json` is kept verbatim
    /// (the port's settings filename is unchanged, only relocated under
    /// `.lingxi/` — see `tools/meta/src/config.rs::CONFIG_FILE_NAME`).
    async fn prompt(&self, _: &PromptOptions) -> String {
        ENTER_WORKTREE_PROMPT.into()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let invocation_id = fresh_invocation_id();
        let started_at = Instant::now();

        let parsed: EnterInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                self.emit_failed(
                    &invocation_id,
                    "invalid_input",
                    started_at.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "EnterWorktree: invalid input: {e}"
                )));
            }
        };

        // 206 `e.path` is a JS truthiness check — an empty string is falsy,
        // so `path: ""` must be treated as ABSENT (create), not as an enter
        // target. `Option::filter` drops the `Some("")` case back to `None`.
        if let Some(path) = parsed.path.filter(|p| !p.is_empty()) {
            self.call_enter_existing(&invocation_id, started_at, path)
                .await
        } else {
            // 206 `validateInput`: `Tze() && !e.path` — a subagent isolated with a
            // cwd override (`ctx.cwd.is_some()`) may NOT CREATE a worktree, since
            // the port's `session_cwd.swap` mutates the SHARED parent cwd. (Entering
            // an existing worktree via `path` above is still allowed, per 206.)
            if ctx.cwd.is_some() {
                let duration_ms = started_at.elapsed().as_millis() as u64;
                self.emit_failed(&invocation_id, "subagent_cwd_override", duration_ms)
                    .await;
                return Err(ToolError::InvalidInput(
                    ENTER_SUBAGENT_CWD_OVERRIDE_MESSAGE.to_string(),
                ));
            }
            self.call_create(&invocation_id, started_at, parsed.name)
                .await
        }
    }
}

/// Audit reason stamped on the `tmux kill-session` command issued by
/// [`kill_worktree_tmux_session`] when `ExitWorktree` removes a worktree that
/// has an attached tmux session (`session.tmux_session_name`). Mirrors
/// `platforms/posix/src/worktree_tmux.rs`'s `WORKTREE_TMUX_AUDIT_REASON` for
/// the create side; kept LOCAL to this crate (rather than reusing that
/// module) to avoid a `tool-worktree -> platform-posix -> lingxi-lsp ->
/// tool-worktree` dependency cycle — see this file's top-of-module doc on
/// why the slug helpers are reimplemented locally for the same reason.
/// `ExitWorktreeTool` only needs the `ProcessRunner`/`Sandbox` seams already
/// on `BuiltinToolContext` (`ctx.process`/`ctx.sandbox`), so a tiny local
/// argv-builder + runner is the least-coupling option (worktree tmux launch
/// plan, Task 5 — the alternative of adding a `kill_tmux_session` method to
/// the `WorktreeManager` trait was rejected: that trait models GIT worktree
/// lifecycle, not tmux, and every existing method maps 1:1 to a git
/// operation).
const WORKTREE_TMUX_KILL_AUDIT_REASON: &str = "worktree_tmux_kill_session";

/// Build the argv (excluding the `tmux` program name) for killing a detached
/// worktree tmux session: `kill-session -t <session_name>`. Byte-faithful to
/// claude-code 2.1.206's `rPe(name) = { let{code}=await
/// Ur("tmux",["kill-session","-t",name]); return code===0 }` (binary
/// @216347635).
#[must_use]
fn build_worktree_tmux_kill_argv(session_name: &str) -> Vec<String> {
    vec![
        "kill-session".to_string(),
        "-t".to_string(),
        session_name.to_string(),
    ]
}

/// Run `tmux kill-session -t <session_name>` through the
/// [`lingxi_core::host::ProcessRunner`]/[`lingxi_core::host::Sandbox`] seam (mirrors
/// `platforms/posix::worktree_tmux::create_worktree_tmux_session`'s pattern
/// for the kill side — see that module for why `bypass_with_audit` is used
/// instead of the internal `SandboxedCommand::__new_sandboxed` constructor).
/// A non-zero exit maps to `Err(stderr)`; a zero exit maps to `Ok(())`
/// (mirrors 206's `rPe` returning `code===0`). The caller (`ExitWorktreeTool::call`)
/// treats a failure here as NON-FATAL — 206's `HCd.call` (`if(s)await
/// rPe(s)`) never inspects `rPe`'s return value before proceeding to remove
/// the worktree, so a tmux hiccup must never block removal.
async fn kill_worktree_tmux_session(
    process: &dyn lingxi_core::host::ProcessRunner,
    sandbox: &dyn lingxi_core::host::Sandbox,
    session_name: &str,
) -> Result<(), String> {
    let pcmd = lingxi_core::host::ProcessCommand {
        command: "tmux".to_string(),
        args: build_worktree_tmux_kill_argv(session_name),
        cwd: None,
        env: HashMap::new(),
        timeout: None,
        stdin: None,
    };
    let sandboxed = sandbox.bypass_with_audit(pcmd, WORKTREE_TMUX_KILL_AUDIT_REASON);
    let output = process.run(&sandboxed).await.map_err(|e| e.to_string())?;
    if output.exit_code != 0 {
        return Err(output.stderr);
    }
    Ok(())
}

/// `ExitWorktreeTool` — restores the original session cwd (and, optionally,
/// removes the worktree/branch) for a worktree entered via
/// `EnterWorktreeTool` (worktree 206 parity plan, Task 8).
///
/// Reads/clears the shared [`tool_api::WorktreeSession`] substrate
/// `EnterWorktreeTool` populates (see `EnterWorktreeTool::record_worktree_session`
/// above): no active session ⇒ a byte-exact no-op — the 206 "Scope" contract
/// (never touches a worktree created manually or in a previous session).
///
/// TMUX (worktree tmux launch plan, Task 5): when `session.tmux_session_name`
/// is `Some(name)` — populated by the boot-time `--worktree --tmux` launch
/// path (`engine-desktop`'s `apply_worktree_launch`); the interactive
/// `EnterWorktreeTool` always records `None` (206's tool never attaches tmux
/// either) —
/// [`Self::call`] kills the session on `remove` (via
/// [`kill_worktree_tmux_session`], non-fatally: a kill failure only logs a
/// `tracing::warn!` and does not block removal) and, on `keep`, leaves it
/// running and surfaces `name` in the result `data.tmux_session_name` plus an
/// additive reattach line in the model-facing message. The reattach wording
/// is byte-recovered from the 2.1.206 binary's `ExitWorktree` tool
/// (`HCd.call`'s keep branch): `` ` Tmux session ${s} is still running;
/// reattach with: tmux attach -t ${s}` `` (binary strings @368812-@368822,
/// near `nDo`/`ELt`'s companion interactive exit-dialog which uses a
/// differently-worded variant for the CLI's OWN session-exit UI — that
/// dialog is a separate, non-tool code path and out of scope here). 206
/// never includes `tmuxSessionName` in `data` on the `remove` path either
/// (the session is already dead by the time the tool result is built), which
/// this port matches by leaving `tmux_session_name` absent from `data` on
/// `remove` rather than surfacing it there.
///
/// MODEL-FACING MESSAGE = 206's `data.message`, NOT its TUI render (worktree
/// 206 parity plan, Task 8 correction): 206's `wCd` React component is a
/// TUI-ONLY renderer of the tool_use block; the string the MODEL actually
/// receives comes from `HCd.mapToolResultToToolResultBlockParam({message:e},t)`
/// returning `{type:"tool_result",content:e,...}` — i.e. `data.message`
/// verbatim, byte-recovered from the 2.1.206 binary (`HCd.call`'s keep/remove
/// branches + its `y9o(originalCwd,state)` cwd-restore-phrase helper). This
/// port has no separate TUI render, so [`Self::call`]'s `model_content` is
/// built to match `data.message` directly: `` `Exited worktree. Your work is
/// preserved at ${path}${branch}. ${cwdPhrase}${tmuxSuffix}` `` on `keep`;
/// `` `Exited and removed worktree at ${path}.${discardNote} ${cwdPhrase}` ``
/// on a successful `remove`; `` `Exited worktree but could not remove it —
/// kept at ${path}. ${cwdPhrase}` `` (em dash) when `remove_worktree` fails
/// (non-fatal — see [`Self::call`]'s removal step). `cwdPhrase` is 206's
/// `y9o`/`kAs` (@222211173): normal branch (byte-exact)
/// `` `Session is now back in ${cwd}.` ``; and the missing-original-cwd
/// fallback — when `session.original_cwd` no longer exists on disk,
/// [`Self::restore_cwd`] (206 `xCd`) falls back through
/// `[worktree_path, $HOME, temp_dir]` and the phrase becomes
/// `` `The original directory ${cwd} no longer exists, so the session is now
/// in ${restoredCwd}.` ``, plus
/// `` ` Consider restarting LingXi from an existing directory.` `` UNLESS the
/// fallback landed on the worktree itself (`fellBackToWorktree`).
///
/// ERRORCODE 4/5 OMITTED: the 206 oracle also refuses removal when the
/// CALLING session isn't the worktree's owner (a pinned/subagent worktree
/// entered via `EnterWorktree({path})`, or a resumed session whose liveness
/// lock another running Claude Code session still holds — errorCode:2) and
/// blocks a pinned-agent session from mutating the PARENT session's
/// process-wide cwd (errorCode:5). The port has no subagent
/// worktree-ownership or pinned-agent-cwd concept (`session_cwd.swap` always
/// moves the whole session) — these two guards are deliberately omitted, not
/// forgotten.
/// Result of [`ExitWorktreeTool::restore_cwd`] — 206 `xCd`'s
/// `{restoredCwd, originalCwdMissing, fellBackToWorktree}`. Drives the
/// `y9o`/`kAs` cwd-restore phrase in the exit message.
struct CwdRestore {
    /// The directory the session was actually restored to.
    restored_cwd: PathBuf,
    /// `true` when `original_cwd` no longer existed and a fallback was used.
    original_cwd_missing: bool,
    /// `true` when the fallback landed on the worktree path itself (206
    /// `o === n`) — suppresses the "Consider restarting" advice.
    fell_back_to_worktree: bool,
}

pub struct ExitWorktreeTool {
    ctx: BuiltinToolContext,
    /// Probe for whether a directory still exists on disk — drives 206 `xCd`'s
    /// missing-original-cwd fallback (`IS(e)` chdir + `realpath` ENOENT check).
    /// Production wires a real `std::fs` probe (see [`Self::new`]); tests inject
    /// a stub to exercise the exists / missing branches without touching the FS.
    dir_exists: fn(&std::path::Path) -> bool,
    /// Optional transcript persister (parity 2.1.212): on a completed exit the
    /// tool writes the `worktree-state` CLEAR record (`worktreeSession: null`,
    /// claude's `gne(null)`) so a subsequent `--resume` does NOT re-restore the
    /// removed/kept worktree. `None` skips the write.
    state_persister: Option<std::sync::Arc<dyn tool_api::WorktreeStatePersister>>,
}

impl ExitWorktreeTool {
    /// Construct a new tool wired to the shared builtin context. The cwd-restore
    /// directory probe defaults to a real `std::fs::metadata` existence check.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self {
            ctx,
            dir_exists: |p| std::fs::metadata(p).is_ok(),
            state_persister: None,
        }
    }

    /// Inject the transcript persister (parity 2.1.212 `saveWorktreeState`).
    #[must_use]
    pub fn with_state_persister(
        mut self,
        persister: std::sync::Arc<dyn tool_api::WorktreeStatePersister>,
    ) -> Self {
        self.state_persister = Some(persister);
        self
    }

    /// Test constructor: the cwd-restore probe reports every directory as still
    /// existing, so `original_cwd` is always taken (the normal
    /// `Session is now back in {cwd}.` branch). Used by the message goldens,
    /// which populate synthetic (non-on-disk) `original_cwd` paths.
    #[cfg(test)]
    fn new_assuming_cwd_exists(ctx: BuiltinToolContext) -> Self {
        Self {
            ctx,
            dir_exists: |_| true,
            state_persister: None,
        }
    }

    /// Test constructor with an explicit directory-existence stub, for
    /// exercising 206 `xCd`'s missing-original-cwd fallback branches.
    #[cfg(test)]
    fn new_with_dir_exists(
        ctx: BuiltinToolContext,
        dir_exists: fn(&std::path::Path) -> bool,
    ) -> Self {
        Self {
            ctx,
            dir_exists,
            state_persister: None,
        }
    }

    /// Restore the session's cwd on exit — a faithful port of 206's `xCd`
    /// (@222210592). If `original_cwd` still exists, swap the session back to
    /// it (`original_cwd_missing = false`). If it is gone, fall back through
    /// `[worktree_path, $HOME, temp_dir]` — 206's `[n, ICd.homedir(), sG()]`,
    /// where `n` (the first candidate) is the worktree path — picking the first
    /// that exists, and recording whether the fallback landed on the worktree
    /// (`fell_back_to_worktree`). The shared `session_cwd` is swapped to the
    /// resolved directory either way.
    ///
    /// Called AFTER the worktree removal (206 order: `het()` remove precedes
    /// `xCd`), so on a successful `remove` the worktree path no longer exists
    /// and the fallback skips past it to `$HOME`; on `keep` (or a failed
    /// remove) the worktree still exists and is used.
    ///
    /// Deviations from 206, both documented and low-risk: (a) 206 rethrows on a
    /// NON-ENOENT `IS(e)` error (e.g. a permissions failure), whereas this port
    /// folds every "not usable" result into the missing→fallback path — the
    /// port's cwd is logical (no real `chdir`), so there is no reason to hard-
    /// fail a restore; (b) 206's temp dir is `CLAUDE_CODE_TMPDIR || "/tmp"`,
    /// this uses [`std::env::temp_dir`] (`TMPDIR || /tmp`) — the same last-resort
    /// directory in practice.
    fn restore_cwd(&self, original_cwd: &Path, worktree_path: &Path) -> CwdRestore {
        let exists = self.dir_exists;
        if exists(original_cwd) {
            self.ctx
                .session_cwd
                .swap(original_cwd.to_path_buf(), vec![original_cwd.to_path_buf()]);
            return CwdRestore {
                restored_cwd: original_cwd.to_path_buf(),
                original_cwd_missing: false,
                fell_back_to_worktree: false,
            };
        }
        // `original_cwd` is gone — 206's fallback order `[n=worktreePath,
        // homedir(), sG()]`.
        let mut candidates: Vec<PathBuf> = vec![worktree_path.to_path_buf()];
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(home));
        }
        candidates.push(std::env::temp_dir());
        // 206 rethrows if NONE of the fallbacks work; `/tmp` effectively always
        // exists, so the `unwrap_or` (keeping `original_cwd`) is a degenerate
        // last resort that keeps this infallible.
        let restored = candidates
            .into_iter()
            .find(|p| exists(p))
            .unwrap_or_else(|| original_cwd.to_path_buf());
        let fell_back_to_worktree = restored == *worktree_path;
        self.ctx
            .session_cwd
            .swap(restored.clone(), vec![restored.clone()]);
        CwdRestore {
            restored_cwd: restored,
            original_cwd_missing: true,
            fell_back_to_worktree,
        }
    }

    async fn emit_started(&self, invocation_id: &str, branch_name: &str) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("invocation_id".into(), verified(invocation_id));
        md.insert("tool_name".into(), verified(EXIT_TOOL_NAME));
        md.insert("_PROTO_branch_name".into(), pii_tagged(branch_name));
        self.ctx.bus.log_event(EXIT_WORKTREE_STARTED, md).await;
    }

    async fn emit_completed(&self, invocation_id: &str, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("invocation_id".into(), verified(invocation_id));
        md.insert("tool_name".into(), verified(EXIT_TOOL_NAME));
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(EXIT_WORKTREE_COMPLETED, md).await;
    }

    async fn emit_failed(&self, invocation_id: &str, error_kind: &str, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("invocation_id".into(), verified(invocation_id));
        md.insert("tool_name".into(), verified(EXIT_TOOL_NAME));
        md.insert("error_kind".into(), verified(error_kind));
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(EXIT_WORKTREE_FAILED, md).await;
    }

    /// Fire the 206 byte-exact single success event (`tengu_worktree_kept` /
    /// `tengu_worktree_removed`) IN ADDITION to the port's own
    /// started/completed/failed lifecycle triad above.
    async fn emit_worktree_event(&self, event_name: &str, branch_name: &str) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("tool_name".into(), verified(EXIT_TOOL_NAME));
        md.insert("_PROTO_branch_name".into(), pii_tagged(branch_name));
        self.ctx.bus.log_event(event_name, md).await;
    }
}

/// `true` when the raw input's `action` field is `"remove"` — drives
/// `is_destructive`/`user_facing_name_for_input`, mirroring the 206
/// `action==="remove"` checks off the RAW `Value` (those trait hooks never
/// see the parsed/validated [`ExitInput`]).
fn is_remove_action(input: &Value) -> bool {
    input.get("action").and_then(Value::as_str) == Some("remove")
}

#[async_trait]
impl Tool for ExitWorktreeTool {
    fn name(&self) -> &str {
        EXIT_TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("exit a worktree session and return to the original directory")
    }
    /// 2.1.206 `userFacingName(e){return e.action==="remove"?"Cleaning up
    /// worktree":"Exiting worktree"}`.
    fn user_facing_name_for_input(&self, input: &Value) -> Option<String> {
        Some(
            if is_remove_action(input) {
                "Cleaning up worktree"
            } else {
                "Exiting worktree"
            }
            .to_string(),
        )
    }
    fn input_schema(&self) -> &Value {
        &EXIT_INPUT_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    /// 2.1.206 `shouldDefer:!0`.
    fn should_defer(&self) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    /// 2.1.206 `isDestructive(e){return e.action==="remove"}`.
    fn is_destructive(&self, input: &Value) -> bool {
        is_remove_action(input)
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "ExitWorktree gated by M2-01 WorktreeManager".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    /// 2.1.206 `async description(){return"Exits a worktree session created
    /// by EnterWorktree and restores the original working directory"}`
    /// (byte-exact).
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Exits a worktree session created by EnterWorktree and restores the original working directory".into()
    }
    /// 2.1.206 `vCd()` tool-use prompt, extracted verbatim from the binary.
    async fn prompt(&self, _: &PromptOptions) -> String {
        EXIT_WORKTREE_PROMPT.into()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let invocation_id = fresh_invocation_id();
        let started_at = Instant::now();

        let parsed: ExitInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                self.emit_failed(
                    &invocation_id,
                    "invalid_input",
                    started_at.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "ExitWorktree: invalid input: {e}"
                )));
            }
        };

        // 0. Subagent-cwd-override guard — 206 `validateInput`'s `Tze()` branch
        // (errorCode:5), unconditional and FIRST (before the no-op session check).
        // A subagent isolated with a cwd override (`ctx.cwd.is_some()`) must not
        // call ExitWorktree: the port's `session_cwd.swap` would mutate the SHARED
        // parent cwd.
        if ctx.cwd.is_some() {
            let duration_ms = started_at.elapsed().as_millis() as u64;
            self.emit_failed(&invocation_id, "subagent_cwd_override", duration_ms)
                .await;
            return Err(ToolError::InvalidInput(
                EXIT_SUBAGENT_CWD_OVERRIDE_MESSAGE.to_string(),
            ));
        }

        // 1. No active `EnterWorktree` session ⇒ byte-exact no-op (206
        // `validateInput`'s `!ky()` branch, errorCode:1). Mirrors
        // `EnterWorktreeTool`'s `already_in_worktree` guard: fires BEFORE
        // `emit_started` — there is no meaningful operation to mark "started".
        let session = self.ctx.worktree_session.lock().unwrap().clone();
        let Some(session) = session else {
            self.emit_failed(
                &invocation_id,
                "no_active_session",
                started_at.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(
                EXIT_NO_ACTIVE_SESSION_MESSAGE.to_string(),
            ));
        };

        // 4. `remove` on an ENTERED (not owned) worktree ⇒ refuse (206
        // `validateInput` `action==="remove" && t.enteredExisting`, errorCode:4).
        // Fires before `emit_started` — a validateInput-level rejection like the
        // no-op / subagent guards above. `keep` on an entered worktree is fine.
        if matches!(parsed.action, ExitAction::Remove) && session.entered_existing {
            self.emit_failed(
                &invocation_id,
                "not_owner",
                started_at.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(exit_not_owner_message(
                &session.worktree_path.to_string_lossy(),
                &session.original_cwd.to_string_lossy(),
            )));
        }

        self.emit_started(&invocation_id, &session.branch_name)
            .await;

        let is_remove = matches!(parsed.action, ExitAction::Remove);
        let discard_changes = parsed.discard_changes.unwrap_or(false);
        let handle = WorktreeHandle {
            path: session.worktree_path.clone(),
            branch_name: session.branch_name.clone(),
            base_commit: session.base_commit.clone(),
        };

        // 2. `remove` without `discard_changes`: gate on the worktree's dirty
        // state (206 `RCd` change-summary + errorCode:2 / can't-verify
        // refusal). The query result is retained in `discard_summary` for the
        // "Discarded …" note in the success message built in step 7 below —
        // 206's own change-summary query (`oUl`) is UNCONDITIONAL for
        // `remove` (used for both this gate and the discard note), falling
        // back to zero counts (`?? {changedFiles:0,commits:0}`) on a
        // failed/missing query rather than blocking when `discard_changes`
        // is set.
        let mut discard_summary: Option<WorktreeChangeSummary> = None;
        if is_remove {
            if discard_changes {
                discard_summary = self
                    .ctx
                    .worktree
                    .worktree_change_summary(&handle)
                    .await
                    .ok()
                    .flatten();
            } else {
                let worktree_path_display = session.worktree_path.to_string_lossy().into_owned();
                match self.ctx.worktree.worktree_change_summary(&handle).await {
                    Ok(Some(summary)) if summary.is_dirty() => {
                        let branch_for_phrase = if session.branch_name.is_empty() {
                            "the worktree branch"
                        } else {
                            session.branch_name.as_str()
                        };
                        // File-then-commit order matches the oracle's push order
                        // (`i.push(uncommitted...)` before `i.push(commit...)`).
                        let mut parts: Vec<String> = Vec::new();
                        if let Some(files) = summary.changed_files_phrase() {
                            parts.push(files);
                        }
                        if let Some(commits) = summary.commits_phrase(branch_for_phrase) {
                            parts.push(commits);
                        }
                        self.emit_failed(
                            &invocation_id,
                            "has_changes",
                            started_at.elapsed().as_millis() as u64,
                        )
                        .await;
                        return Err(ToolError::InvalidInput(exit_has_changes_message(
                            &parts.join(" and "),
                        )));
                    }
                    Ok(Some(summary)) => {
                        // Clean — proceed. Retained for the discard note
                        // below, though `discard_note()` yields "" here
                        // since nothing is dirty.
                        discard_summary = Some(summary);
                    }
                    Ok(None) | Err(_) => {
                        // Fail-closed: git couldn't be queried (or no baseline
                        // commit — `worktree_change_summary`'s documented
                        // "unknown" contract), matching the oracle's `RCd`
                        // returning `null`.
                        self.emit_failed(
                            &invocation_id,
                            "cannot_verify",
                            started_at.elapsed().as_millis() as u64,
                        )
                        .await;
                        return Err(ToolError::InvalidInput(exit_cannot_verify_message(
                            &worktree_path_display,
                        )));
                    }
                }
            }
        }

        // 3. (Restore of the session cwd is deferred to step 6, AFTER the
        // worktree removal — 206 order: `het()` remove precedes `xCd` restore,
        // so the missing-original-cwd fallback sees the post-removal on-disk
        // state of the worktree, not its pre-removal state.)

        // 4. Tmux: kill on `remove`, BEFORE the git-level worktree removal —
        // mirrors 206's `HCd.call` ordering (`if(s)await rPe(s)` precedes its
        // `het()` removal call). `keep` never kills (left running; surfaced
        // in the message/data below). INERT when `tmux_session_name` is
        // `None` (today's only reachable case in production — see this
        // type's doc). A kill failure is NON-FATAL: 206 ignores `rPe`'s
        // return value and proceeds to remove regardless, so this only logs
        // a warning and continues.
        if is_remove {
            if let Some(tmux_name) = session.tmux_session_name.as_deref() {
                if let Err(err) =
                    kill_worktree_tmux_session(&*self.ctx.process, &*self.ctx.sandbox, tmux_name)
                        .await
                {
                    tracing::warn!(
                        session_name = tmux_name,
                        error = %err,
                        "ExitWorktree: failed to kill worktree tmux session; continuing removal"
                    );
                }
            }
        }

        // 5. Remove (if requested) or leave the worktree on disk (`keep`). A
        // removal failure is NON-FATAL, matching 206: `HCd.call`'s `oht()`
        // removal step returns a boolean (`d`) rather than throwing, and
        // `d===false` still returns a normal (`is_error:false`) tool result
        // carrying the "could not remove" message (binary-verified
        // @222215668) — it only skips the `tengu_worktree_removed` event and
        // the discard note below. The port's `remove_worktree`
        // (`platforms/posix::worktree`) only ever produces
        // `WorktreeError::Git`/`WorktreeError::Io` here (a failed `git
        // worktree remove` invocation), both of which collapse into this one
        // non-fatal branch, matching 206's undifferentiated boolean.
        let remove_failed = if is_remove {
            match self.ctx.worktree.remove_worktree(&handle).await {
                Ok(()) => false,
                Err(err) => {
                    tracing::warn!(
                        branch = %session.branch_name,
                        error = %err,
                        "ExitWorktree: failed to remove worktree; session still exits (206 `d===false` is non-fatal)"
                    );
                    true
                }
            }
        } else {
            false
        };

        // 5b. Restore the session's cwd (206 `xCd`, run AFTER removal). On a
        // successful `remove` the worktree path is now gone, so a missing-
        // original-cwd fallback skips past it to `$HOME`; on `keep` (or a
        // failed remove) the worktree still exists and is the fallback target.
        // `swap` (not `set_on_swap`) so the orchestrator's registered cache-
        // invalidation callback fires exactly as it does for `EnterWorktree`.
        let cwd_restore = self.restore_cwd(&session.original_cwd, &session.worktree_path);

        // 6. Clear the session record — a further `ExitWorktree` call (with
        // no intervening `EnterWorktree`) now takes the no-op path. This runs
        // regardless of `remove_failed`: 206 considers the session exited
        // once removal is attempted, whether or not it actually succeeded.
        *self.ctx.worktree_session.lock().unwrap() = None;
        // (parity 2.1.212) Persist the CLEAR record (`worktreeSession: null`,
        // claude's `gne(null)`) so a later `--resume` does not re-restore the
        // exited worktree. Best-effort; no-op when no persister is wired.
        if let Some(persister) = &self.state_persister {
            persister.persist_worktree_state(None).await;
        }

        let duration_ms = started_at.elapsed().as_millis() as u64;
        self.emit_completed(&invocation_id, duration_ms).await;
        // 206's `tengu_worktree_removed` event fires only when the removal
        // actually succeeded (`d===true`, i.e. `!remove_failed`);
        // `tengu_worktree_kept` is unconditional on `keep` (`remove_failed`
        // is always `false` there).
        if !remove_failed {
            self.emit_worktree_event(
                if is_remove {
                    WORKTREE_REMOVED
                } else {
                    WORKTREE_KEPT
                },
                &session.branch_name,
            )
            .await;
        }

        // 7. MODEL-facing message = 206's `data.message` (see this type's
        // module doc for why that differs from the TUI-only `wCd` render).
        // Byte-recovered from the 2.1.206 binary (`HCd.call`'s keep/remove
        // branches + its `y9o` cwd-restore-phrase helper).
        let branch_suffix_str = if !session.branch_name.is_empty() && session.branch_name != "HEAD"
        {
            format!(" on branch {}", session.branch_name)
        } else {
            String::new()
        };
        let original_cwd_display = session.original_cwd.to_string_lossy().into_owned();
        let worktree_path_display = session.worktree_path.to_string_lossy().into_owned();

        // 206 `kAs(originalCwd, xCdResult)` — full port (@222211173):
        //   • cwd still exists → `Session is now back in {original_cwd}.`
        //   • cwd gone, fell back to the worktree →
        //       `The original directory {original_cwd} no longer exists, so the
        //        session is now in {restored_cwd}.`
        //   • cwd gone, fell back to $HOME/tmp (NOT the worktree) → the same,
        //     plus ` Consider restarting LingXi from an existing directory.`
        //     (`Claude`→`LingXi` product-name rebrand, matching this file's
        //     other user-facing strings).
        let cwd_restored_phrase = if !cwd_restore.original_cwd_missing {
            format!("Session is now back in {original_cwd_display}.")
        } else {
            let restored_display = cwd_restore.restored_cwd.to_string_lossy();
            let base = format!(
                "The original directory {original_cwd_display} no longer exists, so the session is now in {restored_display}."
            );
            if cwd_restore.fell_back_to_worktree {
                base
            } else {
                format!("{base} Consider restarting LingXi from an existing directory.")
            }
        };

        let tmux_session_name_for_data: Option<String> = if is_remove {
            None
        } else {
            session.tmux_session_name.clone()
        };
        // Tmux reattach surfacing — ADDITIVE only, on `keep` with a session
        // name (never on `remove`: it was just killed above, and 206 itself
        // never puts `tmuxSessionName` in `data` on that path either). Byte-
        // recovered from 206's `HCd.call` keep branch: `` ` Tmux session
        // ${s} is still running; reattach with: tmux attach -t ${s}` `` —
        // note the LEADING SPACE, joined inline (not on its own line) into
        // the single-string `data.message`.
        let tmux_reattach_suffix = tmux_session_name_for_data
            .as_deref()
            .map(|name| {
                format!(
                    " Tmux session {name} is still running; reattach with: tmux attach -t {name}"
                )
            })
            .unwrap_or_default();

        let message = if !is_remove {
            format!(
                "Exited worktree. Your work is preserved at {worktree_path_display}{branch_suffix_str}. {cwd_restored_phrase}{tmux_reattach_suffix}"
            )
        } else if remove_failed {
            format!(
                "Exited worktree but could not remove it \u{2014} kept at {worktree_path_display}. {cwd_restored_phrase}"
            )
        } else {
            // Commits-first, then uncommitted files (binary-verified push
            // order: `if(u>0)f.push(commit...);if(c>0)f.push(file...)`),
            // joined with " and ", wrapped as " Discarded <parts>." — see
            // `WorktreeChangeSummary::discard_note`.
            let discard_note = discard_summary
                .map(|summary| summary.discard_note())
                .unwrap_or_default();
            format!(
                "Exited and removed worktree at {worktree_path_display}.{discard_note} {cwd_restored_phrase}"
            )
        };

        let mut data = json!({
            "action": if is_remove { "remove" } else { "keep" },
            "branch_name": session.branch_name,
            "worktree_path": worktree_path_display,
            "original_cwd": original_cwd_display,
        });
        if let Some(name) = tmux_session_name_for_data {
            data["tmux_session_name"] = json!(name);
        }

        Ok(ToolCallResult {
            data,
            model_content: Some(message),
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::worktree::WorktreeManager;
    use std::sync::Arc;
    use telemetry::{AnalyticsBus, InMemorySink};
    use tool_api::test_support::{
        ctx_for_file_tools, fresh_ctx, fresh_tx, make_dummy_fs, MockWorktreeManager,
    };

    fn make_bctx(mock: Arc<MockWorktreeManager>) -> (BuiltinToolContext, Arc<InMemorySink>) {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        let mut bctx = ctx_for_file_tools(make_dummy_fs(), bus.clone(), vec![std::env::temp_dir()]);
        bctx.worktree = mock as Arc<dyn WorktreeManager>;
        (bctx, sink)
    }

    #[test]
    fn branch_prefix_matches_lock() {
        assert_eq!(WORKTREE_BRANCH_PREFIX, "worktree-");
    }

    #[test]
    fn path_segment_matches_lock() {
        assert_eq!(WORKTREE_PATH_SEGMENT, ".lingxi/worktrees");
    }

    #[test]
    fn flatten_char_is_plus() {
        assert_eq!(WORKTREE_FLATTEN_CHAR, '+');
    }

    #[test]
    fn flatten_slug_uses_plus() {
        assert_eq!(flatten_slug("user/feature"), "user+feature");
        assert_eq!(flatten_slug("a/b/c"), "a+b+c");
        assert_eq!(flatten_slug("plain"), "plain");
    }

    #[test]
    fn validate_accepts_well_formed() {
        assert!(validate_worktree_slug("feature-x").is_ok());
        assert!(validate_worktree_slug("user/feature").is_ok());
        assert!(validate_worktree_slug("team/area/widget").is_ok());
    }

    #[test]
    fn validate_rejects_spaces() {
        assert!(validate_worktree_slug("bad slug").is_err());
    }

    #[test]
    fn validate_rejects_plus_sign() {
        // The flatten output uses `+`; allowing `+` in inputs would break
        // the injectivity guarantee.
        assert!(validate_worktree_slug("a+b").is_err());
    }

    #[test]
    fn validate_rejects_empty_segment() {
        assert!(validate_worktree_slug("foo/").is_err());
        assert!(validate_worktree_slug("/foo").is_err());
        assert!(validate_worktree_slug("a//b").is_err());
    }

    #[test]
    fn validate_rejects_overlong_slug() {
        let huge = "a".repeat(MAX_WORKTREE_SLUG_LENGTH + 1);
        assert!(validate_worktree_slug(&huge).is_err());
    }

    #[test]
    fn full_branch_name_layout() {
        let slug = "user/feature";
        let full = format!("{}{}", WORKTREE_BRANCH_PREFIX, flatten_slug(slug));
        assert_eq!(full, "worktree-user+feature");
    }

    #[tokio::test]
    async fn enter_creates_worktree_with_locked_layout() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-A"));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = EnterWorktreeTool::new(bctx);
        let res = tool
            .call(json!({ "name": "user/feature" }), fresh_ctx(), fresh_tx())
            .await
            .expect("create must succeed");
        assert_eq!(res.data["branch_name"], "worktree-user+feature");
        let path = res.data["path"].as_str().unwrap();
        assert!(
            path.ends_with("/tmp/repo-A/.lingxi/worktrees/user+feature"),
            "path layout off: {path}"
        );
        assert_eq!(mock.created().len(), 1);
        assert_eq!(mock.created()[0].0, "user/feature");
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&ENTER_WORKTREE_STARTED.to_string()));
        assert!(names.contains(&ENTER_WORKTREE_COMPLETED.to_string()));
        assert!(names.contains(&WORKTREE_CREATED.to_string()));
    }

    #[tokio::test]
    async fn enter_rejects_slug_with_space() {
        let mock = Arc::new(MockWorktreeManager::new());
        let (bctx, sink) = make_bctx(mock);
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = EnterWorktreeTool::new(bctx);
        let err = tool
            .call(json!({ "name": "bad slug" }), fresh_ctx(), fresh_tx())
            .await
            .expect_err("space must reject");
        let msg = format!("{err}");
        assert!(msg.contains("EnterWorktree: invalid slug:"), "msg: {msg}");
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&ENTER_WORKTREE_FAILED.to_string()));
    }

    #[tokio::test]
    async fn enter_maps_git_error_to_internal() {
        let mock = Arc::new(MockWorktreeManager::new());
        mock.script_create_error(WorktreeError::Git("fatal: not a git repository".into()));
        let (bctx, sink) = make_bctx(mock);
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = EnterWorktreeTool::new(bctx);
        let err = tool
            .call(json!({ "name": "ok-slug" }), fresh_ctx(), fresh_tx())
            .await
            .expect_err("git error must surface");
        let msg = format!("{err}");
        assert!(msg.contains("EnterWorktree: git error:"), "msg: {msg}");
        assert!(msg.contains("fatal: not a git repository"), "msg: {msg}");
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&ENTER_WORKTREE_FAILED.to_string()));
    }

    #[tokio::test]
    async fn enter_maps_unsupported_to_internal() {
        let mock = Arc::new(MockWorktreeManager::new());
        mock.script_create_error(WorktreeError::Unsupported);
        let (bctx, sink) = make_bctx(mock);
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = EnterWorktreeTool::new(bctx);
        let err = tool
            .call(json!({ "name": "ok-slug" }), fresh_ctx(), fresh_tx())
            .await
            .expect_err("unsupported must surface");
        assert_eq!(
            format!("{err}"),
            "internal: EnterWorktree: worktrees are not supported on this platform"
        );
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&ENTER_WORKTREE_FAILED.to_string()));
    }

    #[tokio::test]
    async fn enter_does_not_retry_internally() {
        let mock = Arc::new(MockWorktreeManager::new());
        mock.script_create_error(WorktreeError::Git("transient".into()));
        let (bctx, _sink) = make_bctx(mock.clone());
        let tool = EnterWorktreeTool::new(bctx);
        let _ = tool
            .call(json!({ "name": "ok-slug" }), fresh_ctx(), fresh_tx())
            .await;
        // Only the scripted error fires; created list stays empty (no retry).
        assert_eq!(mock.created().len(), 0);
    }

    // ===== 206 golden-message + session-cwd-swap tests (Task 7) ===============

    #[tokio::test]
    async fn create_message_exact_with_branch_suffix() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-create"));
        let (bctx, _sink) = make_bctx(mock);
        let tool = EnterWorktreeTool::new(bctx);
        let res = tool
            .call(json!({ "name": "feat" }), fresh_ctx(), fresh_tx())
            .await
            .expect("create must succeed");
        let path = res.data["path"].as_str().unwrap().to_string();
        assert_eq!(
            res.model_content.as_deref(),
            Some(
                format!(
                    "Created worktree at {path} on branch worktree-feat. The session is now working in the worktree. Use ExitWorktree to leave mid-session, or exit the session to be prompted."
                )
                .as_str()
            )
        );
    }

    #[tokio::test]
    async fn create_message_exact_without_branch_suffix() {
        // The mock always returns a real branch name, so exercise the
        // no-suffix path directly through `branch_suffix` — the golden text
        // around the `{branch}` slot is asserted with a synthesized empty
        // branch to prove the "" case renders with no " on branch" fragment
        // and no double space before the period.
        assert_eq!(branch_suffix(""), "");
        let path = "/tmp/repo-create/.lingxi/worktrees/feat";
        let message = format!(
            "Created worktree at {path}{}. The session is now working in the worktree. Use ExitWorktree to leave mid-session, or exit the session to be prompted.",
            branch_suffix("")
        );
        assert_eq!(
            message,
            "Created worktree at /tmp/repo-create/.lingxi/worktrees/feat. The session is now working in the worktree. Use ExitWorktree to leave mid-session, or exit the session to be prompted."
        );
    }

    #[tokio::test]
    async fn enter_existing_message_exact() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-enter"));
        let (bctx, _sink) = make_bctx(mock);
        let tool = EnterWorktreeTool::new(bctx);
        let target = "/tmp/repo-enter/.lingxi/worktrees/feat";
        let res = tool
            .call(json!({ "path": target }), fresh_ctx(), fresh_tx())
            .await
            .expect("enter existing must succeed");
        assert_eq!(
            res.model_content.as_deref(),
            Some(
                "Entered worktree at /tmp/repo-enter/.lingxi/worktrees/feat on branch worktree-feat. The session is now working in the worktree. Use ExitWorktree to leave mid-session, or exit the session to be prompted."
            )
        );
    }

    #[tokio::test]
    async fn enter_existing_detached_head_has_no_branch_suffix() {
        // `enter_existing` returning `branch_name == "HEAD"` (detached) must
        // NOT render " on branch HEAD" — treated as no branch.
        assert_eq!(branch_suffix("HEAD"), "");

        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-detached"));
        let target = PathBuf::from("/tmp/repo-detached/.lingxi/worktrees/feat");
        // Script the EXACT handle `enter_existing` returns so its
        // `branch_name` is the literal `"HEAD"` a real posix manager would
        // report for a detached-HEAD worktree (git prints "HEAD" for
        // `--abbrev-ref HEAD` when detached).
        mock.script_enter_existing_handle(WorktreeHandle {
            path: target.clone(),
            branch_name: "HEAD".to_string(),
            base_commit: None,
        });
        let (bctx, _sink) = make_bctx(mock);
        let tool = EnterWorktreeTool::new(bctx);
        let res = tool
            .call(
                json!({ "path": target.to_string_lossy() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("enter existing must succeed");
        let msg = res.model_content.as_deref().unwrap();
        assert!(!msg.contains("on branch HEAD"), "msg: {msg}");
        assert_eq!(
            msg,
            "Entered worktree at /tmp/repo-detached/.lingxi/worktrees/feat. The session is now working in the worktree. Use ExitWorktree to leave mid-session, or exit the session to be prompted."
        );
    }

    #[tokio::test]
    async fn empty_string_path_is_treated_as_absent_and_takes_create_path() {
        // 206 `e.path` is a JS truthiness check — `path: ""` is falsy, so it
        // must route to CREATE (no `name` either ⇒ a random slug), never to
        // `call_enter_existing`.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-empty-path"));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = EnterWorktreeTool::new(bctx);
        let res = tool
            .call(json!({ "path": "" }), fresh_ctx(), fresh_tx())
            .await
            .expect("empty-string path must be treated as absent and create");
        assert_eq!(mock.created().len(), 1, "must have created, not entered");
        let msg = res.model_content.as_deref().unwrap();
        assert!(
            msg.starts_with("Created worktree at "),
            "msg should use the CREATE verb: {msg}"
        );
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&WORKTREE_CREATED.to_string()));
        assert!(!names.contains(&WORKTREE_ENTERED_EXISTING.to_string()));
    }

    #[test]
    fn empty_string_path_user_facing_name_is_creating_worktree() {
        let mock = Arc::new(MockWorktreeManager::new());
        let (bctx, _sink) = make_bctx(mock);
        let tool = EnterWorktreeTool::new(bctx);
        assert_eq!(
            tool.user_facing_name_for_input(&json!({ "path": "" }))
                .as_deref(),
            Some("Creating worktree")
        );
    }

    #[tokio::test]
    async fn already_in_worktree_guard_rejects_with_exact_message_and_no_swap() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-guard"));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        // An active worktree session — the port's `ky()` == `worktree_session`
        // is `Some`. Deliberately use a worktree path OUTSIDE `.lingxi/worktrees/`
        // (an existing worktree entered via `path`): the OLD path-substring
        // heuristic would have false-negatived here and wrongly ALLOWED the
        // create; the session-record guard correctly rejects it.
        let external_wt = PathBuf::from("/tmp/external-checkout/feature-wt");
        populate_session(
            &bctx,
            &PathBuf::from("/tmp/repo-guard"),
            &external_wt,
            "feature-wt",
            None,
        );
        let tool = EnterWorktreeTool::new(bctx);
        let err = tool
            .call(json!({}), fresh_ctx(), fresh_tx())
            .await
            .expect_err("create while already in a worktree must reject");
        assert_eq!(
            format!("{err}"),
            format!("invalid input: {ALREADY_IN_WORKTREE_MESSAGE}")
        );
        // No worktree was created; the cwd was NOT swapped again; and the active
        // session record is left intact by the rejected create.
        assert_eq!(mock.created().len(), 0);
        assert_eq!(tool.ctx.cwd(), external_wt, "guard must not swap cwd");
        assert!(
            tool.ctx.worktree_session.lock().unwrap().is_some(),
            "guard must not clear the active session"
        );
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&ENTER_WORKTREE_FAILED.to_string()));
        assert!(!names.contains(&WORKTREE_CREATED.to_string()));
    }

    #[tokio::test]
    async fn entering_existing_worktree_is_allowed_even_when_already_in_one() {
        // The guard only fires when CREATING (no `path`); switching via
        // `path` is allowed even from inside another worktree.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-switch"));
        let (bctx, _sink) = make_bctx(mock);
        let worktree_cwd = PathBuf::from("/tmp/repo-switch/.lingxi/worktrees/first");
        bctx.session_cwd
            .swap(worktree_cwd.clone(), vec![worktree_cwd]);
        let tool = EnterWorktreeTool::new(bctx);
        let target = "/tmp/repo-switch/.lingxi/worktrees/second";
        let res = tool
            .call(json!({ "path": target }), fresh_ctx(), fresh_tx())
            .await
            .expect("switching via path must succeed even mid-worktree");
        assert_eq!(res.data["path"], target);
    }

    #[tokio::test]
    async fn create_from_isolated_subagent_is_refused() {
        // 206 `validateInput` `Tze() && !e.path`: a subagent isolated with a cwd
        // override (`ctx.cwd.is_some()`) may NOT create a worktree — the port's
        // `session_cwd.swap` would mutate the SHARED parent cwd.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-sub"));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let boot_cwd = bctx.cwd();
        let tool = EnterWorktreeTool::new(bctx);
        let mut sub = fresh_ctx();
        sub.cwd = Some(PathBuf::from("/tmp/isolated/agent-wt"));
        let err = tool
            .call(json!({}), sub, fresh_tx())
            .await
            .expect_err("create from an isolated subagent must refuse");
        assert_eq!(
            format!("{err}"),
            format!("invalid input: {ENTER_SUBAGENT_CWD_OVERRIDE_MESSAGE}")
        );
        assert_eq!(mock.created().len(), 0, "no worktree created");
        assert_eq!(
            tool.ctx.cwd(),
            boot_cwd,
            "guard must not swap the shared cwd"
        );
    }

    #[tokio::test]
    async fn enter_existing_from_isolated_subagent_is_allowed() {
        // 206 `Tze() && e.path` is allowed — the create-only guard must NOT block
        // switching into an existing worktree via `path`.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-sub2"));
        let (bctx, _sink) = make_bctx(mock);
        let tool = EnterWorktreeTool::new(bctx);
        let mut sub = fresh_ctx();
        sub.cwd = Some(PathBuf::from("/tmp/isolated/agent-wt"));
        let target = "/tmp/repo-sub2/.lingxi/worktrees/existing";
        let res = tool
            .call(json!({ "path": target }), sub, fresh_tx())
            .await
            .expect("enter-existing via path is allowed from an isolated subagent");
        assert_eq!(res.data["path"], target);
    }

    #[tokio::test]
    async fn worktree_created_event_carries_mid_session_payload() {
        // 206 `N("tengu_worktree_created",{mid_session:true})` — NOT the old
        // {tool_name,_PROTO_branch_name}.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-tel"));
        let (bctx, sink) = make_bctx(mock);
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = EnterWorktreeTool::new(bctx);
        let _ = tool
            .call(json!({ "name": "teltest" }), fresh_ctx(), fresh_tx())
            .await
            .expect("create");
        let ev = sink
            .events()
            .await
            .into_iter()
            .find(|e| e.name == WORKTREE_CREATED)
            .expect("created event fired");
        assert!(ev.metadata.contains_key("mid_session"), "has mid_session");
        assert!(
            !ev.metadata.contains_key("cwd_override"),
            "create: no cwd_override"
        );
        assert!(
            !ev.metadata.contains_key("tool_name"),
            "old tool_name dropped"
        );
        assert!(
            !ev.metadata.contains_key("_PROTO_branch_name"),
            "old branch field dropped"
        );
    }

    #[tokio::test]
    async fn worktree_entered_existing_event_carries_cwd_override_payload() {
        // 206 `N("tengu_worktree_entered_existing",{mid_session:true,cwd_override:true})`.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-tel2"));
        let (bctx, sink) = make_bctx(mock);
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = EnterWorktreeTool::new(bctx);
        let _ = tool
            .call(
                json!({ "path": "/tmp/repo-tel2/.lingxi/worktrees/x" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("enter");
        let ev = sink
            .events()
            .await
            .into_iter()
            .find(|e| e.name == WORKTREE_ENTERED_EXISTING)
            .expect("entered_existing event fired");
        assert!(ev.metadata.contains_key("mid_session"), "has mid_session");
        assert!(
            ev.metadata.contains_key("cwd_override"),
            "enter: has cwd_override"
        );
    }

    #[tokio::test]
    async fn successful_create_swaps_session_cwd_to_worktree_path() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-swap"));
        let (bctx, _sink) = make_bctx(mock);
        let boot_cwd = bctx.cwd();
        let tool = EnterWorktreeTool::new(bctx);
        let res = tool
            .call(json!({ "name": "swaptest" }), fresh_ctx(), fresh_tx())
            .await
            .expect("create must succeed");
        let path = PathBuf::from(res.data["path"].as_str().unwrap());
        assert_eq!(tool.ctx.cwd(), path, "session cwd must swap to worktree");
        assert_ne!(tool.ctx.cwd(), boot_cwd);
    }

    #[tokio::test]
    async fn error_path_does_not_swap_session_cwd() {
        let mock = Arc::new(MockWorktreeManager::new());
        mock.script_create_error(WorktreeError::Git("boom".into()));
        let (bctx, _sink) = make_bctx(mock);
        let boot_cwd = bctx.cwd();
        let tool = EnterWorktreeTool::new(bctx);
        let _ = tool
            .call(json!({ "name": "ok-slug" }), fresh_ctx(), fresh_tx())
            .await
            .expect_err("scripted git error must surface");
        assert_eq!(tool.ctx.cwd(), boot_cwd, "an error path must not swap cwd");
    }

    #[tokio::test]
    async fn invalid_input_error_path_does_not_swap_session_cwd() {
        let mock = Arc::new(MockWorktreeManager::new());
        let (bctx, _sink) = make_bctx(mock);
        let boot_cwd = bctx.cwd();
        let tool = EnterWorktreeTool::new(bctx);
        let _ = tool
            .call(json!({ "name": "bad slug" }), fresh_ctx(), fresh_tx())
            .await
            .expect_err("invalid slug must reject");
        assert_eq!(tool.ctx.cwd(), boot_cwd, "invalid input must not swap cwd");
    }

    #[test]
    fn description_and_prompt_texts_are_206_byte_exact() {
        assert!(
            ENTER_WORKTREE_PROMPT.contains(".lingxi/worktrees/"),
            "prompt must use the rebranded path segment"
        );
        assert!(!ENTER_WORKTREE_PROMPT.contains(".claude/worktrees/"));
        assert!(!ENTER_WORKTREE_PROMPT.contains("CLAUDE.md"));
        assert!(ENTER_WORKTREE_PROMPT.contains("LINGXI.md"));
        assert!(ENTER_WORKTREE_PROMPT.contains("settings.json"));
    }

    #[test]
    fn user_facing_name_depends_on_path_presence() {
        let mock = Arc::new(MockWorktreeManager::new());
        let (bctx, _sink) = make_bctx(mock);
        let tool = EnterWorktreeTool::new(bctx);
        assert_eq!(
            tool.user_facing_name_for_input(&json!({})).as_deref(),
            Some("Creating worktree")
        );
        assert_eq!(
            tool.user_facing_name_for_input(&json!({ "path": "/x" }))
                .as_deref(),
            Some("Entering worktree")
        );
    }

    #[test]
    fn should_defer_is_true() {
        let mock = Arc::new(MockWorktreeManager::new());
        let (bctx, _sink) = make_bctx(mock);
        let tool = EnterWorktreeTool::new(bctx);
        assert!(tool.should_defer());
    }

    /// Populate the shared [`tool_api::WorktreeSession`] cell directly — a
    /// test double for what `EnterWorktreeTool::record_worktree_session`
    /// writes, used by tests that exercise `ExitWorktreeTool` in isolation
    /// (the round-trip test below drives the real `EnterWorktreeTool` too).
    fn populate_session(
        bctx: &BuiltinToolContext,
        original_cwd: &std::path::Path,
        worktree_path: &std::path::Path,
        branch_name: &str,
        base_commit: Option<String>,
    ) {
        *bctx.worktree_session.lock().unwrap() = Some(tool_api::WorktreeSession {
            original_cwd: original_cwd.to_path_buf(),
            worktree_path: worktree_path.to_path_buf(),
            branch_name: branch_name.to_string(),
            base_commit,
            // Default to a CREATED (owned) worktree so the existing keep/remove
            // tests exercise the removable path; the errorCode:4 test overrides
            // `entered_existing` to `true` inline.
            entered_existing: false,
            tmux_session_name: None,
        });
        // Mirror what EnterWorktree would have done: the session is now
        // "inside" the worktree.
        bctx.session_cwd.swap(
            worktree_path.to_path_buf(),
            vec![worktree_path.to_path_buf()],
        );
    }

    /// Recording [`tool_api::WorktreeStatePersister`] test double: captures every
    /// `persist_worktree_state` call's payload (the serialized worktreeSession, or
    /// `None` for a clear record) so tests can assert the enter→persist(Some) /
    /// exit→persist(None) contract (parity 2.1.212).
    #[derive(Default)]
    struct RecordingPersister {
        calls: std::sync::Mutex<Vec<Option<serde_json::Value>>>,
    }

    #[async_trait::async_trait]
    impl tool_api::WorktreeStatePersister for RecordingPersister {
        async fn persist_worktree_state(&self, session: Option<&tool_api::WorktreeSession>) {
            self.calls
                .lock()
                .unwrap()
                .push(session.map(tool_api::WorktreeSession::to_persisted_json));
        }
    }

    #[tokio::test]
    async fn enter_persists_active_worktree_state() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-persist"));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let persister = Arc::new(RecordingPersister::default());
        let tool = EnterWorktreeTool::new(bctx).with_state_persister(persister.clone());
        tool.call(json!({ "name": "feat" }), fresh_ctx(), fresh_tx())
            .await
            .expect("create must succeed");
        let calls = persister.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "enter persists exactly once");
        let payload = calls[0]
            .as_ref()
            .expect("active session persisted, not null");
        assert_eq!(
            payload["worktreePath"].as_str().unwrap(),
            "/tmp/repo-persist/.lingxi/worktrees/feat"
        );
        assert_eq!(payload["worktreeBranch"], "worktree-feat");
        assert_eq!(payload["enteredExisting"], false);
    }

    #[tokio::test]
    async fn exit_persists_clear_record() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-clear"));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let original_cwd = PathBuf::from("/tmp/repo-clear");
        let worktree_path = PathBuf::from("/tmp/repo-clear/.lingxi/worktrees/feat");
        populate_session(&bctx, &original_cwd, &worktree_path, "worktree-feat", None);
        let persister = Arc::new(RecordingPersister::default());
        let tool =
            ExitWorktreeTool::new_assuming_cwd_exists(bctx).with_state_persister(persister.clone());
        tool.call(json!({ "action": "keep" }), fresh_ctx(), fresh_tx())
            .await
            .expect("keep must succeed");
        let calls = persister.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "exit persists exactly once");
        assert!(
            calls[0].is_none(),
            "exit persists the CLEAR record (worktreeSession: null)"
        );
    }

    // ===== 206 ExitWorktree golden tests (Task 8) ==============================

    #[tokio::test]
    async fn exit_no_active_session_is_noop_with_exact_message() {
        let mock = Arc::new(MockWorktreeManager::new());
        let (bctx, sink) = make_bctx(mock);
        bctx.bus.attach_sink(sink.clone()).await;
        let boot_cwd = bctx.cwd();
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let err = tool
            .call(json!({ "action": "keep" }), fresh_ctx(), fresh_tx())
            .await
            .expect_err("no active session must be a no-op error");
        assert_eq!(
            format!("{err}"),
            format!("invalid input: {EXIT_NO_ACTIVE_SESSION_MESSAGE}")
        );
        // No filesystem/cwd changes were made.
        assert_eq!(tool.ctx.cwd(), boot_cwd);
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&EXIT_WORKTREE_FAILED.to_string()));
        assert!(!names.contains(&EXIT_WORKTREE_STARTED.to_string()));
    }

    #[tokio::test]
    async fn exit_from_isolated_subagent_is_refused() {
        // 206 `validateInput` `Tze()` (errorCode:5): unconditional and BEFORE the
        // no-op/session logic. An isolated subagent's ExitWorktree would mutate the
        // SHARED parent cwd, so it is refused even with an active session.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-exsub"));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let wt = PathBuf::from("/tmp/repo-exsub/.lingxi/worktrees/wt");
        populate_session(
            &bctx,
            &PathBuf::from("/tmp/repo-exsub"),
            &wt,
            "worktree-wt",
            None,
        );
        let session_cwd = bctx.cwd();
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let mut sub = fresh_ctx();
        sub.cwd = Some(PathBuf::from("/tmp/isolated/agent-wt"));
        let err = tool
            .call(json!({ "action": "remove" }), sub, fresh_tx())
            .await
            .expect_err("ExitWorktree from an isolated subagent must refuse");
        assert_eq!(
            format!("{err}"),
            format!("invalid input: {EXIT_SUBAGENT_CWD_OVERRIDE_MESSAGE}")
        );
        // Guard fired first: no removal, no swap, session record intact.
        assert_eq!(mock.removed().len(), 0, "no removal");
        assert_eq!(tool.ctx.cwd(), session_cwd, "guard must not swap");
        assert!(
            tool.ctx.worktree_session.lock().unwrap().is_some(),
            "guard must not clear the active session"
        );
    }

    #[tokio::test]
    async fn exit_remove_on_entered_worktree_refuses_not_owner() {
        // 206 errorCode:4: `remove` on a worktree this session ENTERED (via `path`,
        // not created) is refused — this session is not the owner.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-entered"));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let wt = PathBuf::from("/tmp/repo-entered/.lingxi/worktrees/wt");
        let original = PathBuf::from("/tmp/repo-entered");
        *bctx.worktree_session.lock().unwrap() = Some(tool_api::WorktreeSession {
            original_cwd: original.clone(),
            worktree_path: wt.clone(),
            branch_name: "worktree-wt".into(),
            base_commit: None,
            entered_existing: true,
            tmux_session_name: None,
        });
        bctx.session_cwd.swap(wt.clone(), vec![wt.clone()]);
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let err = tool
            .call(json!({ "action": "remove" }), fresh_ctx(), fresh_tx())
            .await
            .expect_err("remove on an entered worktree must refuse");
        assert_eq!(
            format!("{err}"),
            format!(
                "invalid input: {}",
                exit_not_owner_message(&wt.to_string_lossy(), &original.to_string_lossy())
            )
        );
        assert_eq!(mock.removed().len(), 0, "no removal");
        assert!(
            tool.ctx.worktree_session.lock().unwrap().is_some(),
            "guard must not clear the session"
        );
    }

    #[tokio::test]
    async fn exit_keep_on_entered_worktree_is_allowed() {
        // errorCode:4 only blocks `remove`; `keep` on an entered worktree works.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-entkeep"));
        let (bctx, _sink) = make_bctx(mock.clone());
        let wt = PathBuf::from("/tmp/repo-entkeep/.lingxi/worktrees/wt");
        let original = PathBuf::from("/tmp/repo-entkeep");
        *bctx.worktree_session.lock().unwrap() = Some(tool_api::WorktreeSession {
            original_cwd: original.clone(),
            worktree_path: wt.clone(),
            branch_name: "worktree-wt".into(),
            base_commit: None,
            entered_existing: true,
            tmux_session_name: None,
        });
        bctx.session_cwd.swap(wt.clone(), vec![wt]);
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let _ = tool
            .call(json!({ "action": "keep" }), fresh_ctx(), fresh_tx())
            .await
            .expect("keep on an entered worktree is allowed");
        assert_eq!(tool.ctx.cwd(), original, "cwd restored");
        assert!(
            tool.ctx.worktree_session.lock().unwrap().is_none(),
            "session cleared"
        );
        assert_eq!(mock.removed().len(), 0);
    }

    #[tokio::test]
    async fn exit_keep_result_exact_message_restores_cwd_and_clears_session() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-keep"));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let original_cwd = PathBuf::from("/tmp/repo-keep");
        let worktree_path = PathBuf::from("/tmp/repo-keep/.lingxi/worktrees/feat");
        populate_session(&bctx, &original_cwd, &worktree_path, "worktree-feat", None);
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let res = tool
            .call(json!({ "action": "keep" }), fresh_ctx(), fresh_tx())
            .await
            .expect("keep must succeed");
        assert_eq!(
            res.model_content.as_deref(),
            Some(
                "Exited worktree. Your work is preserved at \
                 /tmp/repo-keep/.lingxi/worktrees/feat on branch worktree-feat. \
                 Session is now back in /tmp/repo-keep."
            )
        );
        assert_eq!(tool.ctx.cwd(), original_cwd, "cwd must be restored");
        assert!(
            tool.ctx.worktree_session.lock().unwrap().is_none(),
            "session record must be cleared"
        );
        // `keep` never removes anything.
        assert_eq!(mock.removed().len(), 0);
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&EXIT_WORKTREE_STARTED.to_string()));
        assert!(names.contains(&EXIT_WORKTREE_COMPLETED.to_string()));
        assert!(names.contains(&WORKTREE_KEPT.to_string()));
        assert!(!names.contains(&WORKTREE_REMOVED.to_string()));
    }

    // ── 206 `xCd` missing-original-cwd fallback (residual #2) ───────────────

    #[tokio::test]
    async fn exit_restore_cwd_uses_original_when_it_exists() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-r1"));
        let (bctx, _sink) = make_bctx(mock);
        let original = PathBuf::from("/tmp/repo-r1");
        let wt = PathBuf::from("/tmp/repo-r1/.lingxi/worktrees/feat");
        let tool = ExitWorktreeTool::new_with_dir_exists(bctx, |_| true);
        let r = tool.restore_cwd(&original, &wt);
        assert!(!r.original_cwd_missing);
        assert!(!r.fell_back_to_worktree);
        assert_eq!(r.restored_cwd, original);
        assert_eq!(tool.ctx.cwd(), original, "session swapped back to original");
    }

    #[tokio::test]
    async fn exit_restore_cwd_falls_back_to_worktree_when_original_gone() {
        // `original_cwd` gone but the worktree still exists (the `keep`
        // situation): 206's `n` = worktree path is the first fallback, so the
        // session lands on the worktree and `fell_back_to_worktree` is set.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-r2"));
        let (bctx, _sink) = make_bctx(mock);
        let original = PathBuf::from("/tmp/repo-r2-gone");
        let wt = PathBuf::from("/tmp/repo-r2/.lingxi/worktrees/feat");
        // Only `original` is missing; the worktree (and everything else) exists.
        let tool = ExitWorktreeTool::new_with_dir_exists(bctx, |p| {
            p != std::path::Path::new("/tmp/repo-r2-gone")
        });
        let r = tool.restore_cwd(&original, &wt);
        assert!(r.original_cwd_missing);
        assert!(
            r.fell_back_to_worktree,
            "landed on the worktree (first fallback)"
        );
        assert_eq!(r.restored_cwd, wt);
        assert_eq!(tool.ctx.cwd(), wt);
    }

    #[tokio::test]
    async fn exit_restore_cwd_falls_back_past_worktree_to_tmp_when_both_gone() {
        // BOTH `original_cwd` and the worktree are gone (the `remove`
        // situation, where the worktree was just deleted): the fallback skips
        // past the worktree to a real dir; here the stub reports only the
        // temp dir as existing, so `fell_back_to_worktree` is NOT set.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-r3"));
        let (bctx, _sink) = make_bctx(mock);
        let original = PathBuf::from("/tmp/repo-r3-gone");
        let wt = PathBuf::from("/tmp/repo-r3-gone/.lingxi/worktrees/feat");
        let tool =
            ExitWorktreeTool::new_with_dir_exists(bctx, |p| p == std::env::temp_dir().as_path());
        let r = tool.restore_cwd(&original, &wt);
        assert!(r.original_cwd_missing);
        assert!(!r.fell_back_to_worktree, "did NOT land on the worktree");
        assert_eq!(r.restored_cwd, std::env::temp_dir());
        assert_eq!(tool.ctx.cwd(), std::env::temp_dir());
    }

    #[tokio::test]
    async fn exit_keep_missing_original_cwd_message_falls_back_to_worktree() {
        // `keep` + `original_cwd` gone → 206 `kAs` missing branch with
        // `fellBackToWorktree` (worktree still on disk): the message names the
        // restored dir but OMITS the "Consider restarting" advice.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-km"));
        let (bctx, _sink) = make_bctx(mock.clone());
        let original_cwd = PathBuf::from("/tmp/repo-km-gone");
        let worktree_path = PathBuf::from("/tmp/repo-km/.lingxi/worktrees/feat");
        populate_session(&bctx, &original_cwd, &worktree_path, "worktree-feat", None);
        // Only the original cwd is missing.
        let tool = ExitWorktreeTool::new_with_dir_exists(bctx, |p| {
            p != std::path::Path::new("/tmp/repo-km-gone")
        });
        let res = tool
            .call(json!({ "action": "keep" }), fresh_ctx(), fresh_tx())
            .await
            .expect("keep must succeed even when original cwd is gone");
        assert_eq!(
            res.model_content.as_deref(),
            Some(
                "Exited worktree. Your work is preserved at \
                 /tmp/repo-km/.lingxi/worktrees/feat on branch worktree-feat. \
                 The original directory /tmp/repo-km-gone no longer exists, so \
                 the session is now in /tmp/repo-km/.lingxi/worktrees/feat."
            )
        );
        assert_eq!(tool.ctx.cwd(), worktree_path, "fell back to the worktree");
    }

    #[tokio::test]
    async fn exit_remove_missing_original_cwd_message_advises_restart() {
        // `remove` + BOTH original cwd and worktree gone → 206 `kAs` missing
        // branch WITHOUT `fellBackToWorktree`: appends
        // "Consider restarting LingXi from an existing directory." The exact
        // fallback dir is env-dependent ($HOME/tmp), so this asserts the stable
        // prefix + suffix rather than pinning it.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-rm"));
        let (bctx, _sink) = make_bctx(mock.clone());
        let original_cwd = PathBuf::from("/tmp/repo-rm-gone");
        let worktree_path = PathBuf::from("/tmp/repo-rm-gone/.lingxi/worktrees/feat");
        populate_session(&bctx, &original_cwd, &worktree_path, "worktree-feat", None);
        // Only the temp dir exists — original + worktree both gone.
        let tool =
            ExitWorktreeTool::new_with_dir_exists(bctx, |p| p == std::env::temp_dir().as_path());
        let res = tool
            .call(
                json!({ "action": "remove", "discard_changes": true }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("remove must succeed even when original cwd is gone");
        let msg = res.model_content.unwrap_or_default();
        assert!(
            msg.starts_with(
                "Exited and removed worktree at /tmp/repo-rm-gone/.lingxi/worktrees/feat."
            ),
            "unexpected prefix: {msg}"
        );
        assert!(
            msg.contains(
                "The original directory /tmp/repo-rm-gone no longer exists, so the session is now in "
            ),
            "missing kAs body: {msg}"
        );
        assert!(
            msg.ends_with(" Consider restarting LingXi from an existing directory."),
            "expected restart advice suffix: {msg}"
        );
    }

    #[tokio::test]
    async fn exit_remove_result_exact_message_with_discard_changes() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-remove"));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let original_cwd = PathBuf::from("/tmp/repo-remove");
        let worktree_path = PathBuf::from("/tmp/repo-remove/.lingxi/worktrees/feat");
        populate_session(&bctx, &original_cwd, &worktree_path, "worktree-feat", None);
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let res = tool
            .call(
                json!({ "action": "remove", "discard_changes": true }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("remove must succeed");
        assert_eq!(
            res.model_content.as_deref(),
            Some(
                "Exited and removed worktree at /tmp/repo-remove/.lingxi/worktrees/feat. \
                 Session is now back in /tmp/repo-remove."
            )
        );
        assert_eq!(tool.ctx.cwd(), original_cwd, "cwd must be restored");
        assert!(tool.ctx.worktree_session.lock().unwrap().is_none());
        assert_eq!(mock.removed().len(), 1);
        assert_eq!(mock.removed()[0].branch_name, "worktree-feat");
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&WORKTREE_REMOVED.to_string()));
        assert!(!names.contains(&WORKTREE_KEPT.to_string()));
    }

    #[tokio::test]
    async fn exit_remove_failure_is_non_fatal_with_exact_message() {
        // 206's `oht()` removal step returns a boolean rather than throwing;
        // `d===false` still returns a NORMAL (`is_error:false`) tool result
        // carrying the byte-exact "could not remove" message (em dash,
        // binary-verified @222215668) — it is NOT surfaced as a `ToolError`.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-remove-fail"));
        mock.script_remove_error(WorktreeError::Git("fatal: worktree is dirty".into()));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let original_cwd = PathBuf::from("/tmp/repo-remove-fail");
        let worktree_path = PathBuf::from("/tmp/repo-remove-fail/.lingxi/worktrees/feat");
        populate_session(&bctx, &original_cwd, &worktree_path, "worktree-feat", None);
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let res = tool
            .call(
                json!({ "action": "remove", "discard_changes": true }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("a non-fatal removal failure must still return a normal tool result");
        assert_eq!(
            res.model_content.as_deref(),
            Some(
                "Exited worktree but could not remove it \u{2014} kept at \
                 /tmp/repo-remove-fail/.lingxi/worktrees/feat. \
                 Session is now back in /tmp/repo-remove-fail."
            )
        );
        assert!(!res.is_error);
        // The session still exits (cwd restored, session record cleared)
        // even though the on-disk worktree could not be removed.
        assert_eq!(tool.ctx.cwd(), original_cwd, "cwd must still be restored");
        assert!(tool.ctx.worktree_session.lock().unwrap().is_none());
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&EXIT_WORKTREE_COMPLETED.to_string()));
        // `tengu_worktree_removed` must NOT fire — the removal did not
        // actually succeed.
        assert!(!names.contains(&WORKTREE_REMOVED.to_string()));
        assert!(!names.contains(&WORKTREE_KEPT.to_string()));
    }

    #[tokio::test]
    async fn exit_remove_clean_worktree_does_not_need_discard_changes() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-clean"));
        mock.script_change_summary(Some(WorktreeChangeSummary {
            changed_files: 0,
            commits: 0,
        }));
        let (bctx, _sink) = make_bctx(mock.clone());
        let original_cwd = PathBuf::from("/tmp/repo-clean");
        let worktree_path = PathBuf::from("/tmp/repo-clean/.lingxi/worktrees/feat");
        populate_session(
            &bctx,
            &original_cwd,
            &worktree_path,
            "worktree-feat",
            Some("deadbeef".to_string()),
        );
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let res = tool
            .call(json!({ "action": "remove" }), fresh_ctx(), fresh_tx())
            .await
            .expect("clean remove without discard_changes must succeed");
        assert_eq!(
            res.model_content.as_deref(),
            Some(
                "Exited and removed worktree at /tmp/repo-clean/.lingxi/worktrees/feat. \
                 Session is now back in /tmp/repo-clean."
            )
        );
        assert_eq!(mock.removed().len(), 1);
    }

    #[tokio::test]
    async fn exit_remove_cannot_verify_refuses_with_exact_message_and_no_swap() {
        // No scripted summary → mock returns `Ok(None)` ("unknown"/can't
        // verify), which must refuse a `remove` without `discard_changes`.
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-unknown"));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let original_cwd = PathBuf::from("/tmp/repo-unknown");
        let worktree_path = PathBuf::from("/tmp/repo-unknown/.lingxi/worktrees/feat");
        populate_session(&bctx, &original_cwd, &worktree_path, "worktree-feat", None);
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let err = tool
            .call(json!({ "action": "remove" }), fresh_ctx(), fresh_tx())
            .await
            .expect_err("unverifiable state must refuse");
        assert_eq!(
            format!("{err}"),
            format!(
                "invalid input: {}",
                exit_cannot_verify_message("/tmp/repo-unknown/.lingxi/worktrees/feat")
            )
        );
        // Refused BEFORE the cwd restore/removal — still in the worktree,
        // nothing removed, session record still present.
        assert_eq!(tool.ctx.cwd(), worktree_path);
        assert_eq!(mock.removed().len(), 0);
        assert!(tool.ctx.worktree_session.lock().unwrap().is_some());
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&EXIT_WORKTREE_FAILED.to_string()));
    }

    #[tokio::test]
    async fn exit_remove_dirty_singular_refuses_with_exact_pluralization() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-singular"));
        mock.script_change_summary(Some(WorktreeChangeSummary {
            changed_files: 1,
            commits: 0,
        }));
        let (bctx, _sink) = make_bctx(mock.clone());
        let worktree_path = PathBuf::from("/tmp/repo-singular/.lingxi/worktrees/feat");
        populate_session(
            &bctx,
            &PathBuf::from("/tmp/repo-singular"),
            &worktree_path,
            "worktree-feat",
            None,
        );
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let err = tool
            .call(json!({ "action": "remove" }), fresh_ctx(), fresh_tx())
            .await
            .expect_err("dirty worktree must refuse");
        assert_eq!(
            format!("{err}"),
            format!(
                "invalid input: {}",
                exit_has_changes_message("1 uncommitted file")
            )
        );
        assert_eq!(mock.removed().len(), 0);
        // Refused BEFORE the cwd restore/removal — still in the worktree
        // (the swap did not happen), and the shared session record is still
        // present (not cleared).
        assert_eq!(tool.ctx.cwd(), worktree_path);
        assert!(tool.ctx.worktree_session.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn exit_remove_dirty_plural_both_refuses_with_files_then_commits() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-plural"));
        mock.script_change_summary(Some(WorktreeChangeSummary {
            changed_files: 3,
            commits: 2,
        }));
        let (bctx, _sink) = make_bctx(mock.clone());
        let worktree_path = PathBuf::from("/tmp/repo-plural/.lingxi/worktrees/feat");
        populate_session(
            &bctx,
            &PathBuf::from("/tmp/repo-plural"),
            &worktree_path,
            "worktree-feat",
            Some("deadbeef".to_string()),
        );
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let err = tool
            .call(json!({ "action": "remove" }), fresh_ctx(), fresh_tx())
            .await
            .expect_err("dirty worktree must refuse");
        // Files fragment BEFORE commits fragment (oracle push order), commits
        // fragment carries "on {branch}".
        assert_eq!(
            format!("{err}"),
            format!(
                "invalid input: {}",
                exit_has_changes_message("3 uncommitted files and 2 commits on worktree-feat")
            )
        );
        assert_eq!(mock.removed().len(), 0);
        // Refused BEFORE the cwd restore/removal — still in the worktree
        // (the swap did not happen), and the shared session record is still
        // present (not cleared).
        assert_eq!(tool.ctx.cwd(), worktree_path);
        assert!(tool.ctx.worktree_session.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn exit_remove_with_discard_changes_true_forces_removal_despite_dirty() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-force"));
        mock.script_change_summary(Some(WorktreeChangeSummary {
            changed_files: 5,
            commits: 1,
        }));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let original_cwd = PathBuf::from("/tmp/repo-force");
        let worktree_path = PathBuf::from("/tmp/repo-force/.lingxi/worktrees/feat");
        populate_session(&bctx, &original_cwd, &worktree_path, "worktree-feat", None);
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let res = tool
            .call(
                json!({ "action": "remove", "discard_changes": true }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("discard_changes:true must force removal despite dirty state");
        // Discard note is commits-first, then uncommitted files (binary push
        // order), singular/plural applied independently to each count.
        assert_eq!(
            res.model_content.as_deref(),
            Some(
                "Exited and removed worktree at /tmp/repo-force/.lingxi/worktrees/feat. \
                 Discarded 1 commit and 5 uncommitted files. \
                 Session is now back in /tmp/repo-force."
            )
        );
        assert_eq!(mock.removed().len(), 1);
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&WORKTREE_REMOVED.to_string()));
    }

    #[tokio::test]
    async fn exit_detached_head_has_no_branch_suffix() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-detached-exit"));
        let (bctx, _sink) = make_bctx(mock);
        let original_cwd = PathBuf::from("/tmp/repo-detached-exit");
        let worktree_path = PathBuf::from("/tmp/repo-detached-exit/.lingxi/worktrees/feat");
        populate_session(&bctx, &original_cwd, &worktree_path, "HEAD", None);
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let res = tool
            .call(json!({ "action": "keep" }), fresh_ctx(), fresh_tx())
            .await
            .expect("keep must succeed");
        assert_eq!(
            res.model_content.as_deref(),
            Some(
                "Exited worktree. Your work is preserved at \
                 /tmp/repo-detached-exit/.lingxi/worktrees/feat. \
                 Session is now back in /tmp/repo-detached-exit."
            )
        );
    }

    // ===== worktree tmux launch plan, Task 5: ExitWorktree tmux keep/remove ===

    /// Records the `tmux` argv (if any) [`ExitWorktreeTool`] runs through
    /// `ctx.process`, and returns a canned exit code — a hermetic double for
    /// the [`lingxi_core::host::ProcessRunner`] seam (mirrors the `MockRunner` pattern
    /// in `platforms/posix/src/worktree_tmux.rs`'s tests).
    struct RecordingProcess {
        exit_code: i32,
        stderr: String,
        recorded: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    }

    impl RecordingProcess {
        fn new(exit_code: i32, stderr: &str) -> Self {
            Self {
                exit_code,
                stderr: stderr.to_string(),
                recorded: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.recorded.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl lingxi_core::host::ProcessRunner for RecordingProcess {
        async fn run(
            &self,
            cmd: &lingxi_core::host::SandboxedCommand,
        ) -> Result<lingxi_core::host::ProcessOutput, lingxi_core::host::ProcessError> {
            self.recorded
                .lock()
                .unwrap()
                .push((cmd.inner().command.clone(), cmd.inner().args.clone()));
            Ok(lingxi_core::host::ProcessOutput {
                stdout: String::new(),
                stderr: self.stderr.clone(),
                exit_code: self.exit_code,
                timed_out: false,
            })
        }

        async fn spawn_background(
            &self,
            _cmd: &lingxi_core::host::SandboxedCommand,
        ) -> Result<lingxi_core::host::ProcessHandle, lingxi_core::host::ProcessError> {
            Err(lingxi_core::host::ProcessError::Unsupported)
        }

        async fn kill(
            &self,
            _handle: &lingxi_core::host::ProcessHandle,
        ) -> Result<(), lingxi_core::host::ProcessError> {
            Ok(())
        }

        fn is_available(&self) -> bool {
            true
        }
    }

    /// Like [`populate_session`], but additionally sets
    /// `tmux_session_name: Some(tmux_session_name)` — the launch flow's
    /// `--tmux` boot consumption (Task 4) is what would populate this in
    /// production; these tests populate it directly to exercise
    /// `ExitWorktreeTool`'s tmux keep/remove path in isolation.
    fn populate_session_with_tmux(
        bctx: &BuiltinToolContext,
        original_cwd: &std::path::Path,
        worktree_path: &std::path::Path,
        branch_name: &str,
        tmux_session_name: &str,
    ) {
        *bctx.worktree_session.lock().unwrap() = Some(tool_api::WorktreeSession {
            original_cwd: original_cwd.to_path_buf(),
            worktree_path: worktree_path.to_path_buf(),
            branch_name: branch_name.to_string(),
            base_commit: None,
            entered_existing: false,
            tmux_session_name: Some(tmux_session_name.to_string()),
        });
        bctx.session_cwd.swap(
            worktree_path.to_path_buf(),
            vec![worktree_path.to_path_buf()],
        );
    }

    #[tokio::test]
    async fn exit_remove_with_tmux_session_kills_it_and_still_removes() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-tmux-remove"));
        let (mut bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let process = Arc::new(RecordingProcess::new(0, ""));
        bctx.process = process.clone();
        let original_cwd = PathBuf::from("/tmp/repo-tmux-remove");
        let worktree_path = PathBuf::from("/tmp/repo-tmux-remove/.lingxi/worktrees/feat");
        populate_session_with_tmux(
            &bctx,
            &original_cwd,
            &worktree_path,
            "worktree-feat",
            "wt-x",
        );
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let res = tool
            .call(
                json!({ "action": "remove", "discard_changes": true }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("remove must succeed even with a tmux session attached");

        // The `tmux kill-session -t wt-x` argv was issued.
        assert_eq!(
            process.calls(),
            vec![("tmux".to_string(), build_worktree_tmux_kill_argv("wt-x"))]
        );
        // Removal still succeeded — the tmux kill did not block it.
        assert_eq!(mock.removed().len(), 1);
        assert!(tool.ctx.worktree_session.lock().unwrap().is_none());
        // `remove` never surfaces `tmux_session_name` in `data` (206 doesn't
        // either — the session is already dead by the time the result is
        // built).
        assert!(res.data.get("tmux_session_name").is_none());
        assert_eq!(
            res.model_content.as_deref(),
            Some(
                "Exited and removed worktree at /tmp/repo-tmux-remove/.lingxi/worktrees/feat. \
                 Session is now back in /tmp/repo-tmux-remove."
            )
        );
    }

    #[tokio::test]
    async fn exit_remove_tmux_kill_failure_is_non_fatal_and_still_removes() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-tmux-kill-fail"));
        let (mut bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        // Nonzero exit ⇒ `kill_worktree_tmux_session` returns `Err`.
        let process = Arc::new(RecordingProcess::new(1, "no such session"));
        bctx.process = process.clone();
        let original_cwd = PathBuf::from("/tmp/repo-tmux-kill-fail");
        let worktree_path = PathBuf::from("/tmp/repo-tmux-kill-fail/.lingxi/worktrees/feat");
        populate_session_with_tmux(
            &bctx,
            &original_cwd,
            &worktree_path,
            "worktree-feat",
            "wt-y",
        );
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let res = tool
            .call(
                json!({ "action": "remove", "discard_changes": true }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("a failed tmux kill must NOT fail the remove");

        assert_eq!(
            process.calls(),
            vec![("tmux".to_string(), build_worktree_tmux_kill_argv("wt-y"))]
        );
        // Removal still succeeded despite the kill failure (non-fatal, warn-and-continue).
        assert_eq!(mock.removed().len(), 1);
        assert_eq!(
            res.model_content.as_deref(),
            Some(
                "Exited and removed worktree at /tmp/repo-tmux-kill-fail/.lingxi/worktrees/feat. \
                 Session is now back in /tmp/repo-tmux-kill-fail."
            )
        );
    }

    #[tokio::test]
    async fn exit_keep_with_tmux_session_does_not_kill_and_surfaces_name() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-tmux-keep"));
        let (mut bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let process = Arc::new(RecordingProcess::new(0, ""));
        bctx.process = process.clone();
        let original_cwd = PathBuf::from("/tmp/repo-tmux-keep");
        let worktree_path = PathBuf::from("/tmp/repo-tmux-keep/.lingxi/worktrees/feat");
        populate_session_with_tmux(
            &bctx,
            &original_cwd,
            &worktree_path,
            "worktree-feat",
            "wt-z",
        );
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let res = tool
            .call(json!({ "action": "keep" }), fresh_ctx(), fresh_tx())
            .await
            .expect("keep must succeed");

        // NO tmux call was issued on `keep`.
        assert!(
            process.calls().is_empty(),
            "keep must not kill the tmux session"
        );
        assert_eq!(mock.removed().len(), 0);
        // The session name is surfaced in `data` for reattach.
        assert_eq!(res.data["tmux_session_name"], json!("wt-z"));
        // ... and in the model-facing message, as an additive reattach
        // clause — LEADING-SPACE-joined inline into the single-string
        // message (206's `${g}` suffix), NOT on its own line.
        assert_eq!(
            res.model_content.as_deref(),
            Some(
                "Exited worktree. Your work is preserved at \
                 /tmp/repo-tmux-keep/.lingxi/worktrees/feat on branch worktree-feat. \
                 Session is now back in /tmp/repo-tmux-keep. \
                 Tmux session wt-z is still running; reattach with: tmux attach -t wt-z"
            )
        );
    }

    #[tokio::test]
    async fn exit_inert_without_tmux_session_name_issues_no_tmux_call() {
        // `tmux_session_name: None` (the common case — reached whenever the
        // session wasn't launched with `--worktree --tmux`) must behave
        // exactly as before this task: no `tmux` invocation on either
        // `remove` or `keep`, and no `tmux_session_name` in `data`.
        for action in ["remove", "keep"] {
            let mock = Arc::new(MockWorktreeManager::with_root(format!(
                "/tmp/repo-tmux-inert-{action}"
            )));
            let (mut bctx, sink) = make_bctx(mock.clone());
            bctx.bus.attach_sink(sink.clone()).await;
            let process = Arc::new(RecordingProcess::new(0, ""));
            bctx.process = process.clone();
            let original_cwd = PathBuf::from(format!("/tmp/repo-tmux-inert-{action}"));
            let worktree_path = PathBuf::from(format!(
                "/tmp/repo-tmux-inert-{action}/.lingxi/worktrees/feat"
            ));
            populate_session(&bctx, &original_cwd, &worktree_path, "worktree-feat", None);
            let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
            let input = if action == "remove" {
                json!({ "action": "remove", "discard_changes": true })
            } else {
                json!({ "action": "keep" })
            };
            let res = tool
                .call(input, fresh_ctx(), fresh_tx())
                .await
                .unwrap_or_else(|e| panic!("{action} must succeed: {e}"));
            assert!(
                process.calls().is_empty(),
                "no tmux call expected for {action} with tmux_session_name:None"
            );
            assert!(res.data.get("tmux_session_name").is_none());
        }
    }

    #[test]
    fn exit_is_destructive_and_user_facing_name_follow_action() {
        let mock = Arc::new(MockWorktreeManager::new());
        let (bctx, _sink) = make_bctx(mock);
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        assert!(tool.is_destructive(&json!({ "action": "remove" })));
        assert!(!tool.is_destructive(&json!({ "action": "keep" })));
        assert_eq!(
            tool.user_facing_name_for_input(&json!({ "action": "remove" }))
                .as_deref(),
            Some("Cleaning up worktree")
        );
        assert_eq!(
            tool.user_facing_name_for_input(&json!({ "action": "keep" }))
                .as_deref(),
            Some("Exiting worktree")
        );
    }

    #[test]
    fn exit_should_defer_is_true() {
        let mock = Arc::new(MockWorktreeManager::new());
        let (bctx, _sink) = make_bctx(mock);
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        assert!(tool.should_defer());
    }

    #[test]
    fn exit_input_schema_matches_206_contract() {
        let mock = Arc::new(MockWorktreeManager::new());
        let (bctx, _sink) = make_bctx(mock);
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let schema = tool.input_schema();
        assert_eq!(schema["required"], json!(["action"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(["keep", "remove"])
        );
        assert_eq!(
            schema["properties"]["discard_changes"]["type"],
            json!("boolean")
        );
    }

    #[tokio::test]
    async fn exit_description_is_206_byte_exact() {
        let mock = Arc::new(MockWorktreeManager::new());
        let (bctx, _sink) = make_bctx(mock);
        let tool = ExitWorktreeTool::new_assuming_cwd_exists(bctx);
        let desc = tool
            .description(
                &json!({}),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        assert_eq!(
            desc,
            "Exits a worktree session created by EnterWorktree and restores the original working directory"
        );
    }

    // ===== EnterWorktree → ExitWorktree round trip =============================

    #[tokio::test]
    async fn enter_then_exit_round_trip_records_restores_and_clears() {
        let mock = Arc::new(MockWorktreeManager::with_root("/tmp/repo-roundtrip"));
        let (bctx, sink) = make_bctx(mock.clone());
        bctx.bus.attach_sink(sink.clone()).await;
        let boot_cwd = bctx.cwd();

        // No session before EnterWorktree.
        assert!(bctx.worktree_session.lock().unwrap().is_none());

        let enter = EnterWorktreeTool::new(bctx.clone());
        let enter_res = enter
            .call(json!({ "name": "roundtrip" }), fresh_ctx(), fresh_tx())
            .await
            .expect("enter must succeed");
        let worktree_path = PathBuf::from(enter_res.data["path"].as_str().unwrap());

        // EnterWorktree recorded the session AND swapped the cwd.
        assert_eq!(bctx.cwd(), worktree_path);
        {
            let session = bctx.worktree_session.lock().unwrap();
            let session = session.as_ref().expect("session must be recorded");
            assert_eq!(session.original_cwd, boot_cwd);
            assert_eq!(session.worktree_path, worktree_path);
            assert_eq!(session.branch_name, "worktree-roundtrip");
        }

        let exit = ExitWorktreeTool::new(bctx.clone());
        let exit_res = exit
            .call(json!({ "action": "keep" }), fresh_ctx(), fresh_tx())
            .await
            .expect("exit must succeed");
        assert_eq!(
            exit_res.model_content.as_deref(),
            Some(
                format!(
                    "Exited worktree. Your work is preserved at {} on branch worktree-roundtrip. \
                     Session is now back in {}.",
                    worktree_path.display(),
                    boot_cwd.display()
                )
                .as_str()
            )
        );
        assert_eq!(bctx.cwd(), boot_cwd, "cwd restored to the pre-enter cwd");
        assert!(
            bctx.worktree_session.lock().unwrap().is_none(),
            "session cleared after exit"
        );
    }

    // ===== 206/207 EnterWorktree::check_permissions relocation gate (H-CHG-P3A) =

    /// A context whose live session cwd is `cwd` (trusted set = `[cwd]`). The
    /// mock worktree manager is unused here — `check_permissions` never touches
    /// it (it reads only cwd + the filesystem).
    fn bctx_at(cwd: PathBuf) -> BuiltinToolContext {
        ctx_for_file_tools(make_dummy_fs(), Arc::new(AnalyticsBus::new()), vec![cwd])
    }

    /// A git-repo scaffold on disk (`<root>/.git`, `<root>/.lingxi/worktrees/`),
    /// with `root` already canonicalized so the tests' expected strings are
    /// symlink-free (macOS `/var`→`/private/var`).
    fn scaffold_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".lingxi/worktrees")).unwrap();
        (dir, root)
    }

    /// No `path` (create) ⇒ the tool self-allows: creating a worktree under the
    /// managed root never relocates the permission root (206 `!e.path`).
    #[tokio::test]
    async fn check_permissions_create_allows() {
        let (_d, root) = scaffold_repo();
        let tool = EnterWorktreeTool::new(bctx_at(root));
        let res = tool
            .check_permissions(&json!({ "name": "feat" }), &fresh_ctx())
            .await;
        assert!(
            matches!(res, PermissionResult::Allow { .. }),
            "create must allow, got {res:?}"
        );
    }

    /// A `path` resolving strictly under `<repo>/.lingxi/worktrees/` ⇒ allow,
    /// with `path` rewritten to the realpath (206 `t?.managed` branch,
    /// `updatedInput:{...e, path:t.targetReal}`).
    #[tokio::test]
    async fn check_permissions_managed_path_allows_with_realpath_rewrite() {
        let (_d, root) = scaffold_repo();
        let wt = root.join(".lingxi/worktrees/foo");
        std::fs::create_dir_all(&wt).unwrap();
        let tool = EnterWorktreeTool::new(bctx_at(root.clone()));
        let res = tool
            .check_permissions(&json!({ "path": wt.to_str().unwrap() }), &fresh_ctx())
            .await;
        match res {
            PermissionResult::Allow { updated_input, .. } => {
                let updated = updated_input.expect("managed allow rewrites `path` to realpath");
                assert_eq!(updated["path"], json!(wt.to_string_lossy().into_owned()));
            }
            other => panic!("managed path must allow, got {other:?}"),
        }
    }

    /// A `path` outside the managed root ⇒ ask, with the byte-exact 206/207
    /// relocation confirmation + `SafetyCheck` reason (`classifierApprovable:!1`).
    #[tokio::test]
    async fn check_permissions_unmanaged_path_asks_byte_exact() {
        let (_d, root) = scaffold_repo();
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let tool = EnterWorktreeTool::new(bctx_at(root.clone()));
        let res = tool
            .check_permissions(&json!({ "path": outside.to_str().unwrap() }), &fresh_ctx())
            .await;
        let o = outside.to_string_lossy();
        match res {
            PermissionResult::Ask {
                reason,
                prompt,
                pending_classifier_check,
                ..
            } => {
                assert_eq!(prompt.title, "EnterWorktree");
                assert!(prompt.options.is_empty());
                assert_eq!(
                    prompt.message,
                    format!(
                        "Enter the worktree at \"{o}\"? This moves the session's working directory and write access there, and loads project configuration (LINGXI.md, settings) from that location."
                    )
                );
                match reason {
                    PermissionDecisionReason::SafetyCheck {
                        reason,
                        classifier_approvable,
                        ..
                    } => {
                        assert!(!classifier_approvable, "must not be classifier-approvable");
                        assert_eq!(
                            reason,
                            format!(
                                "permission-root relocation to \"{o}\" \u{2014} a model-supplied worktree outside .lingxi/worktrees/"
                            )
                        );
                    }
                    other => panic!("expected SafetyCheck reason, got {other:?}"),
                }
                assert!(pending_classifier_check.is_none());
            }
            other => panic!("unmanaged path must ask, got {other:?}"),
        }
    }

    /// A `path` carrying a control/format char is displayed sanitized (each such
    /// char → U+FFFD) AND flagged ` (path sanitized for display)` — 206 `s`
    /// clause. The exotic char makes the target non-existent, so there is no
    /// realpath and no "resolves to" clause.
    #[tokio::test]
    async fn check_permissions_sanitizes_display_and_flags_it() {
        let (_d, root) = scaffold_repo();
        // U+200B ZERO WIDTH SPACE (general category Cf).
        let raw = format!("{}/wt\u{200B}x", root.to_string_lossy());
        let tool = EnterWorktreeTool::new(bctx_at(root.clone()));
        let res = tool
            .check_permissions(&json!({ "path": raw }), &fresh_ctx())
            .await;
        let sanitized = format!("{}/wt\u{FFFD}x", root.to_string_lossy());
        match res {
            PermissionResult::Ask { prompt, .. } => assert_eq!(
                prompt.message,
                format!(
                    "Enter the worktree at \"{sanitized}\" (path sanitized for display)? This moves the session's working directory and write access there, and loads project configuration (LINGXI.md, settings) from that location."
                )
            ),
            other => panic!("must ask, got {other:?}"),
        }
    }

    /// When the realpath differs from the lexical resolve (a symlinked target),
    /// the prompt gains the ` (resolves to "…")` clause — 206 `a` clause.
    #[cfg(unix)]
    #[tokio::test]
    async fn check_permissions_shows_resolves_to_for_symlinked_target() {
        let (_d, root) = scaffold_repo();
        let real_target = root.join("real_wt");
        std::fs::create_dir_all(&real_target).unwrap();
        let link = root.join("link_wt");
        std::os::unix::fs::symlink(&real_target, &link).unwrap();
        let tool = EnterWorktreeTool::new(bctx_at(root.clone()));
        let res = tool
            .check_permissions(&json!({ "path": link.to_str().unwrap() }), &fresh_ctx())
            .await;
        let o = link.to_string_lossy();
        let i = real_target.to_string_lossy();
        match res {
            PermissionResult::Ask { prompt, .. } => assert_eq!(
                prompt.message,
                format!(
                    "Enter the worktree at \"{o}\" (resolves to \"{i}\")? This moves the session's working directory and write access there, and loads project configuration (LINGXI.md, settings) from that location."
                )
            ),
            other => panic!("must ask, got {other:?}"),
        }
    }

    /// [`sanitize_display`] — byte-faithful `Dxs`+`AIy`: clean paths pass
    /// through; control/format/exotic-space chars and fancy/ASCII quotes become
    /// U+FFFD; a plain U+0020 space is preserved.
    #[test]
    fn sanitize_display_matches_dxs_aiy() {
        assert_eq!(
            sanitize_display("/repo/clean-path_1.2"),
            "/repo/clean-path_1.2"
        );
        assert_eq!(sanitize_display("a\u{200B}b"), "a\u{FFFD}b"); // Cf zero-width space
        assert_eq!(sanitize_display("a\u{202E}b"), "a\u{FFFD}b"); // Cf RTL override
        assert_eq!(sanitize_display("a\u{00A0}b"), "a\u{FFFD}b"); // Zs no-break space
        assert_eq!(sanitize_display("a\u{2800}b"), "a\u{FFFD}b"); // braille blank
        assert_eq!(sanitize_display("a b"), "a b"); // U+0020 kept
        assert_eq!(
            sanitize_display("a\u{201C}b\u{201D}c"),
            "a\u{FFFD}b\u{FFFD}c"
        ); // fancy quotes
        assert_eq!(sanitize_display("say \"hi\""), "say \u{FFFD}hi\u{FFFD}"); // ASCII dquote
    }

    /// [`lexical_resolve`] mirrors Node `path.resolve(cwd, p)`: collapses
    /// `.`/`..`/dup-separators without touching the filesystem; an absolute `p`
    /// ignores `cwd`.
    #[test]
    fn lexical_resolve_matches_node_semantics() {
        let cwd = Path::new("/repo/sub");
        assert_eq!(lexical_resolve(cwd, "../foo"), PathBuf::from("/repo/foo"));
        assert_eq!(
            lexical_resolve(cwd, "./x/./y"),
            PathBuf::from("/repo/sub/x/y")
        );
        assert_eq!(
            lexical_resolve(cwd, "/abs/./path/.."),
            PathBuf::from("/abs")
        );
        assert_eq!(
            lexical_resolve(cwd, "a/b/../c"),
            PathBuf::from("/repo/sub/a/c")
        );
    }
}
